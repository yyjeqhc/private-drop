# Logging Guidelines

## Logging stack

Server code uses `tracing` / `tracing-subscriber`. Prefer structured fields for lifecycle, target, timing, and reason-code data over formatted prose when the values will be analyzed.

Examples include startup/runtime logs in `src/lib.rs` and tool lifecycle events in `src/tool_request_trace.rs`.

## Levels and content

- `info`: normal lifecycle facts useful to operators, such as startup identity, bound addresses, tool lifecycle completion, and selected runtime metadata.
- `warn`: degraded/recoverable state that needs operator attention but does not invalidate the authoritative operation.
- `error`: invariant/configuration failures or unrecoverable adapter/runtime failures.
- Keep noisy per-item detail out of ordinary logs unless a diagnostic mode explicitly enables it.

## Privacy and forensic tracing

- Never log Authorization headers, tokens, private keys, raw external session identifiers, or credentials.
- Metadata-only tracing should remain safe enough for routine dogfood collection.
- `WEBCODEX_TOOL_REQUEST_TRACE=full` is explicitly sensitive forensic mode: payload traces can include file contents, command input/output, and user messages, and therefore have bounded local retention outside the canonical runtime DB.
- Host-owned opaque identifiers such as ChatGPT `openai/session` are hashed/domain-separated before durable/window use; telemetry should consume the hashed `ClientWindow` projection, not the raw value.

## Scenario: Window-scoped tool-request trace correlation

### 1. Scope / Trigger

Use this contract when an inbound MCP or HTTP adapter has already resolved a `ClientWindow` and tool-request lifecycle tracing is enabled. The purpose is diagnostic grouping of repeated calls from one logical client window.

### 2. Signatures

- Adapter boundary: `ToolRequestLifecycle::set_client_window(Option<&ClientWindow>)`.
- Trace fields: `client_window_key` and `client_window_source`.
- The lifecycle accepts no raw opaque window/session string API.

### 3. Contracts

- `client_window_key` is exactly the existing domain-separated `ClientWindow` hash; tracing must not rehash or mint a second identity.
- `client_window_source` is the existing `ClientWindow` source label.
- Persisted full-trace metadata uses null/absence when no window resolved. Structured logs may render an absent-field sentinel, but it is not an identity.
- Both fields are diagnostic metadata only and must never become authorization, Workflow Session/provenance, Connector selection, retry, execution, or idempotency identity.

### 4. Validation & Error Matrix

- Valid adapter window -> emit hash + source.
- Missing/malformed adapter window -> emit no window identity; do not infer from credential, project, connection, audit, or prior request state.
- Raw `openai/session` -> must be consumed only by the existing `ClientWindow` resolver and must not enter lifecycle logs or trace metadata.
- Trace storage/logging failure -> preserve existing fail-open diagnostic behavior; tool execution semantics do not change.

### 5. Good / Base / Bad Cases

- Good: two independent tool requests from the same resolved `ClientWindow` have different `server_trace_id` values but the same `client_window_key` + source.
- Base: no resolved window produces null/absent window fields and otherwise normal lifecycle tracing.
- Bad: using a credential, project id, MCP connection, Workflow Session, or raw host session value as a fallback correlation key.

### 6. Tests Required

- Persisted full-trace lifecycle metadata contains the expected hashed key/source and never the known raw opaque input.
- Repeated lifecycle instances supplied the same `ClientWindow` expose the same window key/source while retaining distinct request trace ids.
- A lifecycle with no window has no fabricated key/source.
- Existing `ClientWindow` tests continue covering malformed Stateless MCP `_meta["openai/session"]` as unidentified.

### 7. Wrong vs Correct

**Wrong:** pass `_meta["openai/session"]` or another transport credential directly to tracing, or invent a fallback identity when it is missing.

**Correct:** let the adapter's existing resolver produce `Option<ClientWindow>`, then attach only that resolved value to `ToolRequestLifecycle`.
