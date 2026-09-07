# Implementation Plan — Adaptive operator extension discovery

1. Complete Trellis pre-development loading and verify the branch/worktree has no unrelated changes beyond this task directory.
2. Add a canonical `stateless_operator_extension_tool_specs()` composition beside the fixed family ToolSpec helpers; keep `registered_tool_specs()` untouched.
3. Replace MCP repeated extension-family universe composition with the canonical helper while preserving Full Operator auth/admin filtering and Adaptive `stateless_2026` gating.
4. Add explicit admitted-extension route classification without changing ordinary `ModelHidden` behavior.
5. Add an internal default-false discovery capability projection derived only from `ToolProtocolCapabilities`; thread it through the kernel-owned dispatch path to `ToolManifest` without changing public `ToolCall` arguments.
6. Make exact/unfiltered/category/intent manifest generation use one capability-aware spec selection helper and one route projection path. Preserve boundedness, sparse projection, and recommended-flow behavior.
7. Add focused regression tests for Skill, Memory, diagnostic, Adaptive gateway, Full Operator direct, unsupported surfaces, partial capabilities, universe drift, and unchanged invocation enforcement.
8. Update the backend code-spec with the finalized protocol/surface-aware discovery contract.
9. Validate with formatting, focused Rust tests, and the smallest relevant Cargo checks; avoid full workspace/E2E unless focused evidence exposes a gap.
10. Independently review the functional diff for duplicate name registries, ModelHidden/OpenAPI leakage, route/admission drift, capability inference, and authority bypass.
11. Create exactly one functional implementation commit containing product code/tests and the stable backend spec update. Keep Trellis archive/journal bookkeeping out of that commit.
12. Run normal Trellis finish/archive/journal afterward, allowing separate management commits. Never push, create a PR, deploy, restart, or touch the no-Trellis worktree.

## Review Gates

- `registered_tool_specs()` remains generic-only.
- No public/model-controlled Stateless/operator capability argument exists.
- Adaptive extensions are gateway-only; Full Operator extensions are direct only when real protocol capability admitted them.
- `tools/list`, gateway admission, and manifest consume one extension universe composition.
- Exact manifests show canonical input schema/annotations/authority-risk metadata and no output schema.
- Scope/project/permission/capability execution gates are unchanged and tested.
- Legacy/Local/REST/OpenAPI remain extension-free.
- Previous window trace files are untouched.