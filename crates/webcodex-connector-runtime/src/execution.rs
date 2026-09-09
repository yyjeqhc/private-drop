//! Application boundary between durable Task executions and existing jobs.

mod monitor;

use super::workspace;
use crate::{ConnectorExecutionHost, ConnectorJobHostError, ConnectorJobRequest};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::time::Instant;
use webcodex_core::runner_protocol::ShellJobValidationStep;
use webcodex_runner_registry::{RunnerAccess, RunnerRegistry};
use webcodex_store::Database;
use webcodex_store::{
    ConnectorExecution, ConnectorExecutionFailure, ConnectorExecutionKind,
    ConnectorExecutionObservation, ConnectorExecutionReservation, ConnectorExecutionState,
    ConnectorTaskSnapshot, ConnectorTaskStoreError,
};

const DEFAULT_YIELD_MS: u64 = 8_000;
const CANCEL_YIELD_MS: u64 = 5_000;
const REVIEW_POLL_MS: u64 = 100;

#[derive(Clone)]
struct MonitorTiming {
    grace: Duration,
    fast_poll: Duration,
    running_poll: Duration,
    silent_poll: Duration,
    failure_poll_max: Duration,
}

impl Default for MonitorTiming {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(30),
            fast_poll: Duration::from_millis(100),
            running_poll: Duration::from_millis(500),
            silent_poll: Duration::from_secs(1),
            failure_poll_max: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelDispatch {
    ReferencePending,
    Sent,
    Failed,
}

#[cfg(test)]
pub(crate) struct ExecutionAttachGate {
    created: tokio::sync::Barrier,
    resume: tokio::sync::Barrier,
}

#[cfg(test)]
impl ExecutionAttachGate {
    pub(crate) fn new() -> Self {
        Self {
            created: tokio::sync::Barrier::new(2),
            resume: tokio::sync::Barrier::new(2),
        }
    }

    pub(crate) async fn wait_until_job_created(&self) {
        self.created.wait().await;
    }

    pub(crate) async fn release_attach(&self) {
        self.resume.wait().await;
    }
}

#[derive(Clone)]
pub(crate) struct ExecutionService {
    runner_registry: Arc<RunnerRegistry>,
    db: Arc<Database>,
    workspace: workspace::WorkspaceManager,
    yield_ms: u64,
    monitor_timing: MonitorTiming,
    monitors: Arc<Mutex<HashSet<String>>>,
    release_locks: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
    #[cfg(test)]
    monitor_starts: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    attach_gate: Option<Arc<ExecutionAttachGate>>,
}

pub(crate) struct ReviewState {
    pub task: ConnectorTaskSnapshot,
    pub execution: Option<ConnectorExecution>,
    pub heartbeat: bool,
}

impl ExecutionService {
    pub(crate) fn new(
        runner_registry: Arc<RunnerRegistry>,
        db: Arc<Database>,
        workspace: workspace::WorkspaceManager,
    ) -> Self {
        Self {
            runner_registry,
            db,
            workspace,
            yield_ms: DEFAULT_YIELD_MS,
            monitor_timing: MonitorTiming::default(),
            monitors: Arc::new(Mutex::new(HashSet::new())),
            release_locks: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            monitor_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            attach_gate: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_yield_ms(mut self, yield_ms: u64) -> Self {
        self.yield_ms = yield_ms.max(1);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_monitor_timing(mut self, grace_ms: u64, poll_ms: u64) -> Self {
        self.monitor_timing = MonitorTiming {
            grace: Duration::from_millis(grace_ms.max(1)),
            fast_poll: Duration::from_millis(poll_ms.max(1)),
            running_poll: Duration::from_millis(poll_ms.max(1)),
            silent_poll: Duration::from_millis(poll_ms.max(1)),
            failure_poll_max: Duration::from_millis(poll_ms.max(1)),
        };
        self
    }

    #[cfg(test)]
    pub(crate) fn with_attach_gate(mut self, gate: Arc<ExecutionAttachGate>) -> Self {
        self.attach_gate = Some(gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn monitor_start_count(&self) -> usize {
        self.monitor_starts
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn active_monitor_count(&self) -> usize {
        self.monitors.lock().unwrap().len()
    }

    pub(crate) fn reconcile_startup(
        &self,
        project_id: &str,
        now: i64,
    ) -> Result<(usize, usize), ConnectorTaskStoreError> {
        self.db.reconcile_connector_startup(project_id, now)
    }

    pub(crate) fn reserve(
        &self,
        task: &ConnectorTaskSnapshot,
        kind: &str,
        operation_id: &str,
        request_sha256: &str,
        check_plan: &[String],
        check_recipe: Option<&Value>,
        check_workspace_sha256: Option<&str>,
        timeout_secs: u64,
        now: i64,
    ) -> Result<ConnectorExecutionReservation, ConnectorTaskStoreError> {
        self.db.reserve_connector_execution(
            task,
            kind,
            operation_id,
            request_sha256,
            check_plan,
            check_recipe,
            check_workspace_sha256,
            now.saturating_add(timeout_secs as i64),
            now,
        )
    }

    pub(crate) async fn execute(
        &self,
        reservation: ConnectorExecutionReservation,
        task: ConnectorTaskSnapshot,
        command: String,
        cwd: Option<String>,
        timeout_secs: u64,
        host: Arc<dyn ConnectorExecutionHost>,
        runner_access: RunnerAccess,
        validation_steps: Vec<ShellJobValidationStep>,
    ) -> Result<ConnectorExecution, ConnectorTaskStoreError> {
        let execution = match reservation {
            ConnectorExecutionReservation::Existing(execution) => {
                if execution.is_active() && execution.executor_reference.is_some() {
                    self.spawn_monitor(
                        task.clone(),
                        execution.execution_id.clone(),
                        host.clone(),
                        runner_access.clone(),
                    );
                }
                return self
                    .wait_for_terminal_or_arm_continuation(&execution.execution_id, self.yield_ms)
                    .await;
            }
            ConnectorExecutionReservation::Created(execution) => {
                self.db.start_connector_execution(
                    &execution.execution_id,
                    chrono::Utc::now().timestamp(),
                )?
            }
        };
        if execution.state != ConnectorExecutionState::Starting {
            return Ok(execution);
        }
        let submission = match host
            .start_execution_job(ConnectorJobRequest {
                project: task.execution_executor_ref.clone(),
                command,
                timeout_secs,
                cwd,
                validation_steps,
            })
            .await
        {
            Ok(submission) => submission,
            Err(ConnectorJobHostError::Rejected(_)) => {
                return self.db.finish_connector_execution(
                    &execution.execution_id,
                    ConnectorExecutionFailure::Submission("executor_rejected"),
                    chrono::Utc::now().timestamp(),
                )
            }
            Err(ConnectorJobHostError::Adapter(_)) => {
                return self.db.finish_connector_execution(
                    &execution.execution_id,
                    ConnectorExecutionFailure::Submission("execution_adapter_error"),
                    chrono::Utc::now().timestamp(),
                )
            }
            Err(ConnectorJobHostError::OutcomeUnknown(_)) => {
                return self.db.finish_connector_execution(
                    &execution.execution_id,
                    ConnectorExecutionFailure::Unknown("submission_transport_unknown"),
                    chrono::Utc::now().timestamp(),
                )
            }
        };
        #[cfg(test)]
        if let Some(gate) = &self.attach_gate {
            gate.created.wait().await;
            gate.resume.wait().await;
        }
        let attached = self.db.attach_connector_executor(
            &execution.execution_id,
            &submission.job_id,
            &submission.status,
            chrono::Utc::now().timestamp(),
        )?;
        if attached.state == ConnectorExecutionState::CancelRequested
            && self.dispatch_cancel(&task, &attached, host.as_ref()).await == CancelDispatch::Failed
        {
            return self.db.finish_connector_execution(
                &execution.execution_id,
                ConnectorExecutionFailure::Unknown("cancel_transport_unknown"),
                chrono::Utc::now().timestamp(),
            );
        }
        if attached.is_terminal() {
            return Ok(attached);
        }
        self.spawn_monitor(task, execution.execution_id.clone(), host, runner_access);
        self.wait_for_terminal_or_arm_continuation(&execution.execution_id, self.yield_ms)
            .await
    }

    pub(crate) async fn cancel_task(
        &self,
        task: ConnectorTaskSnapshot,
        reason: Option<&str>,
        host: Arc<dyn ConnectorExecutionHost>,
        runner_access: RunnerAccess,
    ) -> Result<Option<ConnectorExecution>, ConnectorTaskStoreError> {
        let requested = self.db.request_connector_execution_cancel(
            &task,
            reason,
            chrono::Utc::now().timestamp(),
        )?;
        let Some(mut execution) = requested else {
            self.release_cancelled_workspace(task).await;
            return Ok(None);
        };
        if execution.is_terminal() {
            self.release_cancelled_workspace(task).await;
            return Ok(Some(execution));
        }
        match self.dispatch_cancel(&task, &execution, host.as_ref()).await {
            CancelDispatch::ReferencePending => return Ok(Some(execution)),
            CancelDispatch::Failed => {
                if let Some(output_tail) =
                    self.bounded_output_tail(&execution, &runner_access).await
                {
                    let _ = self.db.record_connector_mcp_task_output_tail(
                        &execution.execution_id,
                        &output_tail,
                    );
                }
                execution = self.db.finish_connector_execution(
                    &execution.execution_id,
                    ConnectorExecutionFailure::Unknown("cancel_transport_unknown"),
                    chrono::Utc::now().timestamp(),
                )?;
            }
            CancelDispatch::Sent => {
                self.spawn_monitor(
                    task.clone(),
                    execution.execution_id.clone(),
                    host,
                    runner_access,
                );
                execution = self
                    .wait_for_terminal(&execution.execution_id, CANCEL_YIELD_MS)
                    .await?;
            }
        }
        if execution.state == ConnectorExecutionState::Cancelled {
            self.release_cancelled_workspace(task).await;
        }
        Ok(Some(execution))
    }

    pub(crate) async fn wait_for_review(
        &self,
        initial_task: ConnectorTaskSnapshot,
        after_cursor: Option<i64>,
        wait_ms: u64,
    ) -> Result<ReviewState, ConnectorTaskStoreError> {
        let initial = self.review_state(initial_task)?;
        if wait_ms == 0
            || after_cursor.is_some_and(|cursor| initial.task.event_cursor > cursor)
            || initial
                .execution
                .as_ref()
                .is_some_and(ConnectorExecution::is_terminal)
        {
            return Ok(initial);
        }
        let signature = execution_signature(initial.execution.as_ref());
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining.min(Duration::from_millis(REVIEW_POLL_MS))).await;
            }
            let mut current = self.review_state(initial.task.clone())?;
            let timed_out = Instant::now() >= deadline;
            if timed_out
                || current.task.event_cursor > after_cursor.unwrap_or(initial.task.event_cursor)
                || execution_signature(current.execution.as_ref()) != signature
                || current
                    .execution
                    .as_ref()
                    .is_some_and(ConnectorExecution::is_terminal)
            {
                current.heartbeat = timed_out;
                return Ok(current);
            }
        }
    }

    fn review_state(
        &self,
        task: ConnectorTaskSnapshot,
    ) -> Result<ReviewState, ConnectorTaskStoreError> {
        let task =
            self.db
                .connector_task(&task.task_id, &task.project_id, &task.owner_subject_id)?;
        let execution = self.db.latest_connector_execution(
            &task.task_id,
            &task.project_id,
            &task.owner_subject_id,
            None,
        )?;
        Ok(ReviewState {
            task,
            execution,
            heartbeat: false,
        })
    }

    async fn wait_for_terminal_or_arm_continuation(
        &self,
        execution_id: &str,
        wait_ms: u64,
    ) -> Result<ConnectorExecution, ConnectorTaskStoreError> {
        let execution = self.wait_for_terminal(execution_id, wait_ms).await?;
        if execution.is_terminal() {
            return Ok(execution);
        }
        self.db
            .arm_connector_terminal_continuation(execution_id, chrono::Utc::now().timestamp())
    }

    pub(crate) async fn wait_for_terminal(
        &self,
        execution_id: &str,
        wait_ms: u64,
    ) -> Result<ConnectorExecution, ConnectorTaskStoreError> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let execution = self.db.connector_execution(execution_id)?;
            if execution.is_terminal() || Instant::now() >= deadline {
                return Ok(execution);
            }
            tokio::time::sleep(Duration::from_millis(REVIEW_POLL_MS.min(wait_ms.max(1)))).await;
        }
    }

    pub(super) async fn release_cancelled_workspace(&self, task: ConnectorTaskSnapshot) {
        let release_lock = {
            let mut locks = self.release_locks.lock().unwrap();
            locks
                .get(&task.task_id)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(task.task_id.clone(), Arc::downgrade(&lock));
                    lock
                })
        };
        let guard = release_lock.lock().await;
        let warning = if task.isolated {
            let manager = self.workspace.clone();
            let release_task = task.clone();
            match tokio::task::spawn_blocking(move || manager.release_task_workspace(&release_task))
                .await
            {
                Ok(warning) => warning,
                Err(error) => Some(format!("workspace release task failed: {error}")),
            }
        } else {
            None
        };
        if let Some(ref warning) = warning {
            tracing::warn!(task_id = %task.task_id, warning, "cancelled workspace release was incomplete");
        }
        drop(guard);
        let mut locks = self.release_locks.lock().unwrap();
        if Arc::strong_count(&release_lock) == 1
            && locks
                .get(&task.task_id)
                .is_some_and(|lock| lock.ptr_eq(&Arc::downgrade(&release_lock)))
        {
            locks.remove(&task.task_id);
        }
    }

    pub(super) async fn bounded_output_tail(
        &self,
        execution: &ConnectorExecution,
        runner_access: &RunnerAccess,
    ) -> Option<Value> {
        let job_id = execution.executor_reference.as_deref()?;
        self.runner_registry
            .job_log_for_auth(
                Some(runner_access),
                job_id,
                None,
                None,
                Some(200),
                None,
                None,
            )
            .await
            .ok()
            .map(|(_, stdout, stderr, _, _, _)| {
                json!({
                    "stdout": stdout.unwrap_or_default(),
                    "stderr": stderr.unwrap_or_default(),
                    "bounded": true
                })
            })
    }

    pub(crate) async fn projection(
        &self,
        execution: &ConnectorExecution,
        runner_access: &RunnerAccess,
        include_output_tail: bool,
    ) -> Value {
        let output_tail = if include_output_tail {
            self.bounded_output_tail(execution, runner_access).await
        } else {
            None
        };
        execution_projection(execution, chrono::Utc::now().timestamp(), output_tail)
    }

    pub(crate) fn durable_task_projection(&self, execution: &ConnectorExecution) -> Value {
        let projection_at = if execution.is_terminal() {
            execution
                .finished_at
                .or(execution.last_output_at)
                .or(execution.started_at)
                .or(execution.queued_at)
                .unwrap_or(execution.submitted_at)
        } else {
            chrono::Utc::now().timestamp()
        };
        // MCP Tasks polling never re-reads Runner logs. Once terminal, the
        // exact bounded/redacted tail captured at the durable terminal boundary
        // is part of the replay-stable task result.
        let output_tail = execution
            .mcp_task_result_is_finalized()
            .then(|| execution.mcp_task_output_tail.clone())
            .flatten();
        execution_projection(execution, projection_at, output_tail)
    }
}

pub(crate) fn execution_projection(
    execution: &ConnectorExecution,
    now: i64,
    output_tail: Option<Value>,
) -> Value {
    let last_progress_at = execution
        .last_output_at
        .or(execution.started_at)
        .or(execution.queued_at)
        .unwrap_or(execution.submitted_at);
    let capability_outcome = match execution.state {
        ConnectorExecutionState::Succeeded => "completed",
        ConnectorExecutionState::Failed => "failed",
        ConnectorExecutionState::Cancelled => "cancelled",
        ConnectorExecutionState::Interrupted | ConnectorExecutionState::Unknown => {
            "needs_attention"
        }
        ConnectorExecutionState::Accepted
        | ConnectorExecutionState::Queued
        | ConnectorExecutionState::Starting
        | ConnectorExecutionState::Running
        | ConnectorExecutionState::CancelRequested => "in_progress",
    };
    let queue_reason = if execution.failure_source.as_deref() == Some("queue") {
        Some("queue_deadline")
    } else if execution.state == ConnectorExecutionState::Queued {
        Some("executor_queue")
    } else {
        None
    };
    json!({
        "execution_id": execution.execution_id,
        "operation_id": execution.operation_id,
        "kind": execution.kind,
        "submission_status": if execution.failure_source.as_deref() == Some("submission") {
            "rejected"
        } else {
            "accepted"
        },
        "execution_status": execution.state,
        "exit_code": execution.exit_code,
        "terminal_reason": execution.terminal_reason,
        "failure_source": execution.failure_source,
        "failure_code": execution.failure_code,
        "observation_status": if execution.first_status_failure_at.is_some() && execution.is_active() {
            "degraded"
        } else {
            "available"
        },
        "first_status_failure_at": execution.first_status_failure_at,
        "last_successful_observation_at": execution.last_successful_observation_at,
        "status_failure_code": execution.status_failure_code,
        "assertion_status": assertion_status(execution),
        "assertion_evidence": execution.assertion_evidence,
        "checks": check_results(execution),
        "recipe": recipe_projection(execution),
        "capability_outcome": capability_outcome,
        "queued_at": execution.queued_at,
        "queue_age_ms": execution.queued_at.map(|queued| now.saturating_sub(queued) * 1000),
        "queue_reason": queue_reason,
        "blocker_execution_id": execution.blocks_finish().then_some(&execution.execution_id),
        "started_at": execution.started_at,
        "finished_at": execution.finished_at,
        "last_progress_at": last_progress_at,
        "silent_for_ms": now.saturating_sub(last_progress_at) * 1000,
        "stdout_cursor": execution.stdout_cursor,
        "stderr_cursor": execution.stderr_cursor,
        "output_tail": output_tail,
        "blocking": execution.blocks_finish(),
        "next_action": execution_next_action(execution)
    })
}

fn recipe_projection(execution: &ConnectorExecution) -> Value {
    let Some(identity) = execution.check_recipe.as_ref() else {
        return Value::Null;
    };
    json!({
        "id": identity.get("recipe_id"),
        "version": identity.get("recipe_version"),
        "root": identity.get("recipe_root_relative"),
        "checks": identity.get("semantic_checks")
    })
}

fn assertion_status(execution: &ConnectorExecution) -> &'static str {
    if execution.kind != ConnectorExecutionKind::Check {
        return "not_run";
    }
    match execution.state {
        ConnectorExecutionState::Succeeded => "passed",
        ConnectorExecutionState::Failed if execution.failure_source.as_deref() == Some("check") => {
            "failed"
        }
        ConnectorExecutionState::Accepted
        | ConnectorExecutionState::Queued
        | ConnectorExecutionState::Starting
        | ConnectorExecutionState::Running
        | ConnectorExecutionState::CancelRequested => "in_progress",
        ConnectorExecutionState::Failed
        | ConnectorExecutionState::Cancelled
        | ConnectorExecutionState::Interrupted
        | ConnectorExecutionState::Unknown => "not_run",
    }
}

fn check_results(execution: &ConnectorExecution) -> Value {
    if execution.kind != ConnectorExecutionKind::Check {
        return Value::Null;
    }
    let assertion = assertion_status(execution);
    Value::Array(
        execution
            .check_plan
            .iter()
            .enumerate()
            .map(|(index, check)| {
                let status = if index < execution.check_completed {
                    "passed"
                } else if assertion == "failed"
                    && execution.failed_check.as_deref() == Some(check.as_str())
                {
                    "failed"
                } else if index == execution.check_completed && assertion == "in_progress" {
                    "in_progress"
                } else {
                    "not_run"
                };
                json!({ "check": check, "status": status })
            })
            .collect(),
    )
}

fn execution_next_action(execution: &ConnectorExecution) -> &'static str {
    if execution.failure_source.as_deref() == Some("executor")
        && execution
            .failure_code
            .as_deref()
            .is_some_and(|code| code.starts_with("validation_"))
    {
        return "upgrade_agent_and_rerun_checks";
    }
    if execution.failure_code.as_deref() == Some("workspace_provenance_mismatch") {
        return "inspect_workspace_changes_then_rerun_checks";
    }
    match execution.state {
        ConnectorExecutionState::Accepted
        | ConnectorExecutionState::Queued
        | ConnectorExecutionState::Starting
        | ConnectorExecutionState::Running => "review_or_cancel",
        ConnectorExecutionState::CancelRequested => "wait_for_cancellation",
        ConnectorExecutionState::Succeeded | ConnectorExecutionState::Failed => {
            "continue_or_finish"
        }
        ConnectorExecutionState::Cancelled => "start_a_new_task",
        ConnectorExecutionState::Interrupted => "resume_or_reject_on_the_host",
        ConnectorExecutionState::Unknown => "inspect_executor_state_before_continuing",
    }
}

fn execution_signature(
    execution: Option<&ConnectorExecution>,
) -> Option<(ConnectorExecutionState, usize, usize, Option<i64>)> {
    execution.map(|execution| {
        (
            execution.state,
            execution.stdout_cursor,
            execution.stderr_cursor,
            execution.first_status_failure_at,
        )
    })
}

#[cfg(test)]
mod service_tests {
    use super::*;
    use crate::{
        ConnectorHostFuture, ConnectorJobSubmission, ConnectorProjectRegistration,
        ConnectorToolFailure, ConnectorToolRequest,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use webcodex_store::{ConnectorBinding, NewConnectorTask};

    #[derive(Default)]
    struct TestHost {
        starts: AtomicUsize,
        stops: AtomicUsize,
        stop_unknown: AtomicBool,
    }

    impl ConnectorExecutionHost for TestHost {
        fn invoke_tool(
            &self,
            _request: ConnectorToolRequest,
        ) -> ConnectorHostFuture<'_, Result<Value, ConnectorToolFailure>> {
            Box::pin(async { Ok(json!({})) })
        }

        fn register_isolated_project(
            &self,
            _request: ConnectorProjectRegistration,
        ) -> ConnectorHostFuture<'_, Result<(), ConnectorJobHostError>> {
            Box::pin(async { Ok(()) })
        }

        fn start_execution_job(
            &self,
            _request: ConnectorJobRequest,
        ) -> ConnectorHostFuture<'_, Result<ConnectorJobSubmission, ConnectorJobHostError>>
        {
            let number = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                Ok(ConnectorJobSubmission {
                    job_id: format!("job-{number}"),
                    status: "running".to_string(),
                })
            })
        }

        fn stop_execution_job(
            &self,
            _project: String,
            _job_id: String,
        ) -> ConnectorHostFuture<'_, Result<(), ConnectorJobHostError>> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            let unknown = self.stop_unknown.load(Ordering::SeqCst);
            Box::pin(async move {
                if unknown {
                    Err(ConnectorJobHostError::OutcomeUnknown(Some(
                        "transport uncertain".to_string(),
                    )))
                } else {
                    Ok(())
                }
            })
        }
    }

    struct ServiceFixture {
        _temp: tempfile::TempDir,
        db: Arc<Database>,
        task: ConnectorTaskSnapshot,
        service: ExecutionService,
        access: RunnerAccess,
    }

    fn fixture() -> ServiceFixture {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let db = Arc::new(Database::open(&temp.path().join("connector.db")).unwrap());
        db.ensure_connector_binding(ConnectorBinding {
            project_id: "wc_proj_1234567890",
            project_name: "project",
            workspace_id: "wc_ws_1234567890",
            executor_ref: "agent:hosted:project",
            subject_id: "project:wc_pgrant_1111111111111111",
            profile: "personal",
            now: 1,
        })
        .unwrap();
        let root = project.to_string_lossy().into_owned();
        let task = db
            .start_connector_task(NewConnectorTask {
                task_id: "wc_task_0123456789abcdef0123456789abcdef",
                run_id: "wc_run_0123456789abcdef0123456789abcdef",
                project_id: "wc_proj_1234567890",
                workspace_id: "wc_ws_1234567890",
                subject_id: "project:wc_pgrant_1111111111111111",
                goal: "execution test",
                mode: "read_only",
                target_executor_ref: "agent:hosted:project",
                execution_executor_ref: "agent:hosted:project",
                target_root: &root,
                execution_root: &root,
                baseline_commit: None,
                baseline_tree: None,
                isolated: false,
                now: 2,
            })
            .unwrap();
        let context = crate::ConnectorContext {
            project_id: "wc_proj_1234567890".to_string(),
            project_name: "project".to_string(),
            workspace_id: "wc_ws_1234567890".to_string(),
            executor_project: "agent:hosted:project".to_string(),
            executor_root: root,
            runs_root: temp.path().join("runs").to_string_lossy().into_owned(),
            results_root: temp.path().join("results").to_string_lossy().into_owned(),
            project_registry_dir: temp
                .path()
                .join("agent/project-registry")
                .to_string_lossy()
                .into_owned(),
            profile: "personal".to_string(),
            project_grant_id: "wc_pgrant_1111111111111111".to_string(),
        };
        let workspace = workspace::WorkspaceManager::new(&context).unwrap();
        let service =
            ExecutionService::new(Arc::new(RunnerRegistry::default()), db.clone(), workspace);
        ServiceFixture {
            _temp: temp,
            db,
            task,
            service,
            access: RunnerAccess {
                global_visibility: false,
                owner_bypass: false,
                username: Some("owner".to_string()),
                group: None,
            },
        }
    }

    fn reserve(fx: &ServiceFixture, operation_id: &str) -> ConnectorExecutionReservation {
        fx.service
            .reserve(
                &fx.task,
                "command",
                operation_id,
                &format!("hash-{operation_id}"),
                &[],
                None,
                None,
                30,
                3,
            )
            .unwrap()
    }

    #[tokio::test]
    async fn cancel_before_executor_reference_stays_reference_pending() {
        let fx = fixture();
        let reservation = reserve(&fx, "pending-cancel");
        let execution = match reservation {
            ConnectorExecutionReservation::Created(execution) => fx
                .db
                .start_connector_execution(&execution.execution_id, 4)
                .unwrap(),
            _ => panic!("fresh operation must create"),
        };
        assert!(execution.executor_reference.is_none());
        let host = Arc::new(TestHost::default());
        let cancelled = fx
            .service
            .cancel_task(fx.task.clone(), None, host.clone(), fx.access.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.state, ConnectorExecutionState::CancelRequested);
        assert!(cancelled.executor_reference.is_none());
        assert_eq!(host.stops.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn late_attach_after_cancel_dispatches_exact_compensating_stop() {
        let mut fx = fixture();
        let gate = Arc::new(ExecutionAttachGate::new());
        fx.service = fx
            .service
            .clone()
            .with_yield_ms(5)
            // This assertion covers the late-attach compensating dispatch, not
            // the monitor's recurring cancel retry cadence. Keep a second
            // monitor poll outside the test window so CI scheduling cannot add
            // an arbitrary third stop before the assertion runs.
            .with_monitor_timing(60_000, 60_000)
            .with_attach_gate(gate.clone());
        let host = Arc::new(TestHost::default());
        let reservation = reserve(&fx, "late-attach");
        let service = fx.service.clone();
        let task = fx.task.clone();
        let access = fx.access.clone();
        let host_for_execute = host.clone();
        let execute = tokio::spawn(async move {
            service
                .execute(
                    reservation,
                    task,
                    "echo hi".to_string(),
                    None,
                    30,
                    host_for_execute,
                    access,
                    Vec::new(),
                )
                .await
        });
        gate.wait_until_job_created().await;
        let pending = fx
            .service
            .cancel_task(fx.task.clone(), None, host.clone(), fx.access.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.state, ConnectorExecutionState::CancelRequested);
        assert!(pending.executor_reference.is_none());
        assert_eq!(host.stops.load(Ordering::SeqCst), 0);
        gate.release_attach().await;
        let returned = execute.await.unwrap().unwrap();
        let durable = fx.db.connector_execution(&returned.execution_id).unwrap();
        assert_eq!(durable.executor_reference.as_deref(), Some("job-1"));
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
        assert!((1..=2).contains(&host.stops.load(Ordering::SeqCst)));
        assert_eq!(fx.service.monitor_start_count(), 1);
    }

    #[tokio::test]
    async fn retry_of_active_execution_does_not_start_a_second_monitor_or_job() {
        let mut fx = fixture();
        fx.service = fx
            .service
            .clone()
            .with_yield_ms(5)
            .with_monitor_timing(500, 5);
        let host = Arc::new(TestHost::default());
        let first = fx
            .service
            .execute(
                reserve(&fx, "one-monitor"),
                fx.task.clone(),
                "echo hi".to_string(),
                None,
                30,
                host.clone(),
                fx.access.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(first.is_active());
        assert_eq!(fx.service.monitor_start_count(), 1);
        let retry = fx
            .service
            .reserve(
                &fx.task,
                "command",
                "one-monitor",
                "hash-one-monitor",
                &[],
                None,
                None,
                30,
                5,
            )
            .unwrap();
        let _ = fx
            .service
            .execute(
                retry,
                fx.task.clone(),
                "echo hi".to_string(),
                None,
                30,
                host.clone(),
                fx.access.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(host.starts.load(Ordering::SeqCst), 1);
        assert_eq!(fx.service.monitor_start_count(), 1);
        assert_eq!(fx.service.active_monitor_count(), 1);
    }

    #[tokio::test]
    async fn stop_transport_uncertainty_becomes_unknown_not_success() {
        let fx = fixture();
        let reservation = reserve(&fx, "stop-unknown");
        let execution = match reservation {
            ConnectorExecutionReservation::Created(execution) => fx
                .db
                .start_connector_execution(&execution.execution_id, 4)
                .unwrap(),
            _ => panic!("fresh operation must create"),
        };
        let attached = fx
            .db
            .attach_connector_executor(&execution.execution_id, "job-stop", "running", 5)
            .unwrap();
        assert_eq!(attached.state, ConnectorExecutionState::Running);
        let host = Arc::new(TestHost::default());
        host.stop_unknown.store(true, Ordering::SeqCst);
        let cancelled = fx
            .service
            .cancel_task(fx.task.clone(), None, host, fx.access.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.state, ConnectorExecutionState::Unknown);
        assert_eq!(
            cancelled.failure_code.as_deref(),
            Some("cancel_transport_unknown")
        );
        assert!(cancelled.blocks_finish());
    }
}
