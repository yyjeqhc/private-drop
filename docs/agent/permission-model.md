# Authority Model — Decision Layer for Tool Execution

Canonical contract for the WebCodex **authority mode**: how consequential tool
invocations are authorized, recorded, and bounded on a self-hosted deployment.

This document describes the authority contract and current runtime behavior.
Neither existing code nor historical design text proves that a policy is still
needed. Resolve disagreements using current callers, deployment requirements,
and Git history; update implementation, tests, and documentation together while
preserving the explicit authority boundaries below.

**Audience:** agents and maintainers working on self-hosted WebCodex.

**Related docs (link, do not duplicate):**

| Doc | Relationship |
|---|---|
| [`AGENTS.md`](../../AGENTS.md) | Executable trusted-agent contract and hard boundaries |
| [`SECURITY.md`](../../SECURITY.md) | Security model and redaction expectations |
| [`session-model.md`](session-model.md) | Workflow Session vs Action Audit Session |
| [`architecture-decisions.md`](architecture-decisions.md) | Standing authority / observation / API-evolution decisions |
| [`DEPLOYMENT.md`](../DEPLOYMENT.md) | Operator-facing authority configuration and smoke checks |
| [`AUTH_MODEL.md`](../AUTH_MODEL.md) | Authn / tokens / scopes (orthogonal to this decision layer) |

---

## 1. Canonical configuration

```text
WEBCODEX_AUTHORITY_MODE = trusted_agent | restricted
```

| Rule | Behavior |
|---|---|
| Env var | `WEBCODEX_AUTHORITY_MODE` |
| Unset / empty / whitespace | **`trusted_agent`** (self-hosted single-operator product default), source reported as `default` |
| Explicit known value | `trusted_agent` or `restricted`, source `env:WEBCODEX_AUTHORITY_MODE` |
| Unknown non-empty value | Invalid configuration → consequential tools **fail closed** with reason `invalid_authority_mode:{value}` |
| Legacy values | `dev_auto_approve` aliases `trusted_agent`; `require_approval` aliases `restricted`, in either environment variable. Legacy-only configuration reports `migrated_env:WEBCODEX_PERMISSION_MODE`. Both variables may agree; conflicting values and unknown legacy values (including `audit_only`) remain invalid. |

The resolved mode and source are projected on `runtime_status` and on internal
full startup diagnostics as the `authority` object. The canonical external
`work_on_project` response intentionally keeps that full diagnostic block out of
its sparse model-facing projection:

```json
{
  "mode": "trusted_agent",
  "source": "default",
  "project_write": true,
  "shell": true,
  "git": true,
  "network": true,
  "package_install": true,
  "service_control": true,
  "release": "user_task_scoped",
  "human_approval_required": false
}
```

The former `permissions` profile object
(`policy` / `auto_approve` / `release_recommended_policy`) no longer exists on
those payloads. The per-session permission decision **summary** (counters) in
`finish_coding_task` / `session_handoff_summary` is unchanged except that it
carries the new policy names.

---

## 2. Mode semantics

Both modes share the same tool implementations, schemas, session model,
evidence contract, and audit records. The mode changes only the authorization
decision for consequential tools.

### `trusted_agent` (default)

- Project read/write, shell, async jobs, Git operations, script/build
  execution, dependency install, and local service control **auto-execute
  after hard safety checks**. There are no human approval interruptions.
- Push, tag, publish, release, and deploy actions execute only when the
  user's task **explicitly includes that action and target**
  (`release: "user_task_scoped"` in the authority projection). This is an
  agent-contract rule, not an approval queue.
- Every permission-bearing call still records an **auditable decision** on the
  session ledger: `policy=trusted_agent`, `status=auto_approved`,
  `reason=trusted_agent_authority`, plus risk class and project.

### `restricted`

- Consequential runtime tools are **denied** with
  `reason=restricted_requires_human_authorization`.
- The project-bound connector `commands_run` keeps the one-time human approval
  loop (`wc_approvals`, `task_cli approve/deny`).

### Invalid configuration

- Unknown authority/legacy values and conflicting current/legacy settings are rejected: consequential tools fail closed
  (`policy=invalid`, `status=denied`, `reason=invalid_authority_mode:...`).
  Invalid configuration never falls back to allow.

### Connector surface (`commands_run`)

- Under `trusted_agent`, `commands_run` no longer creates approval records or
  `approval_required` interruptions. Instead a durable task event
  `authority_auto_authorized` is recorded with the authority mode, source,
  resolved rule, action hash/summary, risk, principal, and project.
- Under `restricted`, the existing one-time approval loop is unchanged.

---

## 3. Hard boundaries not relaxed by `trusted_agent`

Authority mode is a decision layer above hard safety, never a replacement for
it. The following are enforced independently and are **not** overridable by
any authority mode:

| Hard rule | Enforcement home (examples) |
|---|---|
| OAuth scopes / token classes | Scope check, agent authorization |
| Project boundary / allowed roots | Model requests cannot extend roots. Explicit local operator project selection can persist the exact canonical root; path/namespace/escape checks remain. |
| Explicitly read-only sessions deny writes and shell/jobs | Session guard **before** mutation |
| Unknown explicit `session_id` → `unknown_session_id` | Session resolution |
| Path and sensitive-path policy | File tools + `policy_rejected` / hard-deny detection |
| Concurrent-overwrite guards (stale SHA / guarded edits) | Transactional edit tools |
| Credential redaction | Logging / evidence rules |
| Job cancel / reclaim semantics | Job lifecycle |
| Immutable release targets (no moving published tags) | Release process rules |

`trusted_agent` only means "no extra human gate after hard safety allows
execution." A hard-denied tool outcome suppresses the soft authority attach so
auto-authorization metadata is never claimed on policy/session/path hard
denies.

---

## 4. Execution chain and decision shape

Authoritative gate is ToolRuntime dispatch, evaluated **once** per
permission-bearing request, before mutation:

```text
request / project resolution / explicit Session + recorder authorization
  → business/recorder Session existence + project-mismatch guards
  → explicit business Session guard (e.g. read_only) before mutation
  → agent authorization / scopes
  → authority evaluation (exactly once)
        trusted_agent → auto_approved (reason trusted_agent_authority)
        restricted    → denied (reason restricted_requires_human_authorization)
        invalid       → denied (reason invalid_authority_mode:...)
  → tool execution only on allow
  → hard tool-internal safety checks remain effective
  → hard-denied output suppresses the soft authority attach
  → else attach the same decision + optional ledger record
```

Decision wire shape (stable for ledger / handoff consumers):

| Field | Meaning |
|---|---|
| `required` | `true` when a decision is emitted |
| `policy` | `trusted_agent`, `restricted`, or `invalid` |
| `request_id` | `wc_perm_*` UUID, one per decision |
| `status` | `auto_approved` or `denied` |
| `reason` | `trusted_agent_authority`, `restricted_requires_human_authorization`, `invalid_authority_mode:...` |
| `risk` | Coarse risk label from tool metadata (`write`, `patch`, `shell`, `job`, `destructive`, …) |
| `tool_name` | Runtime tool name |
| `project` | Optional project id from the call |

Read-only / non-permission-bearing tools emit **no** decision object — no fake
approval records. They remain subject to hard safety.

---

## 5. Invariants (must hold)

1. **One authority decision per request** that reaches the gate for a
   permission-bearing tool; one `wc_perm_*` id per decision; the kernel reuses
   the attached decision and never re-evaluates.
2. **Invalid configuration fails closed** — unknown or conflicting legacy settings
   never fall back to allow.
3. **Hard safety is never bypassed by authority mode.**
4. **`restricted` never silently auto-approves** runtime tools.
5. **Read-only / not-required tools never invent approval records.**
6. **Hard-denied tool outcomes suppress the soft authority attach.**
7. **Every trusted-agent auto-execution remains auditable** — session-ledger
   decision records for runtime tools, `authority_auto_authorized` durable
   task events for the connector `commands_run` path.
8. **External release actions stay user-task-scoped** under `trusted_agent`;
   they are not blanket-authorized by the mode.

---

## 6. Layering model

```text
┌─────────────────────────────────────────────────────────────┐
│ AuthN / Scopes / Agent capability  (who may call tools)     │
├─────────────────────────────────────────────────────────────┤
│ Hard safety rules  (path, secrets, session guard, project)  │
│   — not overridable by authority mode                       │
├─────────────────────────────────────────────────────────────┤
│ Authority decision  (trusted_agent | restricted)            │
├─────────────────────────────────────────────────────────────┤
│ ToolRuntime mutation / shell / job execution                │
├─────────────────────────────────────────────────────────────┤
│ Evidence sinks: Workflow ledger, Action Audit, task events  │
│ — record, do not decide                                     │
└─────────────────────────────────────────────────────────────┘
```

**Auth** answers identity and coarse tool scopes.
**Hard safety** answers absolute product/agent safety.
**Authority** answers whether consequential work auto-executes or requires a
human.
**Sessions / Audit / Trace** answer continuity and observability.

Authority never creates, switches, or closes a Workflow Session, and audit
never reverse-controls authority decisions.

---

## 7. Observability

Safe decision fields: `wc_perm_*` id, policy, status, reason, risk, tool name,
project, workflow session id (via ledger context), and for
`authority_auto_authorized` events the mode, source, resolved rule, action
hash/summary, risk, principal, and project.

Forbidden in decision records and task events: full tool parameters, file
contents/patches, secrets/tokens/credentials, user prompts, and large tool
results or stdout/stderr.

---

## 8. Non-goals

- No approval UI or notification system for `trusted_agent`.
- No multi-person approval, RBAC, or distributed policy engine.
- No per-tool authority overrides in env; one global mode plus tool metadata.
- No compatibility alias or migration path for the removed
  `WEBCODEX_PERMISSION_MODE` system.
