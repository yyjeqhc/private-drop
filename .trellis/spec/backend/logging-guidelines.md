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
