# Canonical Tool Audit and Session privacy

## Ownership

`ToolDefinition.audit` is the declaration point for a runtime tool's recording
contract. Tool identity is resolved by `lookup_tool_definition`; neither audit
projector maintains a second list of tool names.

```text
ToolDefinition
  + model contract
  + execution / authority policy
  + Session continuity policy
  + ToolAuditPolicy
      + request field rules and semantic transform
      + result evidence projection
      + persisted context projection
      + bounded execution-excerpt policy
               |
        runtime audit projector
               |
        Session policy projector + existing bounds
               |
        persisted ledger / restore-time sanitization
```

The static types and declarations belong to `webcodex-tool-contracts`, which
still depends only on `webcodex-core` within the workspace. Runtime transforms
belong to `webcodex-tool-runtime-contracts`. `webcodex-workflow-session` also
consumes the declaration crate so recording and restore do not need their own
privacy classification table. This same-layer dependency is explicitly recorded
in `workspace-boundaries.toml`; it does not point back to the runtime crate or
create a cycle.

Request rules select existing metadata, summarize presence/length/count, or
invoke a small semantic transform: process execution, detached execution, script
execution, Job observation, checkpoints, or edits. They are not one variant per
tool. `AuditTypedPolicy` preserves the historically narrower typed recording
stage; an explicitly omitted typed summary stays empty even if raw request
metadata grows. `ToolCall::session_log_arguments` uses the canonical name
accessor and ephemeral serialization rather than matching `ToolCall` variants.

Result field rules and the coding-event counter retain metadata but not private
bodies. `AuditContextPolicy` declares the final context-field selection or the
existing working-tree-status reduction. `AuditExecutionPolicy` declares whether
the existing bounded stdout/stderr excerpt and test-count/assertion reductions
apply. Session input filtering derives from field semantics and execution
families. `EphemeralPreview` can be used for bounded runtime activity but is
removed before ledger persistence; a new execution tool reusing the family does
not need a new Session name branch.

## Canonical evidence is not persisted output

`AuditResultPolicy::SessionEvidence` explicitly preserves canonical evidence for
Session's existing outcome, path, Job, execution, and context reducers. It is an
intermediate value, not a serialized ledger field and not a generic fallback.
The Session recorder persists only the reducers' bounded results. Keeping this
explicit family avoids feeding sparse terminal model projections back into Job
observation or validation evidence.

The existing ActionAudit adapter also consumes this result policy before its
own final sanitizer. That sanitizer continues to remove output text/diff/blob
fields and redact secret-like keys and values; this change neither bypasses it
nor changes the canonical HTTP response. The intermediate projector must not be
mistaken for a replacement for either persistence sink's final sanitizer.

Raw full `stdout`/`stderr` are not thereby enabled for persistence. The existing
execution excerpt contract reads `stdout_tail`/`stderr_tail`, filters suspicious
lines, and retains a bounded tail. Git status has its separate declared bounded
porcelain-status summary. Persistent-shell lifecycle evidence still excludes
command text and stdout/stderr. Existing bounded task-context instruction
previews and Cargo package selectors are not reclassified as private coding-agent
prompts or Skill package bodies.

Legacy structured-validation events can contain bounded selectors instead of a
normalized target id. Their canonical validation-identity field declaration
preserves those existing selectors for evidence correlation. New runtime audit
projections already emit the normalized id and omit private selector text.
No audit value grants a permission, chooses a Session, changes a lifecycle state,
or replaces concrete business arguments or a canonical/model-facing result.

## Fail-closed behavior

There is no default `ToolAuditPolicy` and no optional policy slot on a canonical
definition. Constructing a definition requires an explicit audit declaration.
Unknown names, invalid declarations, non-object generic inputs/results, and
failed typed serialization produce no raw recording payload (`null`). Failed
execution-context deserialization also produces `null`, not the original value.
Context reduction with an absent/invalid policy produces no context evidence;
execution excerpts are omitted unless explicitly declared.

Generic Plugin/SSH audit policies omit arbitrary gateway inputs and outputs.
Their existing specialized governance boundary still records its own admitted,
bounded operation/identity metadata; it does not regain opaque arguments,
binding strings, native destinations, or credentials through generic audit.

Current raw lookup accepts exact canonical names only. A parser compatibility
alias is audited through its canonical `ToolCall` name after parsing; it does not
have a separate audit declaration. Session restore retains only the documented
historical `list_agents` spelling by resolving the `list_runners` declaration.
The retired `start_coding_task` name has no compatibility audit identity.

## Intentional privacy tightening

Previously, an unrecognized request or result branch could clone the entire
input. That fallback is removed. Previously fallback-dependent registered calls
now explicitly select their existing ordinary metadata. The tightened cases are:

- Project registration/creation omit native source paths and description bodies.
- Generic Plugin/SSH recording omits opaque requests and provider results.
- Workspace-symbol queries and edit bodies become presence/change metadata;
  Git-diff argument bodies become counts, and Job observation tokens are omitted.
- Unknown/retired names and malformed recording inputs do not retain raw values.
- Session recording/restore uses the same declarations, removes summarized
  private sources, and omits unknown-policy payloads instead of bounding raw data.

No model contract, MCP/OpenAPI schema, Runner wire protocol, execution authority,
ToolResult representation, or Session lifecycle transition is changed.

## Adding and validating a tool

Add the audit declaration beside the canonical tool definition. Reuse field
rules and semantic families, and explicitly choose context/execution omission
when those evidence contracts do not apply. Add domain privacy examples rather
than introducing tool-name dispatch in either projector.

The completeness test iterates the canonical definition universe, including
registered model-hidden runtime tools. It verifies valid declarations, unique
canonical names, and exact lookup; unregistered internal helpers are not forced
into the public registry. Required Rust fields enforce declaration at construction.
The ordinary workspace-crates CI shard executes this invariant automatically.

Regression coverage includes raw and typed purity; absent/invalid/unknown policy;
serialization and execution-context failure; alias/retired-name behavior;
authority/model-contract independence; shared-family Session privacy; durable
ledger and restore checks across all canonical definitions; and the unchanged
Memory, Skill, computer, conversation, gateway, trace, token, native-path, and
execution-output tests. Focused validation covers the declaration/runtime/Session/
validation crates and the root audit/privacy/Session/gateway integration filters.
