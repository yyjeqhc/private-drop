# Error Handling

## Principles

- Fail closed at authority, credential, target, and destructive-operation boundaries.
- Preserve the difference between a known business failure, transport failure, timeout, cancellation, and `outcome_unknown`; do not retry an uncertain mutation blindly.
- Prefer canonical structured errors/results over adapter-specific string conventions. Root runtime code re-exports the canonical result contract from `webcodex-tool-runtime-contracts`.

## Runtime patterns

- Model-facing runtime validation returns `ToolResult::err(...)` or the domain's structured error before execution starts. `src/tool_runtime/projects.rs` and `runner_authorization.rs` are representative examples.
- Error messages should tell the model what can be safely re-observed or corrected without leaking hidden object existence, private paths, or credentials.
- Transport adapters translate core outcomes to their protocol shape; they must not reinterpret authority or turn protocol success into business success.
- Recovery paths should re-observe authoritative state before retry after an indeterminate transport/result boundary.

## Common mistakes to avoid

- Treating HTTP 200 / JSON-RPC success as proof that the underlying tool business operation succeeded.
- Converting an authorization failure into an existence oracle by returning richer metadata for hidden objects.
- Collapsing `unknown` into failure and then issuing a duplicate mutation.
- Weakening validation, scopes, sandboxing, or tests just to make a failing path pass.
