use serde_json::{json, Value};
use webcodex_core::workflow_session_contract::MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS;

use super::super::input_schemas::{
    session_execution_context_schema, session_guards_schema, session_lifecycle_schema,
    session_mode_schema,
};
use super::common::{
    array_schema, cargo_test_count_assertion_schema, continuation_feedback_schema,
    evidence_history_schema, evidence_integrity_schema, handoff_brief_schema,
    job_lifecycle_summary_schema, nullable_schema, open_object_schema, permission_summary_schema,
    schema_type, task_outcome_schema, validation_delta_schema, wrapped_output_schema,
};

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "start_session" => Some(wrapped_output_schema(vec![
            (
                "success",
                schema_type(
                    "boolean",
                    "True after the in-memory Session context/event commit. JSON ledger persistence may still be pending in the background writer.",
                ),
            ),
            ("session_id", schema_type("string", "Opaque session id.")),
            (
                "project",
                nullable_schema("string", "Optional project associated with the task."),
            ),
            (
                "project_input",
                nullable_schema("string", "Original project input, when provided."),
            ),
            (
                "resolved_project",
                nullable_schema(
                    "string",
                    "Resolved full runtime project id, when a project was provided.",
                ),
            ),
            (
                "title",
                nullable_schema("string", "Optional session title."),
            ),
            ("mode", session_mode_schema("Effective session mode.")),
            (
                "guards",
                session_guards_schema("Effective task guard settings for this session."),
            ),
            (
                "execution_context",
                session_execution_context_schema(
                    "Persistent execution defaults for this Workflow Session.",
                ),
            ),
            (
                "lifecycle",
                session_lifecycle_schema(
                    "Workflow session lifecycle. Create returns active; close_session transitions to closed.",
                ),
            ),
            (
                "created_at",
                schema_type("integer", "Unix timestamp in seconds."),
            ),
            (
                "project_instructions",
                nullable_schema(
                    "object",
                    "Best-effort project-local instruction files loaded at session start (e.g. AGENTS.md). null when no project was provided. Project-local guidance only; does not override system/platform/WebCodex safety policy.",
                ),
            ),
        ])),
        "session_summary" => Some(wrapped_output_schema(vec![
            ("session_id", schema_type("string", "Opaque session id.")),
            (
                "project",
                nullable_schema("string", "Optional project associated with the task."),
            ),
            (
                "title",
                nullable_schema("string", "Optional session title."),
            ),
            ("mode", session_mode_schema("Effective session mode.")),
            (
                "guards",
                session_guards_schema("Effective task guard settings for this session."),
            ),
            (
                "execution_context",
                session_execution_context_schema(
                    "Persistent execution defaults for this Workflow Session.",
                ),
            ),
            (
                "lifecycle",
                session_lifecycle_schema(
                    "Workflow session lifecycle. Missing on pre-lifecycle ledgers is treated as active on load; closed after explicit close_session.",
                ),
            ),
            (
                "created_at",
                schema_type("integer", "Unix timestamp in seconds."),
            ),
            (
                "updated_at",
                schema_type("integer", "Unix timestamp in seconds."),
            ),
            ("counts", open_object_schema("Structured event counters.")),
            (
                "events",
                array_schema(open_object_schema("Bounded session event."), "Recent events."),
            ),
            (
                "messages",
                open_object_schema("Bounded session message-board summary: counts plus at most five recent progress messages; never the full message queue."),
            ),
            (
                "project_instructions",
                nullable_schema(
                    "object",
                    "Summary-only projection of project-local instructions loaded at session start (no content bodies). Present when the session was created with a project. Project-local guidance only; does not override system/platform/WebCodex safety policy.",
                ),
            ),
        ])),
        "update_session_context" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Explicit Workflow Session id that was updated."),
            ),
            (
                "project",
                schema_type("string", "Canonical authorized project scoped to this Workflow Session."),
            ),
            (
                "title",
                nullable_schema("string", "Optional Workflow Session title."),
            ),
            ("mode", session_mode_schema("Effective session mode.")),
            (
                "guards",
                session_guards_schema("Effective task guards."),
            ),
            (
                "lifecycle",
                session_lifecycle_schema("Lifecycle after update; always active on success."),
            ),
            (
                "execution_context",
                session_execution_context_schema(
                    "Complete current in-memory execution defaults after replacement.",
                ),
            ),
            (
                "previous_execution_context",
                session_execution_context_schema(
                    "Complete in-memory execution defaults immediately before replacement.",
                ),
            ),
            (
                "changed",
                schema_type("boolean", "Whether the stored context changed."),
            ),
            (
                "created_at",
                schema_type("integer", "Unix timestamp when the Session was created."),
            ),
            (
                "updated_at",
                schema_type("integer", "Unix timestamp of the session's last update."),
            ),
        ])),
        "close_session" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Explicit wc_sess_* id that was closed."),
            ),
            (
                "lifecycle",
                session_lifecycle_schema("Lifecycle after close; always closed on success."),
            ),
            (
                "already_closed",
                schema_type(
                    "boolean",
                    "True when the session was already closed; no new transition event was recorded.",
                ),
            ),
            (
                "updated_at",
                schema_type("integer", "Unix timestamp of the session's last update."),
            ),
        ])),
        "validation_summary" => Some(validation_summary_tool_output_schema()),
        "post_session_message" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Business session id whose message board was updated."),
            ),
            (
                "message_id",
                schema_type("string", "Created wc_msg_* message id."),
            ),
            ("message", open_object_schema("Created session message.")),
        ])),
        "list_session_messages" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Business session id whose messages were listed."),
            ),
            (
                "messages",
                array_schema(
                    open_object_schema("Session message."),
                    "Newest-first messages matching the filters.",
                ),
            ),
        ])),
        "get_session_assignment" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            ("session_id", schema_type("string", "Exact coordinator/business Session id.")),
            ("message_id", schema_type("string", "Exact open todo wc_msg_* id.")),
            ("todo", open_object_schema("Exact retained open todo.")),
            (
                "direct_replies",
                json!({
                    "type": "array",
                    "maxItems": 16,
                    "items": open_object_schema("Direct reply whose reply_to is the exact todo id."),
                    "description": "Oldest-first complete retained direct-reply set for this assignment; assignment reads fail closed instead of truncating this set."
                }),
            ),
            (
                "assignment_fence",
                json!({
                    "type": "string",
                    "minLength": 48,
                    "maxLength": 48,
                    "pattern": "^wsa1_[A-Za-z0-9_-]{43}$",
                    "description": "Deterministic Session/todo-bound semantic snapshot fence. Pass unchanged as expected_assignment_fence; it is not an observation cursor, authority token, or completion key."
                }),
            ),
        ])),
        "observe_session_messages" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success, including timeout.")),
            ("session_id", schema_type("string", "Explicit business Session id being observed.")),
            (
                "messages",
                array_schema(
                    open_object_schema("Current retained Session message state."),
                    "Ascending observation-order current states changed after the caller cursor; empty for a baseline or unchanged timeout.",
                ),
            ),
            ("observation_token", schema_type("string", "Opaque bounded Session-bound durable observation token. Return it unchanged on the next observation call.")),
            ("changed", schema_type("boolean", "Whether durable message observation revision advanced beyond the supplied token.")),
            ("wait_outcome", json!({"type": "string", "enum": ["immediate", "updated", "timeout"], "description": "Closed one-shot wait outcome; timeout remains a successful tool result."})),
            ("waited_ms", schema_type("integer", "Monotonic elapsed wait duration in milliseconds.")),
            ("history_lost", schema_type("boolean", "True when retention/sanitization means the caller cursor predates message-state history that can no longer be reconstructed completely.")),
            ("has_more", schema_type("boolean", "True when more retained changed message states remain after the returned page; the token advances only through the last returned change.")),
        ])),
        "resolve_session_message" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Business session id containing the message."),
            ),
            (
                "message_id",
                schema_type("string", "Resolved wc_msg_* message id."),
            ),
            ("message", open_object_schema("Resolved session message.")),
        ])),
        "complete_session_message" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            ("session_id", schema_type("string", "Coordinator/business Session id.")),
            ("message_id", schema_type("string", "Completed todo wc_msg_* id.")),
            ("answer_message_id", schema_type("string", "Exactly one created answer wc_msg_* id.")),
            ("completion_id", schema_type("string", "Bounded opaque durable completion identity derived from completion_key.")),
            ("replayed", schema_type("boolean", "True when an idempotent retry returned the original completion.")),
            ("todo", open_object_schema("Resolved todo with resolved_by_message_id correlation.")),
            ("answer", open_object_schema("Created answer with reply_to and trusted author_session_id when available.")),
        ])),
        "session_discussion_summary" => Some(wrapped_output_schema(vec![
            ("success", schema_type("boolean", "Always true on success.")),
            (
                "session_id",
                schema_type("string", "Business session id being summarized."),
            ),
            ("counts", open_object_schema("Structured message counts.")),
            (
                "open_guidance",
                array_schema(
                    open_object_schema("Open guidance message."),
                    "Bounded newest-first open guidance.",
                ),
            ),
            (
                "open_questions",
                array_schema(
                    open_object_schema("Open question message."),
                    "Bounded newest-first open questions.",
                ),
            ),
            (
                "open_risks",
                array_schema(
                    open_object_schema("Open risk message."),
                    "Bounded newest-first open risks.",
                ),
            ),
            (
                "open_todos",
                array_schema(
                    open_object_schema("Open todo message."),
                    "Bounded newest-first open todos.",
                ),
            ),
            (
                "high_priority_open_todos",
                array_schema(
                    open_object_schema("High-priority open todo."),
                    "Bounded high-priority open todos.",
                ),
            ),
            (
                "recent_answers",
                array_schema(
                    open_object_schema("Recent answer message."),
                    "Bounded newest-first answers.",
                ),
            ),
            (
                "recent_completions",
                array_schema(
                    open_object_schema("Structured todo-to-answer completion correlation."),
                    "Bounded recent completion correlations with worker Session provenance when available.",
                ),
            ),
            (
                "recent_progress",
                array_schema(
                    open_object_schema("Recent progress message."),
                    "Bounded newest-first progress messages.",
                ),
            ),
            (
                "recent_decisions",
                array_schema(
                    open_object_schema("Recent decision message."),
                    "Bounded newest-first decision messages.",
                ),
            ),
        ])),
        "session_handoff_summary" => Some(wrapped_output_schema(vec![
            (
                "summary_only",
                schema_type("boolean", "True only for compact summary_only output."),
            ),
            (
                "session_id",
                schema_type("string", "Business session id being handed off."),
            ),
            (
                "project",
                nullable_schema("string", "Optional runtime project id, when provided."),
            ),
            (
                "workspace_clean",
                schema_type(
                    "boolean",
                    "Compact summary_only workspace cleanliness verdict.",
                ),
            ),
            (
                "workspace_conflicts",
                schema_type("integer", "Unresolved workspace conflict count."),
            ),
            (
                "hygiene_clean",
                schema_type("boolean", "Compact summary_only hygiene cleanliness verdict."),
            ),
            (
                "collaboration",
                open_object_schema("Compact bounded collaboration state: open_todo_count, high_priority_open_todos, recent_answers, and recent_completions. Message bodies remain individually bounded and worker execution history is not copied."),
            ),
            (
                "facts",
                open_object_schema("Canonical closeout facts: work_performed, changed_paths, executions, validation counts, resolved/unresolved failures, workspace state, active jobs, and evidence integrity."),
            ),
            (
                "hard_blockers",
                array_schema(schema_type("string", "Deterministic blocker identifier."), "Only confirmed command/safety/consistency blockers."),
            ),
            (
                "advisories",
                array_schema(schema_type("string", "Non-blocking advisory identifier."), "Context-dependent facts for Agent judgment."),
            ),
            ("title", nullable_schema("string", "Optional session title.")),
            ("mode", session_mode_schema("Session mode.")),
            (
                "guards",
                session_guards_schema("Effective session guards."),
            ),
            (
                "execution_context",
                session_execution_context_schema(
                    "Persistent execution defaults for this Workflow Session.",
                ),
            ),
            (
                "lifecycle",
                session_lifecycle_schema(
                    "Workflow session lifecycle. active until explicit close_session; closed sessions remain queryable.",
                ),
            ),
            (
                "created_at",
                schema_type("integer", "Session creation unix timestamp."),
            ),
            (
                "updated_at",
                schema_type("integer", "Session last-update unix timestamp."),
            ),
            (
                "counts",
                open_object_schema("Bounded structured counts: events, failed_tool_calls, messages, open_todos, open_risks, open_questions, open_guidance."),
            ),
            (
                "open_todos",
                array_schema(
                    open_object_schema("Bounded open todo message."),
                    "Bounded newest-first open todos.",
                ),
            ),
            (
                "high_priority_open_todos",
                array_schema(
                    open_object_schema("Bounded high-priority open todo message."),
                    "Bounded newest-first high-priority open todos.",
                ),
            ),
            (
                "recent_answers",
                array_schema(
                    open_object_schema("Bounded recent answer message with reply_to and trusted author_session_id when available."),
                    "Bounded newest-first recent answers.",
                ),
            ),
            (
                "recent_completions",
                array_schema(
                    open_object_schema("Structured todo-to-answer completion correlation."),
                    "Bounded recent completion correlations with worker Session provenance when available.",
                ),
            ),
            (
                "open_risks",
                array_schema(
                    open_object_schema("Bounded open risk message."),
                    "Bounded newest-first open risks.",
                ),
            ),
            (
                "open_questions",
                array_schema(
                    open_object_schema("Bounded open question message."),
                    "Bounded newest-first open questions.",
                ),
            ),
            (
                "open_guidance",
                array_schema(
                    open_object_schema("Bounded open guidance message."),
                    "Bounded newest-first open guidance.",
                ),
            ),
            (
                "recent_progress",
                array_schema(
                    open_object_schema("Bounded recent progress message."),
                    "Bounded newest-first recent progress.",
                ),
            ),
            (
                "recent_decisions",
                array_schema(
                    open_object_schema("Bounded recent decision message."),
                    "Bounded newest-first recent decisions.",
                ),
            ),
            (
                "recent_failed_tools",
                array_schema(
                    open_object_schema("Bounded failed tool call summary: tool_name, error_kind, failure_kind, created_at, write_like, job_like."),
                    "Bounded newest-first recent failed tool calls. Never includes raw input payloads.",
                ),
            ),
            (
                "tool_failures",
                open_object_schema("Pre-declared result-expectation classification from the session ledger. Default success remains fail-closed; matched negative/observation outcomes are expected evidence. unexpected_count remains immutable raw failed-ToolCall evidence; non_actionable_unexpected_count identifies request-scoped validation evidence assertion failures, resolved/stale validation failures, or structurally proven not-started/non-effect attempts; actionable_unexpected_count is the conservative current blocker projection. Expectation mismatches and unexpected successes remain separate integrity evidence. Never includes raw input payloads, command text, stdout/stderr, tails, or excerpts."),
            ),
            (
                "expected_failed_tool_calls",
                array_schema(
                    open_object_schema("Bounded matched-result tool call summary: event_id, tool_name, project, assertion_name, result_expectation, accepted_exit_codes, exit_code, expected_failure_kind, actual_failure_kind, status, success, created_at."),
                    "Failed tool calls whose pre-declared failure/observation or accepted-exit expectation matched.",
                ),
            ),
            (
                "unexpected_failed_tool_calls",
                array_schema(
                    open_object_schema("Bounded unexpected failed tool call summary: event_id, tool_name, project, assertion_name, result_expectation, accepted_exit_codes, exit_code, expected_failure_kind, actual_failure_kind, status, success, created_at."),
                    "Raw bounded unexpected failed tool-call history. Entries may be historical non-actionable evidence; use tool_failures.actionable_unexpected_count for the current blocker projection.",
                ),
            ),
            (
                "expectation_mismatches",
                array_schema(
                    open_object_schema("Bounded expectation mismatch summary: event_id, tool_name, project, assertion_name, result_expectation, accepted_exit_codes, exit_code, expected_failure_kind, actual_failure_kind, status, success, created_at."),
                    "Pre-declared result expectations whose completed result did not match (for example an exit code outside accepted_exit_codes).",
                ),
            ),
            (
                "unexpected_success_tool_calls",
                array_schema(
                    open_object_schema("Bounded unexpected success summary: event_id, tool_name, project, assertion_name, result_expectation, accepted_exit_codes, exit_code, expected_failure_kind, actual_failure_kind, status, success, created_at."),
                    "Calls whose pre-declared failure expectation unexpectedly succeeded.",
                ),
            ),
            (
                "permissions",
                permission_summary_schema("Deterministic bounded permission decision summary from the session ledger. Counts high-risk auto-approved tools only; never includes stdout/stderr, env, tokens, secrets, or raw input content."),
            ),
            (
                "jobs",
                job_lifecycle_summary_schema("Bounded job lifecycle summary for handoff. active_jobs_present is emitted only for blocking_active_count > 0; stop_requested-only jobs use nonblocking jobs_terminal_pending. Never includes stdout/stderr or command text."),
            ),
            (
                "workspace",
                open_object_schema("Bounded workspace summary when project is provided: project, git_available, non_git_project, clean, branch, head, changed_files_count, warnings, suggested_next_actions. Never includes hunks or full diffs."),
            ),
            #[cfg(feature = "workspace-checkpoints")]
            (
                "checkpoints",
                open_object_schema("Bounded checkpoint candidates when project is provided: latest_last_known_good and recent list. Never includes validation.commands or diffs."),
            ),
            (
                "validation",
                open_object_schema("Ledger-derived validation-like tool-call summary with status/reason: not_run, passed, failed, mixed, inconclusive, expected, or unknown. `expected` means a pre-declared negative/observation result matched without proving validator pass. Parser version 3 provides bounded structured diagnostics from bounded validation metadata using canonical diagnostics and failed_test_details fields only. Full and summary_only closeout preserve the same validation evidence. Does not include stdout/stderr bodies and performs no root-cause inference; parser.available remains false when session ledger events lack those fields. latest_status and historical_failures retain the existing final-state and resolved-history semantics."),
            ),
            (
                "review_evidence",
                review_evidence_schema("Ledger-derived non-cargo review evidence summary for full and summary_only outputs. Counts successful read/search/diff/workspace/hygiene inspection tools from the session ledger and exposes bounded tools for compact explainability. For docs-only or read-only audit tasks, validation.status may remain not_run while review_evidence.total is greater than zero. Does not include file contents, stdout/stderr, diff hunks, command text, tokens, secrets, or raw input payloads. Does not change validation.status or make the verdict pass."),
            ),
            (
                "verdict",
                open_object_schema("Legacy aggregate closeout verdict for full and summary_only output: task_outcome fail or evidence_integrity error maps to blocking fail; otherwise task_outcome warn or evidence_integrity warning maps to non-blocking warn; otherwise pass. Resolved evidence history alone does not lower the verdict."),
            ),
            (
                "task_outcome",
                task_outcome_schema("Final task completion outcome with status pass/warn/fail, blocking, and task-only reasons. Resolved validation history and expected-failure audit metadata do not lower this status."),
            ),
            (
                "evidence_history",
                evidence_history_schema("Validation evidence history status: clean, mixed_resolved, mixed_unresolved, or failed. Does not replace validation.status or validation.latest_status."),
            ),
            (
                "evidence_integrity",
                evidence_integrity_schema("Expected-failure and validation-evidence integrity status: clean, warning, or error, with bounded reason identifiers."),
            ),
            (
                "informational_notes",
                array_schema(
                    schema_type("string", "Completed-state informational note."),
                    "Bounded completed-state facts, separate from executable suggested_next_actions.",
                ),
            ),
            (
                "suggested_next_actions",
                array_schema(
                    schema_type("string", "Short suggested action."),
                    "Bounded suggested next actions for the receiving agent.",
                ),
            ),
            (
                "continuation_feedback",
                continuation_feedback_schema("Deterministic continuation feedback for normal and summary_only handoff. A read-only attempt summary plus validation delta over existing handoff evidence; never an LLM summary, never an Agent loop, never a new verdict, and it never re-runs validation, mutates the ledger, refreshes activity, or consumes guidance."),
            ),
            (
                "handoff_brief",
                handoff_brief_schema("Compact deterministic task handoff for a new window, new Agent, or human receiver. It is a read-only projection over already-obtained Session, continuation, workspace, validation, Job, and guidance evidence; it is not Session replay and never restores hidden model context."),
            ),
        ])),
        _ => None,
    }
}

fn validation_summary_tool_output_schema() -> Value {
    let mut schema = wrapped_output_schema(vec![
        (
            "project",
            schema_type("string", "Resolved complete runtime project id."),
        ),
        (
            "session_id",
            schema_type("string", "Explicit business session id queried."),
        ),
        ("validation", validation_evidence_schema()),
        (
            "validation_delta",
            validation_delta_schema("Deterministic diff between the latest validation evidence and the most recent prior comparable validation evidence. A read-only projection derived only from the ledger validation summary above; never re-runs validation, mutates the ledger, changes the verdict, or consumes guidance. unavailable with a stable reason code when the two runs are not proven comparable."),
        ),
    ]);
    schema["properties"]["output"]["additionalProperties"] = json!(false);
    schema
}

fn validation_evidence_schema() -> Value {
    fn current_validation_evidence_schema() -> Value {
        json!({
            "type": "object",
            "description": "Current workspace validation evidence for the current attempt after the latest trusted material workspace-content change. Historical ledger failures remain separately visible and are not erased by this projection.",
            "additionalProperties": false,
            "properties": {
                "status": {"type": "string", "enum": ["passed", "failed", "inconclusive", "expected", "stale", "not_run", "unknown"]},
                "reason": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "latest_status": {"type": "string", "enum": ["passed", "failed", "inconclusive", "expected", "not_run", "unknown"]},
                "events_total": {"type": "integer", "minimum": 0},
                "successes": {"type": "integer", "minimum": 0},
                "failures": {"type": "integer", "minimum": 0},
                "expected_results": {"type": "integer", "minimum": 0},
                "resolved_failure_count": {"type": "integer", "minimum": 0},
                "unresolved_failure_count": {"type": "integer", "minimum": 0},
                "evidence_gap_event_count": {"type": "integer", "minimum": 0, "description": "Request-scoped/inconclusive validation evidence events in the current post-mutation window. This is historical/process evidence within the window, not a persistent task requirement."},
                "stale_failure_count": {"type": "integer", "minimum": 0},
                "evidence_after_latest_content_change": {"type": "boolean"},
                "boundary_reason": {"type": "string", "enum": ["attempt_start", "workspace_content_changed", "attempt_boundary_unavailable"]}
            },
            "required": [
                "status", "reason", "latest_status", "events_total", "successes", "failures",
                "expected_results", "resolved_failure_count", "unresolved_failure_count", "evidence_gap_event_count", "stale_failure_count",
                "evidence_after_latest_content_change", "boundary_reason"
            ]
        })
    }
    let event = validation_event_schema();
    json!({
        "type": "object",
        "description": "Bounded deterministic validation evidence derived only from safe session-ledger metadata. Never contains commands, raw event payloads, validation excerpts, full stdout/stderr, environment variables, or root-cause inference.",
        "additionalProperties": false,
        "properties": {
            "available": schema_type("boolean", "True when validation-like ledger events exist."),
            "status": { "type": "string", "enum": ["not_run", "passed", "failed", "mixed", "inconclusive", "expected", "unknown"] },
            "reason": { "anyOf": [{"type": "string"}, {"type": "null"}] },
            "latest": { "anyOf": [event.clone(), {"type": "null"}] },
            "latest_status": { "type": "string", "enum": ["not_run", "passed", "failed", "inconclusive", "expected", "unknown"] },
            "current_evidence": current_validation_evidence_schema(),
            "historical_failures": validation_historical_failures_schema(),
            "resolved_failures": validation_failure_set_schema(),
            "unresolved_failures": validation_failure_set_schema(),
            "evidence_gaps": validation_failure_set_schema(),
            "source": { "type": "string", "enum": ["session_ledger"] },
            "events_total": { "type": "integer", "minimum": 0 },
            "successes": { "type": "integer", "minimum": 0 },
            "failures": { "type": "integer", "minimum": 0 },
            "expected_results": { "type": "integer", "minimum": 0 },
            "latest_success": event.clone(),
            "latest_failure": event.clone(),
            "events": {
                "type": "array",
                "maxItems": 100,
                "items": event,
                "description": "Bounded validation history only; never raw session events."
            },
            "parser": validation_parser_metadata_schema(),
            "cargo_test_zero_tests_run": schema_type("boolean", "True when a completed, validator-successful cargo_test execution explicitly reported zero tests run, even if a request-scoped count assertion made the raw ToolResult fail."),
            "skipped": schema_type("boolean", "True only when validation summary generation was explicitly skipped by a closeout caller.")
        },
        "required": [
            "available", "status", "reason", "latest", "latest_status", "current_evidence",
            "historical_failures", "resolved_failures", "unresolved_failures", "evidence_gaps",
            "source", "events_total", "events", "parser",
            "cargo_test_zero_tests_run"
        ]
    })
}

fn validation_historical_failures_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "count": { "type": "integer", "minimum": 0 },
            "resolved": { "type": "boolean" },
            "unresolved": { "type": "boolean" }
        },
        "required": ["count", "resolved", "unresolved"]
    })
}

fn validation_failure_set_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "count": { "type": "integer", "minimum": 0 },
            "events": {
                "type": "array",
                "maxItems": 100,
                "items": validation_event_schema()
            }
        },
        "required": ["count", "events"]
    })
}

fn validation_parser_metadata_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "available": { "type": "boolean" },
            "kind": { "type": "string", "enum": ["structured_validation_parser"] },
            "version": { "type": "integer", "enum": [3] },
            "source": { "type": "string", "enum": ["bounded_validation_metadata"] },
            "raw_output_exposed": { "type": "boolean", "enum": [false] },
            "limitations": {
                "type": "array",
                "maxItems": 3,
                "items": { "type": "string", "maxLength": 160 }
            },
            "reason": { "type": "string", "maxLength": 160 }
        },
        "required": ["available", "kind", "version", "source", "raw_output_exposed", "limitations"]
    })
}

fn validation_event_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "tool_name": { "type": "string", "enum": ["cargo_fmt", "cargo_check", "cargo_test", "go_test", "run_process", "run_script", "run_shell", "run_job"] },
            "execution_source": { "type": "string" },
            "identity": { "type": "string", "maxLength": 256 },
            "assertion_name": { "type": "string", "minLength": 1, "maxLength": MAX_MODEL_VALIDATION_ASSERTION_NAME_CHARS },
            "purpose": { "type": "string", "enum": ["validation", "test", "build", "format", "release"] },
            "validation_kind": { "type": "string", "enum": ["format", "check", "test", "build", "release", "validation"] },
            "success": { "type": "boolean", "description": "Immutable raw ToolResult success recorded by the Workflow Session. A request-scoped evidence assertion can make this false even when validator execution and correctness passed." },
            "execution_success": { "type": "boolean", "description": "Derived execution result from authoritative completion state and exit code; independent from request-scoped evidence assertions." },
            "validation_passed": { "type": "boolean", "description": "True when the validator/correctness execution itself passed. This can remain true while the invocation's evidence assertion is insufficient." },
            "failure_class": { "type": "string", "enum": ["none", "execution_or_correctness", "outcome_unknown", "evidence_assertion", "evidence_insufficient", "expected_result"] },
            "expectation_satisfied": { "type": "boolean", "description": "Present for public result expectations; true when the pre-declared expectation matched. This is separate from validation success." },
            "failure_kind": { "type": "string", "enum": ["compile_error", "test_failure", "validation_failed", "timeout", "process_exit", "format_diff", "unknown"] },
            "failure_category": { "type": "string", "enum": ["compile_error", "test_failure", "validation_failed", "timeout", "process_exit", "format_diff", "unknown"] },
            "unresolved_failure": { "type": "boolean" },
            "exit_code": { "type": "integer" },
            "summary": { "type": "string", "maxLength": 80 },
            "command_summary": { "type": "string", "maxLength": 512 },
            "cwd": { "type": "string", "maxLength": 4096 },
            "shell": { "type": "string", "enum": ["sh", "bash", "configured", "remote", "direct_argv"] },
            "execution_state": { "type": "string", "enum": ["not_started", "started", "outcome_unknown", "completed", "cancelled", "timed_out"] },
            "project": { "type": "string", "maxLength": 512 },
            "session_id": { "type": "string", "maxLength": 128 },
            "started_at": { "type": "integer" },
            "completed_at": { "type": "integer" },
            "duration_ms": { "type": "integer", "minimum": 0 },
            "affected_paths": {
                "type": "array",
                "maxItems": 20,
                "items": { "type": "string", "maxLength": 512 }
            },
            "diagnostics": validation_diagnostics_schema(),
            "detected_summary": {
                "type": "object",
                "additionalProperties": true
            },
            "tests_detected": { "type": "boolean" },
            "tests_run_count": { "type": "integer", "minimum": 0 },
            "zero_tests_run": { "type": "boolean" },
            "require_tests": { "type": "boolean" },
            "no_run": { "type": "boolean" },
            "test_count_assertion": cargo_test_count_assertion_schema(),
            "stdout_lines": { "type": "integer", "minimum": 0 },
            "stderr_lines": { "type": "integer", "minimum": 0 },
            "stdout_truncated": { "type": "boolean" },
            "stderr_truncated": { "type": "boolean" },
            "stdout_evidence": { "type": "string" },
            "stderr_evidence": { "type": "string" }
        },
        "required": [
            "tool_name", "execution_source", "identity", "purpose",
            "validation_kind", "success", "validation_passed", "failure_class", "failure_kind", "failure_category",
            "unresolved_failure", "summary", "cwd", "shell", "execution_state",
            "session_id", "stdout_truncated", "stderr_truncated"
        ]
    })
}

fn validation_diagnostics_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "available": { "type": "boolean" },
            "parser": { "type": "string", "enum": ["structured_validation_parser"] },
            "reason": { "type": "string", "maxLength": 160 },
            "diagnostic_count": { "type": "integer", "minimum": 0 },
            "diagnostics": {
                "type": "array",
                "maxItems": 20,
                "items": cargo_diagnostic_schema()
            },
            "returned_diagnostic_count": { "type": "integer", "minimum": 0, "maximum": 20 },
            "diagnostics_truncated": { "type": "boolean" },
            "invalid_diagnostics_omitted": { "type": "integer", "minimum": 0 },
            "test_summary": cargo_test_summary_schema(),
            "failed_test_details": {
                "type": "array",
                "maxItems": 20,
                "items": failed_test_detail_schema()
            },
            "failed_test_details_truncated": { "type": "boolean" },
            "truncated": { "type": "boolean" }
        },
        "required": [
            "available", "parser", "diagnostics", "returned_diagnostic_count",
            "diagnostics_truncated", "invalid_diagnostics_omitted",
            "failed_test_details", "failed_test_details_truncated"
        ]
    })
}

fn cargo_diagnostic_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "severity": { "type": "string", "enum": ["error", "warning", "unknown"] },
            "code": { "type": "string", "maxLength": 64 },
            "file": { "type": "string", "maxLength": 512 },
            "line": { "type": "integer", "minimum": 1 },
            "column": { "type": "integer", "minimum": 1 },
            "message": { "type": "string", "maxLength": 240 }
        },
        "required": ["severity", "message"]
    })
}

fn failed_test_detail_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": { "type": "string", "maxLength": 240 },
            "failure_kind": { "type": "string", "enum": ["assertion", "panic", "unknown"] },
            "file": { "anyOf": [{"type": "string", "maxLength": 512}, {"type": "null"}] },
            "line": { "anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}] },
            "column": { "anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}] }
        },
        "required": ["name", "failure_kind", "file", "line", "column"]
    })
}

fn cargo_test_summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "passed": { "type": "integer", "minimum": 0 },
            "failed": { "type": "integer", "minimum": 0 },
            "ignored": { "type": "integer", "minimum": 0 }
        }
    })
}

fn review_evidence_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": true,
        "properties": {
            "available": schema_type("boolean", "True when review evidence summary is available."),
            "source": schema_type("string", "Review evidence source, usually session_ledger."),
            "total": schema_type("integer", "Total successful review evidence tool calls counted."),
            "read_only_inspection_count": schema_type("integer", "Successful read-only inspection tool calls counted."),
            "search_count": schema_type("integer", "Successful search tool calls counted."),
            "diff_review_count": schema_type("integer", "Successful diff review tool calls counted."),
            "workspace_review_count": schema_type("integer", "Successful workspace review tool calls counted."),
            "hygiene_review_count": schema_type("integer", "Successful hygiene review tool calls counted."),
            "tools": {
                "type": "array",
                "maxItems": 20,
                "description": "Bounded unique review evidence tool names only; never file contents, diff hunks, stdout/stderr, command text, tokens, secrets, or raw input payloads.",
                "items": schema_type("string", "Review evidence tool name.")
            }
        }
    })
}
