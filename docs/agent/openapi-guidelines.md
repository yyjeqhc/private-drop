# OpenAPI / GPT Action Guidelines

Product and integration detail for GPT Actions and OpenAPI exposure.
Repository-wide rules are in [`AGENTS.md`](../../AGENTS.md). This document
owns the API-specific invariants and product guidance linked from that guide.

Related: [`GPT_ACTIONS.md`](../GPT_ACTIONS.md), [`MCP.md`](../MCP.md).

---

## 1. Surface synchronization

When adding or renaming a runtime tool, update **all** of:

1. `ToolCall` enum / parser  
2. `KNOWN_TOOL_NAMES`  
3. metadata  
4. registry schema  
5. OAuth runtime tool policy  
6. OpenAPI accepted names / examples  
7. MCP schema tests  
8. consistency tests  

Runtime parser/metadata/OAuth policy must stay synchronized for every known tool.
The **model-visible** registry, MCP `tools/list`, `tool_manifest`, and GPT Actions
`ToolCallRequest` selector/flattened fields must separately stay synchronized with
`registered_tool_specs()`. Internal or retired runtime names must not leak names
or implementation-only fields into the model-facing Action schema.

## Route/OpenAPI ownership

- `RouteSpec` owns canonical HTTP method/path, scope/auth surface, audit class,
  and whether a route projects as `Hidden`, a dedicated Public Action, or a
  Connector capability binding. Handler mounting remains explicit in the owning
  HTTP modules; OpenAPI metadata is never an authorization source.
- A Public Action projection carries the static product operation policy used by
  `/openapi.json`: operation id, summary/description, request/response component
  identities, explicit consequential policy, and typed request-example identity.
  The Public OpenAPI projector iterates canonical routes directly; do not add a
  second handwritten path/operation registry or post-generation route filter.
- Connector capability semantics remain owned by
  `webcodex_connector_runtime::surface::capability_specs()` (description, input
  and output schemas, and annotations such as `readOnlyHint`). `RouteSpec` owns
  only the capability-to-HTTP-route binding; the Connector OpenAPI projector
  combines those two canonical sources.
- Component construction in `openapi::schemas()` remains a separate manual schema
  projection. Route operation ownership does not require a schema-DSL rewrite.

---

---

## 2. Exposure rules (invariants)

- Do **not** expose legacy `/api/codex` routes in GPT Action OpenAPI.
- Do **not** expose agent token management or pairing endpoints in GPT Action
  OpenAPI.
- WebCodex GPT Actions must stay **below the 30-operation GPT Actions limit**.
  Verify the current OpenAPI operation count through generation/tests rather
  than relying on any document for a fixed count. **Do not hard-code** a live
  operation total in agent rules.

---

## 3. Product labeling and discovery

- Prefer **non-consequential** labels for read-only, discovery, onboarding, and
  bounded local project setup actions.
- `registerProject` and `createProject` are **non-consequential** onboarding
  actions when constrained by agent policy, allowed roots, and non-overwrite
  defaults.
- Keep **destructive actions consequential**: shell/job execution, raw writes,
  patch application, delete/restore/discard, imports, and generic dispatch.
- `callRuntimeTool` is advanced/generic; use dedicated Actions only for stable
  common workflows that fit the operation budget.
- When adding future runtime tools, default to `callRuntimeTool` exposure unless
  there is an explicit product reason and operation-count budget for a dedicated
  Action.
- `listRuntimeTools` full detail discovery includes expanded schemas and may be
  too large for GPT Actions. Daily discovery should prefer
  `callRuntimeTool(tool="tool_manifest")`; once the exact target is known, pass
  `tool_name=<exact-name>` to that manifest call to retrieve its description, input
  schema, current invocation route, and decision-relevant effect/risk/authority
  metadata. Full output schemas and duplicate inventory bookkeeping stay out of
  this exact view.
  Focused `listRuntimeTools` calls should be reserved for schema/debug work and
  pass `summary_only=true` with `category`, `features`, or `limit`. Prefer
  `runtime_status` or the tool manifest for the current tool count; the
  response-size issue is expanded schema/metadata, not a fixed tool count.

---

## 4. Project smoke and capabilities

- `list_projects` project entries expose `capabilities`.
- Smoke selection should prefer `capabilities.recommended_for_smoke=true`.
- Git smoke must require `capabilities.git_available=true`.

---

## 5. Artifact upload tools

Chunked artifact upload tools remain runtime-only through `callRuntimeTool`;
do not promote them to dedicated GPT Action operations:

- `artifact_upload_begin`
- `artifact_upload_chunk`
- `artifact_upload_finish`
- `artifact_upload_abort`

`artifact_upload_chunk`, `artifact_upload_finish`, and `artifact_upload_abort`
must repeat the exact `path` used by `artifact_upload_begin`. This binds
`upload_id` to the requested target artifact path.

Artifact smoke paths should use `artifacts/smoke/<name>.artifact` or
`artifacts/smoke/<name>.txt`. Verify abort cleanup with
`artifact_upload_abort.final_file_exists` or
`read_project_artifact_metadata(allow_missing=true)`, not with expected read
failures. `policy_rejected` means policy blocked the request before a write.

---

## 6. Flattened Action fields

- GPT Actions should prefer **flattened top-level fields** over the direct
  `params` envelope.
- Use `recording_session_id` for generic wrapper recorder metadata.
- Use `session_id` as tool business input.
- When a **model-visible** runtime tool is expected to work through GPT Action
  `callRuntimeTool` with flattened top-level fields, `ToolCallRequest.properties`
  must expose **every** flattened field that GPT Actions need (including nested
  object/list payload fields such as `edits`, `validation`, `labels`,
  `checkpoint_id`, `confirm`, `dry_run`, `include_untracked`, and
  `include_diff_stat`). Internal implementation primitives and retired tool names
  must not contribute flattened model fields. A retired wire name fails closed;
  supported direct callers use the canonical model-visible tool contract and
  `params` envelope rather than an out-of-band compatibility schema.
- Add/update tests that fail when flattened Action fields are missing.
- Do **not** loosen `additionalProperties` to `true` as a workaround — list the
  needed flattened fields explicitly.
