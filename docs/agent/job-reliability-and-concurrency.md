# Job Reliability, Observation, and Runner Concurrency

This note defines the current V1 engineering contract for Job continuation across
Control Server restarts, bounded Job observation, and shared Runner execution
capacity. It complements [Runner](../RUNNER.md), [Testing](../TESTING.md), and
the [architecture decisions](architecture-decisions.md).

The purpose is to keep implementation, operator diagnosis, and model-facing tool
descriptions aligned. It is not a new scheduler, persistence layer, or retry
framework.

## 1. Keep request lifetime, Job lifetime, and observation state separate

Three identities have different lifetimes:

| Thing | Meaning | Expected across Control Server restart |
|---|---|---|
| MCP / HTTP request | One transport request or bounded wait | No. The connection/request may fail immediately. |
| `job_id` | Identity of one already-dispatched execution | Active: when the same reconciliation-capable Runner process survives and reports inventory. Terminal public ordinary Jobs: also via the Server receipt within its original bounded retention window. |
| `after_observation_token` | Opaque lifecycle and bounded log-delta state for one observed Job snapshot | No. Its Server epoch is process-local; a surviving Job should return a reset baseline and fresh token immediately after restart. |

A dropped `observe_jobs`, `job_tail`, or other observation request therefore does **not**
mean that the underlying Job was lost. The caller should keep the original
`job_id` and observe authoritative Job state again before considering any retry.

Observation tokens are opaque. They must be returned unchanged by clients and
must never become business identity, retry identity, authorization, or evidence
that an execution no longer exists.

The first log observation returns a bounded current baseline. A later
cursor-aware token returns only newly observed stdout/stderr when continuity is
provable, or empty tails when only lifecycle metadata changed. `reset` means
continuity could not be proved (for example, retained logs advanced past the
token or the Server epoch changed), so the response contains a bounded recovery
tail and a new current token. One conservative repeat after a reset is expected;
silently assuming missing output is not.

## 2. Control Server restart recovery contract

When `job_state_reconciliation=true`, the intended recovery chain is:

1. A Job is accepted and dispatched to a Runner.
2. The Control Server stops or restarts while the Runner process and command keep
   running.
3. The same Runner process reconnects with the same `client_id` and
   `agent_instance_id` and supplies a complete active Job inventory.
4. The new Server registry validates that inventory and reconstructs the same
   `job_id` and ownership/project/session context.
5. An observation request carrying a token from the old Server epoch refreshes
   immediately rather than waiting for a new command-side event.
6. Later terminal state remains queryable through the same Job identity, subject
   to the normal bounded terminal-retention contract.

The Server-side reconstruction path records
`recovered_after_server_restart=true` and
`recovery_reason_code=server_restart_reconciliation`. Runner inventory is the
recovery authority; the Server must not guess a replacement execution or replay
the original command.

A command that finishes while the Server is down is also recoverable when its
terminal snapshot is still in the Runner's bounded retained inventory.

The production Server also hydrates accepted public ordinary terminal receipts
from `wc_job_receipts` before accepting traffic. Receipt writes happen after the
registry lock is released and cannot change a terminal verdict. The receipt
reuses the safe Job snapshot, excludes executable validation metadata, and fixes
`terminal_observed_at` / `expires_at` at the first accepted terminal observation.
SQLite retains at most 64 receipts per logical Runner for 15 minutes. Expired
receipts are pruned on database open, writes, reads, and the existing recovery
sweep. Historical owner attribution is independent of replacement registration.
A new observation epoch resets old tokens without granting execution authority.

Only Server-admitted Jobs with proven public visibility are receipt candidates.
Inventory-only reconstruction retains its existing reconciliation behavior; it
cannot prove whether an unknown Job was previously a hidden synchronous result,
so it does not independently create a durable receipt. Receipt hydration never
creates a pending request, execution mapping, waiter, or stop/retry/adopt lease.

### What is expected and what is a bug

Expected:

- the in-flight MCP/HTTP request fails because the Server process restarted;
- an old observation token becomes stale and is replaced by a fresh token;
- a Job waits as `agent_queued` when Runner execution capacity is full;
- a legacy Runner without reconciliation support cannot provide this recovery
  guarantee;
- a new Runner process is outside this recovery contract.

A V1 correctness/reliability incident exists when all of the following are true:

- the Job had been dispatched and was active (or retained terminal) before the
  Server restart;
- the Runner process survived: the same `client_id` still reports the same
  process-scoped `agent_instance_id`; reconciliation logs may use
  `process_started_at` as a secondary cross-check;
- the Runner advertises `job_state_reconciliation=true`;
- the Job is present in the Runner inventory supplied after reconnect, or should
  have been present under the complete-active-inventory contract;
- after successful re-registration the same `job_id` is permanently reported as
  unknown or otherwise requires launching a replacement execution.

The important distinction is **request loss versus execution loss**. Treating
both as “retry the command” risks duplicate effects.

## 3. Diagnostic playbook for an `unknown job` after restart

Before retrying work, collect safe runtime facts:

1. Use `runtime_status` / `list_runners` to establish the current Server build,
   Runner connection state, `client_id`, process-scoped `agent_instance_id`,
   reconciliation capability, and Job concurrency state. If reconciliation logs
   are available, cross-check `process_started_at` there; it is not part of the
   current `runtime_status` / `list_runners` projection.
2. Determine whether the Runner process changed. If it changed, do not claim the
   same-process Server-restart recovery contract was violated.
3. If the Runner process is unchanged, inspect the registration/reconciliation
   path: was the original `job_id` present in `job_inventory`?
4. If it was present, verify that `reconcile_inventory_locked` reconstructed or
   updated the Server record instead of dropping it.
5. If it was absent, investigate Runner `JobManager` retention/inventory rather
   than creating a replacement Server Job.
6. Re-observe the original `job_id`. A stale observation epoch should cause an
   immediate token refresh when the Job exists.
7. Only after authoritative lifecycle evidence establishes a safe retry state
   should a caller create a new execution.

Useful reconciliation diagnostics should remain bounded and secret-free. A
summary such as runner instance, active/terminal inventory counts, reconstructed
count, updated count, and missing count is sufficient; command text, log bodies,
credentials, and private paths are not required.

## 4. Runner Job capacity is shared across windows and projects

`max_concurrent_jobs` is a Runner-process execution limit (default 4, valid
range 1..64; out-of-range configuration is rejected). It is not allocated per
ChatGPT window, Workflow Session, or Project.

Opening multiple model windows consumes no Job slot by itself. A slot is consumed
while a Job-backed execution owns Runner execution capacity. Therefore several
windows using different Projects on the same Runner can contend for the same
pool.

For example, with `max_concurrent_jobs = 4`:

- Window A running a build: 1 slot;
- Window B running tests: 1 slot;
- Window C running a long structured process: 1 slot;
- one additional Job may run immediately;
- later accepted Jobs remain the same Jobs and report `agent_queued` until a slot
  is available.

`agent_queued` is not a reason to create another Job. The queued record keeps its
original `job_id`, enters the Runner's complete active inventory, and should also
survive a Control Server restart under the same reconciliation contract.

A structured process or validation may consume a Runner Job slot while it runs
even when it finishes quickly enough for the initiating tool call to return a
terminal result instead of exposing a long-lived handoff to the model.

### Independent concurrency planes

Do not conflate the Job execution pool with other limits. In particular:

- Runner Job execution uses `max_concurrent_jobs`;
- polling request dispatch has its own in-flight bound;
- persistent shells have their own bounded population/lifecycle.

Changing one does not redefine the others. `runtime_status` / `list_runners`
should be used for current `job_concurrency { limit, running, queued }` facts
instead of inferring capacity from the number of browser/model windows. These
are bounded lifecycle-status counts, not an exact free-slot calculation:
`stop_requested` can still own a Runner slot until it becomes terminal, so do
not derive `available_slots` or saturation by subtracting `running` from `limit`.

## 5. Requirements for model-facing tool descriptions

Tool descriptions are part of the reliability contract because they influence
whether a model observes an existing execution or accidentally creates another
one. Keep descriptions concise, but preserve these semantic distinctions.

### Description density and discovery hygiene

The top-level tool description is primarily a **selection surface**, not a mini
reference manual. It should answer what the tool does, when it wins over nearby
choices, and any lifecycle fact that changes retry safety. Put detailed numeric
bounds, wire rules, and field-specific behavior on the relevant input/output
schema instead of repeating them in every top-level description.

For ordinary tools, keep the top-level description as short as its selection and
lifecycle semantics allow. There is no secondary numeric density limit below the
repository hard ceiling (`MODEL_TOOL_DESCRIPTION_MAX_CHARS`, currently 900);
using more of that budget is appropriate when it preserves selection, authority,
retry, continuation, uncertainty, safety, or recovery semantics. Avoid naming
sibling tools merely to restate implementation or fallback details, because
exact-name discovery may otherwise retrieve unrelated tools whose descriptions
happen to mention the queried name. Prefer capability phrasing such as “shell
command tool”, “structured validation”, or “asynchronous execution” unless the
sibling tool name is itself needed to choose correctly.

Generic lifecycle words such as `Job` should be concentrated on actual Job
creation/observation tools. Structured validators and process adapters can say
that long work continues as the **same execution** and returns `job_id`, while
the timeout/output schema carries the detailed handoff contract. This keeps the
retry guarantee without turning every validation description into a Job search
hit.

### Job-producing execution tools

For structured validation/process tools (`cargo_*`, `go_test`, `run_process`,
`run_script`, `run_job`, and future equivalents), the combined model-facing tool
description plus lifecycle input/output schema should make clear that:

- a long operation continues as the **same execution / same Job**;
- handoff is not cancel-and-retry;
- queued execution keeps the same `job_id`;
- loss of the initiating request is not evidence that the Job did not start.

The top-level description normally carries only the selection-critical part of
that contract, such as “same execution” or a stable `job_id`; field descriptions
carry the detailed handoff and lifecycle rules. Avoid wording anywhere in the
model-facing schema that encourages “rerun if the call times out” without
consulting structured lifecycle state.

### Job observation tools

For `job_tail` and `observe_jobs`, the top-level description plus
observation-field schemas should make clear that:

- observation never launches or retries the Job;
- `wait_secs` is one bounded wait, not a subscription;
- `after_observation_token` is an opaque observation cursor, not Job identity;
- first log observation is a bounded baseline and cursor-aware follow-ups are
  delta-only when continuity is provable;
- an unterminated final line is not conclusively consumed; a follow-up may
  conservatively repeat that bounded partial line until a line boundary is observed;
- `reset` is a bounded recovery refresh, not proof that no intervening output
  existed;
- Control Server restart may invalidate the token while leaving `job_id` valid;
- a stale Server epoch should refresh immediately when the same Job has been
  reconciled;
- `unknown_job` after same-process reconciliation is a diagnostic signal, not an
  automatic instruction to create a replacement Job.

### Runtime/operator observation tools

Descriptions for `runtime_status`, `list_runners`, and related operator surfaces
should distinguish connection health from execution capacity and expose safe
facts needed to diagnose recovery:

- Runner process identity/liveness;
- reconciliation capability;
- current Job concurrency limit/running/queued counts;
- Server build/version compatibility where relevant.

Do not imply that a healthy transport proves Job recovery succeeded, or that an
open model window reserves Runner capacity.

## 6. Acceptance coverage

The minimum real-process acceptance scenario for this contract is:

```text
running Runner Job
-> stop Control Server only
-> keep Runner process and command alive
-> restart Control Server
-> Runner re-registers complete inventory
-> same job_id is reconstructed
-> old observation token refreshes immediately
-> command runs exactly once
-> terminal result remains queryable
```

A second scenario should allow the command to become terminal while the Server is
down and verify terminal reconciliation after restart. A repeated-restart
scenario should preserve the same Job identity, monotonic sequence/log cursors,
and single execution.

These expectations already have dedicated coverage in
`docs/TESTING.md` (`e2e_job_reconciliation_ws.sh` and
`e2e_job_recovery_failures_ws.sh`). Unit coverage also verifies old-epoch
observation-token refresh behavior.

## 7. Runner-process restart boundary

Ordinary Jobs are still process-owned by their exact Runner instance. A Runner
process restart therefore makes those child processes unrecoverable and they
converge to `lost`; Server-side inventory reconciliation must never infer native
ownership for them.

`run_detached_process` is the explicit exception. Before payload start it makes
a one-shot durable ownership handoff to a narrow supervisor. The replacement
Runner may transfer the same logical Job to a new `agent_instance_id` only when
bounded durable state, request/context identity, supervisor native start
identity, and lifetime fencing all reconcile exactly. A lost initiating response
is replay-safe only while that logical Job remains in active or retained terminal
history; replay keys are not permanent tombstones after retention expires.

This contract still does not promise:

- survival of an in-flight MCP/HTTP connection across a Control Server process
  restart;
- survival of detached execution across a machine reboot;
- unbounded Job/log or idempotency-key retention;
- a generic distributed scheduler or general child-detach API;
- blind automatic retries for uncertain execution outcomes.

## 8. Implementation reference points

The current contract is implemented and tested primarily in:

- `src/shell_client/state.rs` — Server Job record, observation epoch, and Runner
  concurrency metadata;
- `src/shell_client/reconciliation.rs` — inventory validation and reconstruction;
- `src/tool_runtime/observe_jobs.rs` and `src/job_observation.rs` — bounded
  observation/token behavior;
- `crates/webcodex-runner/src/main.rs` — Runner `JobManager`, inventory, queue,
  and slot reservation;
- `docs/RUNNER.md` — public Job/concurrency behavior;
- `docs/TESTING.md` — real-process restart/reconciliation acceptance coverage.
