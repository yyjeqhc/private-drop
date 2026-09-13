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

## Host admission and background-carrier follow-up — 2026-09-13

Two focused ChatGPT Host probes tightened the presentation and scheduling model further.

First, result-time App admission was tested with one tool whose descriptor deliberately had no App binding and one already-registered UI resource. Three otherwise equivalent ToolResults were returned: a plain result, a result with `_meta.ui.resourceUri`, and a result with both `_meta.ui.resourceUri` and the historical `openai/outputTemplate` compatibility alias. None created a custom App card. The practical Host contract is therefore descriptor-time admission: ToolResult metadata can feed an App that the Host already decided to create, but it cannot promote an ordinary native tool result into a new App presentation after execution. A tool such as `show_changes` consequently cannot use `clean -> no card` / `dirty -> result-time card` on one descriptor. Semantic admission needs a separate explicitly App-bound presentation tool, while ordinary work tools remain unbound.

Second, the background continuation boundary was retested with a dedicated mounted-card carrier that intentionally removed the View visibility gate while preserving exact wake acquire/prepare, nonce, consume, and fail-closed delivery semantics. The original ChatGPT tab remained hidden while another window rearmed the same run. For `background-carrier-a`, three consecutive cycles reached the authoritative terminal state with:

- `status=completed`;
- `wake.state=consumed`;
- `wake.prepareVisibility=hidden`;
- `wake.dispatchVisibility=hidden`.

The run was inspected from the other window before foregrounding the carrier tab, so `consumed` cannot be explained by later foreground reconciliation. This is direct evidence that an already-mounted MCP App can remain a background carrier, dispatch `ui/message` while hidden, and have ChatGPT start and complete the resumed model turn without first foregrounding that tab. It supersedes the earlier `multiturn-bg-b` observation as a capability boundary, but not as a scheduling guarantee: Host/browser throttling, suspension, tab discard, or future policy can still delay or stop a carrier. Durable correctness must continue to rely on server-owned pending Wake state and exact consumption, never on timer liveness or a promise of real-time background execution.

These results split the product surface into two intentional patterns:

- result/report cards such as Work Result are sparse and explicitly admitted, render the authoritative `present_work_result` snapshot once, and stay static until the user presses Refresh; they do not need background timers merely because the Host can run them;
- controller cards such as Durable Agent continuation may deliberately remain live carriers when automatic continuation is a product requirement, but removing a production visibility gate is a separate policy choice and must retain exact Endpoint/generation, lease/fence, `delivery_unknown`, and consume protections.

The capability result therefore supports long-lived Agent carriers without turning every MCP App into a polling surface. Presentation density, refresh cost, and autonomous scheduling remain separate design decisions.

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

The coding result surface follows the same sparsity rule, but production Host probing tightened the admission model further: an unbound descriptor does not create an App later merely because its ToolResult contains `ui.resourceUri` or `openai/outputTemplate`. Ordinary tools therefore have no Work App descriptor binding at all, including `show_changes`, Job observation, validation execution/summary, committed-range review, and coding closeout. A model explicitly calls `present_work_result(project, session_id)` only when one persistent user-visible summary is useful; that binds `ui://webcodex/work-result/v1` once and returns the initial authoritative snapshot. The mounted View then remains static: only an explicit user Refresh performs one ModelHidden/app-only `work_result_state(project, session_id)` exact read. Each refresh re-authorizes the exact Session and Project, reads bounded current worktree status/numstat plus existing Session validation/review evidence, requests no diff hunks, runs no validation/review, and deliberately does not append refresh events to the observed Session ledger. There is no Work Result background timer, visibility-triggered read, or retry loop. The old Changes/Result resources and bounded result projections remain hidden-readable compatibility paths for cached descriptors; compatibility does not imply current descriptor-level admission.

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

The only card-creating entry is the explicit read `present_agent_continuation(agent_id, endpoint_id, expected_controller_generation)`, bound to the current `ui://webcodex/agent-continuation/v15` resource. Bind/recover/state/acquire/prepare/finish/unbind are globally ModelHidden and are projected only as App-visible tools on eligible Stateless MCP 2026 operator surfaces. They do not bind the resource again, so polling/coordination does not create a stream of custom cards. Ordinary communication and coding tools keep native Host presentation.

The View binding is process-local fencing, not durable authority. Every App-only operation re-authorizes the normal communication principal and exact Agent/Endpoint/controller generation. `binding_id` identifies only the current iframe/process instance. For MCP Apps, the Store retains both the v11 SHA-256 fingerprint of the exact current binding fence and, when the Host supplied valid request-scoped identity, the already-domain-separated SHA-256 `ClientWindow` key. Raw `_meta["openai/session"]` is never persisted. Server takeover clears process-local carriers and durable `wake_capable` but preserves both bounded restart-recovery values. The exact old binding fingerprint remains the fallback when no valid ClientWindow exists. With a canonical ClientWindow, the same authenticated principal + exact Endpoint/generation + same Window may restore or replace a lost iframe fence after restart; refresh unbind clears the iframe fingerprint but preserves that same-Window continuity while the Endpoint remains current. Natural lease expiry clears wake capability and the binding fingerprint; v15 preserves only the hashed Window key as a bounded input to the dedicated expired-Endpoint replacement operation and its exact one-hop replay chain, while ordinary bind/state still reject the expired Endpoint. Explicit detach, ordinary Endpoint replacement, or transition to a push carrier clears the continuity value. Push adapters still require a fresh current-process attachment and never inherit Window recovery. On replacement/loss, existing Store reconciliation handles the durable state: pre-fence claim -> revoked Attempt + pending Wake; post-fence prepared/delivered -> `delivery_unknown`.

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

Each card still accepts only one ordinary projected identity. Matching notifications are idempotent; conflicting ordinary identities stop coordination, cancel pending View requests, and leave a bounded error. v15 keeps the v14 narrow exception: only a successful `agent_continuation_recover_endpoint` ToolResult carrying a strict Server-authored replacement envelope may transition the card from its exact current stale selector to exactly `generation+1` for the same Agent. A live successor must match the returned normal projection; an already-recorded successor that has itself naturally expired is instead returned as `agent_continuation=null` plus `successor_needs_recovery=true`, so the card adopts exactly that one selector and asks for the next edge. At most eight successor hops are accepted. The App advances a local identity epoch after every accepted edge so delayed old bind/state responses and old timer callbacks are inert. An already-bound Agent View attempts only its current exact unbind. Tool input is an exact selector, never authorization: every actual read/mutation still runs the Server's existing principal, scope, and resource checks.

Visible status distinguishes script activity, Host initialization, exact identity
selection, and live binding/polling, with separate initialization, binding, and
identity errors. Diagnostics do not display binding ids, claim fences, or consume
tokens. `tools/list` and `resources/list` advertise only the canonical
`ui://webcodex/agent-continuation/v15` and `ui://webcodex/goal-plan/v2` resources.
Agent continuation v1-v14 are hidden read aliases serving the same current template. Reading an alias does not revive an expired Endpoint or bypass exact generation/authorization fencing; the same current template must still complete explicit Server-authorized one-hop replacement transitions. The v11 fingerprint-proven restart fallback, v12 strict restart projection, v13 canonical Host-window refresh fence, and v14 expired-Endpoint replacement remain intact; v15 adds bounded sequential successor replay rather than broadening any stale binding path.

## Remaining verification boundary

Deterministic tests cover surface isolation, protocol fail-closed behavior, authorization/existence hiding, duplicate-View fencing, live-controller protection, same-Window expired-Endpoint replacement, different-Window rejection, concurrent replacement, response-loss replay, bounded E1 -> E2 -> E3 -> ... successor recovery, no rollback after a later generation, replacement before/after Server restart, delayed-old-response App guards, pending Wake preservation, pre-fence recovery, post-fence uncertainty, 50-Message burst coalescing, exact consume/token/generation checks, consume-before-ACK ordering, and secret redaction. The remaining environment-specific step is manual ChatGPT dogfood of v15 with repeated natural successor expiry and reopening the original Conversation/card. That dogfood must verify in-place `Reconnecting… -> Host bound`, the same pending Wake/Delivery continuing through E1/g1 -> E2/g2, and no duplicate Host `ui/message`. Host success remains dispatch acceptance only; background model-turn scheduling remains eventually available/best effort, not an immediate guarantee.

### Production result-channel compatibility follow-up

The next production dogfood reported successful attach, present, and bind in the same OpenAI window, persisted `wake_capable=true`, and no subsequent state/heartbeat. The card reported Host binding unavailable. That evidence confirms tool-input bootstrap and Server bind succeeded; it is consistent with custom ToolResult metadata being absent at the View, but does not by itself distinguish metadata stripping from response loss or other malformed result delivery.

The v3 fix removed the earlier correctness dependency on custom ToolResult `_meta`, but the next production run narrowed the remaining failure further: every `agent_continuation_bind` reached the Server and returned protocol/tool success while the View repeatedly reported `Host binding response unavailable · reconciling`. That is consistent with a Host preserving standard `content` while omitting `structuredContent` from the response returned to a View-originated `tools/call`.

The v4 compatibility path therefore keeps `structuredContent` canonical but duplicates the same bounded machine envelope as JSON in standard `content[0].text` for the app-only Agent continuation coordination tools. The View prefers structured content and falls back to that JSON text. This remains isolated from ordinary model tools because these coordination tools are ModelHidden/App-visible only; no custom ToolResult `_meta` is required. The prepare envelope still excludes Host binding ids, claim fences, private Conversation bodies, and private Agent profile data; its bounded automatic message is intentionally available to the View because it is the exact payload later passed to `ui/message`.

The deterministic Host harness can independently strip custom metadata and `structuredContent` from App-originated calls. Regressions cover the metadata-free/content-only lifecycle, successor Wakes, stable bind retries, View replacement, and conservative malformed/post-timeout prepare handling. Canonical resource revisions advance between production dogfood rounds so a newly presented card cannot silently reuse an older continuation template; prior revisions remain hidden read aliases for existing cards. These local tests do not establish successful production continuation or exact Wake consumption; that remains the named deployment/dogfood boundary.

The v4 production dogfood still produced three successful Server-side binds and no subsequent state call. The serialized bind response grew to the expected compatibility-envelope size, and retries happened after only a few seconds rather than the View's 10-second request timeout. This proves the fallback reached the Host and the Host returned promptly, but the value exposed to the iframe still did not match the App's accepted CallToolResult shapes. The same production run fetched the App resource after presentation, reducing stale-resource caching as an explanation.

The v5 View therefore adds two narrowly validated bridge variants without weakening bind semantics: a one-level nested CallToolResult and the canonical `structuredContent` value returned directly. Both still require `success=true`, an exact valid Agent/Endpoint/generation projection, and `host_binding.bound=true`. Any other successful-but-unusable response remains fail-closed and now renders only a fixed response-shape class such as `empty-object`, `content-only`, or `other-object`; it never displays payload keys, identities, binding ids, Wake data, or continuation tokens. The canonical URI advances to `ui://webcodex/agent-continuation/v5`, with v1-v4 retained only as hidden read aliases.

The v5 production dogfood again produced three successful Server-side binds and no state call. This makes a malformed successful result less persuasive than a Host-side View/tool association failure. The known-good resume-arbiter probe gives its app-only register/acquire descriptors both `ui.visibility=["app"]` and the same `ui.resourceUri` as their owning View, while WebCodex had deliberately omitted `resourceUri` from its six continuation coordination descriptors. v6 aligns the descriptors with that known-good shape: each app-only continuation tool remains model-hidden but is explicitly associated with the canonical continuation resource. The resource association is treated only as a Host compatibility hint, not as authorization or a correctness dependency; model invisibility still prevents these tools from becoming separate model-created cards.

The v6 production dogfood showed that the resource association was compatible but not sufficient: the Server still returned successful bind results while the View never reached `agent_continuation_state`. Switching tabs produced an explicit View unbind followed by fresh bind retries, and the durable Message remained queued, proving that teardown/recreation exposed the same bridge failure without consuming the Wake. Because the Server returned in milliseconds while retries followed the 3-second visible cadence rather than the 10-second View timeout, v7 treats an immediate Host JSON-RPC rejection as the primary diagnostic hypothesis rather than adding more result-envelope guesses.

v7 adds bounded bridge diagnostics and correlation without enabling continuation payload tracing. Every View-originated continuation coordination call carries an optional `wc_app_call_<16 hex>_<sequence>` diagnostic id advertised only on app-visible descriptors. The MCP adapter validates it, attaches it to metadata trace lifecycle events, and strips it before ToolRuntime parsing; it grants no authority and contains no Agent, Endpoint, binding, Wake, Attempt, Message, or token identity. The App renders only fixed failure classes (`bridge-error`, `bridge-timeout`, `bridge-cancelled`, `malformed-result`), numeric JSON-RPC error codes, fixed result-shape classes, and that diagnostic call id. Server metadata tracing also emits a safe continuation result summary containing only tool success, structured/content presence, host-bound/wake-present booleans, queued-delivery count, and a fixed dispatch phase. Full forensic payload capture remains suppressed for all six continuation coordination tools.

The v7 production dogfood finally correlated the same three failed bind attempts end to end. For each exact App call id the Server recorded `protocol_success=1`, `tool_success=1`, canonical `structuredContent`, one standard content block, `host_bound=1`, HTTP 200, and the same 1237-byte response; the View nevertheless classified the Host-exposed value as `response=structured` and never issued a state call. Source review found a local compatibility bug: `structuredOf()` preferred any object-valued Host `structuredContent` before validating that it retained the canonical `success` plus `output/error` envelope, so a Host-projected non-canonical structured object could mask the still-valid standard JSON content fallback. v8 fixes the precedence rather than weakening bind semantics: only canonical structured envelopes win; otherwise the View continues to the canonical JSON text fallback. Remaining malformed results also report one fixed semantic gate such as `structured-envelope-invalid`, `projection-missing`, or `host-bound-not-true`, never payload values.

The v8 production dogfood narrowed the Host projection boundary one layer further: the card reported `response=structured · semantic=projection-invalid`, while the exact correlated Server call again recorded `protocol_success=1`, `tool_success=1`, one canonical structured envelope, one standard content block, `host_bound=1`, HTTP 200, and a 1237-byte response. The v8 parser still preferred a canonical outer Host `structuredContent` envelope before inspecting the Server-authored standard JSON content, so a Host projection that preserves `success/output` but drops or rewrites nested continuation fields can still mask the complete fallback. This is especially compatible with a Host omitting null-valued projection fields such as `wake` or `dispatch_observation`, although the exact Host mutation remains unobserved. v9 therefore treats the standard JSON content copy as the preferred carrier for a full CallToolResult and uses Host `structuredContent` only when that copy is unavailable. Missing projection fields are not normalized or defaulted; fail-closed semantics remain unchanged. Fixed projection diagnostics now distinguish version, identity, display name, queued count, dispatch, and Wake sub-gates without exposing field values.

The v9 production dogfood completed the full continuation path: a Human durable Message created one queued Delivery/Wake; the existing live card prepared and dispatched the automatic Host message; the resumed model turn bootstrapped the exact Agent/Endpoint/Wake, read the Agent Inbox, posted the intended Agent reply, consumed the processed Delivery, and finally consumed the Wake. Subsequent state reported `continuation_consumed`. The remaining issue was presentation quality: the generic resume contract could dominate the visible ChatGPT reply even though the durable Agent work was handled correctly. The Server-generated `resume_hint` now explicitly makes newly queued or otherwise unprocessed Inbox deliveries the current task for the resumed turn and asks the visible final response to reflect the actual work/result rather than repeat the continuation/setup contract. This is a Server-side prompt change only; it does not revise the v9 App template or resource URI, so an already-live v9 card picks it up on the next Wake after the Server is updated.

The v10 follow-up addresses Server restart recovery for an already-open card without weakening Endpoint or View fencing. Server takeover still clears process-local Host carriers, durable `wake_capable`, and safely reconciles in-flight Wake state. The state path now distinguishes that specific condition as `host_binding_missing_in_process` only after ordinary principal and exact Endpoint/controller-generation validation succeeds and only when no process-local binding exists. A different live binding for the same exact Endpoint remains `host_binding_stale`; expired, detached, replaced-generation, authorization, malformed, and generic bridge failures retain their existing fail-closed errors. MCP App bind may recreate the process-local binding for the same exact current Endpoint/generation after takeover when durable `wake_capable=false` and no local carrier exists; push adapters still require a fresh attach. The View preserves its stable `binding_id` and exact identity, clears only obsolete local coordination markers, and performs at most one automatic recovery rebind before resuming normal state polling. It never treats JSON-RPC `-32000`, arbitrary business errors, malformed results, or `host_binding_stale` as restart recovery. The canonical resource advances to `ui://webcodex/agent-continuation/v10`; v1-v9 remain hidden read aliases.

Production v10 dogfood exposed two separate defects in that contract. After a real Server restart the same live View continued issuing `agent_continuation_state`; the Server returned HTTP 200 with protocol success, a canonical structured/content ToolResult, and the intended tool failure, but ChatGPT projected that `isError=true` View-originated result to the iframe only as JSON-RPC `-32000`. The iframe therefore could not read `error_kind=host_binding_missing_in_process`. More importantly, `current exact Endpoint + wake_capable=false + no process-local binding` did not uniquely prove restart: replacement followed by normal unbind can produce the same durable/process-local shape, so changing only ToolResult presentation or treating generic `-32000` as recovery would let a stale View contend for the controller.

v11 makes restart provenance explicit instead of inferred. A successful MCP App bind persists only a SHA-256 fingerprint over a fixed domain separator plus exact `agent_id`, `endpoint_id`, `controller_generation`, and `binding_id`. Replacement installs the successor fingerprint; normal exact unbind, detach, expiry, Endpoint replacement, and push binding remove it. Owned Server takeover preserves the last current MCP App fingerprint while clearing `wake_capable` and all process-local bindings. State first performs ordinary principal and exact current Endpoint/generation checks; when no local binding exists, only an exact supplied-binding fingerprint match yields a normal `success=true` projection with `host_binding.bound=false` and `recovery.kind="host_binding_missing_in_process"`. Wrong/stale View, stale generation, expired/detached Endpoint, authorization failure, malformed binding, or generic bridge failure retains ordinary failure semantics. The App validates that successful projection and performs at most one same-identity/same-`binding_id` rebind; it never recovers from `isError=true`, `host_binding_stale`, or JSON-RPC `-32000`. Pending Wake state remains untouched by the observation, and prepared state still becomes `delivery_unknown` on takeover with no blind resend. The canonical resource is `ui://webcodex/agent-continuation/v11`, `appInfo.version` is `11.0.0`, and v1-v10 remain hidden read aliases.

v12 fixes the descriptor boundary discovered after v11: the runtime projection already emitted `recovery=null` normally and the exact recovery object after a proven restart, but the strict `agent_continuation_projection_schema()` did not declare `recovery`. A Host projecting results through that `additionalProperties=false` schema could therefore remove the marker before the iframe saw it. The published projection now requires `recovery` on every projection, with exactly `null` or the strict object `{kind: "host_binding_missing_in_process"}` and no additional recovery properties. Regression coverage projects a real restart runtime result through that published schema and requires byte-for-value-equivalent projection fields before the App harness proves exactly one bounded rebind. The canonical resource advances to `ui://webcodex/agent-continuation/v12`, `appInfo.version` to `12.0.0`, and v1-v11 remain hidden read aliases.

Production v12 then exposed a distinct lifecycle path: restart recovery itself succeeded, but refreshing that recovered tab caused the old iframe to exact-unbind, clearing its process binding and durable `binding_id` fingerprint. The new iframe necessarily generated a new binding fence, while Server takeover had already cleared the process-local fresh-attachment registry, so the new fence matched none of the v12 registration paths and failed `host_binding_stale`. Production metadata showed that the canonical hashed ClientWindow remained stable across the refresh. v13 therefore separates two identities instead of weakening the binding fence: principal + exact Endpoint/generation + canonical ClientWindow is Host-window continuity, while `binding_id` remains the current iframe/process-instance fence. Stateless ChatGPT requests derive the Window only from `_meta["openai/session"]` through the existing domain-separated SHA-256 `ClientWindow`; the raw Host value never enters durable state. A same-Window refresh may replace its iframe fence even after restart/unbind, while a different Window, foreign principal, stale generation, expired/detached Endpoint, or malformed/missing Window does not gain that path. Missing Window identity falls back only to the existing exact fingerprint path. Normal Window-bound unbind preserves the bounded Window key but clears the current binding fingerprint; push replacement, detach, expiry, and Endpoint replacement clear both. `attached_endpoints` remains process-local and is never repopulated by recovery, so the pre-v11 stale-View class stays closed. The canonical resource advances to `ui://webcodex/agent-continuation/v13`, `appInfo.version` to `13.0.0`, and v1-v12 remain hidden read aliases.

Production v13/v14 dogfood then separated short-term View recovery from long-term Endpoint UX. Same-Window refresh, repeated refresh, reopening the original Conversation after closing the ChatGPT window, and more than two minutes in the background all preserved durable work correctly while the original controller remained recoverable. Once the exact Endpoint lease was truly past its correctness boundary, however, the old card correctly failed closed on E1/g1 and the Host collapsed the underlying App business failure to generic JSON-RPC `-32000`. Manually attaching E2/g2 under the same managed-user principal proved the durable layer was already correct: the queued Delivery and pending Wake survived unchanged. The missing capability was therefore not a longer lease or looser bind rule, but an explicit way for the original card to ask the Server to replace an expired controller in place.

v14 adds `agent_continuation_recover_endpoint` as an App-only idempotent probe+mutation. The App submits its exact stale Agent/Endpoint/generation and current iframe fence; the canonical ClientWindow is supplied only by the Host/runtime sideband and is not a tool argument. The Store's immediate transaction verifies same owner, same Agent, exact old generation, old Endpoint provenance, same durable Window hash, natural expiry, and absence of a newer authoritative generation. A live controller returns `controller_live` without mutation. An eligible stale controller commits E2/g2 and an idempotency record together; concurrent callers converge on that same successor, exact response-loss replay returns the same successor, and replay fails once any later generation becomes authoritative. Different-Window recovery fails closed. The 120-second lease is unchanged.

The View still rejects all ordinary identity changes. Only the dedicated successful replacement envelope may move its identity from the exact current stale selector to exactly `generation+1`; it then increments a local identity epoch, clears obsolete process-local coordination state, shows `Reconnecting…`, and binds the returned Endpoint in the same card. Bind/state Promises and timers capture the epoch so delayed E1/g1 work is inert after E2/g2 is accepted. Generic `-32000` is never treated as proof of expiry: it can only lead to the dedicated probe, whose Server-side result decides live/no-op, authorized replacement, or failure. Restart recovery (`host_binding_missing_in_process` for the same Endpoint) and expired-Endpoint replacement (a new Endpoint/generation) remain separate protocols. The canonical resource advances to `ui://webcodex/agent-continuation/v14`, `appInfo.version` to `14.0.0`, and v1-v13 remain hidden read aliases.

### Branch review follow-up — 2026-09-12

The local review covered the 17 commits from `2d51da7c` through `69725871`,
including Store transactions, process bindings, runtime admission, MCP schemas,
App message ordering, and audit/trace privacy. Four demonstrated defects were
corrected with focused regressions:

- A canonical `endpoint_expired` bind result previously stopped the reopened
  card before its dedicated recovery probe. It now permits that probe while
  other definitive business failures remain terminal. App tests cover the full
  successor bind/dispatch path, response-loss replay, invalid replacement
  envelopes, late old notifications, and a later expiry after a healthy poll.
- Detach previously rejected elapsed leases or returned a no-op for materialized
  expiry, leaving automatic recovery eligible. Exact principal-owned Endpoint
  detach now revokes both recovery values even after expiry.
- Replacement replay previously recreated a fresh process attachment and did
  not recheck the successor's Window continuity. Replay can no longer confer
  push-registration authority after restart or undo a push carrier's revocation
  of MCP App continuity.
- `agent_continuation_recover_endpoint` was missing from the forensic payload
  suppression list. Its binding fence now receives the same trace exclusion as
  the other App coordination tools.

**v15 closes the A4b repeated-successor prerequisite without changing one-hop authority.**
A replay of the E1 recovery operation may return only its uniquely committed E2 successor,
even if the Agent's current generation is already E3 or later. Replay still rechecks the
same owner, Agent, exact predecessor generation, durable successor generation, retained
ClientWindow lineage, detach state, and revocation provenance; it never recreates fresh
push-registration authority.

If that exact E2 successor is itself naturally expired, runtime no longer ordinary-
bootstraps it. Instead the strict successful result carries `agent_continuation=null`, the
exact E1 -> E2 replacement envelope, and `successor_needs_recovery=true`. The App validates
same Agent, exact predecessor Endpoint/generation, canonical successor id,
`new_generation = old_generation + 1`, and `reason=endpoint_expired`, adopts E2, then calls
the same operation with E2. Every later edge is independently durable/idempotent. The App
stops after eight accepted edges and reports ordinary connection-unavailable semantics;
Server code never walks arbitrary Endpoint history. Generic JSON-RPC `-32000` remains
non-evidence and cannot authorize recovery.

A real Store/runtime regression constructs E1 -> E2 -> E3, expires E2, replays E1, and
projects the returned intermediate result through the published strict output schema;
the Host-visible projection must retain the entire exact one-hop proof. The App harness
covers sequential expired successors through a live tail and the eight-hop cap.

Focused validation passed: 113 App tests, 25 runtime/MCP continuation tests,
11 Store communication tests, nine continuation contract/parser/privacy tests,
and four tool-definition invariants. The six no-Window runtime wrappers have
only test callers and are now compiled only in tests; production dispatch keeps
the canonical ClientWindow sideband. `cargo check --locked -p webcodex --lib`,
affected Rust formatting, and diff whitespace checks also passed.

No deployment or production Host test was performed during this review. Real
ChatGPT full-close/reopen scheduling and delivery remain a separate dogfood
verification boundary.
