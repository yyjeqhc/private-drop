# Manual Multi-Window Collaboration

This guide defines bounded collaboration between independent WebCodex Workflow Sessions. It reuses the Session handoff and message board; it is not a scheduler, worker pool, task queue, claim service, shared transcript, or filesystem lock. This manual engineering workflow remains valid alongside the separate durable Agent/Conversation domain.

## Core model

Assume coordinator Session `C` and worker Session `W`.

`C` owns the collaboration todo and bounded answers. `W` owns the worker's tool calls, validation, review evidence, and workspace activity. They are always independent Sessions: the worker does not resume `C`, and WebCodex does not copy `W` execution history into `C`.

Knowing a `session_id`, `message_id`, worker Session id, Job id, checkpoint id, artifact ref, commit SHA, or PR number is not authority. Every read or mutation still passes the normal caller/project/owner authorization checks. A `recording_session_id` is authorized before it can affect ledger recording, provenance, or project-mismatch logic; it does not become business execution context. Project-scoped Session targets require both current authorization to the stored project and an immutable creation-time canonical authority-group fingerprint; project-less Sessions use the same internal durable fence. Direct shared-key access and its OAuth shared-key bridge normalize to the same authority group. Workflow Session selection is always explicit: neither window identity, credentials, project identity, nor recorder provenance selects another business Session implicitly. Collaboration never grants filesystem, shell, Computer, artifact, credential, or other project authority.

## Canonical coordinator -> worker flow

1. **Coordinator posts one bounded todo to `C`.** Use `post_session_message(kind="todo")`. Include the objective, scope, prohibitions, exact stable identifiers, and expected answer shape.
2. **Worker starts its own Session `W`.** Do not resume `C` just to accept the assignment.
3. **Worker atomically reads the executable assignment.** Call `get_session_assignment(session_id=C, message_id=<todo_id>)` once before work. That exact store snapshot contains the open todo, all retained direct replies within the bound, and the opaque `assignment_fence`. `session_handoff_summary` may provide broader background, while `list_session_messages` remains generic browsing; neither substitutes for this executable assignment read.
4. **Worker performs the task under `W`.** Reads, edits, shell/process calls, validation, review evidence, Jobs, checkpoints, and other authoritative activity stay attached to `W`.
5. **Worker completes the exact todo atomically.** Use `complete_session_message(session_id=C, message_id=<todo_id>, answer=<bounded answer>, completion_key=<caller key>, expected_assignment_fence=<exact assignment_fence>)`. On stateless MCP 2026, wrapper metadata `recording_session_id=W` remains separate provenance and is stripped before concrete parsing. One Session-store mutation creates exactly one `kind=answer` reply, resolves the todo, and records the todo -> answer correlation.
6. **Stale or incomplete assignment state is not a blind retry.** `assignment_stale` returns `state_changed=false` plus the current assignment and a durable `fresh_assignment_fence` only when the exact current state is provable. Re-evaluate that returned assignment before using its fresh fence. `assignment_history_lost` and `assignment_too_large` are non-completable from the stale context.
7. **Coordinator reads and validates the result from `C`.** Use exact todo/reply browsing, `session_discussion_summary`, or `session_handoff_summary`, then explicitly re-observe any authoritative Project/Git/Job/artifact state referenced by the answer before consequential follow-up.

```text
coordinator C
  -> post todo(C)

worker W (independent Session)
  -> optional broader handoff(C)
  -> get_assignment(C, todo) -> todo + direct replies + assignment_fence
  -> inspect / edit / validate / review under W
  -> complete(C, todo, answer, completion_key, exact assignment_fence)

coordinator C
  -> exact todo(C) + replies(todo)
  -> discussion/handoff(C)
  -> revalidate authoritative source/state
```

## Atomic completion and retries

`complete_session_message` is the executable-todo completion path. Every current request requires the exact `expected_assignment_fence` returned by `get_session_assignment`; separately posting an answer and resolving a todo is not an equivalent completion contract.

A successful completion records:

- one answer whose `reply_to` is the exact todo id;
- the todo as resolved;
- `resolved_by_message_id` pointing to that exact answer;
- a bounded completion identity derived from `completion_key`;
- a persisted fingerprint of the exact accepted assignment fence for replay correlation;
- `author_session_id` only from an already-authorized explicit recording Session when the tool call is recorded under one.

Without an explicit trusted recorder, `author_session_id` is `null`; callers cannot supply a trusted author identity themselves. Stateless MCP 2026 never derives a Workflow Session identity from transport/window continuity, so callers that want worker provenance pass explicit `recording_session_id` wrapper metadata. A ChatGPT `openai/session` window may bind Project Connector task continuity, but WebCodex does not infer Workflow Session provenance from `mcp-session-id`, HTTP connection state, credentials, project identity, a client window, or a recent Workflow Session.

When a collaboration call has both a recording Session and a target Session, WebCodex authorizes both independently and then requires their stored project scopes to match exactly. `project/project` is allowed only for the same project, `project/project` with different projects is denied, both `project/project-less` directions are denied, and `project-less/project-less` is allowed only after both owner authorities have independently matched. The generic cross-project escape flag never widens this collaboration relationship.

For uncertain responses, retry the same `session_id + message_id + completion_key + expected_assignment_fence` with the same answer metadata. WebCodex returns the original canonical completion without creating another answer. Reusing the same key with different answer content, tags, priority, trusted author, or fence fails closed. Historical completed rows that predate assignment-fence metadata remain queryable after restore but cannot be replayed successfully by a current request; WebCodex never invents a historical fence.

A successful completion is fenced through the Session-ledger writer generation that contains that completion before success is returned. If durable persistence cannot be confirmed, the tool returns `completion_persistence_uncertain` with `failure_kind=outcome_unknown`, `retry_same_completion=true`, and `recovery_kind=retry_same`; retry the exact same `session_id + message_id + completion_key + expected_assignment_fence + payload` to reconcile the already-possible in-memory mutation instead of posting a second answer. `retry_same` is an exact idempotent replay contract, not general retryability. Correlation and idempotency metadata survive restart/persistence restoration. Malformed partial completion structures fail closed and never silently reopen a resolved todo.

## Exact assignment and reply lookup

`list_session_messages` supports narrow exact filters for browsing and coordinator result lookup. It is not the executable assignment source; workers use `get_session_assignment` for the atomic todo + direct-replies + fence snapshot:

```text
message_id=<wc_msg_*>
reply_to=<wc_msg_*>
kind=<optional>
status=<optional>
```

All supplied filters use deterministic AND semantics. `message_id` therefore gives an exact 0/1 lookup even when the Session contains many messages; `reply_to` finds bounded replies to one exact todo.

## Observe message-state delta

`observe_session_messages(session_id=C)` is an optional generic message-state delta primitive, not the happy-path executable assignment read. A no-token call establishes a current baseline without replaying history; a later `after_observation_token=<token>` call returns retained messages whose current state changed after that baseline, with at most one bounded optional wait. Existing backlog returns immediately, a relevant update returns `wait_outcome=updated`, and a deadline returns successful `wait_outcome=timeout` with `changed=false`.

The token is bounded, opaque, bound to the exact Workflow Session, and backed by a durable Session-local monotonic message-observation revision. It is observation state only: it is not authority, an idempotency key, execution identity, an implicit Workflow Session selector, or message-delivery receipt. The same recorder/target authorization fence used by the other collaboration tools applies before any observation result is returned. Token issuance fences the ledger generation containing its revision so a valid token remains usable after Server restart when that Workflow Session can be restored.

Assignment and continuity identities are intentionally separate domains: `assignment_fence` is a semantic todo snapshot, `completion_key` is caller replay identity, `ack_session_context_revision` is model-result continuity evidence, `ack_session_message_ids` is request-scoped retained-guidance proof, an observation token is a generic message-state cursor, business `session_id` names the authorized target, and `recording_session_id` is provenance only. None substitutes for another or grants authority by possession.

Observation tracks real message-state mutation, not deque length. Posts advance it; a resolve advances it only when status/resolution really changes; a new atomic completion advances for the todo resolution and answer creation; exact completion replay and no-op resolve do not advance it. If one retained message changes multiple times between observations, the observer may receive only its latest current state because this primitive is not an event/audit log.

Retention is explicit. Each retained message carries internal latest-revision bookkeeping and the Session maintains a durable low-watermark for evicted observation history. When the caller cursor predates an unrecoverable retention hole, `history_lost=true`; retained current-state delta may still be returned, but it is not represented as complete history. Pagination advances the returned token only through the last change actually returned while `has_more=true`.

Message observation is **not** a delivery receipt, **not** proof of model-context retention, **not** a subscription/stream, and **not** an Agent Wake. It never automatically wakes a model or spawns/routes work. Durable Agent/Conversation/Delivery/Wake state exists in a separate Server-owned domain; this Workflow Session primitive neither creates nor observes that state and does not create presence, typing, scheduler, worker-pool, or routing state.

## Runtime Collaboration Console

The Server-hosted `/runtime` page presents the same authoritative runtime and Workflow Session state without creating a second Session store or collaboration truth. It keeps a bounded Server overview, a focused per-Runner machine view, one compact/searchable Project selector, compact Workflow Session activity, and retained collaboration messages. The narrow Human Join composer is the only collaboration mutation affordance: it posts bounded Session messages through the canonical kernel path described below. Existing Project and Workflow Session console reads retain their `project:read` boundary; Server-wide/Runner-wide facts and full collaboration message/observation/post routes require `runtime:read` and still re-authorize the exact target Session/project.

The collaboration panel establishes an observation baseline before reading the retained snapshot, then uses bounded long-polls and merges deltas by `message_id`. `has_more` is drained before the next wait, while `history_lost` causes a retained-board reload and a new baseline rather than claiming complete history. Manual Refresh reports visible refreshing/success/failure state and preserves prior usable data; a healthy live collaboration loop is not restarted merely because Refresh was clicked, while a paused/failed loop performs a retained reload, new baseline, and bounded reconnect. Session liveness is derived only from WebCodex facts such as a running call, owned running Job, or recent retained activity; it never claims to know whether the host/model is processing, frozen, or present. All aggregate counts remain bounded/truncation-aware. Browser observation remains UI refresh only: it is not a model wake-up, subscription, participant-presence mechanism, scheduler, worker claim, or execution lease.

## Provenance is metadata, not authority

A completed answer can identify the independent worker with `author_session_id` only when the completion carries an already-authorized explicit `recording_session_id`; without that recorder, no author Session is inferred from caller auth, client window, or other ambient state. It is not a caller-authored claim. In stateless MCP 2026, `recording_session_id` is explicit wrapper provenance metadata, not a transport Session and not an authority grant; the legacy `mcp-session-id` header remains irrelevant.

The coordinator may then explicitly inspect `session_handoff_summary(worker_session_id)` if it has authority to that Session. WebCodex does not copy the worker's transcript, validation, diff review, Job logs, or workspace evidence into the coordinator Session merely because the answer references `W`.

Session message bodies are explicit bounded collaboration payloads. Ordinary tool audit stores metadata such as target Session/message ids, body byte counts, tag counts, correlation ids, completion identity, and safe author provenance; it does not persist a second copy of the full todo/answer body or raw completion key.

## Common collaboration patterns

### Coordinator -> implementation worker

`C` posts an implementation todo with exact scope. Worker `W` uses an isolated worktree/Project when writing, performs implementation and validation under `W`, then atomically completes the todo with the commit SHA and concise result. `C` reads the answer and independently re-observes the branch/commit before merge or further delegation.

### Implementation -> independent reviewer

The implementation Session posts a review todo containing the exact commit/range and review constraints. Reviewer Session `W` stays independent, performs its own source reads/tests/review evidence, then returns findings with `complete_session_message`. A no-findings answer is still bounded collaboration metadata, not an automatic acceptance or merge decision.

### Two independent worktrees in parallel

If two workers may write concurrently, give them separate Git worktrees and separate WebCodex Projects/Sessions. The message board coordinates intent and results only. It does not claim a branch, lease a path, serialize edits, or prevent conflicts.

### Cross-host conceptual example

A coordinator on host A may post a todo in `C`; an authorized worker on host B can read the bounded handoff and exact todo, work under its own Session/Project authority, and atomically answer `C`. The same authorization rules still apply. Session/message ids do not delegate owner or project authority, and cross-owner delegation is outside this workflow.

## Message board is not a lock

Do not treat todo state, `reply_to`, `completion_key`, `author_session_id`, or `resolved_by_message_id` as:

- a filesystem/worktree/branch mutex;
- an automatic task claim or lease;
- ownership transfer;
- proof that only one worker inspected the source;
- authority to mutate another Project.

When multiple workers operate on the same source, use normal Git/WebCodex Project isolation and revalidate current state before acting on collaboration messages.

## Human join and acknowledgement ergonomics

The hosted Runtime Console may post `note`, `guidance`, `question`, and `todo` messages into an exact authorized Workflow Session through the same `post_session_message` kernel path. This is a browser affordance, not a Participant entity, membership record, presence signal, or identity-spoofing surface. The browser route keeps the current collaboration metadata authority policy (`runtime:read`) and still applies the stored Session/project authority fence.

High-priority Guidance may opt into `requires_ack`. A Stateless MCP 2026 caller can echo the visible message id in `ack_session_message_ids` on an otherwise ordinary recorded tool call. The original tool executes normally whether the ACK is present, missing, unknown, foreign, or stale. A valid ACK suppresses that Guidance body only for the same request/response. If the model later omits the ACK while the Guidance remains open, the Server may piggyback the bounded body again. The first observed ACK timestamp is observability only; it must never be described as delivered, read, or currently remembered. Durable completion still requires normal message resolution.

## Bounded payload guidance

Keep todos and answers small enough to be useful as handoff state. Prefer stable references over copied authoritative objects:

- worker Session id;
- Job id;
- checkpoint id;
- artifact ref;
- commit SHA;
- PR number.

Do not put bearer tokens, OAuth secrets, private keys, credentials, sensitive connection details, full worker transcripts, or hidden reasoning in Session messages.

## Explicit non-goals

This workflow does not add automatic worker spawning, scheduler/worker pool behavior, generic task queues, automatic claims, work leases, filesystem locks, branch locks, shared transcripts, hidden chain-of-thought transfer, cross-owner delegation, webhook/model callbacks, automatic Job-terminal continuation, or implicit authority inheritance.

The human or coordinator still chooses workers and isolated worktrees/Projects. WebCodex supplies bounded durable collaboration state and deterministic completion correlation, not a multi-agent execution scheduler.

## Relationship to durable Agent/Conversation and asynchronous work

The Session message board remains intentionally scoped to a Workflow Session. It is
still the right primitive for explicit engineering handoffs such as coordinator ->
implementation worker and implementation -> independent reviewer. The existence of
the durable Agent/Conversation domain does not migrate, reinterpret, or obsolete
that workflow.

Long-lived Agent communication now belongs to the separate durable Agent domain:

```text
Durable Agent / Conversation
  Message -> recipient Delivery -> Wake opportunity
            |
            | explicit accept/create work (planned)
            v
Agent Task -> exact fenced TaskAttempt
            |
            +--> CodingAgentRun
            +--> Agent Endpoint continuation
            +--> later concrete execution backend
            |
            v
Workflow Session / Job / execution evidence
```

These arrows are correlations, not authority inheritance. Conversation membership,
Message authorship, Delivery state, Wake state, Session todo state, and object
references never grant Project or execution authority.

A Workflow Session todo and future Agent Task are deliberately different. The todo
is a bounded explicit assignment inside one Session collaboration ledger. An Agent
Task will represent asynchronous work that survives model/window turns and owns
independent TaskAttempt/lease/fencing state. Do not silently convert historical or
current Session todos into Agent Tasks, and do not use `assignment_fence` as an
Agent Task execution lease.

Likewise, the existing Connector Task used by `task_start` / `task_resume` remains a
separate project-bound continuity domain. The standing naming and boundary rules for
future Agent Tasks are in
[`../architecture/durable-agent-runtime.md`](../architecture/durable-agent-runtime.md).

If later product use justifies runnable-frontier scheduling, worker spawning,
capacity management, dependency graphs, or autonomous routing, those mechanisms may
consume durable Agent Task state as input. They remain optional control layers and
must revalidate claim/execution authority at the consequential boundary; they are
not hidden behavior of the Session message board or Conversation substrate.
