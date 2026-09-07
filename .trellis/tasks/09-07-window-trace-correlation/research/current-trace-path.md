# Research — Existing window and tool-request trace path

## Confirmed implementation path

- `src/client_window.rs`
  - `ClientWindow::from_opaque` validates the opaque adapter value and hashes `webcodex.client-window.v1\0 + source + \0 + value` with SHA-256.
  - `stateless_mcp_window` consumes `_meta["openai/session"]` and returns only `ClientWindow(source = "openai-session")`; missing/malformed metadata yields `None`.
  - `mcp_window` provides the existing legacy MCP source and intentionally does not infer identity for a non-initialize request without the session header.
  - `api_window` provides the existing hosted conversation-header / first-party HttpOnly cookie sources.

- `src/mcp.rs`
  - `ToolRequestLifecycle` is created at inbound `/mcp` entry.
  - After JSON-RPC parse and protocol validation, the handler resolves `McpWindow` by protocol era.
  - The resolved identity is already passed onward as business `ToolCallContext.window`; tracing currently does not receive it.
  - The MCP lifecycle does not add a new raw-request/argument payload capture of request params; current full-trace calls observed in this handler capture final responses, so this task must not broaden capture to raw params.

- `src/runtime_http.rs`
  - `/api/tools/call` creates the same lifecycle abstraction.
  - It already resolves `api_window(req, res)` and supplies it to `ToolCallContext.window` immediately before runtime dispatch.
  - Existing API full tracing captures the API request/effective arguments, but API window identity originates from header/cookie resolution rather than ChatGPT MCP `_meta["openai/session"]`.

- `src/tool_request_trace.rs`
  - `ToolRequestLifecycle::log` owns the structured lifecycle log fields.
  - In full mode the same lifecycle metadata is persisted to bounded per-trace `events.jsonl` through the dedicated writer.
  - Request/job correlation is an independent server-trace/Runner-request mechanism and must not be repurposed as window identity.

## Domain constraints confirmed from docs/spec

- `.trellis/spec/backend/logging-guidelines.md`: raw external session identifiers must never be logged; telemetry should consume the hashed `ClientWindow` projection.
- `docs/agent/session-model.md`: ClientWindow is not Workflow Session identity, recorder provenance, or authority; missing stateless window metadata never falls back to credentials/project/connection/prior request.
- `docs/agent/manual-window-collaboration.md`: window identity cannot select or authorize Workflow Sessions.

## Recommended minimum change

Attach the already-resolved optional `ClientWindow` to `ToolRequestLifecycle` and emit only `client_window_key` + `client_window_source`. Wire the two existing adapter resolution points. Do not add a new hash, persistence table, public schema, or business lookup.