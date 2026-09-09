use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;

pub const MAX_ASSERTION_EVIDENCE_BYTES: usize = 16 * 1024;
// Two already-bounded 256 KiB UTF-8 streams can expand substantially under
// JSON escaping. Keep the persisted MCP task snapshot hard-bounded while
// allowing any ordinary Connector output-tail projection to serialize.
pub(crate) const MAX_MCP_TASK_OUTPUT_TAIL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorExecutionContinuationIntent {
    None,
    ArmedForTerminal,
}

impl ConnectorExecutionContinuationIntent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ArmedForTerminal => "armed_for_terminal",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "armed_for_terminal" => Ok(Self::ArmedForTerminal),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                format!("unsupported connector continuation intent: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorTerminalContinuationDeliveryState {
    Unclaimed,
    Claimed,
    Dispatching,
    Delivered,
    DeliveryUnknown,
}

impl ConnectorTerminalContinuationDeliveryState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unclaimed => "unclaimed",
            Self::Claimed => "claimed",
            Self::Dispatching => "dispatching",
            Self::Delivered => "delivered",
            Self::DeliveryUnknown => "delivery_unknown",
        }
    }

    fn from_db(value: &str, index: usize) -> rusqlite::Result<Self> {
        match value {
            "unclaimed" => Ok(Self::Unclaimed),
            "claimed" => Ok(Self::Claimed),
            "dispatching" => Ok(Self::Dispatching),
            "delivered" => Ok(Self::Delivered),
            "delivery_unknown" => Ok(Self::DeliveryUnknown),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                format!("unsupported connector terminal continuation delivery state: {other}")
                    .into(),
            )),
        }
    }
}

// A2 stops at the durable claim/delivery boundary; the next host-adapter slice
// will become the first production reader of the returned claim handle.
#[allow(dead_code)]
pub struct ConnectorTerminalContinuationClaim {
    pub execution: ConnectorExecution,
    pub claim_fence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorExecution {
    pub execution_id: String,
    pub kind: String,
    pub task_id: String,
    pub run_id: String,
    pub state: String,
    pub submitted_at: i64,
    pub queued_at: Option<i64>,
    pub queue_deadline: i64,
    pub started_at: Option<i64>,
    pub last_output_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub stdout_cursor: usize,
    pub stderr_cursor: usize,
    pub exit_code: Option<i32>,
    pub failure_source: Option<String>,
    pub failure_code: Option<String>,
    pub terminal_reason: Option<String>,
    pub operation_id: String,
    pub request_sha256: String,
    pub executor_reference: Option<String>,
    pub first_status_failure_at: Option<i64>,
    pub last_successful_observation_at: Option<i64>,
    pub status_failure_code: Option<String>,
    pub check_plan: Vec<String>,
    pub check_recipe: Option<Value>,
    pub check_completed: usize,
    pub check_workspace_sha256: Option<String>,
    pub validated_workspace_sha256: Option<String>,
    pub failed_check: Option<String>,
    pub assertion_evidence: Option<Value>,
    pub continuation_intent: ConnectorExecutionContinuationIntent,
    pub continuation_armed_at: Option<i64>,
    pub continuation_delivery_state: ConnectorTerminalContinuationDeliveryState,
    pub mcp_task_materialized_at: Option<i64>,
    pub mcp_task_result_finalized_at: Option<i64>,
    pub mcp_task_output_tail: Option<Value>,
}

impl ConnectorExecution {
    pub fn state_is_active(state: &str) -> bool {
        matches!(
            state,
            "accepted" | "queued" | "starting" | "running" | "cancel_requested"
        )
    }

    pub fn is_active(&self) -> bool {
        Self::state_is_active(&self.state)
    }

    pub fn is_terminal(&self) -> bool {
        !self.is_active()
    }

    pub fn terminal_continuation_is_armed(&self) -> bool {
        self.continuation_intent == ConnectorExecutionContinuationIntent::ArmedForTerminal
            && self.continuation_armed_at.is_some()
    }

    pub fn mcp_task_is_materialized(&self) -> bool {
        self.mcp_task_materialized_at.is_some()
    }

    pub fn mcp_task_result_is_finalized(&self) -> bool {
        self.mcp_task_result_finalized_at.is_some()
    }

    pub fn blocks_finish(&self) -> bool {
        self.is_active() || self.state == "unknown"
    }

    pub fn executor_status_recognized(status: &str) -> bool {
        status == "recovering" || RunnerJobLifecycle::from_wire(status).is_ok()
    }
}

pub enum ConnectorExecutionReservation {
    Created(ConnectorExecution),
    Existing(ConnectorExecution),
}

pub enum ConnectorExecutionFailure {
    Submission(&'static str),
    Unknown(&'static str),
    /// A business invariant failed deterministically (e.g. workspace
    /// provenance mismatch): retrying cannot help and neither transport nor
    /// storage is at fault, so it must not masquerade as either. `evidence`
    /// is the model-visible explanation with the concrete mismatch facts.
    Workspace {
        code: &'static str,
        evidence: String,
    },
}

pub struct ConnectorExecutionObservation<'a> {
    pub executor_status: &'a str,
    pub stdout_cursor: usize,
    pub stderr_cursor: usize,
    pub exit_code: Option<i32>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub check_completed: Option<usize>,
    pub failed_check: Option<&'a str>,
    pub assertion_evidence: Option<&'a Value>,
    pub validated_workspace_sha256: Option<&'a str>,
    pub executor_failure_code: Option<&'static str>,
    pub mcp_task_output_tail: Option<&'a Value>,
    pub now: i64,
}

type Fact = Option<&'static str>;
type StateOutcome = (&'static str, Fact, Fact, Fact);
const EXECUTOR: Fact = Some("executor");
const UNKNOWN_REASON: &str = "executor_terminal_unknown";

fn active_state(execution: &ConnectorExecution, state: &'static str) -> StateOutcome {
    let state = if execution.state == "cancel_requested" {
        "cancel_requested"
    } else {
        state
    };
    (state, None, None, None)
}

fn failed(source: &'static str, code: &'static str, reason: &'static str) -> StateOutcome {
    ("failed", Some(source), Some(code), Some(reason))
}

fn unknown(code: &'static str) -> StateOutcome {
    ("unknown", EXECUTOR, Some(code), Some(UNKNOWN_REASON))
}

pub(super) fn observed_state(
    execution: &ConnectorExecution,
    observation: &ConnectorExecutionObservation<'_>,
) -> StateOutcome {
    if let Some(code) = observation.executor_failure_code {
        return failed("executor", code, "executor_protocol_violation");
    }
    if observation.executor_status == "recovering" {
        return active_state(
            execution,
            if execution.state == "queued" {
                "queued"
            } else {
                "running"
            },
        );
    }
    let Ok(lifecycle) = RunnerJobLifecycle::from_wire(observation.executor_status) else {
        // Callers reject unrecognized observation status before this projection.
        // Keep the historical conservative fallback for direct/internal callers.
        return active_state(execution, "running");
    };
    match lifecycle {
        RunnerJobLifecycle::Queued | RunnerJobLifecycle::RunnerQueued => {
            active_state(execution, "queued")
        }
        RunnerJobLifecycle::Running | RunnerJobLifecycle::StartedLegacy => {
            active_state(execution, "running")
        }
        RunnerJobLifecycle::StopRequested => active_state(execution, "running"),
        RunnerJobLifecycle::Completed
            if execution.kind == "check"
                && observation.exit_code == Some(0)
                && observation.check_completed == Some(execution.check_plan.len())
                && observation.failed_check.is_none() =>
        {
            ("succeeded", None, None, Some("exit_zero"))
        }
        RunnerJobLifecycle::Completed if execution.kind == "check" => failed(
            "executor",
            if observation.check_completed.is_none() {
                "validation_progress_missing"
            } else {
                "validation_progress_incomplete"
            },
            "executor_protocol_violation",
        ),
        RunnerJobLifecycle::Completed if observation.exit_code == Some(0) => {
            ("succeeded", None, None, Some("exit_zero"))
        }
        RunnerJobLifecycle::Completed if observation.exit_code.is_none() => {
            unknown("executor_exit_code_missing")
        }
        RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled
            if execution.state == "cancel_requested"
                && execution.failure_code.as_deref() == Some("queue_deadline") =>
        {
            failed("queue", "queue_deadline", "queue_timeout")
        }
        RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled
            if execution.state == "cancel_requested" =>
        {
            ("cancelled", None, None, Some("user_cancelled"))
        }
        RunnerJobLifecycle::Stopped | RunnerJobLifecycle::Cancelled => {
            active_state(execution, "running")
        }
        RunnerJobLifecycle::Timeout | RunnerJobLifecycle::TimedOut => {
            failed("executor", "command_timeout", "timeout")
        }
        RunnerJobLifecycle::Lost => unknown("executor_lost"),
        RunnerJobLifecycle::Failed
            if execution.kind == "check"
                && observation.failed_check.is_some()
                && observation
                    .check_completed
                    .is_some_and(|completed| completed < execution.check_plan.len()) =>
        {
            failed("check", "assertion_failed", "nonzero_exit")
        }
        RunnerJobLifecycle::Failed if execution.kind == "check" => failed(
            "executor",
            if observation.check_completed.is_none() {
                "validation_progress_missing"
            } else {
                "validation_progress_invalid"
            },
            "executor_protocol_violation",
        ),
        RunnerJobLifecycle::Failed | RunnerJobLifecycle::Completed => {
            failed("command", "nonzero_exit", "nonzero_exit")
        }
    }
}

fn query_execution<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<Option<ConnectorExecution>> {
    conn.query_row(sql, params, map_execution).optional()
}

pub(super) fn latest_execution(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> rusqlite::Result<Option<ConnectorExecution>> {
    query_execution(
        conn,
        &format!(
            "SELECT {EXECUTION_COLUMNS} FROM wc_executions
             WHERE task_id = ?1 ORDER BY submitted_at DESC, rowid DESC LIMIT 1"
        ),
        params![task_id],
    )
}

pub(super) fn latest_execution_by_kind(
    conn: &rusqlite::Connection,
    task_id: &str,
    kind: &str,
) -> rusqlite::Result<Option<ConnectorExecution>> {
    query_execution(
        conn,
        &format!(
            "SELECT {EXECUTION_COLUMNS} FROM wc_executions
             WHERE task_id = ?1 AND kind = ?2
             ORDER BY submitted_at DESC, rowid DESC LIMIT 1"
        ),
        params![task_id, kind],
    )
}

pub(super) fn load_execution_by_operation(
    conn: &rusqlite::Connection,
    task_id: &str,
    run_id: &str,
    operation_id: &str,
) -> rusqlite::Result<Option<ConnectorExecution>> {
    query_execution(
        conn,
        &format!(
            "SELECT {EXECUTION_COLUMNS} FROM wc_executions
             WHERE task_id = ?1 AND run_id = ?2 AND operation_id = ?3"
        ),
        params![task_id, run_id, operation_id],
    )
}

pub(super) fn load_execution(
    conn: &rusqlite::Connection,
    execution_id: &str,
) -> rusqlite::Result<Option<ConnectorExecution>> {
    query_execution(
        conn,
        &format!("SELECT {EXECUTION_COLUMNS} FROM wc_executions WHERE id = ?1"),
        params![execution_id],
    )
}

pub(super) const EXECUTION_COLUMNS: &str =
    "id, kind, task_id, run_id, state, submitted_at, queued_at, \
    queue_deadline, started_at, last_output_at, finished_at, stdout_cursor, stderr_cursor, \
    exit_code, failure_source, failure_code, terminal_reason, operation_id, request_sha256, \
    executor_reference, first_status_failure_at, last_successful_observation_at, \
    status_failure_code, check_plan, check_recipe_json, check_completed, check_workspace_sha256, \
    validated_workspace_sha256, failed_check, assertion_evidence_json, \
    terminal_continuation_intent, terminal_continuation_armed_at, \
    terminal_continuation_delivery_state, mcp_task_materialized_at, \
    mcp_task_result_finalized_at, mcp_task_output_tail_json";

pub(super) fn map_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectorExecution> {
    let cursor = |index| {
        let value = row.get::<_, i64>(index)?;
        usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
    };
    Ok(ConnectorExecution {
        execution_id: row.get(0)?,
        kind: row.get(1)?,
        task_id: row.get(2)?,
        run_id: row.get(3)?,
        state: row.get(4)?,
        submitted_at: row.get(5)?,
        queued_at: row.get(6)?,
        queue_deadline: row.get(7)?,
        started_at: row.get(8)?,
        last_output_at: row.get(9)?,
        finished_at: row.get(10)?,
        stdout_cursor: cursor(11)?,
        stderr_cursor: cursor(12)?,
        exit_code: row.get(13)?,
        failure_source: row.get(14)?,
        failure_code: row.get(15)?,
        terminal_reason: row.get(16)?,
        operation_id: row.get(17)?,
        request_sha256: row.get(18)?,
        executor_reference: row.get(19)?,
        first_status_failure_at: row.get(20)?,
        last_successful_observation_at: row.get(21)?,
        status_failure_code: row.get(22)?,
        check_plan: row
            .get::<_, Option<String>>(23)?
            .map(|plan| plan.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        check_recipe: row
            .get::<_, Option<String>>(24)?
            .map(|recipe| {
                serde_json::from_str(&recipe).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        24,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?,
        check_completed: cursor(25)?,
        check_workspace_sha256: row.get(26)?,
        validated_workspace_sha256: row.get(27)?,
        failed_check: row.get(28)?,
        assertion_evidence: row
            .get::<_, Option<String>>(29)?
            .map(|evidence| {
                serde_json::from_str(&evidence).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        29,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?,
        continuation_intent: ConnectorExecutionContinuationIntent::from_db(
            &row.get::<_, String>(30)?,
            30,
        )?,
        continuation_armed_at: row.get(31)?,
        continuation_delivery_state: ConnectorTerminalContinuationDeliveryState::from_db(
            &row.get::<_, String>(32)?,
            32,
        )?,
        mcp_task_materialized_at: row.get(33)?,
        mcp_task_result_finalized_at: row.get(34)?,
        mcp_task_output_tail: row
            .get::<_, Option<String>>(35)?
            .map(|output_tail| {
                serde_json::from_str(&output_tail).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        35,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?,
    })
}

pub(super) fn execution_event_kind(state: &str) -> &'static str {
    match state {
        "accepted" => "execution_accepted",
        "queued" => "execution_queued",
        "starting" | "running" => "execution_started",
        "cancel_requested" => "execution_cancel_requested",
        "succeeded" => "execution_succeeded",
        "cancelled" => "execution_cancelled",
        "interrupted" => "execution_interrupted",
        "unknown" => "execution_unknown",
        _ => "execution_failed",
    }
}

#[cfg(test)]
mod runner_job_lifecycle_projection_tests {
    use super::*;

    fn execution(state: &str) -> ConnectorExecution {
        ConnectorExecution {
            execution_id: "execution".to_string(),
            kind: "command".to_string(),
            task_id: "task".to_string(),
            run_id: "run".to_string(),
            state: state.to_string(),
            submitted_at: 1,
            queued_at: None,
            queue_deadline: 10,
            started_at: None,
            last_output_at: None,
            finished_at: None,
            stdout_cursor: 0,
            stderr_cursor: 0,
            exit_code: None,
            failure_source: None,
            failure_code: None,
            terminal_reason: None,
            operation_id: "operation".to_string(),
            request_sha256: "request".to_string(),
            executor_reference: None,
            first_status_failure_at: None,
            last_successful_observation_at: None,
            status_failure_code: None,
            check_plan: Vec::new(),
            check_recipe: None,
            check_completed: 0,
            check_workspace_sha256: None,
            validated_workspace_sha256: None,
            failed_check: None,
            assertion_evidence: None,
            continuation_intent: ConnectorExecutionContinuationIntent::None,
            continuation_armed_at: None,
            continuation_delivery_state: ConnectorTerminalContinuationDeliveryState::Unclaimed,
            mcp_task_materialized_at: None,
            mcp_task_result_finalized_at: None,
            mcp_task_output_tail: None,
        }
    }

    fn observation(status: &str, exit_code: Option<i32>) -> ConnectorExecutionObservation<'_> {
        ConnectorExecutionObservation {
            executor_status: status,
            stdout_cursor: 0,
            stderr_cursor: 0,
            exit_code,
            started_at: None,
            finished_at: None,
            check_completed: None,
            failed_check: None,
            assertion_evidence: None,
            validated_workspace_sha256: None,
            executor_failure_code: None,
            mcp_task_output_tail: None,
            now: 2,
        }
    }

    #[test]
    fn runner_job_lifecycle_projects_to_connector_execution_semantics() {
        let execution = execution("accepted");
        let cases: [(&str, StateOutcome); 9] = [
            ("queued", ("queued", None, None, None)),
            ("agent_queued", ("queued", None, None, None)),
            ("started", ("running", None, None, None)),
            ("running", ("running", None, None, None)),
            ("stop_requested", ("running", None, None, None)),
            (
                "timeout",
                (
                    "failed",
                    Some("executor"),
                    Some("command_timeout"),
                    Some("timeout"),
                ),
            ),
            (
                "timed_out",
                (
                    "failed",
                    Some("executor"),
                    Some("command_timeout"),
                    Some("timeout"),
                ),
            ),
            (
                "lost",
                (
                    "unknown",
                    Some("executor"),
                    Some("executor_lost"),
                    Some("executor_terminal_unknown"),
                ),
            ),
            ("completed", ("succeeded", None, None, Some("exit_zero"))),
        ];

        for (status, expected) in cases {
            let exit_code = (status == "completed").then_some(0);
            assert_eq!(
                observed_state(&execution, &observation(status, exit_code)),
                expected,
                "{status}"
            );
        }
    }

    #[test]
    fn runner_recovery_overlay_and_unknown_status_keep_boundary_specific_semantics() {
        assert!(ConnectorExecution::executor_status_recognized("recovering"));
        assert!(!ConnectorExecution::executor_status_recognized("unknown"));
        assert_eq!(
            observed_state(&execution("queued"), &observation("recovering", None)),
            ("queued", None, None, None)
        );
        assert_eq!(
            observed_state(&execution("running"), &observation("recovering", None)),
            ("running", None, None, None)
        );
        assert_eq!(
            observed_state(&execution("accepted"), &observation("unknown", None)),
            ("running", None, None, None),
            "direct/internal unknown fallback stays conservative; callers reject it before projection"
        );
    }
}
