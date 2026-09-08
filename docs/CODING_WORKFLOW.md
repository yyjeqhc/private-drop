# Coding Workflow

[English](CODING_WORKFLOW.md) | [简体中文](CODING_WORKFLOW.zh-CN.md)

This guide is for ordinary WebCodex coding/review work. It describes the model-facing workflow, not the internal continuity, audit, or transport protocols used to implement it.

## Normal loop

The ordinary WebCodex coding loop is intentionally small:

```text
work_on_project
→ inspect / search / read
→ edit
→ focused validation
→ review changes
→ finish_coding_task
```

`work_on_project` is the canonical bootstrap for normal coding and review. Give it the current task instruction and then follow the project instructions and tools returned by the connected Server.

## Start or continue a task

Use `work_on_project` for both a new coding task and an explicit continuation. WebCodex keeps bounded Workflow Session evidence so validation, review, and handoff can refer to the same unit of work, but that Session is not an authentication credential and does not widen project access.

For ordinary use you do not need to reason about WebCodex's internal continuity or audit fields. Those are implementation/maintainer contracts.

The built-in workflow includes default guidance for every task, even when no
role is named: inspect the target and applicable rules, preserve existing work,
complete authorized implementation, validate proportionally, observe existing
Jobs instead of duplicating effects, and report evidence honestly. Use only
tools and protocol fields supported by the current exposed schemas.

Behavioral roles add emphasis to those defaults. They are expressed in the task
instruction, not through a separate authority mechanism. For example:

```text
Use the implementation_owner guidance. Implement <task>, run focused validation,
and review the resulting diff.
```

For an independent review:

```text
Use the independent_review guidance. Review <change or commit> independently,
report concrete findings with file/line evidence and impact, and do not edit.
```

To request corrections as well, explicitly add “fix concrete findings and run
focused regression validation.” Naming a review role alone does not authorize edits.

Guidance is delivered in tool results; it is not the client's system prompt and
does not grant execution authority. Host instructions, the user's task,
applicable project rules, authentication, and runtime safety policy still apply.
Delivery is not proof that a model read, retained, or followed the guidance.
Keep guidance enabled unless the current model context already retains it.

## Inspect before editing

Prefer structured project search/read tools over shell commands when they express the task. Read only the files and ranges needed to understand the change, and preserve unrelated work already present in the workspace.

The bootstrap reads a fixed set of instruction entry points; it does not scan
every subdirectory for rules. Before changing a path, inspect applicable nested
instructions and recover any relevant missing or truncated rule content.

For branch/PR review, start with the bounded review/change-summary tools exposed by the current Server, then narrow to targeted reads or diff hunks when needed.

## Editing

Use `apply_text_edits` as the canonical default model-generated editing path after `read_file`/`read_files` when current file content and SHA are available. Pass each existing file's returned SHA as `expected_sha256`; exact selectors are unique by default, with `line_scope`/`occurrence` available for intentional disambiguation. This remains the normal path even when many lines change; line count alone is not a reason to choose `apply_patch`. Use `apply_patch` only when contextual patching is materially more natural, a large/multi-hunk rewrite is awkward to express as guarded exact edits, or patch-style context itself expresses the change relationship more clearly. For repetitive code, each patch chunk needs stable, unique surrounding context such as the containing function, impl, type, test, or module; do not use a repeated single line or short fragment as the mutation anchor. The default `matching_mode=unique` still requires one mutation target. Use `matching_mode=exact_unique` only as an explicit stale-context/concurrency fence after reading exact current source. Use `apply_unified_diff` only when the input is already a standard unified diff.

Guard failures are **zero-write conflicts**, not reasons to weaken the guard. Re-read the current source and regenerate the intended edit against that state.

For `matching_mode_rejected`, keep the matching guard and do not switch to `first_match`. Re-read the current source. If current source plus SHA is available and the intended change is easy to express as exact edits, prefer `apply_text_edits`. If patch form is still materially clearer, consume the bounded `recovery.action=read_files` / `recovery.items`, inspect the candidate windows, add stable unique context, and regenerate a new `matching_mode=unique` patch.

For deterministic `context_mismatch`, consume the bounded `read_files` recovery and regenerate against current source; do not blindly repeat the same patch. If the result is `outcome_unknown`, inspect the workspace before deciding whether any write should be retried.

The exact matching metadata and transactional protocol are maintainer details; see the tool contract/tests when developing WebCodex itself.

## Validation

Prefer structured validation such as `cargo_test`, `cargo_check`, or `go_test` when available. Use the smallest check that can detect the regression, and broaden only when the affected boundary requires it.

When a test invocation must prove that tests actually ran, use `require_tests: true` or `min_tests: N`. These are request-scoped evidence assertions, not persistent Workflow Session requirements. If validator execution succeeds but the requested count cannot be satisfied or proven, closeout retains that invocation as an evidence gap rather than a code/test correctness failure. Otherwise, an exit-zero command that legitimately runs zero tests remains an execution result rather than proof of test coverage.

Use shell/process escape hatches only when the structured validation surface cannot express the check.

## Review and closeout

Review the actual workspace/diff after editing and validation. Passing tests do not replace diff review, and a clean diff does not replace focused validation when behavior changed.

`finish_coding_task` returns a bounded evidence summary for closeout. Treat it as advisory evidence, not as a decision that the work is correct or complete. The model still makes the final engineering judgment and reports the result to the user.

## Long-running work

A command or validation that outlives the synchronous grace period continues as the same WebCodex Job. Observe that Job rather than starting another copy. Recovery/continuation hints returned by a tool are guidance for the next explicit call; WebCodex does not silently retry an uncertain effect.

## Manual multi-window collaboration

Multi-window coordination is an advanced maintainer workflow, not part of the ordinary coding loop. Keep independent writers in separate worktrees/Projects and keep their Workflow Sessions separate. Use the assignment/completion tools returned by the current Server rather than copying another window's execution history.

The exact concurrency, retry, provenance, and cross-Session authorization rules are documented in [Manual Multi-Window Collaboration](agent/manual-window-collaboration.md). Their protocol fields are intentionally omitted here.

## Assessing effectiveness

Runtime tests check that guidance is delivered consistently, remains bounded,
matches its schema, and never becomes authority. The scripted
`scripts/eval_coding_loop.sh` checks tool-loop mechanics; it does not run a model
or measure instruction following.

To measure behavioral benefit, compare the same model, tools, settings, and task
fixtures with and without guidance over repeated runs. Include a small fix, a
review-only task, existing unrelated changes, nested instructions, and an
uncertain long-running effect. Compare correctness and scope preservation first,
then unnecessary clarification, duplicate execution, validation quality, tool
calls, and token cost. Do not infer a success-rate improvement from schema tests.

## Internal protocol details

When developing WebCodex itself, use the maintainer contracts rather than expanding this user guide:

- [Session model](agent/session-model.md) — Workflow Session continuity, messages, and evidence semantics.
- [Authority model](agent/permission-model.md) — execution authority and hard-safety layering.
- [Job reliability and concurrency](agent/job-reliability-and-concurrency.md) — Job recovery/observation contracts.
- [Architecture decisions](agent/architecture-decisions.md) — standing implementation decisions.

Ordinary coding clients should not need those documents to complete normal repository work.
