# MCP App presentation and autonomous-continuation findings

This note records durable findings from the September 11-12, 2026 MCP App presentation and continuation investigation. The experiments ran in a temporary `mcp-stream-probe` deployment rather than the production WebCodex runtime. They establish Host behavior and design constraints; they do **not** make the temporary probe state machine a production contract.

## Why this investigation existed

WebCodex already exposes MCP App result cards, but a durable background agent needs stronger answers than "the card rendered":

- whether successive App-bound tool results reuse one presentation or create multiple cards;
- whether one original card can reflect later server-owned state without binding every later tool call to an App;
- whether background completion can cause ChatGPT to start a later model turn without another user message;
- whether that continuation can repeat across multiple autonomous model turns with real tool side effects;
- what browser/View lifecycle and Host scheduling behavior mean for durability and exactly-once-style delivery.

The probe deliberately separated four things that are easy to conflate: canonical server state, model projection, App/iframe presentation, and Host model-turn scheduling.

## Confirmed presentation behavior

### One App-bound ToolResult creates one custom presentation

Repeated calls using the same tool, the same MCP App resource URI, and the same logical `runId` created distinct custom cards. Reusing a resource URI or logical run identity did not cause ChatGPT to reuse an existing iframe.

The Host's native tool card also remained present. A custom WebCodex MCP App card supplements native ChatGPT presentation; it does not replace or overwrite the Host's own tool card.

**Design consequence:** do not bind a custom App to every high-frequency execution tool if the product wants one persistent workflow card. The App binding itself is a presentation-creation boundary.

### One existing iframe can track later server-owned state

A separate live-plan control bound an App only to the initial start tool. Later model-visible state mutations deliberately carried no App binding. The original iframe polled an app-only exact-state tool and updated itself in place.

Observed behavior included:

- step changes without new custom cards;
- rapid revisions coalescing visually while still converging to the final authoritative state;
- page refresh/iframe rehydration restoring the exact run;
- closing the ChatGPT tab and reopening the same conversation recovering the final server state;
- parallel `runId` values remaining isolated;
- invalid transitions being rejected without corrupting the run.

**Invariant:** the card is an eventually consistent projection of server-owned state, not the owner of workflow state and not an event log. It need not render every revision; it must converge to the authoritative state.

### Model projection and App delivery are separate axes

Large-result controls also showed that model-visible projection, App delivery, and DOM rendering are separate concerns. A large canonical body can be available to the App without being inserted into the card DOM, while structured result placement changes what the model itself receives.

**Design consequence:** production cards should receive a deliberately bounded presentation projection even when the canonical tool result is larger. Do not make card richness depend on routing the full execution trace through the DOM or through model context.

## Confirmed one-shot background continuation

The async handoff probe used this sequence:

1. one model-visible App-bound start tool created the only custom card;
2. a server-side timer advanced background state without a model turn;
3. the App polled exact state through app-only tools;
4. terminal state created an exact event and short wake lease;
5. the App updated model context, crossed a prepare/dispatch fence, then called `ui/message`;
6. a later model turn consumed the exact `runId` / `eventId` / `resumeNonce` through a model-visible tool with no App binding;
7. the original card polled the resulting completed state.

This worked with the View open. It also worked when the ChatGPT tab was closed before background completion: server-side work reached terminal state while no iframe existed, reopening the original conversation rebuilt the View, the App re-read authoritative state, and an automatic later model turn consumed the pending event.

The temporary probe stored state in memory, so this test covered browser/View loss but intentionally did not claim Server-restart durability. A production implementation must use existing durable WebCodex state rather than copying the probe's in-memory map.

## Confirmed autonomous multi-turn continuation

The final probe extended the one-shot handoff into a bounded four-round loop. Each round generated fresh exact continuation data:

- `round`;
- `eventId`;
- `resumeNonce`;
- `continuationToken`;
- server-prescribed `text`.

The automatically resumed model turn had exactly one allowed action: call `presentation_multiturn_write` with those fields unchanged. The server validated all identities and appended exactly one line to `artifacts/async-multiturn/<runId>.txt`. A successful intermediate write armed the next background round; a duplicate write for the same exact round was idempotent and did not append a second line.

A foreground run, `multiturn-bg-d`, completed all four autonomous rounds with no user interaction after the initial start:

- `completedRounds = 4`;
- `lineCount = 4`;
- `artifactPath = artifacts/async-multiturn/multiturn-bg-d.txt`;
- `artifactSha256 = f2d5c0d56930355bab3439c6bab3f61071568d24590daf26b010869268373e24`.

This is direct evidence that, while an eligible foreground View remains active, one user request can lead to repeated cycles of background wait -> App wake -> new model turn -> real tool side effect -> background wait, without additional user messages.

## Important background-tab boundary

A separate run, `multiturn-bg-b`, produced a different observation while the ChatGPT tab was left in the background:

- Round 1 completed;
- Round 2 reached an exact terminal event;
- the iframe continued polling (the poll counter continued increasing well past 200);
- the server wake state reached the probe's `delivered` state after `ui/message` returned successfully;
- no new model turn started while the run remained in that condition.

Keeping or returning the tab to the foreground did not retroactively make that already accepted dispatch start immediately in that run. By contrast, the fresh all-foreground four-round run completed normally.

Therefore the probe's old word `delivered` was stronger than the evidence. A successful `ui/message` RPC proves that the Host accepted the dispatch request; it does **not** prove that a new model turn has started or will start immediately.

Production state should distinguish at least:

- dispatch requested/prepared;
- dispatch accepted by the Host;
- continuation actually consumed by a later model turn.

Only exact continuation consumption is proof that the model turn ran. Background-tab scheduling should be treated as eventually available/best effort, not as a real-time execution guarantee.

## Durable wake safety lessons

The probe intentionally reused the same safety shape already explored by the durable Agent controller work:

- exact run/session binding rather than a global "current" run;
- one controller/View generation winning a short lease;
- a prepare/dispatch fence immediately before Host invocation;
- no automatic resend after the fence when dispatch outcome is uncertain;
- exact event and nonce consumption;
- idempotent consumption/write handling;
- stale or duplicate Views unable to retarget a continuation.

The useful distinction is now empirical rather than hypothetical: these mechanisms protect WebCodex state, while Host acceptance and actual model-turn scheduling remain separate external events.

## Product and architecture consequences

The experiments support the following product rule:

> ChatGPT is a checkpoint and milestone surface; WebCodex WebUI is the execution-trace surface.

A production workflow presentation should therefore prefer:

- one sparse persistent Plan/Workflow App created at a meaningful start boundary;
- ordinary coding/execution tools that keep their native ChatGPT tool cards and do not each bind another custom App;
- server-owned durable Workflow Session / Job state as the authority;
- an App-only bounded read path for presentation refresh;
- high-level phases such as `Understand -> Implement -> Validate -> Review`, current status, a few salient facts, attention/failure state, and an "Open in WebCodex" path rather than full logs/diffs;
- durable terminal/decision events for the few points that truly need another model turn;
- exact continuation identity plus controller generation/lease/fence/consume semantics;
- `dispatch accepted` rather than `model resumed` until the later turn exact-consumes the event.

Do not copy the temporary timer/map implementation into production. Reuse the authoritative Workflow Session, Job lifecycle/observation, ActionAudit/window correlation, and durable Agent continuation primitives that already own persistence, authority, and recovery semantics.

The static Result App follows the same sparsity rule: current tool descriptors bind it only to `list_jobs`, `validation_summary`, and `git_review_summary`. High-frequency observation, validation-run, and worktree-review calls keep native Host presentation; bounded legacy projections remain available only so already-cached older descriptors fail gracefully rather than forcing a compatibility break.

The production Durable Goal G2 implementation applies the same presentation findings to a server-owned Goal: one explicit `present_goal_plan(goal_id)` binds `ui://webcodex/goal-plan/v2`, while the existing View uses the ModelHidden/app-only `goal_plan_state(goal_id)` exact read to converge on SQLite Goal revision. G3 does **not** change that contract: Goal still has no Wake, `ui/message`, model resume, dispatch fence, consume token, background model scheduler, Agent owner/controller relation, or automatic Goal/work execution transition.

## Production G3 mapping

G3 now maps the demonstrated Host primitive onto the existing production Durable Agent substrate instead of copying the temporary probe state machine:

```text
authoritative SQLite Wake / Wake Delivery Attempt
        ↓
exact current Agent Endpoint + controller generation
        ↓
process-local MCP App View binding
        ↓
agent_continuation_wake_acquire   # existing durable claim
        ↓
agent_continuation_wake_prepare   # existing durable dispatch fence
        ↓
View calls Host ui/message exactly once
        ↓
agent_continuation_wake_finish    # dispatch_accepted | delivery_unknown
        ↓
later model turn exact-consumes Wake
```

The only card-creating entry is the explicit read `present_agent_continuation(agent_id, endpoint_id, expected_controller_generation)`, bound to `ui://webcodex/agent-continuation/v3`. Bind/state/acquire/prepare/finish/unbind are globally ModelHidden and are projected only as App-visible tools on eligible Stateless MCP 2026 operator surfaces. They do not bind the resource again, so polling/coordination does not create a stream of custom cards. Ordinary communication and coding tools keep native Host presentation.

The View binding is process-local fencing, not durable authority. Every App-only operation re-authorizes the normal communication principal and exact Agent/Endpoint/controller generation. Bind succeeds only for an Endpoint freshly attached in that Server process. A later View for the same exact Endpoint/generation replaces the previous process binding; the stale iframe cannot heartbeat, acquire, prepare, finish, or teardown the new controller. On replacement/loss, existing Store reconciliation handles the durable state: pre-fence claim -> revoked Attempt + pending Wake; post-fence prepared/delivered -> `delivery_unknown`. Server restart forgets the View binding and cannot revive it from an old endpoint id alone.

The App keeps claim fences entirely Server-side. The bounded automatic message is returned only after prepare and contains exact `agent_id`, `endpoint_id`, `controller_generation`, `wake_id`, and `consume_token`; it contains no Conversation Message body, transcript, Agent private description/specialty labels, credential, principal digest, claim fence, Project authority, or Workflow Session authority. The View generates a stable secure random binding fence and sends it in bind input; same-current-View retries renew without replacing its claim or dispatch phase. The automatic message uses the app-only standard `structuredContent.output.app_protocol` result channel. Neither value enters ordinary model-visible projections, typed audit/session projections, or forensic tool-request payload capture. Continuation correctness does not depend on custom ToolResult `_meta`.

`ui/message` success is recorded only as `dispatch_accepted`. Timeout, reload, View loss, or any post-fence outcome that cannot prove non-delivery becomes `delivery_unknown`; the App never automatically sends a second `ui/message` for that Attempt. If the new model turn starts before the Host ACK is recorded, exact `consume_agent_wake` may win first; the later ACK is idempotent and cannot move the Wake back from `consumed`. Only exact consume is production evidence of `continuation_consumed`.

The production App uses bounded heartbeat/reconciliation. A hidden/background View may renew its exact Endpoint but does not initiate a new automatic `ui/message`; returning to the foreground triggers immediate authoritative reconciliation. Pagehide, beforeunload, and `ui/resource-teardown` stop polling and attempt exact best-effort unbind. Correctness never depends on reliable teardown, browser memory, or localStorage.

## Production App bootstrap

The G3 dogfood exposed a View bootstrap gap: a queued Delivery and pending Wake
could exist while the card stayed at `Initializing`, made no bind/state calls,
and let its Endpoint lease expire. Both the Agent Continuation and Goal Plan
Views previously waited for the initial ToolResult to select their resource.

The v2 Views accept complete `ui/notifications/tool-input` through the canonical
`params.arguments` object defined in the [MCP Apps 2026-01-26 data-passing
contract](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx#data-passing).
Partial input does not select a resource. Agent input selects the exact
`agent_id`, `endpoint_id`, and `expected_controller_generation`; Goal input
selects one canonical `goal_id`. A validated initial ToolResult can provide the
same selector as a fallback and an optional first projection. Either ordering
works, and a missing ToolResult does not block bind/heartbeat or Goal polling
once Host initialization succeeds. Unknown Goal lifecycle permits the first
authoritative read; terminal Goal state still stops polling.

Each card accepts only one identity. Matching notifications are idempotent;
conflicting identities stop coordination, cancel pending View requests, and
leave a bounded error. An already-bound Agent View attempts only its original
exact unbind. Late replies cannot restart it. Tool input is an exact selector,
never authorization: every actual read/mutation still runs the Server's existing
principal, scope, and resource checks.

Visible status distinguishes script activity, Host initialization, exact identity
selection, and live binding/polling, with separate initialization, binding, and
identity errors. Diagnostics do not display binding ids, claim fences, or consume
tokens. `tools/list` and `resources/list` advertise only the canonical
`ui://webcodex/agent-continuation/v3` and `ui://webcodex/goal-plan/v2` resources.
The old v1 URIs and Agent continuation v2 are hidden read aliases serving the same current templates;
they do not revive expired Endpoints or bypass fresh-binding requirements.

## Remaining verification boundary

Deterministic tests cover surface isolation, protocol fail-closed behavior, authorization/existence hiding, duplicate-View fencing, Endpoint replacement and Server restart semantics, pre-fence recovery, post-fence uncertainty, 50-Message burst coalescing, exact consume/token/generation checks, consume-before-ACK ordering, and secret redaction. The remaining environment-specific step is manual ChatGPT dogfood of the production App resource and `ui/message` Host behavior. That dogfood must continue to interpret Host success as dispatch acceptance only; background model-turn scheduling remains eventually available/best effort, not an immediate guarantee.

### Production result-channel compatibility follow-up

The next production dogfood reported successful attach, present, and bind in the same OpenAI window, persisted `wake_capable=true`, and no subsequent state/heartbeat. The card reported Host binding unavailable. That evidence confirms tool-input bootstrap and Server bind succeeded; it is consistent with custom ToolResult metadata being absent at the View, but does not by itself distinguish metadata stripping from response loss or other malformed result delivery.

The v3 fix removes that correctness dependency for both bind and prepare. [MCP structured results](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#structured-content) define `structuredContent`; the [MCP Apps 2026-01-26 contract](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx) uses standard `tools/call` for View requests and requires app-only tools to be excluded from the model's tool list. The repository additionally gates these tools by protocol capability, operator surface, normal communication authority, and exact current process-local carrier. App-originated results are not ordinary model tool transcript entries; the View forwards only the prepared automatic `ui/message`, never a generic model-context update.

The deterministic Host harness can now remove all custom result metadata only from App-originated calls, independently of initial tool-result notifications. Regressions cover metadata-free lifecycle and successor Wakes, stable bind retries, View replacement, and conservative malformed/post-timeout prepare handling. These local tests do not establish successful production v3 continuation or exact Wake consumption; that remains the next named deployment/dogfood step.
