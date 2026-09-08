# Tool composition research and development plan

Status: exploratory design note. This document records current research findings
and a staged direction for reducing model/tool round trips. It is not a current
runtime contract and does not authorize implementation shortcuts around existing
tool, Session, Job, permission, audit, or Project boundaries.

## Motivation

Current WebCodex already has strong primitive tools and several homogeneous batch
surfaces: `read_files`, `search_project_texts`, `observe_jobs`, guarded edit
batches, and structured validation. `run_shell` and `run_script` can also execute
multiple local commands in one remote request.

Those capabilities reduce some transport cost, but they do not provide a general
way to combine *heterogeneous canonical WebCodex tools* in one model-facing MCP
round trip. A review may still need a sequence such as:

```text
git review summary
-> model decision
-> source search
-> model decision
-> multi-file read
-> model decision
-> focused validation
```

For ordinary local inspection, the child process itself is often much shorter
than the surrounding model/tool decision cycles and remote round trips. Using one
large shell script can reduce those cycles, but making shell the universal answer
would discard typed schemas, Project resolution, SHA guards, permission checks,
Workflow Session evidence, Job identity, structured validation, bounded results,
and audit policy. The target is therefore composition *above* canonical tools,
not a return to a remote-shell-only surface.

Window activity correlation provides a useful measurement boundary for this
work. WebCodex can measure time from one inbound request through Server/Runner
processing and can see when the next request arrives. It still cannot observe the
model's private reasoning or prove why time outside WebCodex elapsed. Performance
telemetry must preserve that distinction.

## Research snapshots

### WebCodex Next

The inspected WebCodex Next design already separates model-facing call economy
from canonical effect identity. Its `Apply` envelope can admit multiple typed
operations in one request while creating an independent durable Execution for
each operation. The safety model is "atomic admission, ordered effects": retry,
authority, recovery, and effect truth remain attached to each canonical
Execution rather than to a synthetic aggregate effect.

Later focused adapters apply the same principle more narrowly. In particular, a
multi-file patch adapter translates one model-facing call into several ordinary
canonical patch operations without inventing a new Node batch effect, transaction,
or rollback contract. This is a useful precedent for reducing model-facing
round trips without collapsing underlying effect identities.

WebCodex Next dogfood also found that a very large composed `Apply` schema could
project poorly through a ChatGPT host while focused composition-free tools in the
same server projected explicit arguments correctly. The lesson is not to weaken
the canonical internal model. Keep the canonical substrate expressive, but keep
the ordinary model-facing composition entry small and host-friendly.

### pi-coding-agent

The inspected pi-coding-agent runtime supports multiple tool calls from one
assistant turn. Independent calls can run concurrently, while a tool can request
sequential execution. Its file mutation queue adds a more specific resource rule:
mutations of the same canonical file are serialized while different files need
not share that lock.

The useful ideas are:

- concurrency is an explicit runtime/tool contract rather than a guess from tool
  names;
- a conservative sequential tool can fence a batch when necessary;
- serialization can eventually be scoped to the resource that actually carries
  the race, instead of forcing all unrelated work through one global lock.

WebCodex should adopt the first principle before attempting generalized resource
locking. Resource-level concurrency should be added only for concrete mutation
cases whose authority and race semantics are already understood.

### Codex code and Code Mode

The inspected Codex code has two relevant layers. Its normal tool runtime can
allow multiple compatible tool calls to execute concurrently, with handlers
explicitly opting into parallel execution. In addition, Code Mode exposes a
restricted JavaScript orchestration environment where model-written code can call
canonical tools and can use ordinary control flow or `Promise.all(...)` to compose
them.

The important boundary is that the orchestration code itself has no ambient Node
filesystem or network authority. Effects occur through `tools.*` calls, which
return to the existing canonical tool router. Code therefore expresses
orchestration while tools continue to own authority and effects. Recursive Code
Mode invocation is also excluded.

That shape is more attractive for WebCodex than a large user-facing JSON DAG:
code can express dependencies and parallel branches compactly, while the Server
can keep a small outer MCP schema and a closed nested-tool boundary.

## Combined design principles

A WebCodex composition layer should follow these rules:

1. **One outer call may contain many canonical child invocations, but it must not
   collapse their identities.** Child authority, audit, validation, Job state,
   recovery evidence, and result semantics remain those of the existing tools.
2. **Orchestration never becomes authority.** The parent call supplies the same
   authenticated principal and trusted adapter context; every child still passes
   its ordinary scope, Project, permission, and governance checks.
3. **Code has no ambient effects.** No filesystem, network, process, environment,
   dynamic import, or host API is available except through an explicitly exposed
   `tools` object.
4. **No recursive composition.** A composition program cannot invoke the
   composition tool itself, directly or through adaptive discovery.
5. **Parallelism is opt-in and bounded.** A tool definition or adjacent canonical
   registry owns its concurrency policy. Unknown tools are sequential or denied,
   never optimistically parallel.
6. **Adaptive discovery is not bypassed.** Composition must not become a backdoor
   for model-hidden, gateway-only, or otherwise non-admitted tools. The first
   version should use a closed direct-tool allowlist; any later dynamic admission
   must consume the same canonical discovery policy as direct invocation.
7. **The parent is not a transaction.** If one child effect succeeds and a later
   child fails, WebCodex does not invent rollback. Existing child result truth
   remains authoritative.
8. **Output and execution are bounded independently.** Program size, child-call
   count, concurrent child count, wall time, emitted bytes, and retained child
   result bytes all need explicit ceilings.
9. **Window and Workflow Session semantics stay explicit.** A `ClientWindow`
   remains observation/correlation only. Composition never supplies sticky
   recorder behavior or infers a Workflow Session from a Window.
10. **Direct tools remain first-class.** Composition is an optimization for a
    known plan, not a requirement for ordinary single-tool work.

## Proposed shape

Conceptually:

```text
ChatGPT / model host
        |
        | one outer MCP request
        v
compose_tools / code-mode entry
        |
        v
bounded orchestration runtime
        |
        +--> canonical child invocation A --+
        +--> canonical child invocation B --+--> existing ToolRuntime
        +--> canonical child invocation C --+
                         |
                         v
                 existing Runner / Job /
                 Session / audit machinery
```

A model-facing program could eventually look like:

```javascript
const [review, matches] = await Promise.all([
  tools.git_review_summary({ project: "agent:special:webcodex" }),
  tools.search_project_texts({
    project: "agent:special:webcodex",
    queries: [{ pattern: "ToolConcurrencyPolicy", pattern_mode: "literal" }]
  })
]);

emit(review);
emit(matches);
```

The exact language/runtime is deliberately undecided. Restricted JavaScript is
the leading ergonomic shape because it naturally represents dependencies,
branching, `Promise.all`, and result inspection, but an embedded runtime should
not be selected until portability, startup cost, memory bounds, cancellation, and
sandbox guarantees are measured. An internal test representation may use a
structured plan; that does not imply shipping a generic JSON DAG as the ordinary
model-facing contract.

## Canonical nested invocation boundary

The composition runtime should call one internal canonical dispatch entry rather
than invoke handlers directly. That entry must receive trusted context from the
outer request and then perform the same policy path as a direct model call:

```text
nested tool name + typed arguments
        |
        +--> admission / model-surface allowlist
        +--> canonical ToolCall parsing
        +--> auth + scope checks
        +--> exact Project resolution
        +--> permission / specialized governance
        +--> normal tool dispatch
        +--> Job / validation / audit handling
        +--> bounded ToolResult
```

Each child needs an independent logical invocation identity. A parent composition
correlation id may connect those children for diagnostics, but it is not an
idempotency key, Job id, Session id, permission token, or replacement for the
child invocation identity.

Specialized gateways should be excluded from the first version. If they are ever
admitted, they must continue through their existing action-specific governance
boundary rather than becoming generic nested callbacks.

## Concurrency contract

The first useful runtime contract can remain small:

```text
ToolConcurrencyPolicy::Parallel
ToolConcurrencyPolicy::Sequential
CompositionPolicy::Allowed | Denied
```

The default should be denied or sequential until a tool is reviewed. Read-only,
independent inspection tools are the first candidates for `Parallel`. Mutation,
Session-management, publication, release, Git-index/worktree mutation, and other
shared-state tools stay sequential initially.

A later, evidence-driven extension may add a resource key such as:

```text
file:<canonical project + path>
git:<canonical project/worktree>
job:<job id>
```

That would allow, for example, two proven-independent file mutations while still
serializing two mutations of the same file. This should not become a generic lock
framework before concrete mutation dogfood requires it.

Even parallel-eligible tools need a composition-wide concurrency cap. Parallelism
must improve latency without turning one model call into unbounded Runner/process
fan-out.

## Failure, cancellation, and Job semantics

Child business failures should remain ordinary child results. The orchestration
language can choose fail-fast behavior (`Promise.all`) or explicit isolation
(`Promise.allSettled`) without rewriting already-completed child truth.

The composition runtime itself may fail for invalid code, an unavailable nested
tool, resource-budget exhaustion, runtime exception, cancellation, or a hard
orchestration timeout. Such a parent failure must not claim that already-entered
child effects did not occur.

Long-running child execution needs special care. Existing WebCodex Jobs already
provide the durable same-execution handoff path, so composition should not invent
a second background-process or cell lifecycle. A nested tool may eventually
return its normal `job_id`; the parent can emit that result and the model can use
ordinary Job observation. The first implementation can avoid long-running
children until this projection is proven.

A Server restart should not attempt to resume arbitrary process-local
orchestration code. Canonical child effects keep their existing recovery truth.
This is another reason not to make the composition program itself a new durable
workflow resource.

## Workflow Session recording

This is the main reason to stage composition conservatively.

An explicit authorized outer `recording_session_id` may be propagated as trusted
recorder context to admitted child calls, but each child must record through the
same existing Session path it would use directly. The composition wrapper must
not become a second authoritative business event that double-counts the children.

Read-only/re-observable children are a suitable first slice because they do not
usually advance the Session context checkpoint. Consequential child tools are
harder: several children may independently advance `context_revision`, produce
permission evidence, or create Jobs while the host receives only one parent
response. Before mutation or consequential execution is admitted, WebCodex needs
a deterministic rule for the final parent Session continuity projection and for
intermediate child revisions. It must preserve the existing monotonic revision,
ACK, recovery, and message semantics rather than compressing them into a fake
single child result.

Composition must never use Window affinity or recorder-gap hints to fill in a
missing recorder. The Window work remains diagnostic only.

## Window activity and audit visibility

Composition should reduce outer MCP calls without making the new Windows view
opaque. At minimum the operator must be able to distinguish:

```text
1 outer model-facing composition call
N canonical nested tool invocations
M Runner/process requests
```

The current Window event can remain the outer request boundary, but the design
needs bounded child evidence: for example a parent trace id plus child invocation
ids and safe child tool names. Arguments, outputs, command bodies, patches, raw
host Window ids, and credentials must not be copied into ActionAudit merely for
composition diagnostics.

This also gives performance telemetry a clean accounting model. Future timing
should distinguish, where the existing layers can prove it:

```text
outer MCP request duration
Server admission / dispatch time
nested child duration
Runner request/queue round trip
actual child-process duration
response projection time
```

The interval between a completed WebCodex response and the next inbound request
is outside WebCodex. It may be model inference, host scheduling, user delay, UI
behavior, or something else and must not be labeled as model reasoning without
host evidence.

## Staged implementation plan

### Phase 0 — Measurement and contract inventory

- Use Window/server-trace correlation to measure model-facing outer-call counts,
  tool durations, Runner timing available today, and gaps outside WebCodex.
- Classify existing direct tools by composition eligibility and concurrency safety.
- Identify which current ToolDefinition fields can own the policy without a
  parallel registry.
- Define parent/child diagnostic identity and bounded audit projection before
  changing execution behavior.
- Establish representative review/implementation traces to compare outer MCP
  calls, canonical child calls, Runner calls, and wall time separately.

Success means we can explain where elapsed time is spent without claiming access
to model-private state.

### Phase 1 — Read-only bounded composition prototype

Expose one experimental operator tool with a simple outer schema and a restricted
orchestration program. Start with a closed allowlist of independent inspection
operations such as multi-file reads, source search, Git review/diff inspection,
and other already-direct read-only coding tools.

Initial hard rules:

- no recursive composition;
- no file/Git/Session mutation;
- no generic shell/process execution;
- no plugin/MCP/SSH gateway calls;
- no adaptive long-tail bypass;
- small program-size and child-call limits;
- small parallelism cap;
- shared wall-clock and serialized-output ceilings;
- every child uses canonical ToolRuntime dispatch.

Synthetic delayed-tool tests should prove that independent children actually run
concurrently while a sequential child fences execution as designed. Authority
negative tests should prove that composition cannot call a child the caller could
not invoke directly.

### Phase 2 — Review dogfood and latency comparison

Dogfood the read-only slice on real branch review. Compare:

```text
outer model/tool round trips
canonical tool invocations
Runner requests
WebCodex-owned wall time
end-to-end task wall time
findings / validation quality
```

The target is not fewer canonical facts. The target is fewer *outer model-facing
round trips* for evidence that was already known to be independent.

Keep direct-tool traces as the control. If composition does not materially reduce
latency or makes model behavior less reliable, stop before adding mutation.

### Phase 3 — Structured execution and Job-aware composition

After read-only dogfood is stable, consider bounded structured process/validation
children. Preserve existing same-execution Job handoff and observation rather than
creating a new composition-specific background lifecycle. Define cancellation and
parent timeout behavior before allowing parallel long-running executions.

This phase should also prove that a child Job remains observable and recoverable
after the parent composition request has returned.

### Phase 4 — Guarded mutation, only if justified

Mutation should be last. Before enabling it, close:

- Workflow Session context-revision projection for multiple consequential child
  calls;
- resource-level serialization for file and Git/worktree mutation;
- partial-success/failure presentation with no rollback fiction;
- stale SHA/context recovery per child;
- permission evidence and audit ordering;
- parent retry behavior after an uncertain response.

Start with independent guarded file edits only if dogfood shows a substantial
round-trip benefit beyond existing `apply_text_edits` batching. Git publication,
Session messaging, release/deploy actions, and heterogeneous gateways should stay
outside until they have their own demonstrated composition need.

## What not to build

This work should not become:

- a generic durable workflow/DAG engine;
- a new Session or Agent lifecycle;
- a replacement for Jobs;
- a shell-script wrapper marketed as typed composition;
- a way to bypass adaptive discovery or hidden-tool policy;
- a new permission evaluator;
- an implicit sticky recorder keyed by Window;
- an automatic retry engine;
- a transaction/rollback abstraction over unrelated tools;
- an unbounded parallel task runner.

WebCodex already has the canonical primitives. The composition layer should stay
thin enough that deleting it would leave the underlying tools and their direct
semantics intact.

## Acceptance criteria

A production-ready first slice should satisfy all of the following:

- one outer MCP request can execute multiple explicitly independent canonical
  inspection tools;
- each child sees the same auth/Project/permission rules as direct invocation;
- composition cannot reach tools outside its current admitted allowlist;
- child invocation/audit identity remains observable and bounded;
- concurrent read-only children demonstrate wall-time benefit under controlled
  delay tests;
- sequential policy reliably prevents unsafe overlap;
- total child count, concurrency, program size, wall time, and output are bounded;
- cancellation/timeout never fabricates rollback or "no effect" truth;
- Window activity can still explain outer versus nested WebCodex work without
  exposing raw host identity or payload bodies;
- direct tools continue to work unchanged;
- no Workflow Session is selected from Window identity;
- no mutation support ships until multi-child Session/Job/recovery semantics are
  explicitly proven.

## Open design questions

1. Which embedded restricted-JavaScript runtime, if any, meets Linux/macOS/Windows
   portability, startup, memory, cancellation, and sandbox requirements?
2. Should the model-facing API expose named `tools.<name>(args)` methods, a single
   `tools.call(name, args)` primitive, or generated helpers over the currently
   admitted direct surface?
3. How should nested tool schemas be made ergonomic without recreating the large
   composed-schema projection problem observed in WebCodex Next?
4. What is the smallest canonical concurrency metadata that supports Phase 1
   without prematurely designing resource locks for mutation?
5. How should parent/child invocation identities appear in ActionAudit and the
   Windows view while preserving current privacy policy?
6. What final Session continuity projection is correct once one parent response
   contains several consequential child revisions?
7. How should a parent request report a mixture of synchronous results and child
   Jobs without creating a second Job lifecycle?
8. Which timing boundaries can the current Runner protocol prove directly, and
   which require additive privacy-safe telemetry?

These questions should be answered with focused prototypes and dogfood traces,
not by widening the first implementation preemptively.
