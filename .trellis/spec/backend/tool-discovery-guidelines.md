# Tool Discovery Guidelines

## Scenario: Protocol/surface-aware operator extension discovery

### 1. Scope / Trigger

Use this contract when a transport adapter exposes fixed `ModelHidden` ToolSpecs on a protocol-specific model surface while generic runtime discovery intentionally excludes them. The current case is Stateless MCP 2026 Skill, Memory, and operator-diagnostic extensions.

### 2. Signatures

- Canonical fixed extension universe: `webcodex_tool_contracts::registry::stateless_operator_extension_tool_specs() -> Vec<ToolSpec>`.
- Generic registry remains: `registered_tool_specs() -> Vec<ToolSpec>` and must not absorb protocol-only extensions.
- Request-owned capability source: `ToolProtocolCapabilities::{skill_runtime, skill_management, memory_surface, trace_diagnostics}`.
- Route projection for a protocol-admitted hidden extension: `ModelSurface::runtime_tool_invocation_route_with_operator_extension(tool_name, true)`.
- Model-facing `tool_manifest` input does not contain a protocol, surface, or capability override.

### 3. Contracts

- `tools/list`, Adaptive gateway admission, and `tool_manifest` must consume the same canonical extension ToolSpec universe; do not add a second extension-name registry in an adapter or discovery layer.
- The MCP adapter derives `ToolProtocolCapabilities`; generic dispatch injects default-false capabilities. Discovery must never infer capability from credentials, project identity, transport name, configured runtime surface alone, or prior requests.
- Capability-admitted extensions remain `ModelHidden` generically. Adaptive Runtime reports them as `gateway` via `call_runtime_tool`; Full Operator Runtime reports them as `direct`; unsupported/legacy surfaces keep them absent.
- Exact discovery reuses canonical ToolSpec input schema/annotations plus ToolDefinition-derived authority/risk/effect metadata. Compact exact discovery does not add output schema.
- Discovery describes a contract; execution authorization remains in the kernel's existing OAuth scope, project authority, protocol-capability, permission, and specialized-governance checks.
- Unfiltered/category discovery may inventory admitted extensions through the normal bounded manifest machinery. Intent/recommended-flow ranking remains relevance-focused rather than exhaustively injecting management tools.

### 4. Validation & Error Matrix

| Condition | Discovery result | Invocation authority |
|---|---|---|
| Stateless Adaptive + matching capability | extension discoverable as `gateway` via `call_runtime_tool` | unchanged downstream checks |
| Stateless Full Operator + matching capability | extension discoverable as `direct` | unchanged downstream checks |
| Missing/partial protocol capability | extension unknown/absent | no fallback inference |
| Legacy MCP / generic REST / Local Coding | extension unknown/absent | generic contract unchanged |
| Caller lacks target OAuth/project/admin authority | contract may still be discoverable when protocol capability exists | invocation must still deny |
| New extension family lacks explicit capability mapping | fail closed: not discoverable | no implicit admission |

### 5. Good / Base / Bad Cases

- Good: `skill_list`, `memory_search`, and `read_tool_trace` are exact-discoverable on Stateless Adaptive with gateway routing while remaining absent from Adaptive outer `tools/list`.
- Base: an ordinary generic `tool_manifest(tool_name="skill_list")` returns unknown because protocol capabilities default false.
- Bad: add Skill/Memory/diagnostic specs to `registered_tool_specs()` so exact discovery passes everywhere.
- Bad: maintain separate hard-coded extension name lists in `tools/list`, gateway admission, and `tool_manifest`.
- Bad: infer Stateless/operator capability from an OAuth token, project, transport, or model-supplied argument.

### 6. Tests Required

- Assert the canonical extension universe is the unique ordered union of existing Skill runtime/management, Memory runtime/management, and diagnostic ToolSpec families and remains disjoint from `registered_tool_specs()`.
- Cover exact Skill, Memory, and diagnostic discovery on Adaptive (`gateway`) and Full Operator (`direct`).
- Cover partial/default capabilities as fail-closed and legacy/generic discovery as absent.
- Compare Adaptive unfiltered/category discovery against the real gateway admission classifier so extension names cannot drift.
- Assert Adaptive outer `tools/list` still excludes extension targets.
- Re-run existing Skill admin/capability, Memory scope/permission, and diagnostic capability tests to prove discovery did not grant execution authority.
- Keep existing registered-tool/OpenAPI consistency tests green.

### 7. Wrong vs Correct

**Wrong:** make a hidden extension generically model-visible, or teach each consumer a private list such as `matches!(name, "skill_list" | "memory_search" | ...)`.

**Correct:** compose canonical ToolSpecs once in the contract crate, let the server-owned request capability select the discoverable subset, then project the route from the already-configured `ModelSurface` without changing execution authorization.
