# Durable Agent runtime and asynchronous work

This document defines the standing product direction for durable Agent identity and
asynchronous work in WebCodex. It builds on the implemented Agent/Conversation/Wake
foundation without turning WebCodex into a generic swarm framework or workflow
scheduler.

Implementation details for the current communication and wake substrate live in
[`durable-agent-conversation.md`](durable-agent-conversation.md). Workflow Session
semantics remain defined by [`../agent/session-model.md`](../agent/session-model.md).

## Product direction

The product goal is a durable Agent entity that can outlive any one model turn,
browser window, host connection, Project, or execution carrier.

A user should eventually be able to:

1. create or select a durable Agent;
2. attach a current window/host to that Agent;
3. communicate through durable Conversations and Inbox Deliveries;
4. leave work for the Agent that remains durable while no model is running;
5. later attach another Endpoint and continue as the same Agent;
6. execute accepted work through whatever authorized carrier is appropriate;
7. inspect durable status instead of inferring progress from browser/UI state.

The model process is not the Agent. A model turn executes **on behalf of** a durable
Agent. Closing a window therefore means that an Endpoint disappeared; it does not
mean that the Agent, its Conversations, or its accepted work disappeared.

This direction may eventually make multi-Agent scheduling possible, but scheduling
is not the product definition. WebCodex should first make Agent identity and work
independent from windows and synchronous model turns. Runnable-frontier scheduling,
capacity management, graph execution, and autonomous delegation remain optional
capabilities that require separate product evidence.

## Names that must not collapse

WebCodex already contains several concepts that use similar words. They are separate
domains and must remain explicit in code, schemas, documentation, and reviews.

| Concept | Meaning | Not interchangeable with |
| --- | --- | --- |
| Runtime Project | Runner-registered execution target addressed as `agent:<client_id>:<project_id>` | durable Agent identity |
| Durable Agent | Server-minted `wc_dagent_*` identity representing who acts/communicates | Runner, browser window, credential, Workflow Session |
| Agent Endpoint | Current Host/Client attachment for one Agent, with lifecycle/generation | Agent identity or work ownership |
| Conversation | Durable communication space | Workflow Session, task queue, execution context |
| Agent Delivery | Recipient-specific Inbox state for one Message | model invocation or accepted work |
| Wake Intent | Durable logical processing opportunity for an Agent | Message, Delivery, Agent Task, execution lease |
| Connector Task | Existing project-bound `task_start` / `task_resume` continuity domain | Agent Task |
| Agent Task | Durable unit of explicitly created/accepted asynchronous Agent work | Connector Task, Session todo, Conversation Message |
| Agent TaskAttempt | Durable exact execution-ownership attempt for one Agent Task | Endpoint, Workflow Session, CodingAgentRun |
| Workflow Session | Existing execution/provenance/validation/handoff evidence context | Agent, Conversation, Agent Task |
| Job | Concrete long-running process/validation execution | Agent Task or TaskAttempt |
| CodingAgentRun | Existing ACP delegated coding execution | Agent Task itself |
| Goal | Server-owned high-level durable intent/control state (`wc_goal_*`) | Agent Task, Workflow Session, Job, Project authority, scheduler |

The `agent:` prefix in a runtime Project id is historical Runner-address syntax. It
is unrelated to the durable `wc_dagent_*` Agent identity domain.

In implementation names, prefer `ConnectorTask` for the existing Connector
continuity domain and `AgentTask` / `AgentTaskAttempt` for the asynchronous Agent
work domain. Do not introduce an unqualified new `Task` type where ownership would
be ambiguous.

## Implemented durable Agent foundation

The current implementation already establishes these boundaries, the A2.5 natural-conversation path, and the A3 durable AgentTask/TaskAttempt ownership substrate:

```text
Durable Agent
    |
    +-- Agent Card / profile
    +-- Endpoint attachments + controller generations
    |     +-- process-local exact Host adapter binding
    +-- Conversations
    |     +-- append-only Messages
    |     +-- recipient Deliveries / Inbox
    |
    +-- Wake Intents
    |     +-- Endpoint/generation-bound Wake Delivery Attempts
    |     +-- bounded event-driven continuation scheduling
    |
    +-- Agent Tasks
          +-- explicit durable assignee
          +-- fenced, leased TaskAttempts + Attempt-local controller generation
```

Important current invariants:

- `agent_id` is Server-minted stable identity; Agent Card metadata grants no
  execution authority.
- Endpoint identity is replaceable and principal-bound; lease validity and
  controller generation fence stale attachments.
- Conversation participation grants communication authority only.
- Message, Delivery, Wake Intent, and Wake Delivery Attempt are independent durable
  facts.
- Message/Delivery creation and required Wake coalescing are transactional.
- communication-resource authorization hides existence from unauthorized exact-id
  probes.
- generic database open is storage-only; standalone Server takeover recovery runs
  only after explicit Server instance ownership is acquired.
- dispatch uncertainty after the Wake dispatch fence is preserved rather than
  blindly retried.
- one authenticated communication principal can create/select/update multiple
  durable Agents, explicitly attach different Host windows, and communicate through
  exact-recipient Conversations without making the window the Agent.
- exact Agent/Endpoint/generation bootstrap exposes only bounded IDs, counts,
  high-watermarks, Wake hints, and truthful current adapter capability; authoritative
  Inbox and Conversation reads remain separate. An already-active explicit turn can
  idempotently accept one pending Wake through an `explicit_activation` Attempt and
  recover the same consume token without pretending it requested a new model turn.
- automatically resumed replies can derive stable replay identity from exact Wake
  plus a bounded operation index, closing the reply-committed/response-lost window
  without merging Wake and Delivery consumption.
- Agent Tasks are independent durable work truth with explicit assignment; Conversation
  Message/source references and Project references are correlation only.
- each execution-ownership retry creates a durable TaskAttempt with a distinct opaque
  Attempt fence; lease expiry never transfers assignment and cannot be revived by the
  same Agent.
- carrier replacement within one TaskAttempt increments an Attempt-local controller
  generation without creating another Attempt; older generations are stale.
- AgentTask creation, Attempt start, and terminal completion use durable keyed replay
  in the same authoritative SQLite transaction as their effects, and Task/Attempt,
  fence, generation, lease, terminal result, and accepted replay identity survive
  database reopen/Server restart.
- A3 does not bind or dispatch an execution backend: starting a TaskAttempt does not
  create a CodingAgentRun, Job, Workflow Session, Wake, or Host callback.
- process-local Host bindings are empty after restart; offline Messages, Deliveries,
  and the same logical Wake remain durable until a fresh/replacement exact Endpoint
  generation registers a new live Host carrier.
- project-scoped Memory is unchanged; Agent-scoped Memory is only a future boundary.
- durable Goals are independent high-level intent/control truth; Goal identity, ownership, lifecycle, revision, and correlations grant no Project, Runner, filesystem, Workflow Session, AgentTaskAttempt, CodingAgentRun, or Job authority.

G3 adds an optional production ChatGPT MCP App Host carrier on top of this substrate. It is deliberately a pull bridge rather than a fake Server callback: one explicit `present_agent_continuation(agent_id, endpoint_id, expected_controller_generation)` card binds `ui://webcodex/agent-continuation/v1`, while ModelHidden app-only operations establish one exact process-local View binding, renew the exact Endpoint, acquire through the existing Wake claim state machine, cross the existing durable dispatch fence, and record Host dispatch acceptance or uncertainty. The View itself performs `ui/message` only after prepare succeeds. The process-local binding id fences duplicate/reloaded iframes but grants no authority and is not durable execution truth.

Every bridge operation re-runs ordinary communication authorization and exact Agent/Endpoint/controller-generation validation. Bind additionally requires an Endpoint freshly attached in the current Server process; after restart an old Endpoint id cannot resurrect an old View. Replacing or withdrawing a View reuses existing wake-capability reconciliation: a pre-fence claim is revoked and the logical Wake returns to `pending`, while a prepared/delivered Attempt becomes `delivery_unknown`. The App never blindly resends after the dispatch fence. Host `ui/message` success means only `dispatch_accepted`; only later exact `consume_agent_wake` proves that a continuation model turn actually ran. A consume-before-ACK race is valid and late ACK is idempotent. Hidden/background Views heartbeat but do not initiate a new automatic Host dispatch, so Host scheduling remains best effort/non-immediate.

MCP 2026 Tasks, Connector continuation, Runtime Console, explicit activation, and push `ContinuationAdapter` behavior retain their existing contracts. The MCP App is an optional carrier, not a scheduler or a source of Agent, Task, Goal, Project, Workflow Session, or execution authority. See [`../agent/mcp-app-continuation-experiments.md`](../agent/mcp-app-continuation-experiments.md) for the Host evidence and production mapping.

These invariants, the natural-conversation slice, and the durable A3 ownership
substrate support asynchronous Agent work without introducing a scheduler.

## Durable Goal Phase 1

Phase 1 adds a deliberately small, Control-owned Goal domain. A Goal answers:

> What does the user ultimately want completed, and what is the authoritative high-level durable state of that intent?

It does **not** answer who owns the next execution attempt, which repository operation should run, or whether a concrete coding Session/Job succeeded. Those remain owned by AgentTask/TaskAttempt, Workflow Session, Project/Runner, and Job domains respectively.

The authoritative model is persisted in the existing Server SQLite database using independent `wc_goals`, `wc_goal_correlations`, and `wc_goal_idempotency` tables. The first model is intentionally bounded:

```text
Goal
  goal_id                 # Server-minted wc_goal_*
  owner principal         # stable authorized communication-management principal
  title                   # <= 200 characters
  objective               # <= 8192 UTF-8 bytes
  lifecycle               # active | completed | cancelled
  revision                # monotonic, starts at 1
  created_at / updated_at
  terminal_at?
  terminal_reason?        # <= 4096 UTF-8 bytes
  correlations[]          # <= 64 explicit AgentTask / Workflow Session identities
```

`active`, `completed`, and `cancelled` are the complete Phase 1 authoritative lifecycle. Terminal state is immutable. Presentation phases such as `implementing`, `blocked`, or `waiting_validation` are not stored as Goal lifecycle. Unknown persisted lifecycle/correlation values fail closed rather than becoming a new state implicitly.

Goal tools are Control-side only: `create_goal`, `get_goal`, `list_goals`, `update_goal`, `associate_goal_agent_task`, and `associate_goal_workflow_session`. They do not declare Project requirements or Runner capabilities. Reads use the existing stable communication-read principal model; mutations use communication management. Exact Goal reads combine id and owner in the durable lookup so a foreign id is indistinguishable from a nonexistent id. Create/update/association use durable keyed replay; exact duplicate retries replay, while changed reuse of the same key fails closed. `list_goals` returns bounded summaries rather than objective text or correlation identities.

Correlation is explicit durable identity only:

- Goal → AgentTask association first re-authorizes the exact AgentTask under its existing owner-principal rules.
- Goal → Workflow Session association first runs the existing exact Session authority fence, including creation-time authority fingerprint validation and normal authorization for any bound Project.
- the Goal store then persists only the target identity and timestamp; it never persists the target's fence, token, authority, ledger, Job state, or other private execution data;
- Jobs remain traceable through their existing Workflow Session/AgentTask provenance. Phase 1 intentionally does not duplicate a Goal → Job truth.

A Goal reference never becomes inherited authority. A Goal that references a Project indirectly through an AgentTask still has no Project authority; a Goal that references a Session is not a Session credential; a Goal linked to an AgentTask does not own that TaskAttempt. Any later dereference must run the target domain's normal checks again.

No current execution path accepts or requires `goal_id`: `work_on_project`, read/edit/search, shell/process, Job handoff/observation, validation, Git, and `finish_coding_task` retain their existing semantics. Goal is not selected from ClientWindow, OpenAI/MCP session data, Project identity, credential, Conversation, or Workflow Session. The historical Runner/Codex metadata field named `goal_id` remains compatibility metadata and is **not** the `wc_goal_*` durable identity.

Lifecycle is independent across domains. `finish_coding_task` reports/finishes one Workflow Session concern and does not complete a Goal. AgentTask/TaskAttempt terminal completion likewise does not transition a Goal without a future explicit contract. Phase 1 creates no Goal scheduler, Wake/controller, automatic AgentTask, TaskAttempt, Workflow Session, CodingAgentRun, Runner request, process, or Job.

### G2 — Goal Plan MCP App presentation

G2 is implemented as a projection over the exact durable Goal rather than a second state machine:

```text
authoritative Goal Store
        ↓
exact owner-authorized Goal read
        ↓
bounded Goal Plan projection
        ↓
present_goal_plan(goal_id)   # model-visible, App-bound, read-only
        ↓
ui://webcodex/goal-plan/v1
        ↓
goal_plan_state(goal_id)     # ModelHidden, App-only exact polling read
```

`present_goal_plan` is the only Goal tool bound to the Goal Plan App resource. Each explicit presentation call may create a new Host card; Goal mutations, AgentTask changes, Workflow Session changes, validation, Jobs, and `finish_coding_task` do not create or refresh cards. Ordinary coding/execution tools keep their native Host presentation.

The projection is intentionally sparse: exact `goal_id`, bounded title/objective, authoritative `active | completed | cancelled` lifecycle, monotonic revision, `updated_at`, optional terminal timestamp, and bounded AgentTask/Workflow Session correlation counts. It exposes no correlation identities, Session ledger, TaskAttempt fence, authority fingerprint, Wake/consume token, credential, Job log, stdout, or stderr. G2 does not synthesize `implementing`, `blocked`, `validating`, `reviewing`, or any other durable/presentation phase when the current durable facts do not prove one.

`goal_plan_state` is globally ModelHidden. A UI-capable Stateless MCP 2026 operator surface advertises it to the Host with MCP Apps `ui.visibility = ["app"]`; it is not part of the ordinary model tool universe, Adaptive gateway targets, REST runtime surface, or legacy MCP surface. The protocol-surface gate is not Goal authority: every polling call still derives the existing stable communication principal and independently performs the exact owner-scoped Goal read. App/iframe possession, ClientWindow, Project, Workflow Session, Conversation, credential transport state, and correlation do not select or authorize a Goal.

The App receives exact `goal_id` and revision in the initial presentation result, polls the authoritative Store by that id, and avoids full DOM updates while revision is unchanged. Active cards converge again after foreground/visibility changes; terminal Goals stop periodic polling and retain a stable terminal presentation. Teardown/page unload stops timers. Refresh/reopen requires no localStorage or Server process-local Goal map: the rebuilt View can recover current state from the exact durable identity and SQLite truth. Multiple Views observing the same Goal are safe because both presentation tools are pure reads.

There is no stable Goal page in the Web UI yet, so G2 deliberately omits an `Open in WebCodex` link rather than emitting a dead or semantically incorrect URL.

**G3 — production Host continuation adapter.** G3 is now implemented for explicit Durable Agent Endpoints, independently of Goal. The public `present_agent_continuation` entry requires exact `agent_id`, `endpoint_id`, and `expected_controller_generation`; it does not infer a target from Goal, Workflow Session, Project, ClientWindow, credential, recent Agent, recent Task, or any other ambient state. The associated App bridge is available only on an App-enabled Stateless MCP 2026 operator-capable surface, where its hidden tools are projected with `ui.visibility = ["app"]`; they remain absent from the ordinary model universe, Local Coding, legacy MCP, REST/generic runtime and Adaptive gateway targets, with a kernel protocol-capability gate as the final backstop.

The durable lifecycle remains exactly the pre-existing Agent Endpoint / Wake / Wake Delivery Attempt lifecycle. The App View is only a live Host controller/carrier: `bind` establishes one current process-local View fence, `state` heartbeats and renews the exact Endpoint, `wake_acquire` delegates to the existing claim path, `wake_prepare` crosses the existing durable dispatch fence and revalidates exact binding, and `wake_finish` records only accepted/unknown Host dispatch outcome. Claim fence and binding secret are not durable/model-visible truth; the automatic message carries the exact consume token only through App-private MCP result metadata, not ordinary structured model content or audit/trace payloads. `dispatch_prepared`, `dispatch_accepted`, and `dispatch_unknown` are bounded Host observations, not new authoritative Wake states. Only exact durable `consume_agent_wake` establishes `continuation_consumed`.

G3 does **not** connect the G1/G2 Goal domain to this carrier. Goal remains `active | completed | cancelled` high-level Control truth and `present_goal_plan` remains read-only presentation. No Goal revision/completion, AgentTask, Workflow Session, Job, Project, ClientWindow, or credential automatically creates or retargets a continuation. These boundaries follow the [September 11–12, 2026 MCP App continuation findings](../agent/mcp-app-continuation-experiments.md): one sparse card converges from authoritative state, Host acceptance is not model resumption, and background Host scheduling is not an immediate guarantee.

## Asynchronous Agent work

An **Agent Task** represents durable work that has been explicitly accepted or
created for later completion. It answers:

> What work remains to be completed?

It must survive model-turn completion, Endpoint detach, browser closure, Server
restart, and temporary absence of an execution carrier.

An Agent Task is not created merely because a Conversation Message exists. A Message
is communication; converting or accepting a request into work must be an explicit
durable transition. An Agent Task may retain stable origin references such as `conversation_id` and
`source_message_id`, but the Message body remains owned by the Conversation domain.

A3 does not define a global work-stealing queue. Before an Agent Task can create an
Attempt, it has an explicit current assignee Agent established by Task creation,
acceptance, or a separate authorized reassignment transition. An unassigned Task may
exist if a real UI/workflow needs it, but it is not claimable by arbitrary Agents.
Changing the assignee is an explicit durable mutation; lease expiry alone never
silently transfers work to a different Agent.

Likewise, an Agent Task that references a Project does not receive Project authority.
The Project reference identifies where work may need to occur; every actual execution
still passes the normal Project/Runner/filesystem/permission checks.

A first Agent Task model should stay deliberately small. Candidate durable fields
are:

```text
AgentTask
  task_id
  creator / owner principal attribution
  assignee_agent_id?  # must be explicit before creating an Attempt
  bounded title / instruction
  source_conversation_id?
  source_message_id?
  referenced_project_id?
  state
  created_at
  updated_at
  terminal result / reason references?
```

The exact state enum should be chosen by the implementing slice, not expanded in
advance for hypothetical graph execution. The first slice needs only enough states
to distinguish available/active work from terminal success/failure and any explicit
cancellation that is actually required.

## TaskAttempt is the execution ownership unit

An Agent Task does not record a mutable `claimed_by` field that is repeatedly reused
across retries. Each concrete attempt is a separate durable **Agent TaskAttempt**.

```text
AgentTask
   |
   +-- TaskAttempt 1 -> expired / failed
   +-- TaskAttempt 2 -> failed
   +-- TaskAttempt 3 -> completed
```

A TaskAttempt answers:

> Who owns this exact execution attempt, and which generation of that attempt is
> still allowed to make progress?

Candidate durable fields are:

```text
AgentTaskAttempt
  attempt_id
  task_id
  attempt_number
  assignee_agent_id
  state
  lease_expires_at
  attempt_fence
  attempt_controller_generation
  workflow_session_id?
  execution_kind?
  execution_ref?
  created_at
  started_at?
  terminal_at?
```

The Attempt belongs to the durable Agent named by the Agent Task's current explicit
assignment, not to a browser window. An Endpoint or other execution carrier is only
a current way of executing that Attempt.

### Two different stale-execution fences

Attempt retry and carrier replacement are different events and need different
fences.

**Attempt fence** separates one TaskAttempt from a later TaskAttempt:

```text
Attempt 1 (fence A) -> lease expires
Attempt 2 (fence B) -> becomes current

late heartbeat(A) -> stale
late complete(A)  -> stale
late fail(A)      -> stale
```

The same Agent reclaiming work after Attempt 1 expires creates Attempt 2. Matching
`agent_id` must never revive an expired Attempt.

**Attempt controller generation** separates execution carriers inside the same Attempt:

```text
Attempt 2
  Agent A
  attempt_fence = B
  attempt_controller_generation = 4
       |
       +-- old carrier binding / attempt generation 3 -> stale
       +-- replacement carrier binding / attempt generation 4 -> current
```

Carrier replacement therefore does not necessarily create a new Attempt, but the
old carrier must lose the ability to heartbeat, dispatch, or submit a terminal
result. `attempt_controller_generation` is Attempt-local freshness metadata, not a
credential or a grant of Project authority. It is distinct from the existing Agent
Endpoint controller generation. When the carrier is an Agent Endpoint, the binding
must also preserve and validate the exact `endpoint_id` plus that Endpoint's own
controller generation.

If an execution backend already has a stronger exact identity (for example a
CodingAgentRun `run_id` plus provider-instance fencing), bind and revalidate that
identity rather than weakening it into a generic controller token.

## Attempt authority and lease semantics

Possession of an Attempt id, fence, lease timestamp, Agent id, or Attempt controller
generation is never sufficient authority by itself.

The intended admission rule is conceptually:

```text
normal caller authorization
+ authorized Agent Task visibility / operation
+ exact active task_id + attempt_id
+ exact assignee_agent_id
+ unexpired Attempt lease
+ exact opaque attempt_fence
+ current attempt_controller_generation when a replaceable carrier is bound
+ normal Project / executor authority for consequential execution
```

`attempt_fence` is conflict-detection/freshness metadata, not a bearer credential.
A lease that is already expired cannot be renewed by the stale owner and cannot
submit completion. A new authorized start/claim by the current assignee after expiry
creates a new Attempt.

Planner or scheduler output, if one is added later, is advisory. In A3, assignment
is explicit rather than selected by a scheduler. The authoritative Attempt
start/claim and dispatch mutations must re-check current Agent Task state, current
assignee, Attempt state, lease, Project/executor authority, and any dependency rules
that eventually exist.

## Idempotency, replay, and uncertainty

Agent Task mutations should follow the existing WebCodex durable patterns:

- create-like operations use caller-generated idempotency keys plus canonical
  request fingerprints;
- exact uncertain retry returns the original Agent Task/TaskAttempt/result;
- reusing one key for changed intent fails closed;
- duplicate terminal completion is exact replay, not a second side effect;
- expired Attempt heartbeat/completion is rejected even when the Agent is the same;
- once a later Attempt exists, an older Attempt remains permanently stale;
- restart restores durable Task/Attempt truth rather than inferring ownership from
  process-local state.

Execution dispatch has a separate uncertainty boundary. If a backend dispatch may
have happened, the TaskAttempt must preserve that ambiguity and reconcile the exact
execution rather than minting a replacement execution blindly. Existing
CodingAgentRun `outcome_unknown` / provider-instance semantics are the preferred
first backend precedent.

## Communication, Wake, Task, and execution are separate

The expected relationship is:

```text
Human / Agent
    |
    v
Conversation Message
    |
    | explicit accept/create work
    v
Agent Task
    |
    | exact claim
    v
Agent TaskAttempt
    |
    +--> CodingAgentRun
    |       or
    +--> Agent Endpoint continuation
    |       or future concrete backend
    v
Workflow Session / Job / backend execution evidence
    |
    v
exact Attempt terminal transition
    |
    v
Agent Task result
    |
    v
Conversation reply / durable notification
```

The arrows are correlations, not authority inheritance.

A Wake Intent says that an Agent should receive another processing opportunity. It
is not an Agent Task claim and does not mean work completed. Consuming a Wake does
not consume Agent Task work. Consuming Inbox Deliveries does not complete an Agent
Task. An Agent Task may remain durable while the Agent has no wake-capable Endpoint.

A Workflow Session remains the execution/provenance/validation context for concrete
work. It does not become the Agent Task identity. A Session todo remains a useful
manual coordinator/worker assignment primitive, but it is not silently upgraded
into an Agent Task.

## Authority and privacy boundaries

Agent entity work crosses communication and execution domains, so the boundary must
remain explicit from the first Agent Task slice.

Standing rules:

- Agent identity or Agent Card metadata grants no execution authority.
- Conversation membership, authorship, mention, Delivery, or Wake possession grants
  no Agent Task execution authority.
- Agent Task assignment grants no Project, Runner, filesystem, Job, Workflow
  Session, Computer, or CodingAgent authority.
- An Agent Task's Project reference grants no access to that Project.
- Claim/dispatch/completion re-authorize their owning Agent Task domain and any
  actual Project/executor target at the consequential boundary.
- Agent Task visibility, Agent Task management, assignment/claim, and underlying execution
  authority are conceptually distinct even if the first implementation can safely
  reuse a smaller closed scope set.
- exact Task/Attempt lookups should follow the existing authorization-result privacy
  discipline: an unauthorized caller must not use guessed opaque ids as an
  existence oracle.
- generic audit/telemetry should keep ids, state, counts, generations, and bounded
  metadata, not duplicate full Agent Task instructions, Conversation bodies, secrets, or
  opaque fences/tokens.

Do not name new scopes merely to make the model look symmetric. Introduce a scope
only when the actual first Agent Task surface creates a distinct authority audience.

For the first A3 slice, Agent assignment must not manufacture a new bearer
principal. Task creation/assignment/start/heartbeat/completion run under the current
authenticated caller and must prove that caller may operate the exact Task and its
current assignee Agent in the Task domain. A simple first implementation may reuse
the Agent's existing owner communication principal when that is the actual product
audience, but doing so does not confer Project or executor authority and does not
make `agent_id`, `task_id`, `attempt_id`, or an opaque fence a credential.

Execution-carrier admission is then additive rather than implicit. A
CodingAgentRun-backed Attempt does not require any browser/Host Endpoint; it
re-authorizes the exact Project and CodingAgent backend under their normal rules. An
Endpoint-backed Attempt additionally requires the exact currently authorized
Endpoint plus its generation/binding fences. This keeps Task ownership independent
from window presence while preventing either Agent ownership or Endpoint possession
from silently broadening execution authority.

## Agent continuity across windows

The durable identity model is intentionally stronger than window continuity:

```text
Window / Host Endpoint E1 disappears
        |
        v
Durable Agent still exists
        |
        +-- Agent Card
        +-- Conversation / Inbox / Wake
        +-- future Agent Memory
        +-- Agent Tasks / TaskAttempts
        |
        v
Endpoint E2 attaches later
```

A replacement Endpoint may continue the same Agent, subject to current principal
ownership, Endpoint generation, TaskAttempt controller fencing, and the execution
backend's own rules. No model is assumed to remain resident between turns.

This also leaves a clean future boundary for Agent-scoped Memory and Skills. Current
Project Memory remains unchanged until a deliberate migration/namespace design is
implemented. Agent Skills are later additive capability/configuration, not part of
TaskAttempt authority.

## Execution backend sequence

Do not build a universal execution-provider framework in the Agent Task foundation.
Use concrete backends first.

### A3 — Agent Task + fenced TaskAttempt

The current A3 implementation establishes durable Agent Task and TaskAttempt semantics only:

- explicit work creation;
- explicit assignment/acceptance and atomic Attempt start/claim by that assignee;
- lease + exact Attempt fencing;
- carrier/controller-generation fencing where needed;
- exact heartbeat/completion/replay;
- restart recovery;
- authority/privacy boundaries;
- minimal observation/listing needed to dogfood the domain.

A3 does **not** automatically choose an assignee, spawn workers, operate a global
claimable queue, or choose execution capacity.

A4a is intentionally not implemented in this foundation: `start_agent_task_attempt`
creates durable ownership/fencing truth only and does not start a CodingAgentRun or
any other execution backend.

### A4a — TaskAttempt -> existing CodingAgentRun

Use the existing ACP CodingAgentRun as the first real execution backend. It already
has durable run identity, caller/project/provider intent binding, provider-instance
fencing, uncertain-dispatch handling, and restart reconciliation.

This is deliberately the first backend because it proves that Agent Task execution
is independent from ChatGPT browser windows.

### A4b — TaskAttempt -> Agent Endpoint continuation

After a production Host continuation adapter exists, allow a TaskAttempt to execute
through a wake-capable Agent Endpoint. The TaskAttempt remains Agent-owned; the
Endpoint remains a replaceable carrier.

Only after both backends reveal repeated common machinery should WebCodex extract a
minimal shared execution binding/adapter abstraction.

## Scheduling is optional derived capability

Later dogfood may show that many durable Agent Tasks benefit from a runnable-frontier
scheduler. If so, scheduling should derive from durable work, not from browser tabs
or UI idle state.

Useful future invariants include:

- planning is advisory; explicit assignment plus claim/dispatch remains authoritative;
- runnable work and live execution reservations drive capacity decisions;
- active execution reservations form a floor only for the carrier class they
  actually consume;
- semantic durable state transitions create wake pressure; visual UI state does not;
- stale workers/carriers cannot renew expired leases or submit late results.

Capacity is therefore potentially per execution class rather than simply
"number of active Agent Tasks = number of ChatGPT windows".

This is a possible A5 product slice, not an A3 requirement and not WebCodex's north
star.

## Dependencies and workflow graphs come later

Do not add dependency DAGs, fan-out/reducers, graph node kinds, conditional routing,
supersteps/barriers, or parent/child Agent orchestration merely because another
system demonstrates them.

First prove:

```text
Agent Task -> exact TaskAttempt -> concrete execution -> exact terminal result
```

Add dependency semantics only after real workflows require `A -> B` or fan-out/join.
Add an explicit workflow graph only after dependency plus conditional-routing use
cases justify a third abstraction layer. Superstep/BSP-style coordination has no
current roadmap commitment.

## A3 acceptance matrix

The first Agent Task foundation should close at least these cases:

| Case | Required result |
| --- | --- |
| two concurrent start/claim requests for one explicitly assigned Agent Task | exactly one authoritative Attempt is created/accepted |
| exact start/claim retry | returns the same Attempt without redispatch |
| heartbeat before lease expiry | succeeds only for exact current Attempt/controller |
| heartbeat after lease expiry | rejected as stale |
| completion after lease expiry | rejected as stale |
| same assigned Agent starts again after expiry | creates a new Attempt |
| different Agent tries after expiry without reassignment | rejected; lease expiry does not transfer assignment |
| authorized explicit reassignment, then new assignee starts | creates a new Attempt for the new current assignee |
| Attempt 1 responds after Attempt 2 exists | Attempt 1 remains permanently stale |
| Endpoint/carrier replacement inside one Attempt | old Attempt controller/binding is fenced |
| Server restart | Agent Task/TaskAttempt truth and accepted replay identities survive |
| Endpoint detach | Task/Attempt durable state does not disappear |
| duplicate completion | exact replay, no repeated terminal side effect |
| changed replay | idempotency conflict |
| Conversation participant references Agent Task | receives no implicit Agent Task execution authority |
| Agent Task references Project | receives no implicit Project authority |
| unauthorized exact Agent Task/Attempt id | does not disclose foreign-resource existence |

## Explicit non-goals for the next slice

The Agent Task foundation must not expand into:

- a generic swarm scheduler or worker pool;
- automatic Agent spawning or autonomous delegation;
- runnable-frontier/capacity autoscaling;
- dependency DAG, fan-out/reducer, graph DSL, or superstep;
- a universal execution-provider framework;
- Agent parent/child hierarchy;
- production ChatGPT continuation unless that is the dedicated A4b slice;
- Agent-scoped Memory migration or Agent Skills;
- federation/A2A compatibility;
- PostgreSQL/distributed multi-Server scheduling;
- a generic Actor/Event/Entity ORM.

The design goal is narrower: make a durable Agent able to own and resume
asynchronous work correctly, with explicit identity, authority, replay, and stale
execution fencing. More automated coordination can grow from that foundation only
when real product use demonstrates the need.
