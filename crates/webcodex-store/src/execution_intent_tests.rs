use super::*;

fn database() -> (tempfile::TempDir, std::path::PathBuf, Database) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("execution-intent.db");
    let db = Database::open(&path).unwrap();
    (temp, path, db)
}

fn task(db: &Database, suffix: &str) -> ConnectorTaskSnapshot {
    db.ensure_connector_binding(ConnectorBinding {
        project_id: "wc_proj_intent",
        project_name: "intent",
        workspace_id: "wc_ws_intent",
        executor_ref: "agent:hosted:intent",
        subject_id: "user:intent",
        profile: "personal",
        now: 10,
    })
    .unwrap();
    db.start_connector_task(NewConnectorTask {
        task_id: &format!("wc_task_{suffix}"),
        run_id: &format!("wc_run_{suffix}"),
        project_id: "wc_proj_intent",
        workspace_id: "wc_ws_intent",
        subject_id: "user:intent",
        goal: "test durable terminal continuation intent",
        mode: "normal",
        target_executor_ref: "agent:hosted:intent",
        execution_executor_ref: "agent:hosted:intent",
        target_root: "/workspace/intent",
        execution_root: "/workspace/runs/intent",
        baseline_commit: Some("0123456789abcdef"),
        baseline_tree: Some("fedcba9876543210"),
        isolated: true,
        now: 11,
    })
    .unwrap()
}

fn reserve(db: &Database, task: &ConnectorTaskSnapshot, operation_id: &str) -> ConnectorExecution {
    match db
        .reserve_connector_execution(
            task,
            "command",
            operation_id,
            "request-hash",
            &[],
            None,
            None,
            100,
            12,
        )
        .unwrap()
    {
        ConnectorExecutionReservation::Created(execution) => execution,
        ConnectorExecutionReservation::Existing(_) => panic!("expected fresh execution"),
    }
}

fn succeed(db: &Database, execution_id: &str, now: i64) -> ConnectorExecution {
    db.observe_connector_execution(
        execution_id,
        ConnectorExecutionObservation {
            executor_status: "completed",
            stdout_cursor: 1,
            stderr_cursor: 1,
            exit_code: Some(0),
            started_at: Some(now - 1),
            finished_at: Some(now),
            check_completed: None,
            failed_check: None,
            assertion_evidence: None,
            validated_workspace_sha256: None,
            executor_failure_code: None,
            mcp_task_output_tail: None,
            now,
        },
    )
    .unwrap()
}

#[test]
fn reservation_is_unarmed_and_arm_is_durable_idempotent_and_replay_stable() {
    let (_temp, path, db) = database();
    let task = task(&db, "durable");
    let execution = reserve(&db, &task, "op-durable");
    assert_eq!(
        execution.continuation_intent,
        ConnectorExecutionContinuationIntent::None
    );
    assert_eq!(execution.continuation_armed_at, None);

    let armed = db
        .arm_connector_terminal_continuation(&execution.execution_id, 20)
        .unwrap();
    assert_eq!(
        armed.continuation_intent,
        ConnectorExecutionContinuationIntent::ArmedForTerminal
    );
    assert_eq!(armed.continuation_armed_at, Some(20));
    let rearmed = db
        .arm_connector_terminal_continuation(&execution.execution_id, 30)
        .unwrap();
    assert_eq!(rearmed.continuation_armed_at, Some(20));

    let replay = db
        .reserve_connector_execution(
            &task,
            "command",
            "op-durable",
            "request-hash",
            &[],
            None,
            None,
            100,
            31,
        )
        .unwrap();
    let replay = match replay {
        ConnectorExecutionReservation::Existing(execution) => execution,
        ConnectorExecutionReservation::Created(_) => {
            panic!("exact replay created a second execution")
        }
    };
    assert_eq!(replay.execution_id, execution.execution_id);
    assert_eq!(replay.continuation_armed_at, Some(20));

    drop(db);
    let reopened = Database::open(&path).unwrap();
    let restored = reopened
        .connector_execution(&execution.execution_id)
        .unwrap();
    assert_eq!(
        restored.continuation_intent,
        ConnectorExecutionContinuationIntent::ArmedForTerminal
    );
    assert_eq!(restored.continuation_armed_at, Some(20));
}

#[test]
fn mcp_task_materialization_and_terminal_result_finalize_durably_together() {
    let (_temp, path, db) = database();
    let task = task(&db, "mcp-final");
    let execution = reserve(&db, &task, "op-mcp-final");
    db.arm_connector_terminal_continuation(&execution.execution_id, 20)
        .unwrap();
    let materialized = db
        .materialize_connector_execution_mcp_task_for_subject(
            &execution.execution_id,
            "wc_proj_intent",
            "user:intent",
            21,
        )
        .unwrap();
    assert_eq!(materialized.mcp_task_materialized_at, Some(21));
    assert_eq!(materialized.mcp_task_result_finalized_at, None);

    let tail = serde_json::json!({
        "stdout": "durable stdout\n",
        "stderr": "durable stderr\n",
        "bounded": true
    });
    let terminal = db
        .observe_connector_execution(
            &execution.execution_id,
            ConnectorExecutionObservation {
                executor_status: "completed",
                stdout_cursor: 2,
                stderr_cursor: 2,
                exit_code: Some(0),
                started_at: Some(22),
                finished_at: Some(23),
                check_completed: None,
                failed_check: None,
                assertion_evidence: None,
                validated_workspace_sha256: None,
                executor_failure_code: None,
                mcp_task_output_tail: Some(&tail),
                now: 23,
            },
        )
        .unwrap();
    assert_eq!(terminal.state, ConnectorExecutionState::Succeeded);
    assert_eq!(terminal.mcp_task_result_finalized_at, Some(23));
    assert_eq!(terminal.mcp_task_output_tail.as_ref(), Some(&tail));

    drop(db);
    let reopened = Database::open(&path).unwrap();
    let restored = reopened
        .connector_execution(&execution.execution_id)
        .unwrap();
    assert_eq!(restored.mcp_task_materialized_at, Some(21));
    assert_eq!(restored.mcp_task_result_finalized_at, Some(23));
    assert_eq!(restored.mcp_task_output_tail.as_ref(), Some(&tail));
    let replay = reopened
        .materialize_connector_execution_mcp_task_for_subject(
            &execution.execution_id,
            "wc_proj_intent",
            "user:intent",
            30,
        )
        .unwrap();
    assert_eq!(replay.mcp_task_materialized_at, Some(21));
    assert_eq!(replay.mcp_task_result_finalized_at, Some(23));
}

#[test]
fn ready_query_requires_both_armed_intent_and_terminal_state() {
    let (_temp, _path, db) = database();
    let first_task = task(&db, "armed");
    let armed = reserve(&db, &first_task, "op-armed");
    db.arm_connector_terminal_continuation(&armed.execution_id, 20)
        .unwrap();
    assert!(db.terminal_ready_connector_executions().unwrap().is_empty());

    let terminal = succeed(&db, &armed.execution_id, 21);
    assert_eq!(
        terminal.continuation_intent,
        ConnectorExecutionContinuationIntent::ArmedForTerminal
    );
    let ready = db.terminal_ready_connector_executions().unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].execution_id, armed.execution_id);

    let second_task = task(&db, "unarmed");
    let unarmed = reserve(&db, &second_task, "op-unarmed");
    let unarmed = succeed(&db, &unarmed.execution_id, 22);
    assert_eq!(
        unarmed.continuation_intent,
        ConnectorExecutionContinuationIntent::None
    );
    let ready = db.terminal_ready_connector_executions().unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].execution_id, armed.execution_id);
}

#[test]
fn startup_reconciliation_preserves_armed_intent_and_makes_it_ready() {
    let (_temp, _path, db) = database();
    let task = task(&db, "restart");
    let execution = reserve(&db, &task, "op-restart");
    db.arm_connector_terminal_continuation(&execution.execution_id, 20)
        .unwrap();

    let recovery = db
        .reconcile_connector_executions("wc_proj_intent", 30)
        .unwrap();
    assert_eq!(recovery.1, 1);
    let interrupted = db.connector_execution(&execution.execution_id).unwrap();
    assert_eq!(interrupted.state, ConnectorExecutionState::Interrupted);
    assert_eq!(
        interrupted.continuation_intent,
        ConnectorExecutionContinuationIntent::ArmedForTerminal
    );
    assert_eq!(interrupted.continuation_armed_at, Some(20));
    assert_eq!(
        db.terminal_ready_connector_executions().unwrap()[0].execution_id,
        execution.execution_id
    );

    let events = db
        .connector_task_events(&task.task_id, "wc_proj_intent", "user:intent", 20)
        .unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "execution_interrupted"));
}

#[test]
fn corrupt_execution_state_fails_closed_on_load() {
    let (_temp, _path, db) = database();
    let task = task(&db, "corrupt-state");
    let execution = reserve(&db, &task, "op-corrupt-state");
    let conn = db.conn_for_tests();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    conn.execute(
        "UPDATE wc_executions SET state = 'future_state' WHERE id = ?1",
        [&execution.execution_id],
    )
    .unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
        .unwrap();
    drop(conn);

    assert!(db.connector_execution(&execution.execution_id).is_err());
}
