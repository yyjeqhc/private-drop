# Directory Structure

## Workspace shape

WebCodex is a Rust workspace. The root package (`src/`) owns the Server and transport/adaptor integration; reusable domains live under `crates/`.

Real examples from `Cargo.toml`:

- `crates/webcodex-store` — durable SQLite state.
- `crates/webcodex-connector-runtime` — Connector Task runtime and project/window binding.
- `crates/webcodex-tool-contracts` and `crates/webcodex-tool-runtime-contracts` — canonical tool contracts shared across surfaces.
- `crates/webcodex-process`, `crates/webcodex-workspace`, `crates/webcodex-computer` — reusable execution/capability components.
- `src/mcp.rs`, `src/openapi.rs`, and related adapter modules — public transport projections around the core runtime.

## Placement rules

- Put protocol-neutral durable/domain logic in the owning crate instead of a transport adapter.
- Keep MCP/HTTP/ChatGPT-specific metadata and rendering in the adapter layer; do not make it core execution authority.
- Add a new crate only for a demonstrated reusable boundary. Follow the existing `webcodex-*` naming convention.
- Tests belong in existing domain test module trees. Process/network fixtures should not be hidden in large inline `#[cfg(test)]` blocks.
- Domain documentation belongs under `docs/agent/` or `docs/architecture/`; do not duplicate those contracts in unrelated code comments.

## Examples to follow

- `src/client_window.rs` keeps host/transport window identity adapter-local and hashes opaque host values before persistence/use.
- `crates/webcodex-store/src/schema.rs` owns durable table shape rather than scattering SQL schema across adapters.
- `src/tool_runtime/tool_result.rs` is a thin compatibility facade over the canonical runtime-contract crate instead of a second result definition.
