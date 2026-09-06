# ACP Coding Agent Run Contract

This document defines the execution, identity, lifecycle, configuration,
permission, observation, and recovery contract for Runner-owned Agent Client
Protocol (ACP) coding agents.

The product model is a **protocol-aware detached Job**, but the product object is
not a WebCodex Job. `CodingAgentRun` is a separate execution primitive whose
payload is ACP protocol state and structured coding-agent activity rather than a
shell command plus stdout/stderr.

## Current implementation

WebCodex implements this contract as a Codex-first vertical slice without changing the
product identity model. `CodingAgentRun` remains separate from Jobs and Workflow
Sessions, and the Server/Runner boundary is a closed typed protocol rather than
an ACP JSON-RPC tunnel.

The Runner owns an `[acp]` / `[[acp.agents]]` startup configuration. Each agent
entry supplies a logical id/name, executable, argv, explicit `env_from_env`
mappings, and an operator ceiling for run-level ACP config option ids. There is
no production `codex-acp` default. The provider child is always spawned after
`env_clear()` and receives only configured mappings. Missing source variables
fail before provider process start.

WebCodex exposes exactly three model tools: `coding_agent_start`,
`coding_agent_observe`, and `coding_agent_cancel`. They require the independent
`coding_agent:run` OAuth scope. Direct shared-key, open-anonymous, project
credentials, existing OAuth clients, `project:write`, `job:run`, and `mcp:local`
do not imply it. The hosted shared-key OAuth bridge can add this scope only via
an explicit client-provisioning opt-in; existing clients are never widened on
Server upgrade.

`recording_session_id` is carried by the existing generic stateless recorder
wrapper rather than duplicated as CodingAgentRun business input. It can attach
bounded `coding_agent_started`, `coding_agent_waiting_permission`, and
`coding_agent_terminal` lifecycle evidence to an exact Project-matching
Workflow Session. Recorder provenance never crosses the Server/Runner execution
request, never grants Run authority, and never stores prompt/reasoning/tool
bodies or the private ACP session id.

The Runner durable record intentionally retains only recovery identity and
certainty metadata. Immediately before writing `session/prompt`, it durably
crosses `prompt_dispatch_may_have_occurred`; any nonterminal restart from that
phase becomes `lost/outcome_unknown` and is never redispatched. A correlated
terminal result is durably recorded before active execution state is reclaimed.

The Runner uses stable ACP v1 schema types and a narrow stdio client. It advertises only
implemented client capabilities and supports initialize, session/new, validated
session/set_config_option, session/prompt, session/update,
session/request_permission, and session/cancel. Unsupported agent-to-client
requests fail closed. Permission requests are normalized for observation, never
auto-allowed, receive ACP `Cancelled` after a bounded deadline (or before prompt
cancel), and the same prompt is then observed to terminal correlation.

## Product model

The intended user flow is:

```text
ChatGPT / API client
  -> coding_agent_start
  -> exact WebCodex Project + Runner + configured ACP provider
  -> CodingAgentRun R
  -> Runner-owned ACP child
  -> initialize
  -> session/new
  -> optional validated session/set_config_option calls
  -> session/prompt
  -> structured session/update observations
  -> correlated terminal prompt response
```

WebCodex owns:

- exact Runner and registered Project routing;
- provider identity and stale-provider fencing;
- run admission and idempotent initiation;
- bounded lifecycle, timeout, cancellation, and process-tree cleanup;
- sanitized structured observation;
- provenance, authorization, audit, and telemetry;
- recovery classification when transport or process state is uncertain.

The ACP agent owns:

- coding reasoning and planning;
- its own shell/edit/tool decisions;
- its own sandbox and approval behavior;
- account, organization, model, and provider policy;
- provider-specific coding behavior.

WebCodex must not turn ACP into a second implementation of WebCodex file, shell,
or patch tools. The ACP child is a delegated local coding agent, not an MCP tool
provider and not a raw JSON-RPC endpoint exposed to remote callers.

## Four identities that must remain separate

```text
Workflow Session W
      |
      | optional provenance / evidence relationship only
      v
CodingAgentRun R          model-visible: wc_agent_run_...
      |
      | Runner-private protocol execution
      v
ACP session S             never model authority
      |
      v
Codex / future ACP agent

WebCodex Job J             independent process-execution primitive
```

### Workflow Session

A Workflow Session remains the bounded coding-task evidence and collaboration
ledger described by `session-model.md`. A Run may record the `wc_sess_*` that
initiated it, but that reference is provenance only.

Knowing a Workflow Session id does not authorize a Run. Recorder metadata must
never be accepted as Run authority, and a Run must continue to enforce its own
caller/project/provider ownership on observe and cancel.

### CodingAgentRun

`CodingAgentRun` is the WebCodex business identity for one admitted autonomous
coding turn. The public id is opaque and WebCodex-owned
(`wc_agent_run_*`). It is the only agent-execution identity a model needs
to retain after successful admission.

A Run owns the bounded normalized event history, current state, exact Project,
exact Runner instance, exact ACP provider instance, a bounded initiation-intent
fingerprint, optional Workflow Session provenance, and the Runner-private ACP
session id. Raw prompt/config input is execution data, not durable authority
identity, and should be discarded once it is no longer needed for active dispatch.

### ACP session

The raw ACP `sessionId` is a provider/protocol identity. It must stay Runner
private and must never be accepted as Project, Workflow Session, or Run
authority.

Provider support for `session/load` can help recover a session, but does not
prove that an in-flight prompt is safe to retry. The ACP session id must not be
exposed or used as the product Run id.

### Job

A WebCodex Job is stdout/stderr/process/exit oriented. A CodingAgentRun is
agent-message/reasoning/tool/file/terminal/usage/permission oriented. Reusing the
Job record would either discard ACP structure or overload Job semantics with a
second event model.

WebCodex should reuse proven Job machinery where the semantics really match:

- `ManagedChild` process-tree ownership and cleanup;
- bounded timeout and cancellation patterns;
- detached initiation/idempotency concepts;
- opaque observation-token patterns;
- Runner inventory and same-instance reconciliation concepts;
- existing `execution_state`, `failure_kind`, and `recovery_kind` vocabulary.

It should not serialize ACP updates into stdout/stderr or make `job_id` an alias
for `run_id`.

## Protocol requirements

### Transport and correlation

The Runner ACP client uses newline-delimited JSON-RPC 2.0 over stdio.
Each request/response pair carries a JSON-RPC id. Notifications such as
`session/update` and `session/cancel` are not terminal acknowledgements.
Agent-to-client requests such as `session/request_permission` have their own
request ids and require a correlated client response.

Identity lifetime is intentionally asymmetric:

| Identifier | Lifetime and authority |
|---|---|
| JSON-RPC request id | Connection/process-local correlation only; never recovery or authority identity. |
| ACP `sessionId` | Provider-private. Persistence/loading across adapter processes is a negotiated provider capability, not a universal ACP/WebCodex guarantee. |
| ACP provider instance id | WebCodex Runner process/provider-instance fence; replacement makes old requests stale. |
| `wc_agent_run_*` | WebCodex business identity, retained independently of one HTTP/MCP request and reconciled only from authoritative Runner Run state. |
| Workflow `wc_sess_*` | Independent evidence/collaboration identity; optional Run provenance only. |

The Runner must own the JSON-RPC id space/correlation machinery. Remote callers
must never provide a JSON-RPC method or id.

### Initialize and version negotiation

The client sends `initialize` with its supported ACP protocol version and client
capabilities, and validates the returned negotiated version and advertised
capabilities before creating a session. Unsupported/malformed negotiation is a
pre-prompt failure and must fail closed.

WebCodex must advertise only ACP client capabilities that it actually implements.
`session/request_permission` is a baseline client method, while client-side
filesystem, terminal, and elicitation methods are optional capability-gated
surfaces. WebCodex must not advertise `fs.readTextFile`, `fs.writeTextFile`, terminal,
or elicitation support merely because the selected agent can use those concepts.
An unexpected unsupported agent-to-client request must receive a bounded
fail-closed protocol response rather than hanging the Run.

### Session creation

The Runner supplies the exact registered Project root as the `session/new`
`cwd`. The caller cannot provide an arbitrary cwd. MCP server inputs, if any are
supported later, are Runner-owned; WebCodex should send only the closed configuration
it explicitly supports.

The new-session response supplies the private session id and current advertised
modes/models/config options. These are provider observations, not authority.

### Prompt lifecycle

`session/prompt` is one correlated long-running request. `session/update`
notifications may arrive before its response. The correlated prompt response
with a `stopReason` is the terminal protocol result for the turn.

The Run contract has no separate durable acknowledgement proving exactly when a
prompt became safe to retry. Once the Runner has successfully written/flushed
the prompt request, loss of the ACP process/stdio or its correlated response can
leave the effect uncertain.

### Cancel

`session/cancel` is a notification naming the private ACP session. WebCodex should
send it only from Runner-owned state, then continue bounded observation for the
prompt's correlated terminal response. A successful write of the cancel
notification alone is not proof that cancellation took effect.

### Permission requests

`session/request_permission` is an ACP agent-to-client request, not a WebCodex
runtime-tool permission evaluation. The client must implement it because the
current Codex adapter can actually send it. It must never default to allow.

Current `codex-acp` bridges Codex approval activity through this callback and
fails closed when the ACP client's approval interaction fails or is cancelled.
This reinforces the product boundary: WebCodex performs one admission decision
for the Run, then the delegated agent owns its normal internal coding policy;
WebCodex does not rerun `PermissionEvaluator` for every ACP tool action.

### Error and process-exit semantics

Request-level protocol failures are JSON-RPC errors; terminal prompt outcomes
are represented by the correlated prompt result/stop reason. The ACP child
process has a separate OS lifetime. Process exit is diagnostic/lifecycle input,
not a substitute for a terminal prompt result.

If the child exits after prompt dispatch without a correlated prompt response,
WebCodex cannot infer success or failure from its exit code alone. The Run must
be treated as uncertain unless exact protocol recovery proves otherwise.

## Configuration semantics

### `config` omitted or `{}` means no WebCodex override

For WebCodex:

```text
config omitted
or
config = {}
```

means **send no `session/set_config_option` calls**.

It does not mean "the ACP adapter behaves exactly like a bare local Codex CLI".
The adapter itself may have defaults. Advertised defaults are provider policy,
not WebCodex overrides; inspect the selected live session instead of hard-coding
option names, modes, or values from a previous provider version.

Therefore the precise inheritance contract is:

> WebCodex inherits the selected Runner-owned ACP provider's effective defaults
> by abstaining from run-level ACP config overrides.

The Run should record a bounded sanitized snapshot of the effective advertised
config ids/current values needed for diagnosis. It must not claim that those
values came directly from `~/.codex`, an account, or organization policy.

### Explicit run-level overrides

For a non-empty caller `config` object WebCodex must perform this order:

1. start/initialize the exact configured provider;
2. create the private ACP session;
3. obtain the session's advertised config options;
4. reject any caller key not currently advertised;
5. reject any value not legal for that advertised option;
6. apply Runner/operator allow/deny policy for remotely overridable options;
7. call `session/set_config_option` only for approved explicit overrides;
8. validate the returned refreshed config options;
9. only then dispatch the prompt.

An invalid override is `not_started` with `recovery_kind=fix_input`; it must not
partially begin the coding prompt.

The operator policy is an explicit allowlist of option ids rather than
a generic policy language. Permitting one id delegates selection among that
session's currently advertised legal values; unknown newly advertised option ids
remain non-overridable. If a future provider exposes materially different
authority levels as values of one option, add an explicit value ceiling for that
concrete need rather than implying an existing per-value policy.

### Fields remote callers never control

`coding_agent_start` must not accept:

- executable or argv;
- arbitrary environment or secret/API-key material;
- arbitrary cwd;
- transport selection;
- raw ACP JSON-RPC method/params/id;
- raw ACP session id.

Those belong to Runner configuration or closed Runner protocol.

## Runner-owned ACP provider configuration

ACP uses its own Runner configuration section, separate from
`[mcp]` or the existing Claude MCP tool-provider router. ACP is a bidirectional,
long-lived coding-agent protocol with callbacks and structured turn state; the
MCP gateway is a `tools/list`/`tools/call` provider surface. Reusing the latter
would collapse distinct semantics.

A minimal configuration is:

```toml
[acp]
max_concurrent_runs = 1

[[acp.agents]]
id = "codex"
name = "Codex"
executable = "/runner/owned/path/to/codex-acp"
args = []

[acp.agents.env_from_env]
# Explicit operator mappings only. Values never go to the Server.
HTTPS_PROXY = "HTTPS_PROXY"
```

The exact executable example is operator-specific; WebCodex must not prescribe
`npx -y` as a production default or download packages at request time.

`[acp]` is startup/restart-owned. A configuration change requires Runner restart;
there is no hot ACP provider replacement.

Each advertised provider needs bounded sanitized identity such as:

```text
provider_id             # logical id selectable within the exact Project Runner
provider_instance_id    # opaque internal fence for this startup-owned provider instance
name
configured/routing capability facts only
```

ACP configuration is startup/restart-owned and has no hot provider replacement,
so a second `provider_revision` authority token has no demonstrated purpose.
The exact Runner `agent_instance_id` plus opaque `provider_instance_id` are the
replacement fence. A future hot-reload design can add a revision only if it
creates a distinct live replacement boundary.

Public callers select only the logical provider id after Project resolution. The
Server captures the exact Runner/provider instance internally and revalidates it
immediately before dispatch; ephemeral provider-instance identity is not a
model input. A new Runner/provider instance makes an already-bound request stale
before Run start. The Server must never receive executable path, argv, PID,
environment values, credential material, stderr, local config contents, or raw
ACP auth data.

`env_from_env` is resolved only on the Runner immediately before spawn. The
caller supplies neither source names nor values. The ACP child must clear the inherited process environment
first, then inject only operator-declared `env_from_env` mappings. Missing mapped
sources fail before child start. WebCodex must preserve existing secret-redaction
rules; it must never silently inherit the Runner's complete environment or log
resolved values.

Admission capacity is an ACP-run plane, not Job concurrency. WebCodex should use a
small bounded `max_concurrent_runs` and **reject before start when full** rather
than add queueing/scheduling states. Do not infer ACP capacity from
`max_concurrent_jobs` and do not build a worker pool.

## Project binding and confinement truth

Project binding gives WebCodex three real guarantees:

1. the selected Run is routed to the exact Runner owning the registered Project;
2. the Run records that exact Project identity;
3. `session/new.cwd` starts at the Runner-authoritative Project root.

That is not a filesystem sandbox.

`cwd == Project root` does **not** prove that the delegated agent can read or
write only that tree. ACP itself is not a filesystem confinement mechanism.
The selected coding agent may apply its own sandbox, OS policy, account/org
policy, and approval mode; those controls can be stronger or weaker than
WebCodex file-tool path rules and may evolve independently.

For the current Codex adapter, the effective mode influences Codex sandbox and
approval behavior. WebCodex may report the provider's sanitized advertised
configuration, but it must not translate that into a claim of WebCodex Project
isolation unless WebCodex separately enforces such isolation.

The product description should therefore say **operator-configured delegated
local coding agent**. It must not promise parity with WebCodex `read_file` /
`apply_text_edits` filesystem isolation.

## Permission-request exceptional path

The normal WebCodex authority decision happens once at `coding_agent_start`:

```text
caller auth + exact Project + provider fence + config override policy
  -> WebCodex start admission decision
  -> ACP Run starts
  -> delegated agent applies its own coding policy
```

An ACP `session/request_permission` callback is not fed back through the normal
WebCodex `PermissionEvaluator`, because that would create a second per-action
policy layer over the agent's own approval system.

WebCodex nevertheless must implement the callback. The minimum safe behavior is:

- emit a bounded sanitized `permission_request` Run event;
- enter `waiting_permission` while a bounded response deadline is active;
- never choose an allow option automatically;
- if `coding_agent_cancel` cancels the prompt while a permission request is
  outstanding, answer that request with ACP `Cancelled` as required by v1 and
  then continue the cancel path;
- if only the permission-response deadline expires, answer ACP `Cancelled`.
  Do not synthesize an option selection or mutate the Agent's persistent policy;
- then continue observing the same prompt until it reaches a terminal result or
  becomes lost.

This makes the implementation safe but intentionally incomplete for providers/configurations that
frequently require interactive approval. A later operator UI or model-visible
permission-response capability requires separate evidence and authority design;
it is not part of this contract by implication.

## Minimal CodingAgentRun lifecycle

Use exactly these product states initially:

```text
starting
running
waiting_permission
completed
failed
cancelled
lost
```

Keep prompt dispatch certainty as structured execution metadata rather than
multiplying states. Reuse the existing concepts `not_started`, `started`,
`completed`, and `outcome_unknown` where applicable.

| State | Coding execution / prompt fact | Retry rule | Recovery |
|---|---|---|---|
| `starting` | Run admitted; provider/session/config setup may be in progress. Prompt may still be `not_started`. | Never create a second Run for the same initiation key; observe/reconcile the admitted Run. | `wait` or `reobserve`. |
| `running` | Prompt request was dispatched; agent may have accepted it. | No blind prompt retry. | `reobserve`; after transport failure use `reconcile`. |
| `waiting_permission` | Prompt is active and a real ACP permission callback is pending. | No prompt retry and no automatic allow. | `wait`; WebCodex fail-closes the permission deadline. |
| `completed` | Correlated terminal prompt result proves normal terminal completion. | No retry of the same Run. | `none`. |
| `failed` | Deterministic terminal failure is known, or setup failed before prompt dispatch. | The retained Run itself is terminal and same-key replay only returns it. A new initiation with a new idempotency key is safe only when `execution_state=not_started`; otherwise caller must not infer retry safety. | `none` for the retained terminal Run. Pre-admission tool-call failures may separately use `fix_input` or exact `retry_same`. |
| `cancelled` | Cancellation reached a correlated terminal cancelled result, or the Run was cancelled while prompt was provably not started. Pre-prompt cancellation uses `execution_state=not_started`, no ACP `stop_reason`/`error_code`, and a bounded terminal message stating that no ACP prompt was dispatched. | Do not resend the cancelled prompt as a retry. | `none`. |
| `lost` | No terminal prompt result is available and exact continuation cannot currently be proved. Prompt may have run. | Never blind retry. | `reconcile` / `reobserve`; create a new Run only after authoritative evidence establishes safety or the user intentionally requests new work. |

ACP v1 terminal `stopReason` mapping is closed: `end_turn` becomes
`completed`; `cancelled` becomes `cancelled`; `max_tokens`, `max_turn_requests`, and `refusal` become deterministic `failed` outcomes. Those
non-success stop reasons are correlated terminal responses, so they are not
`lost`, but they also do not prove that the turn had no coding effects and do not
create retry authority. An unknown stop reason is a fail-closed protocol failure,
not normal completion. A correlated JSON-RPC error for the prompt is likewise a
terminal `failed` outcome; loss of correlation/transport before any terminal
response is what produces `lost`.

A `cancelled/not_started` Run is not an ACP terminal response: its `stop_reason`
remains absent because the prompt never crossed the dispatch boundary. Only a
correlated post-dispatch ACP cancellation carries `stopReason=cancelled`.

`lost` is an uncertainty state, not proof that the coding process had no effect.

A timeout is not a separate initial state. On a Run deadline, request cancellation
and wait for a bounded terminal result. A correlated cancelled result becomes
`cancelled`; losing the process/transport before correlation becomes `lost`.
Setup deadline failures before prompt dispatch become `failed` with
`execution_state=not_started`.
A retained terminal `failed/not_started` Run therefore never advertises
`retry_same`: deterministic same-key replay is observation/idempotency only and
cannot redispatch that terminal Run. After correcting the underlying setup issue,
a caller may intentionally create a new initiation with a new idempotency key;
that is distinct from replaying the old initiation.

## Initiation and retry safety

`coding_agent_start` is consequential autonomous execution. It needs a required
bounded caller-chosen `idempotency_key`, using the same semantic pattern as
`run_detached_process` but a separate CodingAgentRun namespace.

The idempotency identity is the stable authenticated principal plus the bounded
caller key; the initiation intent is a separate conflict check. Following the
existing detached-Job precedent, WebCodex derives `run_id` deterministically
from an ACP-specific domain separator, canonical stable principal identity, and
the key. Do not randomly mint the Run id unless an equally strong durable
admission mapping is committed before dispatch; WebCodex should not add that extra
persistence concept.

Before first dispatch, compute a bounded canonical `intent_fingerprint` for the
execution-affecting start intent. Replaying the same derived `run_id` with the
same fingerprint returns/observes that Run and cannot dispatch the prompt twice.
A different Project, logical provider, prompt, config, timeout, or other
execution-affecting intent under the same key is an idempotency conflict before
dispatch. The Runner Run record/inventory carries the `run_id` and fingerprint
needed for post-Server-restart reconciliation, not the caller's raw key or a
retained raw prompt solely for idempotency checking.

The initiation key is not authority and must not be copied to the Runner,
persisted in ordinary evidence, or logged. Deterministic Run identity is what
lets a retry after Server restart meet an already-running authoritative Run
instead of creating a second execution.

Deterministic identity alone is insufficient across a Runner restart because the
new Runner must still know whether that logical Run may already have produced
effects. WebCodex therefore needs a **minimal durable Runner-local Run record** (or an
equally strong existing durable mechanism) for admitted CodingAgentRuns. This is
not a durable transcript and not automatic ACP-session recovery. It stores only
bounded authority/lifecycle facts such as `run_id`, `intent_fingerprint`, exact
Project identity, logical provider identity, conservative dispatch phase, and
terminal metadata.

The dispatch phase must include a crash-safe conservative barrier persisted
**before** writing `session/prompt`. Once that barrier is durable, a restart may
only conclude that the prompt *may have been dispatched* until a correlated
terminal result is durably recorded. A crash after the barrier but before the
actual write therefore sacrifices retryability and recovers as `lost`; that is
preferable to duplicate coding effects. Only a record proven to have remained
strictly before this barrier may recover as `not_started`. A correlated terminal
result may later replace the barrier with bounded terminal metadata. Raw prompt,
config bodies, event transcript, idempotency key, credentials, and ACP messages
must not be stored in this durable record.

Consequently, a same-key initiation after either Server or Runner restart must
first reconcile the deterministic `run_id` against the durable Run record and
current Runner inventory. A matching `lost`, active, or retained-terminal record
is observed/returned and never redispatched; a mismatched fingerprint conflicts.
If the required durable record is unavailable or corrupt after a possibly
started Run, fail closed rather than treating absence as proof of `not_started`.

The most important uncertainty rule is:

```text
session/prompt successfully dispatched
+ ACP transport/process lost before correlated terminal response
= outcome_unknown / Run lost
!= retry session/prompt
```

`session/load` support does not change this rule. Loading the conversation can
recover a durable ACP session, but it does not by itself prove whether an
in-flight prompt completed, partially acted, or never ran.

## Observation delta contract

The model surface is:

```text
coding_agent_start
coding_agent_observe
coding_agent_cancel
```

`coding_agent_observe` accepts:

```text
run_id
 after_observation_token?  # opaque, exact Run-bound
 wait_secs?                # one bounded wait
```

The Runner is authoritative for the Run's bounded event ring and monotonically
ordered observation revision/sequence. The Server returns normalized only-new
events when continuity is provable. It never returns the full transcript on
every call and never exposes raw ACP JSON-RPC.

Initial normalized event kinds:

```text
agent_message
reasoning
plan
tool_activity
file_change
terminal_activity
usage
permission_request
terminal
```

Not every provider must emit every kind. WebCodex maps only protocol/provider updates
whose semantics are understood; unknown raw update variants are ignored or
recorded as a bounded diagnostic count, not forwarded verbatim.

The response must make retention explicit:

- token is opaque and bound to exactly one `run_id`;
- no token returns a bounded current baseline, not unbounded history;
- token calls return only retained changes after that cursor;
- `history_lost=true` when the requested cursor predates reconstructable retained
  events;
- `has_more`/continuation advances only through the last returned event;
- `wait_secs` is one bounded wait, not a stream or subscription;
- serialized model output has a fixed budget;
- terminal state is returned even when there are no new textual events.

Raw reasoning and tool payloads can contain sensitive or very large data. WebCodex
must define per-event bounds/redaction. The bounded Runner observation ring may
retain model-facing message/reasoning/tool summaries needed to observe the Run,
but existing durable Action Audit, generic model-ergonomics telemetry, and
Workflow Session lifecycle evidence must not automatically persist prompt text,
agent-message/reasoning bodies, or raw ACP tool inputs/results. Durable surfaces
should record bounded lifecycle/size/kind metadata unless a future explicit
evidence feature defines otherwise. Environment values, auth data, absolute
provider executable paths, raw credentials, and arbitrary stderr are never event
content.

## Cancel, timeout, restart, and replacement

### Cancel

`coding_agent_cancel` requires exact Run authorization and should be idempotent.
For an active prompt the Runner sends `session/cancel` once, then observes the
same prompt toward terminal state. It must not launch a replacement session or
prompt.

### Control Server restart

A surviving Runner process can retain a Run and its event ring independently of
the Server request that started/observed it. WebCodex uses the Runner reconciliation pattern with a bounded
active/recent-terminal CodingAgentRun inventory. The same Runner `agent_instance_id`, exact `run_id`,
Project, provider instance, `intent_fingerprint`, state, and observation revision
are the recovery authority. The deterministic principal+idempotency-key Run id
allows a retried initiation to correlate with that recovered inventory without
sending the raw key to the Runner.

A Server restart invalidates process-local waits/tokens as needed, but it must
not imply that the Run should be restarted. Re-observation should return a
bounded reset/baseline and fresh token when the Run is still authoritative.

### Runner restart / provider replacement

A new Runner process has a new `agent_instance_id` and new ACP provider instance
identity. WebCodex does not claim transparent active-Run recovery across that boundary.
At startup it first loads the minimal durable Run records described above. A
record strictly before the prompt-dispatch barrier may close as deterministic
`not_started`; a record at/after that barrier without a durable correlated
terminal result recovers as `lost`; a retained terminal record stays terminal
although its in-memory event history may have been lost. The new Runner must not
turn a missing in-memory Run into permission to redispatch a deterministic
`run_id` whose durable record says effects were possible.

Even when a provider can load a durable session in a new adapter process,
WebCodex does not automatically recover active Runs across processes. `session/load` alone does
not provide exact in-flight prompt reconciliation, and guessing would risk
re-executing coding effects.

A stale Server request internally bound to an old provider instance must fail
closed before spawning or retargeting any agent.

## Authorization and scope

ACP delegated autonomous coding requires the independent scope:

```text
coding_agent:run
```

Do **not** infer it from any of:

```text
project:write
job:run
job:detach
mcp:local
```

Those scopes authorize different primitives. Giving an existing credential the
ability to start an autonomous coding agent merely because it can edit a file,
run a Job, or call a local MCP tool would silently broaden authority.

Registering the new scope is not permission to add it to existing default scope
ceilings. In particular, WebCodex must leave direct shared-key model scopes, the OAuth
shared-key bridge defaults, open-anonymous scopes, Project-credential connector
scopes, and already-issued legacy OAuth clients unchanged unless an explicit
operator/consent path grants `coding_agent:run`. The first usable ACP flow needs
such an explicit opt-in issuance path; it must not obtain usability by silently
expanding existing credentials.

For public tools:

- start requires `coding_agent:run` plus normal authorization for the exact
  writable Project;
  Concretely, delegated start also requires `project:write` and the current
  Runner registration must still have `allow_patch=true`; the Runner rechecks
  that writable binding immediately before admitting/spawning the ACP run.
- observe/cancel require the same Run visibility/ownership and exact Project
  boundary; knowing `run_id` is never sufficient;
- Workflow Session provenance grants no additional authority;
- provider selection is constrained to the sanitized providers advertised by
  the exact Runner instance.

One new scope is enough initially. Do not add separate run/observe/cancel scopes
without a demonstrated consumer that needs that split.

## Implementation and regression coverage

The implementation is split across the [core protocol](../../crates/webcodex-core/src/coding_agent.rs),
[Runner ACP lifecycle](../../crates/webcodex-runner/src/webcodex_runner/coding_agent.rs),
and [Server runtime](../../src/tool_runtime/coding_agent.rs). The Server/Runner
boundary uses closed typed operations; it does not tunnel arbitrary ACP JSON-RPC.
The Runner owns the provider process tree and must reap it after terminal protocol
state. Keep process cleanup, provider fencing, and dispatch certainty separate.

### Focused validation

Regression tests should use a fake bounded ACP stdio process for deterministic protocol
coverage and one opt-in real Codex ACP smoke for compatibility. Cover at least:

- initialize/new/prompt/update/terminal normalization;
- invalid and valid config override sequencing;
- permission request never auto-allows;
- cancel -> correlated terminal cancellation;
- provider crash before vs after prompt dispatch;
- stale provider instance;
- idempotent start replay, intent conflict, and post-Server-restart replay without
  duplicate dispatch;
- minimal client-capability advertisement and fail-closed unsupported callbacks;
- event retention/history loss and token Run binding;
- Server restart with same Runner inventory and intent fingerprint;
- Runner restart at each dispatch boundary: before durable barrier, after barrier
  before prompt write, after prompt write before terminal, and terminal-before-
  projection, proving no duplicate prompt dispatch;
- missing/corrupt durable record after possible dispatch fails closed;
- Runner/provider replacement -> lost/fail closed;
- project/scope/Workflow-Session authority boundaries;
- environment redaction and bounded serialized output;
- cleared child environment plus explicit `env_from_env` injection;
- terminal child-process cleanup;
- durable audit/telemetry privacy for prompt, message, reasoning, and tool bodies.

## Explicitly deferred

The current implementation does not include:

- Claude as a second ACP provider;
- browser-hosted ACP sessions;
- raw ACP JSON-RPC tools;
- model-visible permission-response tooling;
- automatic permission allow;
- hot ACP provider reload/generic provider framework;
- ACP v2 or experimental extensions;
- scheduler, worker pool, automatic worker spawning, or orchestration;
- integration with durable Agent/Conversation/Participant state, presence, or typing;
- durable Operation DAG;
- unification of Workflow Session, Job, ACP Session, and CodingAgentRun;
- a claim that Project cwd is a filesystem sandbox;
- automatic `session/load` recovery of uncertain in-flight prompts;
- tool slimming or unrelated model-ergonomics work.
