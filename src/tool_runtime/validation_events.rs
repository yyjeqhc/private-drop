//! Root-owned live validation adapter.
//!
//! Canonical validation planning/evidence semantics live in `webcodex-validation`;
//! this module retains only authorization, live Job materialization, SessionStore
//! mutation, and model-facing ToolResult composition.

use serde_json::{json, Value};
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;
use webcodex_tool_runtime_contracts::tool_audit::{
    assertion_validation_identity, is_structured_validation_target_identity,
    is_validation_execution_identity,
};
use webcodex_validation::{
    event_is_job_acceptance_only, validation_adapter_for_tool,
    validation_summary_for_session_events,
};
use webcodex_workflow_session::{
    canonical_tool_call_finished_events, safe_model_facing_assertion_name, SessionEvent,
    SessionSummary,
};

use super::session_context::{
    session_project_mismatch_result, unknown_session_result, SessionProjectMismatch,
};
use super::{ToolResult, ToolRuntime};
use crate::auth::AuthContext;

pub(crate) use webcodex_validation::{
    current_validation_evidence_for_session, event_observes_validation_activity,
    skipped_validation_summary, validation_summary_from_events,
    CurrentValidationEvidenceProjection,
};

const DEFAULT_PUBLIC_VALIDATION_EVENT_LIMIT: usize = 20;
const MAX_PUBLIC_VALIDATION_EVENT_LIMIT: usize = 100;
const PUBLIC_VALIDATION_SESSION_EVENT_LIMIT: usize = 200;

#[cfg(test)]
pub(crate) fn validation_summary_for_session(summary: &SessionSummary) -> Value {
    validation_summary_for_session_events(summary, &summary.events, 10)
}

impl ToolRuntime {
    pub(crate) async fn validation_summary_tool(
        &self,
        project: String,
        session_id: String,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "validation_summary", auth)
            .await
        {
            return result;
        }
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let Some(summary) = self
            .sessions
            .summary(&session_id, Some(PUBLIC_VALIDATION_SESSION_EVENT_LIMIT))
        else {
            return unknown_session_result(&session_id);
        };
        if summary.project.as_deref() != Some(resolved.resolved_id.as_str()) {
            let mismatch = SessionProjectMismatch {
                session_project: summary.project.unwrap_or_else(|| "<unscoped>".to_string()),
                request_project: resolved.resolved_id,
            };
            return session_project_mismatch_result(&session_id, "validation_summary", &mismatch);
        }
        let limit = limit
            .unwrap_or(DEFAULT_PUBLIC_VALIDATION_EVENT_LIMIT)
            .clamp(1, MAX_PUBLIC_VALIDATION_EVENT_LIMIT);
        let mut validation = self
            .validation_summary_for_session_with_jobs(&summary, limit, auth)
            .await;
        remove_public_validation_input_summaries(&mut validation);
        // Pure read-only projection derived only from the ledger validation
        // summary above. Never re-runs validation, mutates the ledger, or
        // changes the verdict; `unavailable` with a stable reason code when the
        // two validation attempts are not proven comparable.
        let validation_delta = super::continuation_feedback::validation_delta_value(&validation);
        ToolResult::ok(json!({
            "project": resolved.resolved_id,
            "session_id": session_id,
            "validation": validation,
            "validation_delta": validation_delta,
        }))
    }

    pub(crate) async fn validation_summary_for_session_with_jobs(
        &self,
        summary: &SessionSummary,
        limit: usize,
        auth: Option<&AuthContext>,
    ) -> Value {
        self.materialize_session_validation_job_terminals(summary, auth)
            .await;
        let refreshed = self
            .sessions
            .summary(
                &summary.session_id,
                Some(PUBLIC_VALIDATION_SESSION_EVENT_LIMIT),
            )
            .unwrap_or_else(|| summary.clone());
        let mut events = refreshed.events.clone();
        self.append_terminal_generic_job_validation_events(&refreshed, &mut events, auth)
            .await;
        events.sort_by_key(|event| {
            (
                event.timestamp,
                event.finished_at.unwrap_or(event.timestamp),
            )
        });
        validation_summary_for_session_events(&refreshed, &events, limit)
    }

    /// Preserve generic `run_job` and promoted `run_shell` validation evidence without mixing
    /// those shell Jobs into the durable structured-validation marker ledger. Structured
    /// validation Jobs carry explicit validation metadata and are materialized above; these
    /// generic executions remain a read-time projection from the retained acceptance event plus
    /// the authoritative terminal Job state.
    async fn append_terminal_generic_job_validation_events(
        &self,
        summary: &SessionSummary,
        events: &mut Vec<SessionEvent>,
        auth: Option<&AuthContext>,
    ) {
        let Some(project) = summary.project.as_deref() else {
            return;
        };
        let accepted = canonical_tool_call_finished_events(&summary.events)
            .into_iter()
            .filter(|event| {
                matches!(event.tool_name.as_str(), "run_job" | "run_shell")
                    && event.job_id.is_some()
                    && event_observes_validation_activity(event)
                    && event_is_job_acceptance_only(event)
            })
            .cloned()
            .collect::<Vec<_>>();

        for mut observed in accepted {
            let Some(job_id) = observed.job_id.as_deref() else {
                continue;
            };
            let status = self
                .job_status_for_auth(job_id.to_string(), false, auth)
                .await;
            if !status.success
                || status.output.get("terminal").and_then(Value::as_bool) != Some(true)
                || status.output.get("session_id").and_then(Value::as_str)
                    != Some(summary.session_id.as_str())
                || status.output.get("project").and_then(Value::as_str) != Some(project)
            {
                continue;
            }
            // A generic execution carrying structured validation metadata belongs to the durable
            // materialization path and must not be synthesized a second time here.
            if status
                .output
                .get("validation")
                .is_some_and(Value::is_object)
            {
                continue;
            }

            let log = self
                .job_log_for_auth(job_id.to_string(), None, Some(200), auth, None, None)
                .await;
            let job_status = status
                .output
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let job_lifecycle = RunnerJobLifecycle::from_wire(job_status).ok();
            let exit_code = status.output.get("exit_code").and_then(Value::as_i64);
            let succeeded =
                job_lifecycle == Some(RunnerJobLifecycle::Completed) && exit_code == Some(0);
            observed.status = Some(if succeeded { "succeeded" } else { "failed" }.to_string());
            observed.exit_code = exit_code;
            observed.started_at = status.output.get("started_at").and_then(Value::as_i64);
            observed.finished_at = status.output.get("ended_at").and_then(Value::as_i64);
            if let Some(completed_at) = observed.finished_at {
                observed.timestamp = completed_at;
            }
            observed.duration_ms = status.output.get("duration_ms").and_then(Value::as_u64);
            observed.failure_kind = (!succeeded).then(|| match job_lifecycle {
                Some(RunnerJobLifecycle::Timeout | RunnerJobLifecycle::TimedOut) => {
                    "timeout".to_string()
                }
                Some(RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled) => {
                    "cancelled".to_string()
                }
                Some(RunnerJobLifecycle::Lost) => "execution_lost".to_string(),
                _ => "command_exit_nonzero".to_string(),
            });

            let mut output = if log.success {
                log.output
            } else {
                json!({
                    "stdout_tail": "",
                    "stderr_tail": "",
                    "stdout_truncated": true,
                    "stderr_truncated": true,
                })
            };
            for field in ["purpose", "command_summary", "cwd", "shell", "executor"] {
                if output.get(field).is_none_or(Value::is_null) {
                    output[field] = observed
                        .validation_output_summary
                        .as_ref()
                        .and_then(|value| value.get(field))
                        .cloned()
                        .unwrap_or(Value::Null);
                }
            }
            output["execution_state"] = json!(match job_lifecycle {
                Some(RunnerJobLifecycle::Timeout | RunnerJobLifecycle::TimedOut) => "timed_out",
                Some(RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled) => "cancelled",
                Some(RunnerJobLifecycle::Lost) => "lost",
                _ => "completed",
            });
            output["exit_code"] = status
                .output
                .get("exit_code")
                .cloned()
                .unwrap_or(Value::Null);
            observed.validation_output_summary =
                super::sessions::execution_output_summary_for_tool_result(
                    &observed.tool_name,
                    &output,
                );
            if observed.validation_output_summary.is_some() {
                events.push(observed);
            }
        }
    }

    async fn materialize_session_validation_job_terminals(
        &self,
        summary: &SessionSummary,
        auth: Option<&AuthContext>,
    ) {
        let Some(project) = summary.project.as_deref() else {
            return;
        };
        self.materialize_validation_job_terminals_for_sessions(
            project,
            std::slice::from_ref(&summary.session_id),
            auth,
        )
        .await;
    }

    pub(crate) async fn materialize_validation_job_terminals_for_sessions(
        &self,
        project: &str,
        session_ids: &[String],
        auth: Option<&AuthContext>,
    ) {
        // Absence from `grouped` is eviction authority for the bounded durable
        // materialization marker set. Hold one runtime-shared ordering fence from
        // authoritative snapshot acquisition through every Session mutation so
        // a snapshot acquired later can never materialize first and then have a
        // still-retained marker removed by an older snapshot. The batch lock is
        // deliberately stronger than a per-Session lock because candidate
        // acquisition itself is batched across Sessions.
        #[cfg(test)]
        self.validation_terminal_reconciliation_test_hook
            .before_reconciliation_lock();
        let _reconciliation_guard = self.validation_terminal_reconciliation.lock().await;
        let mut grouped = self
            .validation_job_candidates_for_sessions(project, session_ids, auth)
            .await;
        #[cfg(test)]
        self.validation_terminal_reconciliation_test_hook
            .after_snapshot_acquired()
            .await;
        for session_id in session_ids {
            let Some(jobs) = grouped.remove(session_id) else {
                continue;
            };
            self.materialize_validation_job_candidates(project, session_id, &jobs, auth)
                .await;
        }
    }

    async fn materialize_validation_job_candidates(
        &self,
        project: &str,
        session_id: &str,
        jobs: &[Value],
        auth: Option<&AuthContext>,
    ) {
        let retained_terminal_job_ids = jobs
            .iter()
            .filter(|job| {
                job.get("status")
                    .and_then(Value::as_str)
                    .and_then(|status| RunnerJobLifecycle::from_wire(status).ok())
                    .is_some_and(RunnerJobLifecycle::is_terminal)
            })
            .filter_map(|job| job.get("job_id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        for job in jobs {
            let Some(job_id) = job.get("job_id").and_then(Value::as_str) else {
                continue;
            };
            let status_name = job
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !RunnerJobLifecycle::from_wire(status_name)
                .is_ok_and(RunnerJobLifecycle::is_terminal)
            {
                continue;
            }
            let status = self
                .job_status_for_auth(job_id.to_string(), false, auth)
                .await;
            if !status.success
                || status.output.get("terminal").and_then(Value::as_bool) != Some(true)
                || status.output.get("session_id").and_then(Value::as_str) != Some(session_id)
                || status.output.get("project").and_then(Value::as_str) != Some(project)
            {
                continue;
            }
            let validation = status.output.get("validation").and_then(Value::as_object);
            let structured_execution = job.get("structured_execution").and_then(Value::as_object);
            let terminal_status = status
                .output
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(status_name);
            let terminal_lifecycle = RunnerJobLifecycle::from_wire(terminal_status).ok();
            let (
                tool_name,
                validation_target_id,
                validation_tool,
                validation_passed,
                assertion_name,
            ) = if let Some(validation) = validation {
                let Some(tool_name) = validation
                    .get("tool")
                    .and_then(Value::as_str)
                    .filter(|tool| validation_adapter_for_tool(tool).is_some())
                else {
                    continue;
                };
                let Some(identity) = validation
                    .get("validation_target_id")
                    .and_then(Value::as_str)
                    .filter(|value| is_structured_validation_target_identity(value))
                else {
                    continue;
                };
                (
                    tool_name,
                    identity,
                    Some(tool_name),
                    validation.get("passed").and_then(Value::as_bool),
                    None,
                )
            } else {
                let Some(metadata) = structured_execution else {
                    continue;
                };
                let Some(source) = metadata
                    .get("execution_source")
                    .and_then(Value::as_str)
                    .filter(|source| matches!(*source, "run_process" | "run_script"))
                else {
                    continue;
                };
                let Some(identity) = metadata
                    .get("validation_identity")
                    .and_then(Value::as_str)
                    .filter(|identity| is_validation_execution_identity(identity))
                else {
                    continue;
                };
                let validation_tool = metadata
                    .get("validation_tool")
                    .and_then(Value::as_str)
                    .filter(|tool| validation_adapter_for_tool(tool).is_some());
                let assertion_name = metadata
                    .get("assertion_name")
                    .and_then(Value::as_str)
                    .and_then(|value| safe_model_facing_assertion_name(source, value))
                    .filter(|value| assertion_validation_identity(value) == identity);
                (source, identity, validation_tool, None, assertion_name)
            };
            let log = self
                .job_log_for_auth(job_id.to_string(), None, Some(200), auth, None, None)
                .await;
            let mut output = if log.success {
                log.output
            } else {
                json!({
                    "stdout_tail": "",
                    "stderr_tail": "",
                    "stdout_truncated": true,
                    "stderr_truncated": true,
                })
            };
            if let Some(validation) = validation {
                for field in [
                    "passed",
                    "warnings_count",
                    "errors_count",
                    "tests_detected",
                    "tests_run_count",
                    "tests_passed",
                    "tests_failed",
                    "zero_tests_run",
                    "require_tests",
                    "no_run",
                    "test_count_assertion",
                    "diagnostics",
                ] {
                    if let Some(value) = validation.get(field) {
                        output[field] = value.clone();
                    }
                }
            } else {
                let detected = super::jobs::detected_job_summary(
                    job.get("command_summary").and_then(Value::as_str),
                    job.get("purpose").and_then(Value::as_str),
                    terminal_status,
                    status.output.get("exit_code").and_then(Value::as_i64),
                    output
                        .get("stdout_tail")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    output
                        .get("stderr_tail")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
                for field in [
                    "tests_detected",
                    "tests_run_count",
                    "tests_passed",
                    "tests_failed",
                    "zero_tests_run",
                ] {
                    if let Some(value) = detected.get(field) {
                        output[field] = value.clone();
                    }
                }
            }
            for field in ["purpose", "command_summary", "cwd", "shell", "executor"] {
                if output.get(field).is_none_or(Value::is_null) {
                    output[field] = job.get(field).cloned().unwrap_or(Value::Null);
                }
            }
            if output.get("purpose").is_none_or(Value::is_null) {
                output["purpose"] = json!("validation");
            }
            if let Some(validation_tool) = validation_tool {
                output["validation_tool"] = json!(validation_tool);
            }
            output["execution_state"] = json!(match terminal_lifecycle {
                Some(RunnerJobLifecycle::Timeout | RunnerJobLifecycle::TimedOut) => "timed_out",
                Some(RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled) => "cancelled",
                Some(RunnerJobLifecycle::Lost) => "lost",
                _ => "completed",
            });
            output["exit_code"] = status
                .output
                .get("exit_code")
                .cloned()
                .unwrap_or(Value::Null);
            let validation_output_summary =
                super::sessions::execution_output_summary_for_tool_result(tool_name, &output);
            self.sessions.record_validation_job_terminal(
                session_id,
                job_id,
                &retained_terminal_job_ids,
                tool_name,
                super::sessions::session_tool_contract(tool_name),
                Some(project.to_string()),
                validation_target_id,
                assertion_name.as_deref(),
                terminal_status,
                status.output.get("exit_code").and_then(Value::as_i64),
                validation_passed,
                status.output.get("started_at").and_then(Value::as_i64),
                status.output.get("ended_at").and_then(Value::as_i64),
                status.output.get("duration_ms").and_then(Value::as_u64),
                validation_output_summary,
            );
        }
    }
}

fn remove_public_validation_input_summaries(validation: &mut Value) {
    for field in ["latest", "latest_success", "latest_failure"] {
        if let Some(event) = validation.get_mut(field).and_then(Value::as_object_mut) {
            event.remove("input_summary");
        }
    }
    if let Some(events) = validation.get_mut("events").and_then(Value::as_array_mut) {
        for event in events {
            if let Some(event) = event.as_object_mut() {
                event.remove("input_summary");
            }
        }
    }
}
