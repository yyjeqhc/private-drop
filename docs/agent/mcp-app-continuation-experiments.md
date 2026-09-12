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

## Recommended implementation order

The Host mechanism is no longer the main unknown. The lowest-risk implementation sequence is:

1. **Improve the existing static Result App projection first.** Reuse the current `webcodex/presentation` contract and display richer bounded Job/validation/worktree context without adding polling or wake behavior.
2. **Add a dedicated workflow/plan presentation projection.** Keep it read-only and server-owned initially; one start card should represent the high-level Workflow Session rather than every tool call.
3. **Add App-only exact workflow-state refresh.** This gives the single card eventual convergence without changing ordinary model-visible tool behavior.
4. **Integrate the durable continuation controller only after the presentation identity is stable.** Reuse exact endpoint/controller generation, wake lease, dispatch fence, and consume semantics; do not make Host continuation a core execution dependency.
5. **Treat background model-turn scheduling as non-immediate.** Backend work must remain independently durable, and a pending decision must still be recoverable when an eligible View/model activation becomes available later.

This staged path lets WebCodex improve the user-visible card immediately while keeping the more consequential continuation adapter isolated and reviewable.
