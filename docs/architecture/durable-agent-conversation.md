# Durable Agent identity, Conversation, and wake foundation

A1 adds a concrete communication domain without changing the meaning of Workflow Sessions or project Memory. A2 extends that domain with durable wake intent, Endpoint lifecycle/generation fencing, dispatch uncertainty, and exact continuation consume semantics without making model execution part of Conversation or Inbox state. A2.5 connects those facts through a production-shaped, process-local Host binding/controller boundary, a bounded natural-conversation bootstrap, and explicit Agent selection in Runtime Console. The standing direction beyond this implemented substrate is defined in [`durable-agent-runtime.md`](durable-agent-runtime.md).

## Domain boundaries

| Concept | Durable meaning | Explicitly not |
| --- | --- | --- |
| Agent | Server-generated `wc_dagent_*` identity plus a mutable Agent Card | a browser window, MCP connection, credential, Project, Workflow Session, or authority grant |
| Agent Endpoint | A principal-bound Host/Client attachment with a Server-assigned controller generation, bounded lease, lifecycle, and wake capability metadata | the Agent identity itself or execution authority |
| Conversation | Durable communication space containing Human and Agent participants | a Workflow Session, Task scheduler, or execution context |
| Conversation Message | Append-only transcript fact with a stable per-Conversation sequence and author provenance | recipient read state or a current-state delta |
| Agent Delivery | Recipient-specific queued/consumed Inbox state pointing to one Message | a duplicate Message or model invocation |
| Wake Intent | Durable logical continuation saying an Agent should receive another processing opportunity | a Message, Inbox Delivery, or Host delivery attempt |
| Wake Delivery Attempt | One Endpoint/generation-bound attempt to deliver a Wake Intent through a continuation adapter | the durable communication fact or a grant of execution authority |
| Agent Task | Planned durable asynchronous work accepted/created for an Agent | Connector Task, Conversation Message, Session todo, or execution authority |
| Workflow Session | Existing execution, provenance, validation, Job, workspace, todo, and guidance context | a chat room |

An Agent Card contains a mutable non-unique handle, display name, description, bounded specialty labels, profile revision, and timestamps. These fields are self-description metadata. Canonical identity is only the Server-generated `agent_id`, and neither identity nor metadata grants Project, filesystem, Runner, Agent Task, or Workflow Session authority.

## Authoritative persistence

The Control-owned SQLite database remains the standalone authoritative transactional store. A1 adds independent tables for:

- Agent identities and owner communication principals;
- Agent Endpoints and their attachment principals;
- Conversations and participants;
- append-only ordered Messages;
- recipient-specific Agent Deliveries;
- operation-scoped idempotency records;
- coalesced Agent Wake Intents and Endpoint/generation-bound Wake Delivery Attempts.

This reuses the existing durable database lifecycle instead of adding another JSON truth or process-local registry. Generic `Database::open` is storage-only: it may run schema migration and owner-independent housekeeping, but it never declares a Wake worker dead or performs takeover recovery. The standalone Control Server holds an exclusive instance guard bound to its exact database state for its full lifetime; crash/takeover Wake reconciliation runs only after a successor acquires that ownership proof. This is deliberately standalone coordination, not a distributed lease or cluster protocol. The schema is concrete to the current communication/wake use case rather than a generic actor/event framework. It does not assume SQLite is process memory, and the domain can later be mapped to another transactional backend without changing its IDs or public semantics.

Agent Endpoint lifecycle, Conversation lifecycle, and Agent Delivery state are closed typed Store contracts. Their existing SQLite strings remain storage encodings and public serialization values; unknown persisted lifecycle values fail closed before authorization or delivery authority is evaluated. This does not change the independently typed Wake or Agent Task/TaskAttempt state machines.

A Message append, sequence allocation, all requested Agent Deliveries, required Wake Intent coalescing/creation, Conversation update, and idempotency record commit in one immediate transaction. A forced Wake insertion failure therefore leaves no Message, no partial Inbox state, no Wake, no consumed sequence, and no stale idempotency outcome.

## Identity and authorization

`communication:read` and `communication:manage` are independent scopes. Communication principals are stable hashes of an authenticated account/shared-key subject; secrets and hashes are never returned. Runner transport credentials, Project credentials, and project-scoped OAuth subjects cannot become communication principals.

Creating or updating an Agent, attaching an Endpoint, and adding an Agent to a new Conversation require ownership by the same communication principal. An Agent-authored Message, Agent-scoped Conversation read, Inbox read/consume, and Wake operation additionally require the exact current Endpoint id and controller generation bound to that principal and Agent. Human-authored Messages require the current principal to be the Conversation's Human participant.

Conversation participation authorizes only communication. Existing Project, Workflow Session, Runner, filesystem, Session fence, and Memory authority checks are not consulted, inherited, or relaxed.

Exact communication-resource authorization is existence-hiding. Agent ownership, Endpoint attachment authority, Conversation participation, reply targets, and Delivery recipients are scoped in the authoritative lookup before resource state is inspected. A well-formed resource id outside the caller's communication principal or authorized parent therefore has the same public not-found contract as a nonexistent id; foreign owner, host, lifecycle, Message presence, and Inbox recipient metadata are never used as an error distinction. Once access is proven, own-resource diagnostics such as profile revision conflicts, detached/stale Endpoints, closed Conversations, and desired-state consume retries remain explicit.

## Ordering, delivery, and replay

Each Conversation owns a monotonic `next_seq`; Message rows are unique by `(conversation_id, seq)`. Transcript reads use an exclusive `after_seq` cursor and never consume Inbox state.

Each Delivery is unique by `(message_id, recipient_agent_id)` and has its own monotonic `delivery_order`, queued/consumed state, and timestamps. Inbox reads use an exclusive `after_delivery_order` cursor. An offline Agent needs no Endpoint for Delivery creation; a later principal-bound Endpoint can read and consume the queued Delivery.

Message/Delivery cardinality is deliberately independent from model-turn cardinality. Multiple queued Deliveries for one Agent coalesce into a bounded pending Wake Intent while every Message and Delivery remains independently durable. A later Message can update the pending Wake high-water mark/count without replacing its logical identity.

Wake delivery uses a separate Attempt state machine. Claim is Endpoint/generation fenced and leased. Failure before the durable dispatch fence revokes the Attempt and returns the same Wake Intent to `pending`; after the fence, an unconfirmed Host outcome becomes `delivery_unknown` and is never unconditionally redispatched. Endpoint detach, replacement, expiry materialization, and authoritative standalone Server takeover reconcile stale claims or ambiguous dispatched work conservatively. Exact Wake consume is bound to `wake_id`, target Agent, current Endpoint, controller generation, and an opaque per-Attempt consume token. Wake consume never consumes Agent Deliveries, and Delivery consume never consumes a Wake.

Create-Agent, attach-Endpoint, create-Conversation, and ordinary post-Message mutations require caller-generated idempotency keys. The store hashes both key and canonical request. Idempotency lookup is communication-principal-scoped. Exact replay returns the original durable resource with `replayed=true` and `state_changed=false`; changed reuse is rejected. Exact Message replay is checked before current Endpoint/lifecycle validation so a caller can recover a committed response after the original Endpoint detached without duplicating the Message or Deliveries. After a Wake has crossed either the Host dispatch fence or the explicit-activation fence, its resumed reply may instead use the exact `wake_id` plus a bounded per-Wake operation index. A still-`pending` Wake cannot mint this reply identity; an already-active manual turn must explicitly activate it first. That pair derives a stable internal replay identity without making the body or replaceable Endpoint an identity or exposing a raw idempotency key; different indexes permit multiple intentional replies from one Wake. The first commit still requires the exact current Endpoint generation, while an exact retry can recover across a replacement carrier. A changed retry at an occupied index conflicts and must re-read the authoritative Conversation. Reply targets are resolved only inside the already-authorized Conversation, and Delivery consume resolves only inside the authorized Agent Inbox. Detach and consume are desired-state idempotent.

Request and response payloads, list pages, participant counts, profile fields, Message bodies, active Endpoints, Conversations, and Messages per Conversation have explicit bounds. Audit and Workflow Session context projections retain IDs, counts, cursors, and mutation state but omit Agent description contents, specialty-label contents, Message bodies, and idempotency keys.

## Runtime Console and tool surfaces

The model-visible Control surface provides explicit tools to create/list/update Agents, attach/detach Endpoints, bootstrap a current Agent activation, create/list/read Conversations, post Messages, list an Agent Inbox, consume Deliveries, and consume one exact Agent Wake after its durable dispatch fence. Server-owned output schemas remain complete even when a Compact model-facing `tools/list` projection omits `outputSchema`. Endpoint heartbeat/renewal remains Host infrastructure rather than model bookkeeping.

`bootstrap_agent_conversation` requires `communication:manage` plus the exact acting Agent, Endpoint, and controller generation. An optional explicit Conversation and Wake select durable records; no HTTP/MCP connection, `Mcp-Session-Id`, credential, Project, Workflow Session, or previous request supplies hidden continuity. By default the bounded result contains the Agent Card, Endpoint, selected Conversation summary, queued Inbox count/high-watermark, safe Wake identity/state/count metadata, reply-operation bounds, and current process adapter capability. It contains no transcript, Inbox Message body, claim fence, principal digest, Host secret, or raw replay key.

When the caller is already in an explicitly started model turn and supplies `activation_idempotency_key`, the same bootstrap can accept one exact `pending` Wake through a durable `explicit_activation` Wake Delivery Attempt. This does not request another turn and does not make the Endpoint generally wake-capable. The response includes the exact consume token; exact retry with the same activation key recovers the same Attempt and token after response loss, while changed reuse fails closed. Audit and Session projections omit both the activation key and consume token. The caller must still read authoritative Inbox/Conversation state and exact-consume only the Wake and Deliveries it processed.

Runtime Console exposes the same domain through hidden same-origin POST routes. Its minimal Chat panel can:

- create and select an Agent;
- inspect and update its Agent Card and queued count;
- explicitly “Continue as” the selected Agent by attaching/detaching the current browser as a non-wake-capable Endpoint;
- create/open a Conversation;
- render participants and the ordered transcript with Human/Agent provenance;
- post Human-authored Messages or Agent-authored Messages through the selected exact Endpoint/generation;
- inspect and consume an attached Agent's queued Deliveries.

The page performs bounded polling on the existing eight-second Runtime Console cadence and renews its own non-wake-capable Endpoint lease through a hidden same-origin route. Polling, refreshing the page, attaching/renewing an Endpoint, and receiving a Delivery do not invoke or wake a model. Endpoint detach on page exit is best effort. Lease validity and generation fencing are checked synchronously at operation admission and are the immediate correctness boundary. Replacement or authoritative Server takeover lazily materializes durable `expired` Endpoint state, while claim recovery lazily releases expired pre-dispatch claims; A2 does not run a periodic Endpoint/Wake reaper and does not promise a database write exactly when wall clock crosses a lease deadline. The durable Agent, transcript, Inbox, and Wake state never depend on browser lifetime.

A2.5 adds an event-driven `AgentContinuationController` around the narrow Host-neutral `ContinuationAdapter`. Its process-local registry stores only a callable adapter handle, adapter kind, principal, and exact Agent/Endpoint/generation binding. Message commit, exact Wake consume, and adapter registration schedule a bounded deduplicated dispatch opportunity; the queue capacity equals the durable Agent capacity, so one deduplicated notification per possible Agent cannot overflow a smaller process-local queue. No permanent polling loop is introduced. Dispatch still uses the durable claim and dispatch-fence state machine. Repeated events therefore cannot turn a 50-Message burst into 50 simultaneous model turns: every Message and Delivery remains durable while unresolved Wake state stays bounded.

Host registration is the only path that projects an attached Endpoint as `wake_capable=true`; model and Runtime Console attach input cannot self-declare that capability. Registration is accepted only for an exact Endpoint freshly attached through the current Server process, so a successor cannot reuse a pre-restart callback binding. Exact Host unregistration withdraws the process-local handle, projects capability false, and reconciles that Endpoint's pre-fence claim back to the same pending Wake or any post-fence prepared/delivered Attempt to `delivery_unknown`, without deleting the durable Endpoint or Inbox. Detach, replacement, expiry, or a stale dispatch evicts/fences the exact process-local binding. The controller revalidates the prepared Attempt and current Endpoint generation immediately before Host invocation, so a replacement already committed before a late callback is rejected; uncertainty after a crossed dispatch fence remains conservative. A Server restart starts with an empty registry and takeover recovery clears persisted capability projections because callback handles do not survive. Reconnect creates/replaces an Endpoint, registers a new exact adapter, and dispatches the same pending logical Wake rather than consuming or substituting it.

The checked-in deterministic adapter is test-only. September 2026 MCP App experiments demonstrated a Host continuation primitive in practice: an App can use `ui/message` to request a later ChatGPT model turn, and a foreground View completed a bounded four-round autonomous continuation loop. That probe did not implement the exact durable Agent Endpoint contract, and Host acceptance did not guarantee immediate execution while the tab was backgrounded; the evidence and limits are recorded in [`../agent/mcp-app-continuation-experiments.md`](../agent/mcp-app-continuation-experiments.md). Runtime Console therefore still reports its Endpoint as polling-only and non-wake-capable. Without a registered production adapter, a Wake remains durable `pending`; the next explicit Host/model activation can use idempotent explicit activation in bootstrap, read it, continue, and exact-consume it. Experimental App continuation, explicit activation, or a test adapter is not evidence that the production durable Agent adapter is installed.

## Next boundaries

The next standing work-domain boundary is an independent Agent Task plus fenced Agent TaskAttempt, described in [`durable-agent-runtime.md`](durable-agent-runtime.md). A Conversation Message may later become the stable origin of explicitly accepted work, but Message/Delivery/Wake state is not silently reinterpreted as an Agent Task, claim, lease, or execution result. Existing Connector Tasks and Workflow Session todos remain separate domains.

A separate later slice may add a demonstrated production Host continuation adapter and dogfood plus an explicit operator/Host reconciliation policy for long-lived `delivery_unknown` ambiguity. Until then, `delivery_unknown` remains observable and is not blindly returned to `pending`. ChatGPT MCP Apps remain one possible Endpoint/continuation adapter rather than a core dependency.

Current project-scoped Memory behavior is unchanged. Agent identity is stable enough for a future Memory principal or namespace keyed by `agent_id`; A1/A2 do not migrate Memory, add Agent Skills, schedule work, spawn autonomous workers, implement Agent Task/DAG orchestration, federation, A2A compatibility, PostgreSQL, or distributed multi-Server leases.
