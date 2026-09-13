//! `session_handoff_summary` — read-only structured handoff for degraded or
//! contaminated execution context recovery (GPT long-task window routed to a
//! degraded/contaminated context, context pollution, or continuing in a fresh
//! window), multi-agent, and multi-window scenarios.
//!
//! Aggregates session info, message-board state, recent progress/decisions,
//! open todos/risks/questions/guidance, recent failed tool calls, and optional
//! workspace + checkpoint metadata. Never calls an LLM; never generates
//! natural-language summaries. Output is always bounded and never includes
//! full diffs, file contents, stdout/stderr bodies, validation commands,
//! secrets, tokens, or raw session input payloads.

use super::continuation_feedback::{
    continuation_feedback_value, continuation_projection_hooks, continuation_validation_snapshot,
    ContinuationFeedbackInput, ContinuationToolFailureSnapshot,
};
use super::handoff_brief::{build_handoff_brief, HandoffBriefInput};
use super::permissions::permission_summary_from_events;
use super::session_context::{session_project_mismatch_result, SessionProjectMismatch};
use super::sessions::{
    canonical_tool_call_finished_events, tool_failure_summary_from_events, SessionEvent,
    SessionSummary, TOOL_EXPECTATION_RESULT_UNEXPECTED_FAILURE,
};
use super::sessions::{SessionDiscussionCounts, SessionDiscussionSummary, SessionMessage};
use super::tool_result::ToolResult;
use super::ToolRuntime;
use crate::auth::AuthContext;
use serde_json::{json, Value};
use std::collections::HashSet;
use webcodex_tool_contracts::{
    runtime_tool_session_evidence_policy, ToolFailureEvidence, ToolReviewEvidence,
};

pub(crate) use webcodex_workflow_session::closeout_work_projection;

pub(super) const DEFAULT_HANDOFF_LIMIT: usize = 20;
const MAX_HANDOFF_LIMIT: usize = 100;
const HANDOFF_CLOSEOUT_SESSION_EVENT_LIMIT: usize = 200;
const MAX_RECENT_FAILED_TOOLS: usize = 10;
const MAX_RECENT_PROGRESS: usize = 10;
const MAX_RECENT_DECISIONS: usize = 10;
const MAX_OPEN_ITEMS: usize = 20;
#[cfg(feature = "workspace-checkpoints")]
const MAX_RECENT_CHECKPOINTS: usize = 10;
const HANDOFF_MESSAGE_CHARS: usize = 240;

/// Actionable guidance only for a validation failure that still belongs to the
/// current evidence window. Identity reuse is conditional: it strengthens
/// correlation when the same logical validation is intentionally rerun, but is
/// never a requirement to clean stale audit history.
pub(crate) const VALIDATION_IDENTITY_REUSE_ACTION: &str =
    "address the current validation failure; when intentionally rerunning it, reuse the original assertion_name when supplied and the same validation identity";

impl ToolRuntime {
    pub(crate) async fn session_handoff_summary(
        &self,
        session_id: String,
        project: Option<String>,
        include_workspace: Option<bool>,
        include_checkpoints: Option<bool>,
        include_validation: Option<bool>,
        summary_only: bool,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let limit = limit
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_HANDOFF_LIMIT)
            .min(MAX_HANDOFF_LIMIT);
        let include_workspace = include_workspace.unwrap_or(true);
        let include_checkpoints = include_checkpoints.unwrap_or(true);
        #[cfg(not(feature = "workspace-checkpoints"))]
        let _ = include_checkpoints;
        let include_validation = include_validation.unwrap_or(true);

        let authorized_target = match self
            .authorize_session_target(&session_id, "session_handoff_summary", auth)
            .await
        {
            Ok(resolved) => resolved,
            Err(result) => return result,
        };
        if let Some(request_project) = project
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        {
            let requested = match self
                .resolve_project_input_for_auth(request_project, auth)
                .await
            {
                Ok(resolved) => resolved,
                Err(err) => return err.into_tool_result(),
            };
            if let Some(target) = authorized_target.as_ref() {
                if target.resolved_id != requested.resolved_id {
                    return session_project_mismatch_result(
                        &session_id,
                        "session_handoff_summary",
                        &SessionProjectMismatch {
                            session_project: target.resolved_id.clone(),
                            request_project: requested.resolved_id,
                        },
                    );
                }
            }
        }

        // --- session basic info + display-bounded events ---
        let summary = match self.sessions.summary(&session_id, Some(limit)) {
            Some(summary) => summary,
            None => return super::unknown_session_result(&session_id),
        };
        // Canonical closeout evidence must not depend on the caller's display
        // limit. Reuse one fixed bounded Session snapshot for validation,
        // tool-failure actionability, review evidence, and outcome projection.
        let closeout_session = self
            .sessions
            .summary(&session_id, Some(HANDOFF_CLOSEOUT_SESSION_EVENT_LIMIT))
            .unwrap_or_else(|| summary.clone());

        // --- message board state ---
        let (discussion, guidance_available) =
            match self.sessions.discussion_summary(&session_id, Some(limit)) {
                Ok(discussion) => (discussion, true),
                Err(_) => {
                    // UnknownSession is already caught by summary() above; any
                    // other error is treated as an empty board.
                    (
                        SessionDiscussionSummary {
                            counts: SessionDiscussionCounts {
                                total: 0,
                                open: 0,
                                resolved: 0,
                                guidance: 0,
                                progress: 0,
                                risk: 0,
                                todo: 0,
                                question: 0,
                                answer: 0,
                                decision: 0,
                                open_guidance: 0,
                                open_questions: 0,
                                open_risks: 0,
                                open_todos: 0,
                            },
                            open_guidance: Vec::new(),
                            open_questions: Vec::new(),
                            open_risks: Vec::new(),
                            open_todos: Vec::new(),
                            high_priority_open_todos: Vec::new(),
                            recent_answers: Vec::new(),
                            recent_completions: Vec::new(),
                            recent_progress: Vec::new(),
                            recent_decisions: Vec::new(),
                        },
                        false,
                    )
                }
            };

        // --- recent failed tool calls (from finished events) ---
        let recent_failed_tools: Vec<Value> = canonical_tool_call_finished_events(&summary.events)
            .into_iter()
            .filter(|event| event.status.as_deref() == Some("failed"))
            .rev()
            .take(MAX_RECENT_FAILED_TOOLS)
            .map(|event| {
                json!({
                    "tool_name": event.tool_name,
                    "error_kind": event.error_kind,
                    "failure_kind": event.failure_kind,
                    "created_at": event.timestamp,
                    "write_like": event.write_like,
                    "job_like": event.shell_like,
                })
            })
            .collect();

        let failed_tool_calls = recent_failed_tools.len();
        let tool_failures = handoff_tool_failure_summary(&summary.events, &closeout_session.events);
        let expected_failed_tool_calls = output_recent(&tool_failures, "recent_expected");
        let unexpected_failed_tool_calls = output_recent(&tool_failures, "recent_unexpected");
        let expectation_mismatches = output_recent(&tool_failures, "recent_mismatches");
        let unexpected_success_tool_calls =
            output_recent(&tool_failures, "recent_unexpected_successes");

        let open_todos = bound_messages(&discussion.open_todos, MAX_OPEN_ITEMS);
        let high_priority_open_todos =
            bound_messages(&discussion.high_priority_open_todos, MAX_OPEN_ITEMS);
        let open_risks = bound_messages(&discussion.open_risks, MAX_OPEN_ITEMS);
        let open_questions = bound_messages(&discussion.open_questions, MAX_OPEN_ITEMS);
        let open_guidance = bound_messages(&discussion.open_guidance, MAX_OPEN_ITEMS);
        let recent_answers = bound_messages(&discussion.recent_answers, MAX_RECENT_PROGRESS);
        let recent_completions = discussion
            .recent_completions
            .iter()
            .take(MAX_RECENT_PROGRESS)
            .cloned()
            .collect::<Vec<_>>();
        let recent_progress = bound_messages(&discussion.recent_progress, MAX_RECENT_PROGRESS);
        let recent_decisions = bound_messages(&discussion.recent_decisions, MAX_RECENT_DECISIONS);

        let counts = json!({
            "events": summary.events.len(),
            "failed_tool_calls": failed_tool_calls,
            "messages": discussion.counts.total,
            "open_todos": discussion.counts.open_todos,
            "open_risks": discussion.counts.open_risks,
            "open_questions": discussion.counts.open_questions,
            "open_guidance": discussion.counts.open_guidance,
        });

        let mut warnings = Vec::new();
        let jobs_project = match project
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        {
            Some(project) => self
                .resolve_project_input_for_auth(project, auth)
                .await
                .map(|resolved| resolved.resolved_id)
                .unwrap_or_else(|_| project.to_string()),
            None => summary.project.clone().unwrap_or_default(),
        };
        let jobs_project = (!jobs_project.is_empty()).then_some(jobs_project);
        let jobs = self
            .active_jobs_summary(jobs_project.as_deref(), auth, 10)
            .await;
        if let Some(job_warnings) = jobs.get("warnings").and_then(Value::as_array) {
            warnings.extend(job_warnings.iter().cloned());
        }

        let mut output = json!({
            "session_id": summary.session_id,
            "project": summary.project,
            "title": summary.title,
            "mode": summary.mode,
            "guards": summary.guards,
            "execution_context": summary.execution_context,
            "lifecycle": summary.lifecycle,
            "created_at": summary.created_at,
            "updated_at": summary.updated_at,
            "counts": counts,
            "permissions": permission_summary_from_events(&summary.events, DEFAULT_HANDOFF_LIMIT),
            "open_todos": open_todos,
            "high_priority_open_todos": high_priority_open_todos,
            "recent_answers": recent_answers,
            "recent_completions": recent_completions,
            "open_risks": open_risks,
            "open_questions": open_questions,
            "open_guidance": open_guidance,
            "recent_progress": recent_progress,
            "recent_decisions": recent_decisions,
            "recent_failed_tools": recent_failed_tools,
            "tool_failures": tool_failures,
            "expected_failed_tool_calls": expected_failed_tool_calls,
            "unexpected_failed_tool_calls": unexpected_failed_tool_calls,
            "expectation_mismatches": expectation_mismatches,
            "unexpected_success_tool_calls": unexpected_success_tool_calls,
            "review_evidence": review_evidence_summary_for_session(&closeout_session),
            "jobs": jobs,
            "warnings": warnings,
        });

        // --- optional workspace summary ---
        let has_project = project
            .as_deref()
            .map(|p| !p.trim().is_empty())
            .unwrap_or(false);
        if has_project && include_workspace {
            let project = project.clone().unwrap_or_default();
            let workspace = self.handoff_workspace_summary(&project).await;
            output["workspace"] = workspace;
        }

        // --- optional checkpoint candidates ---
        #[cfg(feature = "workspace-checkpoints")]
        if has_project && include_checkpoints {
            let project = project.clone().unwrap_or_default();
            let checkpoints = self.handoff_checkpoint_summary(&project, limit).await;
            output["checkpoints"] = checkpoints;
        }

        // --- optional ledger-derived validation summary ---
        // Validation shares the independent fixed closeout snapshot above, so
        // caller display limits cannot shrink attempt-boundary detection or any
        // canonical closeout classification. When validation is not requested,
        // continuation feedback reports it as explicitly unavailable rather than
        // masquerading as `not_run`.
        let feedback_validation: Value = if include_validation {
            self.validation_summary_for_session_with_jobs(
                &closeout_session,
                DEFAULT_HANDOFF_LIMIT,
                auth,
            )
            .await
        } else {
            json!({ "available": false, "not_requested": true })
        };
        if include_validation {
            output["validation"] = feedback_validation.clone();
        }
        let continuation_current_validation =
            super::validation_events::current_validation_evidence_for_session(
                &closeout_session,
                20,
            );
        let (work_performed, changed_paths) = closeout_work_projection(&summary.events);
        output["work_performed"] = work_performed;
        output["changed_paths"] = changed_paths;
        let reconciliation = reconcile_closeout_evidence(
            output.get("tool_failures").unwrap_or(&Value::Null),
            &closeout_session,
            &feedback_validation,
        );

        // Continuation feedback: a read-only attempt-summary + validation-delta
        // projection reused across handoff, finish, and start. Built from the
        // independent bounded evidence snapshot, validation value, and job
        // metadata already gathered here; never re-runs validation, mutates the
        // ledger, refreshes activity, or consumes guidance.
        output["continuation_feedback"] = continuation_feedback_value(ContinuationFeedbackInput {
            session_summary: &closeout_session,
            validation: &feedback_validation,
            jobs: output.get("jobs").unwrap_or(&Value::Null),
            discussion: &discussion,
            continuation: "continued",
            suggest_exploration_continuity: false,
            workspace_conflicts: output
                .pointer("/workspace/counts/conflicted")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0,
            hooks: continuation_projection_hooks(),
            current_validation: continuation_validation_snapshot(&continuation_current_validation),
            tool_failures: ContinuationToolFailureSnapshot::new(
                &reconciliation.actionable_unexpected_event_ids,
            ),
        });
        output["tool_failures"] = reconciliation.tool_failures;
        if include_validation {
            output["validation"] = reconciliation.validation;
        }

        // --- bounded suggested next actions ---
        output["suggested_next_actions"] = json!(handoff_suggested_next_actions(&output));
        output["handoff_brief"] = build_handoff_brief(HandoffBriefInput {
            session_summary: &closeout_session,
            continuation_feedback: output.get("continuation_feedback").unwrap_or(&Value::Null),
            workspace_requested: include_workspace,
            workspace: output.get("workspace"),
            validation_requested: include_validation,
            validation: Some(&feedback_validation),
            jobs: output.get("jobs"),
            guidance_available,
            existing_suggested_actions: output.get("suggested_next_actions"),
        });

        let compact = compact_handoff_output(&output);
        if summary_only {
            return ToolResult::ok(compact);
        }
        for field in [
            "facts",
            "hard_blockers",
            "advisories",
            "task_outcome",
            "evidence_history",
            "evidence_integrity",
            "informational_notes",
            "verdict",
        ] {
            output[field] = compact.get(field).cloned().unwrap_or(Value::Null);
        }

        ToolResult::ok(output)
    }

    /// Build a bounded workspace summary reusing the read-only `show_changes`
    /// git inspection path. Returns only clean/branch/head/counts/warnings/
    /// suggested_next_actions — never hunks, full diffs, or file contents.
    async fn handoff_workspace_summary(&self, project: &str) -> Value {
        let show_result = self
            .show_changes(project.to_string(), None, Some(false), None, None, None)
            .await;
        if !show_result.success {
            // Non-git project or git failure: do not fail the whole handoff.
            // Surface a structured warning instead.
            let mut warnings: Vec<Value> = show_result
                .output
                .get("warnings")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            warnings.push(json!({
                "kind": "git_unavailable",
                "message": "git-backed workspace inspection unavailable; project may not be a git repository",
            }));
            return json!({
                "project": project,
                "git_available": false,
                "non_git_project": show_result.output.get("non_git_project").cloned().unwrap_or(json!(false)),
                "clean": true,
                "branch": null,
                "head": null,
                "changed_files_count": 0,
                "counts": {},
                "warnings": json!(warnings),
                "suggested_next_actions": [],
            });
        }
        let counts = show_result
            .output
            .get("counts")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let changed_files_count = counts
            .as_object()
            .map(|obj| {
                let modified = obj.get("modified").and_then(Value::as_u64).unwrap_or(0);
                let added = obj.get("added").and_then(Value::as_u64).unwrap_or(0);
                let deleted = obj.get("deleted").and_then(Value::as_u64).unwrap_or(0);
                let renamed = obj.get("renamed").and_then(Value::as_u64).unwrap_or(0);
                let copied = obj.get("copied").and_then(Value::as_u64).unwrap_or(0);
                let untracked = obj.get("untracked").and_then(Value::as_u64).unwrap_or(0);
                let conflicted = obj.get("conflicted").and_then(Value::as_u64).unwrap_or(0);
                modified + added + deleted + renamed + copied + untracked + conflicted
            })
            .unwrap_or(0);

        // Carry warnings from show_changes and add a handoff-specific warning
        // when git is unavailable so the receiver immediately sees the gap.
        let mut warnings: Vec<Value> = show_result
            .output
            .get("warnings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let git_available = show_result
            .output
            .get("git_available")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !git_available {
            warnings.push(json!({
                "kind": "git_unavailable",
                "message": "git-backed workspace inspection unavailable; project may not be a git repository",
            }));
        }

        json!({
            "project": project,
            "git_available": json!(git_available),
            "non_git_project": show_result.output.get("non_git_project").cloned().unwrap_or(json!(false)),
            "clean": show_result.output.get("clean").cloned().unwrap_or(json!(true)),
            "branch": show_result.output.get("branch").cloned().unwrap_or(Value::Null),
            "head": show_result.output.get("head").cloned().unwrap_or(Value::Null),
            "changed_files_count": changed_files_count,
            "counts": counts,
            "warnings": json!(warnings),
            "suggested_next_actions": show_result.output.get("suggested_next_actions").cloned().unwrap_or_else(|| json!([])),
        })
    }

    /// Build a bounded checkpoint summary using the read-only
    /// `workspace_checkpoint_list` path. Returns the latest
    /// `last_known_good` checkpoint (preferring `validation_status == passed`)
    /// and a bounded recent list. Never returns validation.commands or diffs.
    #[cfg(feature = "workspace-checkpoints")]
    async fn handoff_checkpoint_summary(&self, project: &str, limit: usize) -> Value {
        let list_result = self
            .workspace_checkpoint_list(project.to_string(), Some(limit))
            .await;
        if !list_result.success {
            return json!({
                "latest_last_known_good": Value::Null,
                "recent": [],
            });
        }
        let checkpoints = list_result.output["checkpoints"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Find the latest last_known_good, preferring validation_status == passed.
        let mut latest_lkg: Option<&Value> = None;
        for checkpoint in &checkpoints {
            let kind = checkpoint.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != "last_known_good" {
                continue;
            }
            let candidate_status = checkpoint
                .get("validation_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let is_passed = candidate_status == "passed";
            match &latest_lkg {
                None => {
                    latest_lkg = Some(checkpoint);
                }
                Some(current) => {
                    let current_status = current
                        .get("validation_status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let current_passed = current_status == "passed";
                    let candidate_time = checkpoint
                        .get("created_at")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let current_time = current
                        .get("created_at")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    // Prefer passed; among same pass status prefer newer.
                    if (is_passed && !current_passed)
                        || (is_passed == current_passed && candidate_time > current_time)
                    {
                        latest_lkg = Some(checkpoint);
                    }
                }
            }
        }

        let latest_lkg_value = latest_lkg
            .map(|checkpoint| {
                json!({
                    "checkpoint_id": checkpoint.get("checkpoint_id").cloned().unwrap_or(Value::Null),
                    "kind": checkpoint.get("kind").cloned().unwrap_or(Value::Null),
                    "labels": checkpoint.get("labels").cloned().unwrap_or_else(|| json!([])),
                    "validation_status": checkpoint.get("validation_status").cloned().unwrap_or(json!("unknown")),
                    "created_at": checkpoint.get("created_at").cloned().unwrap_or(Value::Null),
                    "title": checkpoint.get("title").cloned().unwrap_or(Value::Null),
                })
            })
            .unwrap_or(Value::Null);

        let recent: Vec<Value> = checkpoints
            .iter()
            .take(MAX_RECENT_CHECKPOINTS)
            .map(|checkpoint| {
                json!({
                    "checkpoint_id": checkpoint.get("checkpoint_id").cloned().unwrap_or(Value::Null),
                    "kind": checkpoint.get("kind").cloned().unwrap_or(Value::Null),
                    "validation_status": checkpoint.get("validation_status").cloned().unwrap_or(json!("unknown")),
                    "created_at": checkpoint.get("created_at").cloned().unwrap_or(Value::Null),
                    "title": checkpoint.get("title").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();

        json!({
            "latest_last_known_good": latest_lkg_value,
            "recent": recent,
        })
    }
}

/// Bound a list of session messages for handoff output: limit count and
/// truncate message bodies. Never includes raw full bodies.
fn bound_messages(messages: &[SessionMessage], max_items: usize) -> Vec<Value> {
    messages
        .iter()
        .take(max_items)
        .map(|message| {
            json!({
                "message_id": message.message_id,
                "created_at": message.created_at,
                "kind": message.kind,
                "status": message.status,
                "priority": message.priority,
                "message": bound_chars(&message.message, HANDOFF_MESSAGE_CHARS),
                "tags": message.tags,
                "reply_to": message.reply_to,
                "author_session_id": message.author_session_id,
                "resolved_at": message.resolved_at,
                "resolved_by_message_id": message.resolved_by_message_id,
            })
        })
        .collect()
}

fn bound_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn handoff_tool_failure_summary(
    display_events: &[SessionEvent],
    closeout_events: &[SessionEvent],
) -> Value {
    let display = tool_failure_summary_from_events(display_events, MAX_RECENT_FAILED_TOOLS);
    let mut canonical = tool_failure_summary_from_events(closeout_events, MAX_RECENT_FAILED_TOOLS);
    // Counts and actionability are canonical closeout facts, while bounded
    // recent lists remain presentation-only and respect the caller display
    // window exactly as before.
    for key in [
        "recent_expected",
        "recent_unexpected",
        "recent_mismatches",
        "recent_unexpected_successes",
    ] {
        canonical[key] = output_recent(&display, key);
    }
    canonical
}

fn output_recent(tool_failures: &Value, key: &str) -> Value {
    tool_failures.get(key).cloned().unwrap_or_else(|| json!([]))
}

fn compact_handoff_output(output: &Value) -> Value {
    let workspace_checked = output.get("workspace").is_some();
    let workspace_clean = output
        .get("workspace")
        .and_then(|workspace| workspace.get("clean"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let workspace_conflicts = output
        .pointer("/workspace/counts/conflicted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut compact = json!({
        "summary_only": true,
        "project": output.get("project").cloned().unwrap_or(Value::Null),
        "session_id": output.get("session_id").cloned().unwrap_or(Value::Null),
        "workspace_clean": workspace_clean,
        "workspace_conflicts": workspace_conflicts,
        "hygiene_clean": true,
        "jobs": compact_jobs(output.get("jobs").unwrap_or(&Value::Null)),
        "permissions": compact_permissions(output.get("permissions").unwrap_or(&Value::Null)),
        "tool_failures": compact_tool_failures(output.get("tool_failures").unwrap_or(&Value::Null)),
        "validation": compact_validation(output.get("validation").unwrap_or(&Value::Null)),
        "review_evidence": compact_review_evidence(output.get("review_evidence").unwrap_or(&Value::Null)),
        "collaboration": {
            "open_todo_count": output.pointer("/counts/open_todos").and_then(Value::as_u64).unwrap_or(0),
            "high_priority_open_todos": output.get("high_priority_open_todos").cloned().unwrap_or_else(|| json!([])),
            "recent_answers": output.get("recent_answers").cloned().unwrap_or_else(|| json!([])),
            "recent_completions": output.get("recent_completions").cloned().unwrap_or_else(|| json!([])),
        },
        "work_performed": output.get("work_performed").cloned().unwrap_or_else(|| json!([])),
        "changed_paths": output.get("changed_paths").cloned().unwrap_or_else(|| json!([])),
        "continuation_feedback": output
            .get("continuation_feedback")
            .cloned()
            .unwrap_or_else(|| json!({
                "status": "not_applicable",
                "reason_code": "no_continuation_evidence",
                "deterministic": true,
                "llm_summary": false,
            })),
        "handoff_brief": output.get("handoff_brief").cloned().unwrap_or(Value::Null),
        "warnings": output.get("warnings").cloned().unwrap_or_else(|| json!([])),
        "suggested_next_actions": output.get("suggested_next_actions").cloned().unwrap_or_else(|| json!([])),
    });
    apply_compact_workflow_outcomes(&mut compact, workspace_checked, None);
    compact
}

pub(crate) fn compact_jobs(jobs: &Value) -> Value {
    json!({
        "active_count": jobs.get("active_count").and_then(Value::as_u64).unwrap_or(0),
        "blocking_active_count": jobs.get("blocking_active_count").and_then(Value::as_u64).unwrap_or(0),
        "nonblocking_active_count": jobs.get("nonblocking_active_count").and_then(Value::as_u64).unwrap_or(0),
        "terminal_pending_count": jobs.get("terminal_pending_count").and_then(Value::as_u64).unwrap_or(0),
        "warnings": jobs.get("warnings").cloned().unwrap_or_else(|| json!([])),
    })
}

pub(crate) fn compact_permissions(permissions: &Value) -> Value {
    json!({
        "required_count": permissions.get("required_count").and_then(Value::as_u64).unwrap_or(0),
        "manual_approved_count": permissions.get("manual_approved_count").and_then(Value::as_u64).unwrap_or(0),
        "auto_approved_count": permissions.get("auto_approved_count").and_then(Value::as_u64).unwrap_or(0),
        "total_approved_count": permissions.get("total_approved_count").and_then(Value::as_u64).unwrap_or(0),
        "hard_denied_count": permissions.get("hard_denied_count").and_then(Value::as_u64).unwrap_or(0),
    })
}

pub(crate) fn compact_tool_failures(tool_failures: &Value) -> Value {
    json!({
        "expected_count": tool_failures.get("expected_count").and_then(Value::as_u64).unwrap_or(0),
        "unexpected_count": tool_failures.get("unexpected_count").and_then(Value::as_u64).unwrap_or(0),
        "non_actionable_unexpected_count": tool_failures.get("non_actionable_unexpected_count").and_then(Value::as_u64).unwrap_or(0),
        "actionable_unexpected_count": actionable_unexpected_failure_count(tool_failures),
        "expectation_mismatch_count": tool_failures.get("expectation_mismatch_count").and_then(Value::as_u64).unwrap_or(0),
        "unexpected_success_count": tool_failures.get("unexpected_success_count").and_then(Value::as_u64).unwrap_or(0),
    })
}

pub(crate) fn compact_validation(validation: &Value) -> Value {
    if validation.is_object() {
        return validation.clone();
    }
    json!({
        "status": validation.get("status").cloned().unwrap_or_else(|| json!("not_run")),
        "reason": validation.get("reason").cloned().unwrap_or_else(|| json!("no_validation_tool_invoked")),
        "latest_status": validation
            .get("latest_status")
            .cloned()
            .unwrap_or_else(|| compact_validation_latest_status_fallback(validation)),
        "historical_failures": validation
            .get("historical_failures")
            .cloned()
            .unwrap_or_else(compact_validation_historical_failures_fallback),
        "cargo_test_zero_tests_run": validation_has_cargo_test_zero_tests(validation),
    })
}

pub(crate) fn validation_has_cargo_test_zero_tests(validation: &Value) -> bool {
    validation.get("latest_status").and_then(Value::as_str) == Some("inconclusive")
        && validation
            .get("cargo_test_zero_tests_run")
            .and_then(Value::as_bool)
            == Some(true)
}

pub(crate) fn review_evidence_summary_for_session(summary: &SessionSummary) -> Value {
    review_evidence_summary_from_events(&summary.events)
}

fn review_evidence_summary_from_events(events: &[SessionEvent]) -> Value {
    let mut read_only_inspection_count = 0_u64;
    let mut search_count = 0_u64;
    let mut diff_review_count = 0_u64;
    let mut workspace_review_count = 0_u64;
    let mut hygiene_review_count = 0_u64;
    let mut total = 0_u64;
    let mut tools: Vec<String> = Vec::new();

    for event in canonical_tool_call_finished_events(events) {
        if event.status.as_deref() != Some("succeeded") {
            continue;
        }
        match runtime_tool_session_evidence_policy(event.tool_name.as_str()).review {
            ToolReviewEvidence::None => continue,
            ToolReviewEvidence::ReadOnlyInspection => read_only_inspection_count += 1,
            ToolReviewEvidence::Search => search_count += 1,
            ToolReviewEvidence::DiffReview => diff_review_count += 1,
            ToolReviewEvidence::WorkspaceReview => {
                // `show_changes(include_diff=true)` is one successful call that
                // contributes both workspace and diff dimensions, but total
                // still increments only once below.
                workspace_review_count += 1;
                if event.diff_review_like {
                    diff_review_count += 1;
                }
            }
            ToolReviewEvidence::HygieneReview => {
                workspace_review_count += 1;
                hygiene_review_count += 1;
            }
        }
        total += 1;
        push_unique_tool(&mut tools, &event.tool_name);
    }

    json!({
        "available": true,
        "source": "session_ledger",
        "read_only_inspection_count": read_only_inspection_count,
        "search_count": search_count,
        "diff_review_count": diff_review_count,
        "workspace_review_count": workspace_review_count,
        "hygiene_review_count": hygiene_review_count,
        "total": total,
        "tools": tools,
    })
}

pub(crate) fn compact_review_evidence(review_evidence: &Value) -> Value {
    let tools: Vec<Value> = review_evidence
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .take(20)
                .map(|tool| json!(tool))
                .collect()
        })
        .unwrap_or_default();

    json!({
        "available": review_evidence.get("available").and_then(Value::as_bool).unwrap_or(false),
        "total": review_evidence.get("total").and_then(Value::as_u64).unwrap_or(0),
        "read_only_inspection_count": review_evidence
            .get("read_only_inspection_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "search_count": review_evidence
            .get("search_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "diff_review_count": review_evidence
            .get("diff_review_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "workspace_review_count": review_evidence
            .get("workspace_review_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "hygiene_review_count": review_evidence
            .get("hygiene_review_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        "tools": tools,
    })
}

fn push_unique_tool(tools: &mut Vec<String>, tool_name: &str) {
    if !tools.iter().any(|tool| tool == tool_name) {
        tools.push(tool_name.to_string());
    }
}

fn compact_validation_latest_status_fallback(validation: &Value) -> Value {
    let latest_status = match validation.get("status").and_then(Value::as_str) {
        Some("passed") => "passed",
        Some("failed") => "failed",
        Some("inconclusive") => "inconclusive",
        Some("not_run") => "not_run",
        _ => "unknown",
    };
    json!(latest_status)
}

fn compact_validation_historical_failures_fallback() -> Value {
    json!({
        "count": 0,
        "resolved": false,
        "unresolved": false,
    })
}

pub(crate) fn apply_compact_workflow_outcomes(
    output: &mut Value,
    workspace_checked: bool,
    hygiene_checked: Option<bool>,
) {
    let outcomes = compact_workflow_outcomes(output, workspace_checked, hygiene_checked);
    install_compact_workflow_outcomes(output, outcomes);
}

fn compact_workflow_outcomes(
    output: &Value,
    workspace_checked: bool,
    hygiene_checked: Option<bool>,
) -> Value {
    let mut blocking_reasons: Vec<&'static str> = Vec::new();
    let mut warning_reasons: Vec<&'static str> = Vec::new();
    let mut integrity_errors: Vec<&'static str> = Vec::new();
    let mut integrity_warnings: Vec<&'static str> = Vec::new();
    let mut informational_notes: Vec<&'static str> = Vec::new();
    let mut actions = string_array(output.get("suggested_next_actions"));

    if !workspace_checked {
        push_unique(&mut warning_reasons, "workspace_not_checked");
        push_unique_action(&mut actions, "run show_changes before final handoff");
    }
    let workspace_conflicts = count_field(output, "workspace_conflicts");
    if workspace_conflicts > 0 {
        push_unique(&mut blocking_reasons, "workspace_conflicts");
        push_unique_action(&mut actions, "resolve workspace conflicts before closeout");
    } else if output
        .get("workspace_clean")
        .and_then(Value::as_bool)
        .is_some_and(|clean| !clean)
    {
        push_unique(&mut warning_reasons, "workspace_dirty");
        push_unique_action(&mut actions, "review workspace changes with show_changes");
    }

    if let Some(false) = hygiene_checked {
        push_unique(&mut warning_reasons, "hygiene_not_checked");
        push_unique_action(&mut actions, "run workspace_hygiene_check before closeout");
    }
    if output
        .get("hygiene_clean")
        .and_then(Value::as_bool)
        .is_some_and(|clean| !clean)
    {
        push_unique(&mut warning_reasons, "workspace_hygiene_findings");
        push_unique_action(&mut actions, "review workspace hygiene before closeout");
    }
    if count_field(output, "hygiene_secret_like_paths") > 0 {
        push_unique(&mut blocking_reasons, "sensitive_path_risk");
        push_unique_action(&mut actions, "review secret-like paths before closeout");
    }
    if output
        .get("hygiene_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        push_unique(&mut warning_reasons, "workspace_hygiene_truncated");
    }

    let jobs = output.get("jobs").unwrap_or(&Value::Null);
    if jobs
        .get("blocking_active_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push_unique(&mut blocking_reasons, "blocking_active_jobs");
        push_unique_action(&mut actions, "stop or await blocking active jobs");
    }
    if jobs
        .get("terminal_pending_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push_unique(&mut warning_reasons, "jobs_terminal_pending");
    }

    let validation = output.get("validation").unwrap_or(&Value::Null);
    let tool_failures = output.get("tool_failures").unwrap_or(&Value::Null);
    let expected_count = count_field(tool_failures, "expected_count");
    let unexpected_count = count_field(tool_failures, "unexpected_count");
    let expectation_mismatch_count = count_field(tool_failures, "expectation_mismatch_count");
    let unexpected_success_count = count_field(tool_failures, "unexpected_success_count");
    if actionable_unexpected_failure_count(tool_failures) > 0 {
        push_unique(&mut blocking_reasons, "unexpected_tool_failures");
        push_unique_action(
            &mut actions,
            "review unexpected failed tool calls before proceeding",
        );
    }
    if expectation_mismatch_count > 0 {
        push_unique(&mut blocking_reasons, "expectation_mismatches");
        push_unique(&mut integrity_errors, "expectation_mismatches");
        push_unique_action(
            &mut actions,
            "review result expectation mismatches before proceeding",
        );
    }
    if unexpected_success_count > 0 {
        push_unique(&mut integrity_warnings, "unexpected_successes");
        push_unique_action(
            &mut actions,
            "review failure expectations that unexpectedly succeeded",
        );
    }
    if expected_count > 0
        && unexpected_count == 0
        && expectation_mismatch_count == 0
        && unexpected_success_count == 0
    {
        push_unique(
            &mut informational_notes,
            "declared result expectations matched",
        );
    }
    if count_field(tool_failures, "non_actionable_unexpected_count") > 0 {
        push_unique(
            &mut informational_notes,
            "non-actionable failed tool calls are retained as historical/process evidence",
        );
    }
    if validation
        .pointer("/evidence_gaps/count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        push_unique(
            &mut informational_notes,
            "validation evidence gaps are retained separately from correctness failures",
        );
    }

    let validation_status = validation.get("status").and_then(Value::as_str);
    let current_validation_status = validation
        .pointer("/current_evidence/status")
        .and_then(Value::as_str)
        .or(validation_status);
    let resolved_failure_count = validation
        .pointer("/resolved_failures/count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unresolved_failure_count = validation
        .pointer("/unresolved_failures/count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let current_unresolved_failure_count = validation
        .pointer("/current_evidence/unresolved_failure_count")
        .and_then(Value::as_u64)
        .unwrap_or(unresolved_failure_count);
    let evidence_history_status = if validation_historical_failures_resolved(validation) {
        "mixed_resolved"
    } else {
        match validation_status {
            Some("mixed") => "mixed_unresolved",
            Some("failed") => "failed",
            _ if validation_historical_failures_unresolved(validation) => "mixed_unresolved",
            _ => "clean",
        }
    };

    match current_validation_status {
        Some("not_run") => {
            let review_evidence_total = output
                .get("review_evidence")
                .and_then(|review_evidence| review_evidence.get("total"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if review_evidence_total > 0 {
                push_unique(
                    &mut warning_reasons,
                    "validation_not_run_with_review_evidence",
                );
                push_unique_action(
                    &mut actions,
                    "decide whether task-appropriate validation is needed before closeout",
                );
            } else {
                push_unique(&mut warning_reasons, "validation_not_run");
                push_unique_action(
                    &mut actions,
                    "run validation or review before closeout when applicable",
                );
            }
        }
        Some("stale") => {
            push_unique(&mut warning_reasons, "validation_stale_after_changes");
            push_unique_action(
                &mut actions,
                "run task-appropriate validation when warranted",
            );
        }
        Some("failed") if current_unresolved_failure_count > 0 => {
            push_unique(&mut blocking_reasons, "validation_failed");
            push_unique_action(&mut actions, VALIDATION_IDENTITY_REUSE_ACTION);
        }
        Some("inconclusive") => {
            push_unique(&mut warning_reasons, "validation_inconclusive");
            push_unique_action(
                &mut actions,
                "run validation that proves the intended test assertion, or explicitly set require_tests=false when zero tests are intentional",
            );
        }
        Some("unknown") | None => {
            push_unique(&mut warning_reasons, "validation_unknown");
        }
        Some(_) => {}
    }
    if validation_historical_failures_resolved(validation) {
        push_unique(
            &mut informational_notes,
            "historical validation failures were resolved by later successful validation",
        );
    } else if validation_historical_failures_unresolved(validation)
        && current_unresolved_failure_count == 0
    {
        push_unique(
            &mut informational_notes,
            "historical validation failures remain without exact identity resolution but are not current workspace blockers",
        );
    }
    if validation_has_cargo_test_zero_tests(validation) {
        push_unique(&mut integrity_warnings, "cargo_test_zero_tests");
        push_unique_action(
            &mut actions,
            "cargo_test ran zero tests; verify the test filter or command",
        );
    }

    if actions.is_empty() {
        actions.push("proceed with handoff or closeout".to_string());
    }
    let task_status = if blocking_reasons.is_empty() {
        if warning_reasons.is_empty() {
            "pass"
        } else {
            "warn"
        }
    } else {
        "fail"
    };
    let evidence_integrity_status = if !integrity_errors.is_empty() {
        "error"
    } else if !integrity_warnings.is_empty() {
        "warning"
    } else {
        "clean"
    };
    let task_warning_reasons = warning_reasons.clone();
    for reason in &integrity_warnings {
        push_unique(&mut warning_reasons, *reason);
    }
    let legacy_status = if task_status == "fail" || evidence_integrity_status == "error" {
        "fail"
    } else if task_status == "warn" || evidence_integrity_status == "warning" {
        "warn"
    } else {
        "pass"
    };
    let verdict = json!({
        "status": legacy_status,
        "blocking": task_status == "fail" || evidence_integrity_status == "error",
        "blocking_reasons": blocking_reasons.clone(),
        "warning_reasons": warning_reasons.clone(),
        "suggested_next_actions": actions.clone(),
    });
    let executions = validation
        .get("events")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let validation_skipped = matches!(current_validation_status, Some("not_run" | "stale"))
        || validation
            .get("skipped")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let facts = json!({
        "work_performed": output.get("work_performed").cloned().unwrap_or_else(|| json!([])),
        "changed_paths": output.get("changed_paths").cloned().unwrap_or_else(|| json!([])),
        "executions": executions,
        "validations_passed": validation.get("successes").and_then(Value::as_u64).unwrap_or(0),
        "validations_failed": validation.get("failures").and_then(Value::as_u64).unwrap_or(0),
        "validations_skipped": {
            "count": u64::from(validation_skipped),
            "reason": validation.get("reason").cloned().unwrap_or(Value::Null),
        },
        "resolved_failures": validation.get("resolved_failures").cloned().unwrap_or_else(|| json!({"count": resolved_failure_count, "events": []})),
        "unresolved_failures": validation.get("unresolved_failures").cloned().unwrap_or_else(|| json!({"count": unresolved_failure_count, "events": []})),
        "workspace_state": {
            "checked": workspace_checked,
            "clean": output.get("workspace_clean").cloned().unwrap_or(Value::Null),
            "conflicts": workspace_conflicts,
            "hygiene_checked": hygiene_checked,
            "hygiene_clean": output.get("hygiene_clean").cloned().unwrap_or(Value::Null),
        },
        "active_jobs": output.get("jobs").cloned().unwrap_or_else(|| json!({})),
        "evidence_integrity": {
            "status": evidence_integrity_status,
            "error_reasons": integrity_errors,
            "warning_reasons": integrity_warnings,
        },
    });

    json!({
        "facts": facts,
        "hard_blockers": blocking_reasons,
        "advisories": warning_reasons,
        "task_outcome": {
            "status": task_status,
            "blocking": task_status == "fail",
            "blocking_reasons": verdict["blocking_reasons"],
            "warning_reasons": task_warning_reasons,
        },
        "evidence_history": {
            "status": evidence_history_status,
        },
        "evidence_integrity": {
            "status": evidence_integrity_status,
            "error_reasons": integrity_errors,
            "warning_reasons": integrity_warnings,
        },
        "informational_notes": informational_notes,
        "verdict": verdict,
    })
}

pub(crate) fn actionable_unexpected_failure_count(tool_failures: &Value) -> u64 {
    tool_failures
        .get("actionable_unexpected_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| count_field(tool_failures, "unexpected_count"))
}

#[derive(Debug, Clone)]
pub(crate) struct CloseoutEvidenceReconciliation {
    pub(crate) validation: Value,
    pub(crate) tool_failures: Value,
    pub(crate) actionable_unexpected_event_ids: HashSet<String>,
}

/// Reconcile immutable ledger history into the single current closeout view used
/// by both handoff and finish projections. Raw failure counts and validation
/// events remain intact; only their current actionability/status is projected.
pub(crate) fn reconcile_closeout_evidence(
    tool_failures: &Value,
    summary: &SessionSummary,
    validation: &Value,
) -> CloseoutEvidenceReconciliation {
    let validation = validation.clone();
    let current = super::validation_events::current_validation_evidence_for_session(summary, 100);
    let mut projected = tool_failures.clone();
    let raw_unexpected = count_field(tool_failures, "unexpected_count");
    let unexpected_events = canonical_tool_call_finished_events(&summary.events)
        .into_iter()
        .filter(|event| unexpected_failure_event(event))
        .collect::<Vec<_>>();
    let non_actionable_event_ids = unexpected_events
        .iter()
        .copied()
        .filter(|event| {
            current
                .non_actionable_tool_failure_event_ids
                .contains(&event.event_id)
                || unexpected_failure_is_proven_non_actionable(event)
        })
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();
    let non_actionable_unexpected = non_actionable_event_ids.len() as u64;
    let non_actionable_unexpected = non_actionable_unexpected.min(raw_unexpected);
    let actionable_unexpected_event_ids = unexpected_events
        .into_iter()
        .filter(|event| !non_actionable_event_ids.contains(&event.event_id))
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();
    projected["non_actionable_unexpected_count"] = json!(non_actionable_unexpected);
    projected["actionable_unexpected_count"] =
        json!(raw_unexpected.saturating_sub(non_actionable_unexpected));
    CloseoutEvidenceReconciliation {
        validation,
        tool_failures: projected,
        actionable_unexpected_event_ids,
    }
}

fn unexpected_failure_event(event: &SessionEvent) -> bool {
    event.kind == "tool_call_finished"
        && event.status.as_deref() == Some("failed")
        && event
            .failure_expectation_result
            .as_deref()
            .unwrap_or(TOOL_EXPECTATION_RESULT_UNEXPECTED_FAILURE)
            == TOOL_EXPECTATION_RESULT_UNEXPECTED_FAILURE
}

fn unexpected_failure_is_proven_non_actionable(event: &SessionEvent) -> bool {
    let effect = event.effect_evidence.as_ref();
    let execution_state = effect.and_then(|effect| effect.execution_state.as_deref());
    if event.failure_kind.as_deref() == Some("outcome_unknown")
        || event.error_kind.as_deref() == Some("outcome_unknown")
        || execution_state == Some("outcome_unknown")
    {
        return false;
    }
    // ExecutionPurpose is caller-declared evidence metadata, not effect-safety
    // proof. Once a generic shell/process command has started, a failed result
    // remains actionable regardless of whether its declared purpose was
    // diagnostic, validation, or anything else.
    if effect.and_then(|effect| effect.command_started) == Some(true)
        || matches!(
            execution_state,
            Some("started" | "completed" | "cancelled" | "timed_out")
        )
    {
        return false;
    }
    if effect.and_then(|effect| effect.command_started) == Some(false)
        || execution_state == Some("not_started")
    {
        return true;
    }
    let failure_evidence = runtime_tool_session_evidence_policy(&event.tool_name).failure;
    if effect.and_then(|effect| effect.state_changed) == Some(false)
        && ((!event.shell_like && !event.git_like)
            || failure_evidence == ToolFailureEvidence::ProvenNoStateChangeNonActionable)
    {
        return true;
    }
    event.read_like && !event.write_like && !event.shell_like && !event.git_like
}

fn install_compact_workflow_outcomes(target: &mut Value, outcomes: Value) {
    for field in [
        "facts",
        "hard_blockers",
        "advisories",
        "task_outcome",
        "evidence_history",
        "evidence_integrity",
        "informational_notes",
        "verdict",
    ] {
        target[field] = outcomes.get(field).cloned().unwrap_or(Value::Null);
    }
}

fn validation_historical_failures_resolved(validation: &Value) -> bool {
    validation
        .get("successes")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
        && validation
            .pointer("/historical_failures/resolved")
            .and_then(Value::as_bool)
            == Some(true)
        && validation
            .pointer("/historical_failures/unresolved")
            .and_then(Value::as_bool)
            == Some(false)
}

fn validation_historical_failures_unresolved(validation: &Value) -> bool {
    validation
        .pointer("/historical_failures/unresolved")
        .and_then(Value::as_bool)
        == Some(true)
}

fn count_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn push_unique<T>(values: &mut Vec<T>, value: T)
where
    T: PartialEq,
{
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn push_unique_action(actions: &mut Vec<String>, action: &str) {
    if !actions.iter().any(|existing| existing == action) {
        actions.push(action.to_string());
    }
}

/// Build a bounded list of suggested next actions based on the handoff state.
fn handoff_suggested_next_actions(output: &Value) -> Vec<String> {
    let mut actions = Vec::new();
    let push = |actions: &mut Vec<String>, action: &str| {
        if !actions.iter().any(|existing| existing == action) {
            actions.push(action.to_string());
        }
    };

    let tool_failures = output.get("tool_failures").unwrap_or(&Value::Null);
    let expectation_mismatch_count = tool_failures
        .get("expectation_mismatch_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unexpected_success_count = tool_failures
        .get("unexpected_success_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if actionable_unexpected_failure_count(tool_failures) > 0 {
        push(
            &mut actions,
            "review unexpected failed tool calls before proceeding",
        );
    }
    if expectation_mismatch_count > 0 {
        push(
            &mut actions,
            "review result expectation mismatches before proceeding",
        );
    }
    if unexpected_success_count > 0 {
        push(
            &mut actions,
            "review failure expectations that unexpectedly succeeded",
        );
    }
    let open_todos = output["counts"]["open_todos"].as_u64().unwrap_or(0);
    if open_todos > 0 {
        push(&mut actions, "address open todos");
    }
    let open_risks = output["counts"]["open_risks"].as_u64().unwrap_or(0);
    if open_risks > 0 {
        push(&mut actions, "mitigate open risks");
    }
    let open_questions = output["counts"]["open_questions"].as_u64().unwrap_or(0);
    if open_questions > 0 {
        push(&mut actions, "answer open questions");
    }
    if let Some(workspace) = output.get("workspace") {
        if workspace.get("git_available").and_then(Value::as_bool) == Some(true) {
            let clean = workspace
                .get("clean")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !clean {
                push(&mut actions, "review workspace changes with show_changes");
            }
        }
    }
    if let Some(checkpoints) = output.get("checkpoints") {
        let lkg_is_null = checkpoints
            .get("latest_last_known_good")
            .is_none_or(Value::is_null);
        if lkg_is_null {
            push(
                &mut actions,
                "consider creating a last_known_good checkpoint",
            );
        }
    }
    let validation = output.get("validation").unwrap_or(&Value::Null);
    if validation
        .pointer("/current_evidence/unresolved_failure_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            validation
                .pointer("/unresolved_failures/count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        })
        > 0
    {
        push(&mut actions, VALIDATION_IDENTITY_REUSE_ACTION);
    }
    if validation_has_cargo_test_zero_tests(validation) {
        push(
            &mut actions,
            "cargo_test ran zero tests; verify the test filter or command",
        );
    }
    if actions.is_empty() {
        push(
            &mut actions,
            "session is ready for handoff; proceed with the next task step",
        );
    }
    actions
}
