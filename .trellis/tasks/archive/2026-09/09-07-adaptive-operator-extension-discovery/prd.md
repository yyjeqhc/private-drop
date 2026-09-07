# Adaptive operator extension discovery

## Goal

Make `tool_manifest` describe the same Stateless MCP 2026 operator extension contracts that the active model surface can actually invoke, while preserving generic `ModelHidden` semantics and all existing authorization/capability gates.

## Background / Confirmed Facts

- Generic runtime discovery is intentionally based on `registered_tool_specs()`; `ModelHidden` tools stay parser/kernel-known without entering the generic model-visible registry or OpenAPI contract (`crates/webcodex-tool-contracts/src/registry/tool_specs.rs:13-15`, `docs/agent/openapi-guidelines.md:24-28`).
- Stateless Full Operator appends Skill runtime/management, Memory runtime/management, and operator diagnostic ToolSpecs in `src/mcp/tools.rs:45-87`; legacy MCP does not append them.
- Adaptive Runtime direct `tools/list` remains intentionally small, while `adaptive_runtime_gateway_target_specs(stateless_2026)` admits generic long-tail targets plus the same Stateless extension families through `call_runtime_tool` (`src/mcp/tools.rs:89-106`).
- `tool_manifest` exact, unfiltered, category, and intent paths currently source only `registered_tool_specs()` (`src/tool_runtime/surface.rs:192-216`, `297-352`), creating the known discovery/invocation asymmetry.
- `ToolProtocolCapabilities` is set by the MCP adapter from `stateless_2026 && model_surface.supports_operator_extensions()` and already gates Skill, Memory, and trace diagnostic execution in the kernel (`src/mcp/tools.rs:1727-1817`, `src/tool_runtime/kernel.rs:290-369`). Generic API/runtime callers receive default-false capabilities.
- `ModelSurface::runtime_tool_invocation_route` deliberately rejects non-model-visible ToolDefinitions before classifying direct/gateway routes (`src/model_surface.rs:99-128`). Hidden extension discovery therefore needs an explicit surface-aware admitted-extension route rather than a change to generic `ModelHidden` visibility.

## Requirements

1. **Single extension contract universe.** Introduce one canonical composition of the current Stateless operator extension ToolSpecs: Skill runtime, Skill management, Memory runtime, Memory management, and operator diagnostics. `tools/list`, Adaptive gateway admission, and `tool_manifest` must consume that composition rather than each maintaining the family list.
2. **Capability-scoped discovery.** `tool_manifest` may add extension ToolSpecs only when the current inbound call carries the corresponding server-derived `ToolProtocolCapabilities`. Generic dispatch, REST/OpenAPI, Local Coding, legacy MCP, Project Connector, and unsupported calls remain extension-free.
3. **Surface-correct routes.** For a capability-admitted extension, Adaptive Runtime discovery reports `availability=gateway` and `gateway_tool=call_runtime_tool`; Stateless Full Operator reports `availability=direct` and no gateway. Existing generic tool routing is unchanged.
4. **Canonical exact contract.** Exact extension discovery reuses canonical ToolSpec input schema/annotations and ToolDefinition-derived effect/risk/approval/idempotency/authority metadata. It continues to omit output schema from the compact exact view.
5. **Discovery is not authorization.** Manifest visibility must not bypass OAuth scope, project authority, Skill/Memory capability checks, permission evaluation, or specialized governance. Invocation remains governed by the existing kernel/adapters.
6. **Fail closed.** Missing or partial Stateless operator capability is never inferred from transport, credential, project, runtime exposure alone, prior calls, or tool name. Only explicit server-owned per-request capability context can add extension discovery.
7. **Bounded list semantics.** Unfiltered/category discovery must include capability-admitted extensions in the same bounded manifest machinery. Intent ranking and recommended flows remain relevance-focused; do not inject management tools into unrelated flows merely for inventory symmetry.
8. **No generic contract expansion.** Do not add extension ToolSpecs to `registered_tool_specs()`, make `ModelHidden` globally visible, add OpenAPI names/flattened fields, or add a model-controlled protocol/surface claim.
9. **No unrelated changes.** Do not modify the previous window trace feature or perform unrelated refactors.

## Acceptance Criteria

- [x] Stateless Adaptive exact discovery succeeds for representative Skill runtime (`skill_list`), Memory (`memory_search` or `memory_set`), and operator diagnostic (`read_tool_trace`) extensions and reports gateway routing through `call_runtime_tool`.
- [x] Stateless Full Operator exact discovery for the same capability-admitted extension classes reports direct routing and canonical contract metadata/schema.
- [x] Stateless Adaptive unfiltered/category discovery includes extension names admitted by the same canonical extension universe used by gateway admission; a regression assertion prevents name drift between discovery and admission.
- [x] Generic direct runtime discovery, Local Coding, legacy MCP, Project Connector, and REST/OpenAPI do not expose the extension universe.
- [x] Missing/partial protocol capabilities keep the corresponding extension unknown/absent; there is no credential/project/transport fallback.
- [x] Existing Skill/Memory/diagnostic scope, authority, capability, and permission denials still occur on invocation even after discovery succeeds.
- [x] `registered_tool_specs()` and the generic OpenAPI model-visible contract remain unchanged.
- [x] Focused tests, formatting, the smallest relevant Cargo check, and an independent final functional diff review pass.

## Out of Scope

- Promoting extension tools to Adaptive direct tools.
- Adding extension output schemas to compact `tool_manifest` exact results.
- Redesigning OAuth, project authorization, permission evaluation, Skill/Memory execution, or MCP protocol capability negotiation.
- New analytics, persistence, dashboards, or changes to tool-request window tracing.

## Open Questions

None. The user supplied the product, compatibility, authority, and scope decisions explicitly.
