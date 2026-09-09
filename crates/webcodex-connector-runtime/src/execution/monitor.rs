use super::*;
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;
use webcodex_core::validation_bridge::sanitize_bridge_text;
use webcodex_core::validation_evidence::{PARSER_KIND, PARSER_VERSION};
use webcodex_store::MAX_ASSERTION_EVIDENCE_BYTES;
use webcodex_validation::{validation_adapter_for_tool, ValidationFailureEvidence};

fn durable_assertion_evidence(
    check: &str,
    recipe_identity: Option<&Value>,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Value {
    let tool = recipe_identity.and_then(|identity| {
        let checks = identity.get("semantic_checks")?.as_array()?;
        let index = checks.iter().position(|candidate| candidate == check)?;
        identity
            .get("tool_identities")?
            .as_array()?
            .get(index)?
            .as_str()
    });
    let (failure_kind, diagnostics) = tool
        .and_then(validation_adapter_for_tool)
        .map(|adapter| {
            let diagnostics = adapter.parse(stdout, stderr, true);
            let failure_kind = adapter.map_failure_kind(ValidationFailureEvidence {
                success: false,
                reported_failure_kind: Some("command_exit_nonzero"),
                exit_code: exit_code.map(i64::from),
                diagnostics: Some(&diagnostics),
                stdout_excerpt: stdout,
                stderr_excerpt: stderr,
            });
            (failure_kind, Some(diagnostics))
        })
        .unwrap_or(("process_exit", None));
    let parser = diagnostics.as_ref().map(|_| PARSER_KIND);
    let parser_version = diagnostics.as_ref().map(|_| PARSER_VERSION);
    let mut evidence = json!({
        "failed_check": check,
        "failure_kind": failure_kind,
        "exit_code": exit_code,
        "parser": parser,
        "parser_version": parser_version,
        "diagnostics": diagnostics
    });
    sanitize_evidence(&mut evidence);
    if serde_json::to_vec(&evidence).is_ok_and(|bytes| bytes.len() <= MAX_ASSERTION_EVIDENCE_BYTES)
    {
        evidence
    } else {
        json!({
            "failed_check": check,
            "failure_kind": failure_kind,
            "exit_code": exit_code,
            "parser": parser,
            "parser_version": parser_version,
            "diagnostics": null
        })
    }
}

fn sanitize_evidence(value: &mut Value) {
    match value {
        Value::String(text) => *text = sanitize_bridge_text(text),
        Value::Array(items) => items.iter_mut().for_each(sanitize_evidence),
        Value::Object(fields) => fields.values_mut().for_each(sanitize_evidence),
        _ => {}
    }
}

struct MonitorRegistration {
    execution_id: String,
    registry: Arc<Mutex<HashSet<String>>>,
}

impl Drop for MonitorRegistration {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.registry.lock() {
            registry.remove(&self.execution_id);
        }
    }
}

impl ExecutionService {
    pub(super) fn spawn_monitor(
        &self,
        task: ConnectorTaskSnapshot,
        execution_id: String,
        host: Arc<dyn ConnectorExecutionHost>,
        runner_access: RunnerAccess,
    ) -> bool {
        {
            let mut monitors = self.monitors.lock().unwrap();
            if !monitors.insert(execution_id.clone()) {
                return false;
            }
        }
        #[cfg(test)]
        self.monitor_starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let service = self.clone();
        let registration = MonitorRegistration {
            execution_id: execution_id.clone(),
            registry: self.monitors.clone(),
        };
        tokio::spawn(async move {
            let _registration = registration;
            service
                .monitor(task, execution_id, host, runner_access)
                .await;
        });
        true
    }

    async fn monitor(
        &self,
        task: ConnectorTaskSnapshot,
        execution_id: String,
        host: Arc<dyn ConnectorExecutionHost>,
        runner_access: RunnerAccess,
    ) {
        let mut status_failures = 0_u32;
        let mut first_status_failure = None;
        loop {
            let execution = match self.db.connector_execution(&execution_id) {
                Ok(execution) => execution,
                Err(error) => {
                    tracing::warn!(execution_id, error = %error, "execution monitor lost durable state");
                    return;
                }
            };
            let now = chrono::Utc::now().timestamp();
            if execution.is_terminal() {
                if execution.state == "cancelled" {
                    self.release_cancelled_workspace(task).await;
                }
                return;
            }
            let current = if execution.state == "queued" && now >= execution.queue_deadline {
                self.db
                    .request_connector_queue_timeout(&execution_id, now)
                    .unwrap_or(execution)
            } else {
                execution
            };
            if current.state == "cancel_requested" {
                let _ = self.dispatch_cancel(&task, &current, host.as_ref()).await;
            }
            match self
                .refresh_once(&task, &execution_id, &runner_access)
                .await
            {
                Ok(updated) => {
                    status_failures = 0;
                    first_status_failure = None;
                    if updated.is_terminal() {
                        if updated.state == "cancelled" {
                            self.release_cancelled_workspace(task).await;
                        }
                        return;
                    }
                }
                Err((failure_code, error)) => {
                    status_failures = status_failures.saturating_add(1);
                    let failure_started = first_status_failure.get_or_insert_with(Instant::now);
                    let degraded = match self.db.record_connector_execution_status_failure(
                        &execution_id,
                        failure_code,
                        chrono::Utc::now().timestamp(),
                    ) {
                        Ok(execution) => execution,
                        Err(store_error) => {
                            tracing::warn!(
                                execution_id,
                                error = %store_error,
                                "execution monitor could not persist degraded status"
                            );
                            return;
                        }
                    };
                    if failure_code == "workspace_provenance_mismatch" {
                        // Deterministic invariant failure: finishing now beats
                        // burning the grace window on identical retries.
                        let _ = self.db.finish_connector_execution(
                            &execution_id,
                            ConnectorExecutionFailure::Workspace {
                                code: failure_code,
                                evidence: error,
                            },
                            chrono::Utc::now().timestamp(),
                        );
                        return;
                    }
                    if failure_started.elapsed() >= self.monitor_timing.grace {
                        tracing::warn!(
                            execution_id,
                            error,
                            "executor terminal state became unknown"
                        );
                        let _ = self.db.finish_connector_execution(
                            &execution_id,
                            ConnectorExecutionFailure::Unknown(failure_code),
                            chrono::Utc::now().timestamp(),
                        );
                        return;
                    }
                    let delay = self.monitor_delay(&degraded, status_failures);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            let current = match self.db.connector_execution(&execution_id) {
                Ok(execution) => execution,
                Err(_) => return,
            };
            tokio::time::sleep(self.monitor_delay(&current, status_failures)).await;
        }
    }

    fn monitor_delay(&self, execution: &ConnectorExecution, status_failures: u32) -> Duration {
        if status_failures > 0 {
            let multiplier = 1_u32 << status_failures.saturating_sub(1).min(4);
            return (self.monitor_timing.fast_poll * multiplier)
                .min(self.monitor_timing.failure_poll_max);
        }
        if matches!(
            execution.state.as_str(),
            "accepted" | "starting" | "queued" | "cancel_requested"
        ) {
            return self.monitor_timing.fast_poll;
        }
        let now = chrono::Utc::now().timestamp();
        let last_progress = execution
            .last_output_at
            .or(execution.started_at)
            .unwrap_or(execution.submitted_at);
        if now.saturating_sub(last_progress) >= 10 {
            self.monitor_timing.silent_poll
        } else {
            self.monitor_timing.running_poll
        }
    }

    async fn refresh_once(
        &self,
        task: &ConnectorTaskSnapshot,
        execution_id: &str,
        runner_access: &RunnerAccess,
    ) -> Result<ConnectorExecution, (&'static str, String)> {
        let execution = self
            .db
            .connector_execution(execution_id)
            .map_err(|error| ("task_store_error", error.to_string()))?;
        let job_id = execution.executor_reference.as_deref().ok_or_else(|| {
            (
                "executor_reference_pending",
                "execution has no executor reference".to_string(),
            )
        })?;
        let (job, _, _, stdout_cursor, stderr_cursor, _wait) = self
            .runner_registry
            .job_log_for_auth(
                Some(runner_access),
                job_id,
                Some(execution.stdout_cursor),
                Some(execution.stderr_cursor),
                None,
                None,
                None,
            )
            .await
            .map_err(|error| ("executor_status_unavailable", error))?;
        let executor_failure_code = job.error.as_deref().and_then(executor_failure_code);
        let lifecycle = RunnerJobLifecycle::from_wire(&job.status).ok();
        let terminal_candidate = executor_failure_code.is_some()
            || lifecycle.is_some_and(RunnerJobLifecycle::is_terminal);
        let mcp_task_output_tail = if terminal_candidate {
            self.bounded_output_tail(&execution, runner_access).await
        } else {
            None
        };
        if let Some(output_tail) = mcp_task_output_tail.as_ref() {
            self.db
                .record_connector_mcp_task_output_tail(execution_id, output_tail)
                .map_err(|error| ("task_store_error", error.to_string()))?;
        }
        if lifecycle == Some(RunnerJobLifecycle::Lost) {
            return Err((
                "executor_status_unavailable",
                job.error
                    .unwrap_or_else(|| "executor job authority was lost".to_string()),
            ));
        }
        if !ConnectorExecution::executor_status_recognized(&job.status) {
            return Err((
                "executor_status_unrecognized",
                format!("executor returned unrecognized status '{}'", job.status),
            ));
        }
        let progress = job.validation_progress.as_ref();
        let check_completed = progress.map(|progress| progress.completed);
        let failed_check = progress.and_then(|progress| progress.failed_step.as_deref());
        let assertion_evidence = if execution.kind == "check" && failed_check.is_some() {
            let (_, full_stdout, full_stderr, _, _, _) = self
                .runner_registry
                .job_log_for_auth(Some(runner_access), job_id, None, None, None, None, None)
                .await
                .unwrap_or_else(|_| (job.clone(), None, None, 1, 1, Default::default()));
            let stdout = full_stdout.unwrap_or_default();
            let stderr = full_stderr.unwrap_or_default();
            failed_check.map(|check| {
                durable_assertion_evidence(
                    check,
                    execution.check_recipe.as_ref(),
                    job.exit_code,
                    &stdout,
                    &stderr,
                )
            })
        } else {
            None
        };
        let check_succeeded_completely = execution.kind == "check"
            && lifecycle == Some(RunnerJobLifecycle::Completed)
            && job.exit_code == Some(0)
            && progress.is_some_and(|progress| {
                progress.completed == execution.check_plan.len()
                    && progress.current_step.is_none()
                    && progress.failed_step.is_none()
            });
        let validated_workspace_sha256 = if check_succeeded_completely {
            let manager = self.workspace.clone();
            let precondition_task = task.clone();
            let current = tokio::task::spawn_blocking(move || {
                manager.action_precondition(&precondition_task)
            })
            .await
            .ok()
            .and_then(Result::ok);
            match current {
                Some(current)
                    if execution.check_workspace_sha256.as_deref() == Some(current.as_str()) =>
                {
                    Some(current)
                }
                Some(current) => {
                    // The checks passed; only the bookkeeping invariant broke.
                    // This is deterministic — grace retries cannot help — so
                    // fail with the honest category and the evidence the
                    // operator needs, instead of the transport/storage lies.
                    let manager = self.workspace.clone();
                    let sample_task = task.clone();
                    let untracked = tokio::task::spawn_blocking(move || {
                        manager.untracked_sample(&sample_task, 5)
                    })
                    .await
                    .unwrap_or_default();
                    return Err((
                        "workspace_provenance_mismatch",
                        workspace_provenance_mismatch_detail(
                            execution.check_workspace_sha256.as_deref(),
                            &current,
                            &untracked,
                        ),
                    ));
                }
                None => None,
            }
        } else {
            None
        };
        self.db
            .observe_connector_execution(
                execution_id,
                ConnectorExecutionObservation {
                    executor_status: &job.status,
                    stdout_cursor,
                    stderr_cursor,
                    exit_code: job.exit_code,
                    started_at: job.started_at,
                    finished_at: job.ended_at,
                    check_completed,
                    failed_check,
                    assertion_evidence: assertion_evidence.as_ref(),
                    validated_workspace_sha256: validated_workspace_sha256.as_deref(),
                    executor_failure_code,
                    mcp_task_output_tail: mcp_task_output_tail.as_ref(),
                    now: chrono::Utc::now().timestamp(),
                },
            )
            .map_err(|error| ("task_store_error", error.to_string()))
    }

    pub(super) async fn dispatch_cancel(
        &self,
        task: &ConnectorTaskSnapshot,
        execution: &ConnectorExecution,
        host: &dyn ConnectorExecutionHost,
    ) -> CancelDispatch {
        let Some(job_id) = execution.executor_reference.as_ref() else {
            return CancelDispatch::ReferencePending;
        };
        if host
            .stop_execution_job(task.execution_executor_ref.clone(), job_id.clone())
            .await
            .is_ok()
        {
            CancelDispatch::Sent
        } else {
            CancelDispatch::Failed
        }
    }
}

fn workspace_provenance_mismatch_detail(
    expected: Option<&str>,
    current: &str,
    untracked: &[String],
) -> String {
    let expected = expected
        .map(|sha| &sha[..sha.len().min(12)])
        .unwrap_or("unknown");
    let current = &current[..current.len().min(12)];
    if untracked.is_empty() {
        return format!(
            "the checks passed but the workspace fingerprint changed during validation \
             (expected {expected}, found {current}); no untracked files were detected. \
             Inspect or revert workspace changes made during validation, then rerun the \
             checks with a new operation_id"
        );
    }
    format!(
        "the checks passed but the workspace fingerprint changed during validation \
         (expected {expected}, found {current}); untracked files present: {}. Add a \
         .gitignore covering generated build artifacts, then rerun the checks with a new \
         operation_id",
        untracked.join(", ")
    )
}

fn validation_protocol_failure_code(error: &str) -> Option<&'static str> {
    let code = error
        .strip_prefix("executor protocol violation: ")?
        .split(':')
        .next()?;
    match code {
        "validation_progress_missing" => Some("validation_progress_missing"),
        "validation_progress_unexpected" => Some("validation_progress_unexpected"),
        "validation_progress_incomplete" => Some("validation_progress_incomplete"),
        "validation_progress_invalid" => Some("validation_progress_invalid"),
        "validation_plan_invalid" => Some("validation_plan_invalid"),
        _ => Some("validation_progress_invalid"),
    }
}

fn executor_failure_code(error: &str) -> Option<&'static str> {
    webcodex_core::runner_protocol::validation_infrastructure_failure_code(error)
        .or_else(|| validation_protocol_failure_code(error))
}

#[cfg(test)]
mod provenance_tests {
    use super::workspace_provenance_mismatch_detail;

    #[test]
    fn mismatch_detail_only_recommends_gitignore_for_untracked_paths() {
        let untracked = workspace_provenance_mismatch_detail(
            Some("aaaaaaaaaaaaaaaa"),
            "bbbbbbbbbbbbbbbb",
            &["target/junk.o".to_string()],
        );
        assert!(untracked.contains("target/junk.o"), "{untracked}");
        assert!(untracked.contains(".gitignore"), "{untracked}");

        let tracked =
            workspace_provenance_mismatch_detail(Some("aaaaaaaaaaaaaaaa"), "bbbbbbbbbbbbbbbb", &[]);
        assert!(
            tracked.contains("Inspect or revert workspace changes"),
            "{tracked}"
        );
        assert!(!tracked.contains(".gitignore"), "{tracked}");
    }
}
