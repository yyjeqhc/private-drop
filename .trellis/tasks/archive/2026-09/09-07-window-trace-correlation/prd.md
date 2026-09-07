# Window-scoped tool request trace correlation

## Goal

Allow operators to aggregate inbound model-facing tool-request trace events by the already-resolved logical ChatGPT/client window, while preserving the existing privacy and authority boundaries around `ClientWindow`.

## Background

- `ClientWindow` already validates opaque adapter-provided window values and immediately domain-separates/hashes them under `webcodex.client-window.v1`; only the hash key and source survive resolution.
- Stateless MCP 2026 resolves `_meta["openai/session"]` into `ClientWindow(source = "openai-session")`; missing or malformed metadata yields no window identity.
- Legacy MCP and HTTP/API adapters already resolve their own natural `ClientWindow` sources.
- `ToolRequestLifecycle` currently emits lifecycle metadata to structured logs and, in full trace mode, to the bounded trace store, but it has no window-scoped correlation fields.
- Workflow Session selection/provenance, Connector continuity, authorization, retry, and idempotency already have separate explicit contracts and must remain separate from trace correlation.

## Requirements

1. An already-resolved `ClientWindow` may be attached to the lifecycle for one inbound tool request as diagnostic metadata only.
2. Lifecycle trace metadata must expose a stable hashed window key and its existing `ClientWindow` source so repeated calls from the same resolved window can be grouped.
3. Raw host/transport window values, including `_meta["openai/session"]`, must never be copied into lifecycle logs, the trace store, the runtime database, or model-facing tool output.
4. Missing or malformed window identity must remain absent. Trace correlation must not fall back to credentials, project identity, connection state, audit session identity, prior requests, or any other inferred identity.
5. Window trace metadata must not participate in authorization, Workflow Session selection or recorder provenance, Connector task selection, trusted provenance, retry decisions, execution identity, or idempotency identity.
6. Preserve current Stateless MCP 2026, legacy MCP, and HTTP/API window-resolution semantics. Reuse `ClientWindow`; do not create a ChatGPT-specific telemetry identity type or hashing scheme.
7. Keep the change local to tool-request tracing and adapter wiring. Do not add analytics tables, dashboards, database migrations, or unrelated refactors.
8. Add focused regression coverage for stable safe metadata and absence/fail-closed behavior; update the existing logging guidance only where it clarifies the new trace contract.

## Acceptance Criteria

- [x] Two lifecycle events supplied the same resolved `ClientWindow` expose the same 64-hex hashed window key and the same source while retaining independent per-request `server_trace_id` values.
- [x] A Stateless MCP 2026 `openai/session` value is never present verbatim in persisted lifecycle metadata; only its existing `ClientWindow` hash/source projection is eligible for tracing.
- [x] A lifecycle with no valid `ClientWindow` emits no synthetic/fallback window identity.
- [x] Legacy MCP and HTTP/API continue to use their existing `ClientWindow` resolution paths, with no change to authority or continuity decisions.
- [x] No new database/schema, model-facing argument/result field, Workflow Session selector, retry token, or idempotency key is introduced.
- [x] Focused tests and the smallest relevant Rust validation pass, followed by an independent final diff review.

## Out of Scope

- Database-backed window analytics, dashboards, aggregate statistics, or a new telemetry persistence model.
- Changing how ChatGPT/OpenAI, legacy MCP, or HTTP opaque window identities are validated or hashed.
- Inferring a window when an adapter does not provide one.
- Reusing the window hash as authority, provenance, Session continuity, retry, or idempotency state.

## Open Questions

None. The user supplied the product, privacy, compatibility, and scope decisions explicitly.
