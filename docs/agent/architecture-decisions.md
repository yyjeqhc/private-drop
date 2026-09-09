# Agent Architecture Decisions

Standing design context for agents working on WebCodex. **Executable constraints
live in [`AGENTS.md`](../../AGENTS.md).** This file explains durable product
structure so agents do not re-litigate settled shape during ordinary tasks.

Related product docs: [`ARCHITECTURE.md`](../ARCHITECTURE.md),
[`../architecture/durable-agent-runtime.md`](../architecture/durable-agent-runtime.md),
[`TESTING.md`](../TESTING.md).

---

## 1. Session dual model (architecture, not an operation checklist)

WebCodex has **two different "session" concepts**. They share a name in casual
speech but are **not interchangeable** and must not be merged by accident.
Full naming, lifecycle, compatibility, and non-goals:
[`session-model.md`](session-model.md).

### Workflow session (coding / tool ledger)

| Aspect | Contract |
|---|---|
| ID form | `wc_sess_*` |
| Purpose | Coding-task workflow: start/finish coding task, tool events, validation evidence, handoff |
| Storage | In-memory ledger with durable JSON-oriented session records (product surface for MCP / runtime tools) |
| Identity rules | Existing Workflow Session effects require an explicit business `session_id` or authorized wrapper `recording_session_id`; unknown ids fail closed and omission never infers a Session |
| Mutation policy | `normal` uses ordinary authority/permission rules; `read_only` denies write-like and shell/job-like tools; guard denial before mutation. The pre-0.4 `inspect` Session mode is retired and malformed persisted v2 rows remain row-closed rather than becoming Normal. |

Do **not** change `wc_sess_*` ID format, ledger event shape, or lifecycle
semantics casually. Session / guard / explicit-targeting work must preserve the
invariants linked from `AGENTS.md` §6 (domain rules) and the Session model.

### Durable Agent and collaboration boundary (standing)

A Workflow Session remains a bounded **execution and evidence unit** even when its
session-local message board is used for coordinator/worker handoff. Do not turn
`wc_sess_*` lifecycle, ownership, or evidence history into a generic chat room,
Agent identity, task queue, worker pool, or scheduler.

The independent durable collaboration domain now exists: Server-minted Agents,
replaceable Agent Endpoints, Conversations, Messages, recipient Deliveries, and
Wake Intents/Attempts. The earlier `Room/Discussion` placeholder is superseded by
this concrete Agent/Conversation model. Standing rules are:

- a durable Agent is not a browser window, MCP connection, credential, Runner,
  Runtime Project, or Workflow Session;
- Conversation participation governs communication only. It never confers Project,
  Workflow Session, Job, Artifact, shell, Computer, CodingAgent, or filesystem
  authority;
- Message, Delivery, Wake, and execution are separate durable facts. Message/read
  state never proves model-context retention, and Wake never proves Agent Task completion;
- each concrete execution may still use an independent Workflow Session for tool
  calls, validation, Jobs, checkpoints, and review evidence; pure communication
  does not require an execution Session;
- the Session message board remains the explicit manual coordinator/worker handoff
  substrate. It is not migrated into Conversation and its todo semantics are not an
  Agent Task lease;
- the planned asynchronous work object is an independent **Agent Task** with an
  exact fenced **Agent TaskAttempt**. It is not the existing Connector Task and is
  not inferred merely because a Conversation Message exists;
- references among Conversation, Agent Task, Workflow Session, Job, CodingAgentRun,
  commit, PR, or Artifact provide correlation only. Dereferencing always re-runs
  the referenced object's normal authorization;
- automatic worker spawning, runnable-frontier scheduling, capacity management,
  dependency graphs, or orchestration are optional later control layers, not the
  definition of a durable Agent and not authority sources.

The standing Agent/asynchronous-work design is documented in
[`../architecture/durable-agent-runtime.md`](../architecture/durable-agent-runtime.md).
Current communication/wake implementation details live in
[`../architecture/durable-agent-conversation.md`](../architecture/durable-agent-conversation.md).
Current bounded Session handoff behavior remains documented in
[`manual-window-collaboration.md`](manual-window-collaboration.md).

### Action audit session (HTTP / operator audit)

| Aspect | Contract |
|---|---|
| ID form | UUID (HTTP action audit session) |
| Purpose | Operator/API action audit trail, idle timeout, transport-level audit |
| Storage | SQLite-backed audit records (not the coding ledger) |
| Isolation | Separate from workflow sessions; no automatic cross-reference today |

When docs or code say "session", identify which kind is meant. Cross-wiring
workflow ledger APIs to audit UUIDs (or the reverse) is a design change, not a
drive-by fix.

### Project Connector continuity (standing)

The canonical project-bound path reuses the existing SQLite Connector Task,
run, and event model. It adds only a lightweight durable exact mapping:

```text
hashed client window + authenticated subject + connector project + root hash
→ current durable connector task
```

`task_start` owns duplicate-free context create/continue, instruction append,
project switch/restore, read-only-to-write workspace upgrade after scope
checks, and selective context-fingerprint refresh. This mapping is neither a Workflow
Session nor an Action Audit Session, and it must not dual-write either ledger.
Raw transport identifiers are never persisted or exposed as tool fields.

Connector execution mode is intentionally binary: `normal` means writable work
inside the managed isolated Git worktree, with stable-result handoff and
host-local human accept/reject; `read_only` means analysis without project
writes, commands, or checks. Pre-0.4 `inspect` is retired with no alias or
OS-specific restricted-shell successor. A durable legacy inspect Connector Task
keeps its historical row for review/reject/diagnosis but fails closed on any
execution, mutation, continuation upgrade, or accept path.

Mode transitions are one-way with respect to writable authority: `read_only →
read_only`, `read_only → normal`, and `normal → normal` are valid; `normal →
read_only` is rejected. Canonical durable shape is part of the same contract:
`read_only` is non-isolated and executes at the target root, while `normal` is
isolated with a Git baseline. Any inconsistent persisted row fails closed, any
isolated writable result requires structured checks before finish, and only a
canonical normal isolated result may apply a patch during host-local accept.

### Correlation decision (standing)

If the two systems are linked later, use **optional, explicit, one-way**
correlation only:

| Decision | Choice |
|---|---|
| Direction | Action Audit → `workflow_session_id: Option<String>` (`wc_sess_*`) |
| Authority | Store on the audit side (prefer event/record); Workflow Session does not own an Action Audit id list |
| Lifecycle | Independent; audit never drives Workflow create/close/guards |
| Inference | Forbidden from current Action Session, time, thread, connection, window identity, or other implicit Workflow Session selection |
| Missing field | Keep unlinked behavior |
| Bad format | Parameter error; never silent remap |

The optional correlation contract is summarized above; dual-model lifecycle and
identity rules live in [`session-model.md`](session-model.md).

### Authority decision layer (standing)

Authority is a **decision layer** for whether a consequential tool invocation
may proceed under the active mode. It is **not** a Workflow Session manager,
not Action Audit, and not lifecycle tracing.

| Layer | Owns |
|---|---|
| Authority | auto-authorize / deny outcomes (`trusted_agent` \| `restricted`) |
| Workflow Session | task context and bounded evidence (`wc_sess_*`) |
| Action Audit | HTTP/operator action facts (SQLite) |
| Lifecycle trace | optional request-path observation |

**Default mode is `trusted_agent`** (self-hosted single-operator product
default): no human wait, no approval interruptions, every permission-bearing
call still records an auditable decision. Hard safety (path, secrets, session
guards, scopes, agent policy) remains independent and **not overridable** by
authority mode.

**Implementation:** module under `src/tool_runtime/permissions/`;
authoritative single evaluation at ToolRuntime **dispatch** before mutation;
kernel reuses the attached decision and does not re-evaluate. Modes:
`trusted_agent` auto-authorizes after hard safety; `restricted` denies runtime
tools; unknown values and any set legacy `WEBCODEX_PERMISSION_MODE` fail
closed (see §6). Full contract: [`permission-model.md`](permission-model.md).

---

## 2. Internal API evolution (background)

WebCodex is an **internal / self-use** project. There are no supported external
API consumers, public SDKs, or third-party stable clients of the runtime tool
surface today.

Standing executable rules (also summarized in `AGENTS.md`):

1. Do not retain compatibility fields for hypothetical consumers.
2. Do not emit both a canonical field and an alias field for the same concept.
3. Do not add deprecated aliases, legacy fallbacks, dual-output shapes, or
   version-translation layers without a concrete migration requirement.
4. When duplicate representations are found, choose one canonical structured
   representation and delete the others from outputs, schemas, tests, and docs
   in the same change.

Before keeping any compatibility layer, name a **specific consumer** or a
**specific public contract**. A `version` (or parser version) field may identify
protocol shape; it is not a reason to keep duplicate or alias fields.

When external stable consumers genuinely exist later, revise this decision
explicitly and define a bounded migration window for that concrete contract.

---

## 3. Test organization guidance

Executable editing rules for tests live in `AGENTS.md`. Additional layout
guidance:

- Prefer a `tests/` submodule over large ordinary test blocks in production
  `mod.rs` files.
- `src/tool_runtime/mod.rs` must remain a runtime module, not a test warehouse.
  Domain groups under `src/tool_runtime/tests/` include `schema`, `tool_call`,
  `dispatch`, `sessions`, `checkpoint`, `files`, `git`, `jobs`, and `metadata`.
- Shared setup belongs in `tests/support.rs` or a narrow domain helper.
- Prefer table-driven tests for repeated matrices; keep exact assertions for
  security, destructive actions, required schema fields, session guards, and
  transport envelopes.
- Suggested soft limits: split files beyond ~2,000 lines or mixed domains;
  extract fixtures when a single test exceeds ~80 lines.
- After mechanical test moves, keep names and assertions stable first; semantic
  cleanup in a separate change.
- Use `#[ignore]` only for real external dependencies, long network behavior, or
  intentionally heavy integration; document why.

See also [`TESTING.md`](../TESTING.md).

---

## 4. Validation evidence semantics (product)

- Dedicated validation tools and `run_shell`/terminal `run_job` calls declaring
  `validation`, `test`, `build`, `format`, or `release` purpose project into the
  same bounded execution-evidence contract. Tool name is not the source of
  truth for whether validation occurred.
- Evidence carries execution source, stable assertion/command identity,
  purpose, bounded command summary, project-relative cwd, shell/executor,
  execution state, exit code, detected summary, bounded output metadata,
  timestamps, and failure classification. Full unbounded logs are not ledger
  evidence. The projection distinguishes immutable raw ToolResult success from
  validator/correctness success and request-scoped evidence gaps.
- Structured validation target identity describes what was executed. Cargo test
  count assertions such as `require_tests` / `min_tests` are invocation-scoped
  evidence requirements, not part of that target identity and not durable task
  obligations.
- Retry resolution for real validation failures is exact by stable target
  identity. A later successful validation can resolve only failures for that
  identity; it never deletes or rewrites historical events. Request-scoped
  evidence assertion failures remain visible as evidence gaps rather than
  correctness failures.
- Closeout and review expose `historical_failures`, `resolved_failures`,
  `unresolved_failures`, and separate evidence-gap facts. Resolved or stale
  history is advisory; only current actionable command/test failures are hard
  blockers.
- `validation_summary` is a read of existing ledger evidence; it does not
  re-run Cargo/shell or replace `finish_coding_task`. Handoff and finish reuse
  this projection instead of building independent validation truth.
- `continuation_feedback` (surfaced by `finish_coding_task` and
  `session_handoff_summary`; its `validation_delta` part
  also by `validation_summary`) is a deterministic, read-only projection of
  the prior attempt over existing ledger/evidence/Job/message-board state. It
  is never an LLM summary, never a new verdict, never a second attempt state
  machine, and introduces no new persistent table. Validation delta is only
  comparable when scope/evidence and parser identity are proven; otherwise it
  reports a stable reason code. `scope_identity` is an opaque, domain-separated
  SHA-256 over the normalized structured scope — it never re-exposes command
  text or absolute paths. When the attempt boundary has been evicted by the
  bounded event window, the projection reports `complete = false` rather than
  masquerading a truncated window as the session start. See
  [`session-model.md`](session-model.md) §Continuation feedback.

Closeout is an Agent-ready fact package, not a context-free engineering judge.
Its primary layers are `facts`, `hard_blockers`, and `advisories`. Ordinary
dirty worktrees, bounded truncation, optional validation not observed, and
resolved history are advisory. Permission/session-guard denial, unresolved
workspace conflicts, command/test failures, blocking active executions,
sensitive-path risks, and consistency errors are deterministic blockers.

---

## 5. Refactor preference (design stance)

- Prefer small, reviewable refactors over unbounded accretion when a module
  becomes a dumping ground.
- Do not mix behavior changes with mechanical moves unless unavoidable; report
  any semantic change explicitly.
- Do not preserve obsolete compatibility layers by default (see §2).
- Structural refactors that reduce coupling or clarify ownership are allowed
  when scoped to the task; unrelated broad rewrites are not.

---

## 6. Canonical two-mode authority with fail-closed legacy env rejection

The permission-mode system (`WEBCODEX_PERMISSION_MODE` with
`dev_auto_approve` / `audit_only` / `require_approval`) is replaced by one
canonical authority mode.

| Decision | Choice |
|---|---|
| Env var | `WEBCODEX_AUTHORITY_MODE` = `trusted_agent` \| `restricted` |
| Default (unset/empty) | `trusted_agent`; source reported as `default` |
| `trusted_agent` | Consequential runtime tools auto-execute after hard safety with no approval interruptions; external release actions remain user-task-scoped; every permission-bearing call records an auditable ledger decision (`policy=trusted_agent`, `status=auto_approved`, `reason=trusted_agent_authority`) |
| `restricted` | Runtime tools deny (`restricted_requires_human_authorization`); connector `commands_run` keeps the one-time human approval loop |
| Legacy env set | Invalid configuration; consequential tools fail closed with `invalid_authority_mode:...` and source `rejected_legacy_env:WEBCODEX_PERMISSION_MODE`. No alias, no migration |
| Shared surfaces | Both modes share the same tool implementations, schemas, session model, evidence, and audit records |
| Projection | `runtime_status` and internal full startup diagnostics report one canonical `authority` object; the sparse external `work_on_project` projection omits it. The old `permissions` profile object is deleted |
| Connector | Under `trusted_agent`, `commands_run` records a durable `authority_auto_authorized` task event instead of approval records or `approval_required` interruptions |

Hard boundaries are never relaxed by authority mode: OAuth scopes, project
boundary/allowed roots, explicitly read-only sessions, path and sensitive-path
policy, concurrent-overwrite guards, credential redaction, job cancel/reclaim,
and immutable release targets. Full contract:
[`permission-model.md`](permission-model.md).

---

## 7. Connection layers are an observation contract

`runtime_status.connection_layers` reports facts that were actually observed;
it never infers readiness from configuration.

| Decision | Choice |
|---|---|
| Layer envelope | Every layer carries `{status, observed_at, source, age_secs, stale_after_secs, reason_code}` plus layer facts |
| No config-inferred readiness | `connector_endpoint` readiness comes only from readiness probes or successful connector requests; configuration presence never implies `ready`. `runner_process` never fakes "running"; a stale registration is never presented as callable |
| Explicit Workflow targeting | Full-runtime Workflow Sessions have no process-local or durable window binding. `runtime_status` exposes no Workflow binding layer. Ordinary project tools without an explicit business Session or authorized wrapper recorder execute unlinked to Workflow Session state. This remains separate from Connector-owned window/project/task continuity |
| Full-runtime start/continue | `work_on_project(session_id=<id>)` continues exactly that authorized Active same-project Session; omission creates a fresh Workflow Session. Stable window or credential identity never selects a Workflow Session. `work_on_project` calls the shared coding workflow engine directly; there is no second internal ToolCall identity |
| Canonical model coding bootstrap | `work_on_project` is the external runtime coding bootstrap. `registered_tool_specs` defines the canonical model-visible runtime universe used by discovery and generic ToolCall admission. A startup-selected model surface may project that universe more narrowly: `local_coding` lists its focused typed set, `adaptive_runtime` lists a smaller typed core plus one long-tail gateway, and `full_operator_runtime` expands the runtime universe. Retired wire names such as `start_coding_task` fail closed before dispatch and never contribute selector names or flattened model fields |
| Runtime exposure selection | The Server owns one top-level `RuntimeExposure`. Complete `WEBCODEX_CONNECTOR_SURFACE=task-v1` configuration selects `ProjectConnector`, exposed publicly as `project_connector`; ProjectConnector is a project-bound ConnectorTask capability contract, not a `ModelSurface`. Without Connector configuration, exposure is `Runtime(ModelSurface)`: an unset `WEBCODEX_MCP_MODEL_SURFACE` selects `local_coding`, while `local-coding-v1`, `adaptive-runtime-v1`, and `full-operator-v1` select `local_coding`, `adaptive_runtime`, and `full_operator_runtime` explicitly. `adaptive_runtime` direct admission/order is statically declared by canonical `ToolDefinition`s; ordinary model-visible runtime tools default to the bounded long-tail gateway unless explicitly promoted to direct. Gateway calls keep the target tool's existing scope, authority, permission, argument, effect, and Session/ACK semantics. A Connector + `WEBCODEX_MCP_MODEL_SURFACE` conflict, an unsupported value, or partial Connector configuration fails startup. MCP GET/initialize/discovery, `runtime_status.runtime_exposure`, and the startup log report the same flattened exposure name |
| Meaningful-activity rule | `last_successful_tool_call` records only successful meaningful calls, scoped by principal/project/surface/session/tool. `runtime_status`, `list_tools`, `list_runners`, `list_projects`, and `tool_manifest` never refresh it. Bounded in-memory store; no arguments, outputs, or secrets |
| Independence | Layers degrade independently; `not_observed` on one layer must not be collapsed into a global offline verdict |

---

## 8. Canonical external startup projection (hard cut)

`work_on_project` is the only external runtime coding bootstrap. The retired
`start_coding_task` wire/API name and its advanced input schema are not accepted.
The shared coding workflow engine owns startup behavior directly. Bounded
`minimal|standard|full` diagnostic projections are retained only behind test
seams; those controls are not a public tool argument or ToolCall identity.

| Decision | Choice |
|---|---|
| Removed wire entry | `start_coding_task` has no current ToolDefinition or compatibility schema and follows ordinary unknown-tool rejection; `work_on_project` is canonical |
| External projection | `work_on_project` returns one deterministic sparse startup projection and does not expose full runtime/connection/authority diagnostics |
| Internal `standard` | Default bounded Coding brief used by shared startup plumbing: strict session/project/workspace, incremental repository instructions, bounded continuation evidence, semantic-navigation summary, blockers/warnings, and concrete next actions |
| Internal `minimal` / `full` | Retained only as implementation-level projection modes for internal callers/tests; they are not generic HTTP/MCP tool inputs |
| Rule snapshot lifecycle | Fresh sessions load bounded content; unchanged same-process continuations reuse the in-memory fingerprint snapshot without repeating content; source additions/deletions/content/truncation changes return new bounded content; explicit or restart-restored Sessions reload because durable storage never contains rule bodies |
| Unknown/removed external fields | Unknown or removed tool names fail closed before legacy argument interpretation; `work_on_project` keeps its own strict schema |

No alias or dual shape is kept for the removed flags (consistent with §2).

---

## 9. Mixed-version diagnostics without compatibility fallback

Runner registration reports `process_started_at` and
`build {version, git_commit, git_dirty}`; `runtime_status` projects
package/protocol compatibility separately from exact source alignment.

| Decision | Choice |
|---|---|
| Compatibility shape | `version_compatibility.status` is `compatible \| version_mismatch \| capability_mismatch \| no_runners`; each Runner reports `version_matches_server`, protocol facts, and compatibility reason/action. Package-version compatibility is not exact source identity. |
| Source alignment | `version_compatibility.source_alignment.status` is `aligned \| different \| unknown \| no_runners`; per-Runner source alignment reports `git_commit_matches_server` and `source_matches_server`. Exact alignment is true only when commits match and both builds explicitly report `git_dirty=false`; differing commits or a dirty side are different, incomplete build facts are unknown. |
| Connected ≠ compatible | Transport liveness never implies protocol/package compatibility or exact source alignment. |
| Direction | Compatibility and source-alignment facts provide separate actions; there are no fallback shims or version-translation layers. |
| Shell dialects | `ShellProfilesSummary` reports `default_dialect` (`sh` \| `bash` \| `custom`) and `available_dialects`; each profile entry reports `dialect`. The server never guesses the remote shell; custom profiles that do not map to sh/bash report `custom`, and agents needing deterministic syntax must pass an explicit `shell=sh\|bash`. No PATH/env/init-script contents are ever sent |

---

## 10. Model execution and durable continuation

The standing direction for model-facing execution is defined in
[`ARCHITECTURE.md`](../ARCHITECTURE.md). The durable decisions are:

1. **Structured lifecycle is execution truth.** Retry safety must not depend on
   interpreting prose. `command_started`, completion state, failure
   classification, Job state, and guidance must not contradict one another.
2. **Prefer direct argv/process execution for ordinary commands.** Shell command
   strings remain an escape hatch for real shell semantics; long script content
   belongs in a bounded payload channel rather than an ever-larger quoted string.
3. **One execution may outlive one tool/model turn.** When work exceeds a short
   synchronous grace window, the same execution should continue as a durable Job;
   handoff must not be implemented as cancel-and-retry.
4. **Job/observation is the continuation API.** Durable Job identity, lifecycle,
   bounded logs, observation token, cancellation, ownership, and recovery/lost
   semantics remain OS-, transport-, and presentation-neutral. Batch observation
   should reuse this model rather than create a second scheduler or revision
   system.
5. **Optional host UI is an adapter, not an owner.** MCP Apps or another host may
   observe Jobs and later resume a model, but core execution cannot depend on
   Apps, MCP Tasks, MRTR, elicitation, progress extensions, or iframe state. If
   automatic model resume is provided, exactly one durable continuation domain
   owns each logical resume event; independent Job Views, cards, or Host views do
   not race to resume the model. For Agent-bound continuation, the Agent Wake /
   Wake Delivery Attempt domain owns that logical continuation; Host/controller
   state is adapter-local delivery state rather than a second WebCodex
   continuation truth.
6. **Transport fallback must preserve execution semantics.** Polling, WebSocket,
   and QUIC may differ in delivery behavior, but none may silently duplicate a
   command or turn a transport stall into a false pre-start rejection.

Do not broaden an execution task into fleet upgrade management, Windows SCM
productization, a generic process/service API, PTY support, or polished MCP App UI
unless the user task explicitly requires that scope.

---

## 11. 0.4 compatibility floor

`v0.4.0` is the new compatibility floor. The `0.3.x -> 0.4.0` boundary is an
intentional pre-release cleanup boundary: release upgrade notes may require a
coordinated change for the Runner generation cleanup, retired CLI aliases,
pre-0.4 persisted-state cleanup, Tool/runtime surface cleanup, and authority or
configuration cleanup. That pre-0.4 freedom does not continue through the
`0.4.x` patch series.

The `v0.4.0` tag is a compatibility reference point, not a blanket promise to
retain every spelling that appeared in that release. During active development,
compatibility code is retained when it has a concrete consumer: accepted
persisted state, mixed-version Server/Runner operation, a current external
workflow or installer, or a required fail-closed security/privacy migration
boundary. An implementation plus tests that only assert that implementation
exists is not by itself a consumer. Published-but-unused CLI/API aliases and
duplicate machine-readable fields may therefore be removed after an exact
consumer search.

Where a concrete consumer does exist, compatibility remains narrow and
fail-closed. Protocol generation 2 remains the 0.4 Server/Runner rolling
baseline; additive capabilities do not expand its required set, and absence is
handled as unavailable rather than inferred authority. Durable DB, Workflow
Session, and registry state is migrated or quarantined deterministically rather
than silently reinterpreted. MCP `structuredContent` remains the canonical
machine-readable `tools/call` result; `content.text` is only the concise human
fallback.

The product concept and public lifecycle namespace are **Runner**. Before the
`v0.4.0` compatibility floor, the local primary config filename is normalized
from `agent.toml` to `runner.toml`. The compatibility contract is intentionally
narrow and deterministic: a config directory containing only legacy
`agent.toml` continues to use that file; one containing only `runner.toml` uses
the canonical file; a directory containing both fails closed rather than
choosing a winner; a directory containing neither creates/targets
`runner.toml`. Explicit `--config PATH` remains exact and does not inspect a
sibling filename. Explicit `--profile` similarly selects its authoritative
profile directory before environment defaults are considered.
`WEBCODEX_RUNNER_CONFIG` is the canonical default-path env override, while
`WEBCODEX_AGENT_CONFIG` remains a legacy alias; setting both is an error only
when environment defaults are actually consulted.

The same pre-`v0.4.0` normalization applies to the Runner-owned project
registry: `project_registry_dir` and `project-registry/` are canonical for new
state, while a sole legacy `projects_dir` field or `projects.d/` directory may
continue to identify existing state in place. New and legacy fields together,
or both default directory names together, fail closed; WebCodex does not merge,
copy, rename, or choose between two registries implicitly. The registry remains
a directory of Runner-owned project registration records, not a second workspace
or project-root abstraction.

Project registration provenance is also normalized before the `v0.4.0` floor.
The generic project-record `kind` field remains open project metadata, while the
optional `registration_source` field describes how the record entered the
Runner registry (`explicit` or `auto_registered`). New path auto-registration
persists `registration_source = "auto_registered"` and does not persist a fake
`kind`. For old records only, absent `registration_source` plus the exact
historical `kind = "auto_registered"` sentinel remains a compatibility fallback;
a present new field is authoritative. This interpretation is kept separate from
the raw record representation used for project revision/CAS hashing, so merely
upgrading WebCodex does not change an unchanged legacy record's revision. During
rolling upgrades a new Runner may still project the historical sentinel on its
inventory and path-operation wire results for a newly auto-registered project
with no genuine kind so an old Server can classify it, while new Servers use the
explicit additive provenance field.
Public `list_projects.source` remains `agent_registered` / `auto_registered`.

The pre-0.4 managed-temporary-project lifecycle is retired rather than carried
into the `v0.4.0` product contract. Current project creation uses explicit
`create_project`; existing directories use `work_on_project(path)` or
`register_project`. The retired `temporary_projects_root` key no longer has a
typed Runner configuration meaning. Runner configuration intentionally ignores
obsolete unknown top-level keys, so an old file containing this key remains
loadable without preserving its former validation, warning, or runtime field.
Existing project-registry records whose generic
`kind = "managed_temporary"` value predates this cleanup remain readable as
ordinary registrations; current Server projections do not treat that value as
an active lifecycle. A legacy Server request containing the old
`managed_temporary_project` create flag fails closed on a new Runner before any
filesystem mutation. This retirement does not change protocol generation,
baseline capabilities, `client_id`, `agent_project_id`, or runtime project ids.

Other older `agent_*` names below have concrete token, persisted-state, or wire
consumers and are therefore retained rather than cosmetically duplicated. In
particular, `WEBCODEX_AGENT_TOKEN`, `wc_agent_*`, `agent_instance_id`, runtime
project ids of the form `agent:<client_id>:<project_id>`, and established
DB/wire `agent_*` fields keep their existing names. This local filename migration does not imply
a Server/Runner protocol-generation or wire-identity rename.

Compatibility never requires retaining a known authentication bypass, unsafe
authority, ambiguous or stale identity, or weakened fail-closed validation. A
security- or correctness-required break is allowed only when it is explicit,
narrowly scoped, documented, and accompanied by the required upgrade action in
release notes.
