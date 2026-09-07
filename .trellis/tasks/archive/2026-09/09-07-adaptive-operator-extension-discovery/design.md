# Design — Adaptive operator extension discovery

## Problem Boundary

The gap is a read-side projection mismatch. Stateless MCP already owns two authoritative facts for each request: the configured `ModelSurface` and server-derived `ToolProtocolCapabilities`. Canonical extension ToolSpecs already exist, and invocation is already capability/scope/permission gated. Only `tool_manifest` loses the per-request capability context and therefore searches the generic registry alone.

The fix connects those existing facts without making protocol/surface identity a model argument and without changing execution authorization.

## Canonical Extension Universe

Add one contract-level helper in `webcodex-tool-contracts` that composes the five existing fixed extension families in stable order: Skill runtime, Skill management, Memory runtime, Memory management, then operator diagnostics. It returns canonical ToolSpecs and does **not** modify `registered_tool_specs()`.

Consumers that need the complete Stateless extension universe use this composition:

- Stateless Full Operator `tools/list`, preserving current caller scope/admin projection rules.
- Adaptive gateway admission, gated only by `stateless_2026` membership because target authorization remains downstream.
- `tool_manifest`, after per-request protocol capability filtering.

## Internal Discovery Capability Context

`ToolProtocolCapabilities` remains the adapter-to-kernel authority for protocol features. Derive a narrow internal discovery projection containing only `skill_runtime`, `skill_management`, `memory_surface`, and `trace_diagnostics`.

Generic dispatch defaults this projection to all false. The kernel derives it from the real `ToolProtocolCapabilities` and threads it through a kernel-only dispatch path to `ToolManifest`; no public `ToolCall` field or model schema changes.

This projection controls only which contracts may be described. Existing kernel scope/capability/project/permission checks remain authoritative for execution.

## Manifest Spec Selection

One internal helper starts from `registered_tool_specs()` and conditionally appends extension entries from the unified contract universe according to the discovery capability projection. Classification reuses existing Skill/Memory name predicates and canonical diagnostic-family membership; it does not infer capability from auth, transport, project, or runtime exposure.

All manifest paths use that selected spec vector: exact lookup, unfiltered category map/counts, category filtering, and intent ranking/filtering.

Intent semantics stay relevance-ranked rather than exhaustive: only names explicitly ranked by an intent appear for that intent. Extension presence does not force management tools into unrelated intent/recommended flows. Exact plus unfiltered categories plus category discovery provide the complete selectable universe.

## Route Projection

Do not make `ModelHidden` generically model-visible. Extend route classification with an explicit private `operator_extension_admitted` input:

- ordinary generic tool: current route behavior unchanged;
- admitted extension + Adaptive Runtime: `gateway` via `call_runtime_tool`;
- admitted extension + Full Operator Runtime: `direct`;
- Local Coding or unadmitted extension: unavailable/absent.

The existing generic route method delegates with `operator_extension_admitted=false`.

Adaptive gateway admission consumes the same canonical extension membership and admitted-extension route classification. MCP-only gateways such as `mcp_tool`/`ssh_resource` keep their existing specialized admission because they are not part of this ToolSpec extension universe.

## Caller Scope Semantics

Full Operator `tools/list` remains caller-scope filtered exactly as today. `tool_manifest` is contract discovery, not an authorization oracle: when the protocol capability exists it may describe a contract and its canonical authority requirements even if later invocation is scope-denied. That matches existing generic manifest behavior.

## Compatibility / Security

- Legacy MCP: protocol capabilities false → no extension manifest specs.
- Local Coding: operator capabilities false → no extension manifest specs.
- HTTP/REST/OpenAPI: default protocol capabilities false and generic registry unchanged.
- Project Connector: existing `tool_manifest` unavailable behavior remains.
- Stateless Adaptive: extensions remain absent from outer `tools/list`, but discoverable/callable through `call_runtime_tool`.
- Stateless Full Operator: admitted extension routes are direct; `tools/list` caller filtering is unchanged.
- No output schema is added to exact compact manifest.
- Discovery never mutates scope, project authority, permission, retry, idempotency, or provenance.

## Validation Strategy

1. Contract helper equals the union of the five existing extension families without duplicates.
2. Adaptive gateway target universe and manifest capability universe cannot drift in extension names.
3. Stateless Adaptive exact discovery covers Skill runtime, Memory, and diagnostic with gateway route and canonical metadata.
4. Stateless Full Operator exact discovery reports direct for representative extensions.
5. Adaptive unfiltered/category discovery exposes extensions without making them outer direct tools.
6. Legacy/Local/generic dispatch cannot discover extension names; partial capabilities expose only their family.
7. Existing Skill-management admin/capability, Memory scope/permission, and diagnostic admin/capability invocation tests remain green.
8. Generic registry/OpenAPI consistency tests remain green.

## Expected Functional Files

- `crates/webcodex-tool-contracts/src/registry/tool_specs.rs` and re-export: canonical extension universe.
- `src/model_surface.rs`: explicit admitted-extension route classification.
- `src/tool_runtime/kernel.rs`, `dispatch.rs`, `discovery_tools.rs`, `surface.rs`: internal capability threading and manifest selection.
- `src/mcp/tools.rs`: consume unified extension universe for Full Operator projection and Adaptive admission.
- Existing focused test modules and one backend code-spec update.

No changes are expected in window trace code, OpenAPI generation, database code, or public tool input schemas.