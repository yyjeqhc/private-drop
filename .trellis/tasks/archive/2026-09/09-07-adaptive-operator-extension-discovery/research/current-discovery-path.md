# Research — current operator extension discovery path

## Current asymmetry

`registered_tool_specs()` resolves only model-visible ToolDefinitions. Skill/Memory/operator diagnostic ToolSpecs are fixed canonical contracts but globally ModelHidden. Stateless Full Operator `tools/list` appends those families, and Stateless Adaptive `call_runtime_tool` gateway admission appends the same families. `tool_manifest` exact/list/filter paths use only `registered_tool_specs()`, so admitted hidden targets can be executable but undiscoverable.

## Authority path

The MCP adapter computes Stateless operator capabilities from protocol era plus configured model surface and passes them as `ToolProtocolCapabilities` into the kernel. The kernel independently gates Skill runtime/management, Memory, and trace diagnostics before generic dispatch, then applies normal OAuth scope/project authority/permission. Discovery should consume these capabilities but must not alter them or derive replacements from auth/project/transport.

## Route gap

`ModelSurface::runtime_tool_invocation_route` first rejects names that are not generic model-visible. That is correct for ordinary callers, but cannot classify ModelHidden operator extensions even when the current Stateless request explicitly admitted them. An explicit admitted-extension route is required; globally weakening the visibility check would leak `ModelHidden` semantics.

## Maintainability gap

`src/mcp/tools.rs` currently repeats five extension family functions in both Full Operator projection and Adaptive gateway target composition. Adding `tool_manifest` as a third repetition would worsen drift. The contract crate is the natural owner for one stable ordered union because all five canonical ToolSpec producers already live there and are independent of runtime state.

## Dispatch context observation

`ToolManifest` reaches `dispatch_discovery_tool` through generic dispatch, which receives auth but not `ToolProtocolCapabilities`. `ToolRuntime` knows configured `ModelSurface` but cannot infer Stateless capability from it. The safest path is a private default-false discovery-capability value derived in the kernel and threaded only through the kernel-owned dispatch route; generic/API dispatch remains false.

## Intent / boundedness observation

The unfiltered sparse manifest exposes categories/counts rather than duplicating every tool. Category filtering enumerates the selected spec universe. Intent filtering is an explicit relevance rank list and should not become exhaustive merely to include operator management tools. Exact, unfiltered categories, and category discovery can provide a complete selectable universe without bloating recommendation flows.