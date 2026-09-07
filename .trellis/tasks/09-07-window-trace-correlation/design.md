# Design — Window-scoped tool request trace correlation

## Boundary

The behavior gap is narrow: adapters already resolve a safe `ClientWindow`, and `ToolRequestLifecycle` already owns inbound trace lifecycle metadata, but the lifecycle never receives the resolved window. The change therefore belongs at the adapter-to-lifecycle boundary, not in authentication, ToolRuntime authority, Workflow Session state, Action Audit identity, or Connector persistence.

Expected product-code changes:

- `src/tool_request_trace.rs`: store an optional safe `ClientWindow` projection on `ToolRequestLifecycle` and include its hashed key/source in lifecycle log and full-trace metadata events.
- `src/mcp.rs`: after existing legacy/stateless MCP window resolution, attach `window.identity.as_ref()` to the lifecycle.
- `src/runtime_http.rs`: after existing `api_window` resolution, attach that window to the lifecycle.
- Focused tests in the existing trace/client-window or adapter test modules as needed.
- `.trellis/spec/backend/logging-guidelines.md`: document the concrete trace field semantics if implementation establishes stable field names.

Explicitly not changing: `ClientWindow` hashing/validation, ToolCallContext authority behavior, Workflow Session selection/provenance, Connector task mapping, Action Audit session identity, request/job correlation, database schemas, or model-facing tool schemas/results.

## Data Flow

```text
adapter-owned raw opaque window value
        |
        | existing validation + domain-separated SHA-256
        v
   ClientWindow { key, source }
        |                       \
        | existing business     \ new diagnostic-only copy/projection
        | context                 v
        v                 ToolRequestLifecycle
 ToolCallContext.window          |
                                +--> structured tracing fields
                                +--> full trace metadata event fields
```

For Stateless MCP 2026, the raw `_meta["openai/session"]` is consumed only by `stateless_mcp_window`; the lifecycle receives only `ClientWindow`. For invalid/missing metadata, the resolved identity remains `None` and the lifecycle records no fallback identity.

## Trace Metadata Contract

Use two additive fields on lifecycle events:

- `client_window_key`: the existing `ClientWindow::key()` 64-character lowercase SHA-256 value when present.
- `client_window_source`: the existing bounded/static `ClientWindow::source()` label when present.

Structured logs may use a sentinel for absent optional fields if required by the tracing macro, but persisted JSON trace metadata should represent absence as `null` (or omit it) rather than manufacture an identity. No raw adapter value is accepted by the lifecycle API.

The lifecycle should clone/store the already-resolved `ClientWindow` (or an equivalent private safe projection) rather than rehashing. This prevents a second telemetry-specific identity domain and makes the privacy boundary explicit in the type accepted by tracing.

## Ordering and Adapter Compatibility

### MCP

Keep protocol validation and current `mcp_window` / `stateless_mcp_window` resolution unchanged. Immediately after the existing `window` is resolved, attach `window.identity.as_ref()` to the lifecycle. Earlier pre-parse/protocol-error events legitimately have no resolved window because the adapter has not established one yet. Subsequent request/dispatch/response events can be grouped by the resolved window.

### HTTP/API

Keep `api_window(req, res)` unchanged, including hosted conversation header precedence and first-party cookie minting. Attach the returned `ClientWindow` to the lifecycle at the existing resolution point before ToolRuntime dispatch. Do not move cookie creation into tracing and do not add a new request field.

### Legacy MCP

Use the same optional `ClientWindow` returned by the existing legacy MCP resolver. A missing legacy session header remains no identity for non-initialize requests; no fallback is added.

## Privacy and Security

- The tracing lifecycle API accepts only `ClientWindow`, never an opaque raw string.
- No field is added to model-facing request/response schemas.
- The window metadata is observation-only and never read by kernel authority, Workflow Session, Connector continuity, retry, or idempotency code.
- Existing full-trace payload capture behavior is not broadened. In particular, MCP request params are not newly captured, avoiding persistence of raw `_meta["openai/session"]`.
- Trace storage failure remains fail-open for diagnostics and cannot affect tool execution.

## Validation Strategy

1. Unit-level trace test in full mode: attach a `ClientWindow` created from a known raw opaque value, emit lifecycle metadata, flush the trace writer, and assert persisted metadata contains the expected hash/source but not the raw opaque value.
2. Stability test: two independent lifecycle trace IDs using the same window must carry the same window key/source.
3. Absence test: lifecycle without a window must not contain a fabricated key/source.
4. Existing `client_window` tests continue proving malformed stateless OpenAI identity resolves to `None` and source-domain separation remains intact.
5. Run formatting, the focused trace/client-window tests, and the smallest relevant crate/package check.

## Rollback

The change is additive metadata. Reverting the lifecycle fields/setter and the two adapter setter calls restores previous tracing without data/schema migration.