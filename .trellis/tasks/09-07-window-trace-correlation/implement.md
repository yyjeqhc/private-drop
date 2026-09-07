# Implementation Plan — Window-scoped tool request trace correlation

1. Re-read the current task artifacts and Trellis backend/guides pre-development checklists before product-code edits.
2. Add optional `ClientWindow` metadata to `ToolRequestLifecycle` using the existing resolved abstraction only.
   - Add a narrow setter or constructor-independent attachment method so adapters can resolve the window at their existing point in request handling.
   - Add `client_window_key` and `client_window_source` to structured lifecycle logs and full trace metadata events.
   - Keep absent identity absent and do not alter payload capture.
3. Wire existing adapter resolutions into the lifecycle.
   - MCP: attach `window.identity.as_ref()` immediately after the current protocol-era-specific resolution.
   - HTTP/API: attach the result of the existing `api_window` call before runtime dispatch.
   - Do not change ToolCallContext or business continuity semantics.
4. Add focused tests in the existing trace test module proving safe persistence, stable grouping across independent request trace IDs, and no fallback metadata when no window is attached.
5. If stable field names are established, update backend logging guidance with their diagnostic-only/privacy semantics.
6. Run Trellis quality check and focused validation:
   - `cargo fmt` for affected Rust files/workspace formatting.
   - focused tests for `tool_request_trace` / `client_window` behavior.
   - smallest relevant Cargo check for the server crate (avoid full workspace/E2E unless a focused gap requires it).
7. Independently review the final diff for scope, raw-session leakage, authority/provenance/retry/idempotency cross-wiring, adapter semantic changes, and accidental payload-capture expansion.
8. Recheck branch/HEAD/worktree and create one local task commit only. Do not push, create a PR, deploy, or restart services.

## Review Gates

- No raw `openai/session` appears in new tracing/logging/storage paths.
- No fallback identity is introduced.
- No database/model-facing schema changes.
- Window metadata is write-only diagnostic context from adapters into tracing; no business subsystem consumes it.
- Existing adapter window resolution remains authoritative and unchanged.

## Trellis Closeout Constraint

The user explicitly requires one local commit and no external side effects. Trellis archive/journal helpers that would auto-create additional commits must not be run if doing so would violate that one-commit constraint; record this as a closeout compatibility deviation rather than rewriting history.