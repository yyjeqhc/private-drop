use super::execution_model::ConnectorTerminalContinuationDeliveryState;
use super::*;
use std::sync::{Arc, Barrier};

fn database() -> (tempfile::TempDir, std::path::PathBuf, Database) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("continuation-delivery.db");
    let db = Database::open(&path).unwrap();
    (temp, path, db)
}

fn task(db: &Database, suffix: &str) -> ConnectorTaskSnapshot {
    db.ensure_connector_binding(ConnectorBinding {
        project_id: "wc_proj_delivery",
        project_name: "delivery",
        workspace_id: "wc_ws_delivery",
        executor_ref: "agent:hosted:delivery",
        subject_id: "user:delivery",
        profile: "personal",
        now: 10,
    })
    .unwrap();
    db.start_connector_task(NewConnectorTask {
        task_id: &format!("wc_task_{suffix}"),
        run_id: &format!("wc_run_{suffix}"),
        project_id: "wc_proj_delivery",
        workspace_id: "wc_ws_delivery",
        subject_id: "user:delivery",
        goal: "test durable terminal continuation delivery",
        mode: "normal",
        target_executor_ref: "agent:hosted:delivery",
        execution_executor_ref: "agent:hosted:delivery",
        target_root: "/workspace/delivery",
        execution_root: &format!("/workspace/runs/{suffix}"),
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
        ConnectorExecutionReservation::Existing(_) => panic!("expected a fresh execution"),
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

fn armed_terminal(
    db: &Database,
    suffix: &str,
    operation_id: &str,
    armed_at: i64,
    finished_at: i64,
) -> ConnectorExecution {
    let task = task(db, suffix);
    let execution = reserve(db, &task, operation_id);
    db.arm_connector_terminal_continuation(&execution.execution_id, armed_at)
        .unwrap();
    succeed(db, &execution.execution_id, finished_at)
}

fn stored_fence(db: &Database, execution_id: &str) -> Option<String> {
    db.conn_for_tests()
        .query_row(
            "SELECT terminal_continuation_claim_fence FROM wc_executions WHERE id = ?1",
            rusqlite::params![execution_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn assert_stale(result: Result<ConnectorExecution, ConnectorTaskStoreError>) {
    match result {
        Err(ConnectorTaskStoreError::InvalidState(message)) => {
            assert!(message.contains("claim fence is stale"), "{message}");
        }
        _ => panic!("stale claim fence unexpectedly mutated delivery state"),
    }
}

#[test]
fn readiness_requires_armed_terminal_unclaimed_and_claim_is_exactly_once() {
    let (_temp, _path, db) = database();

    let unarmed_task = task(&db, "unarmed");
    let unarmed = reserve(&db, &unarmed_task, "op-unarmed");
    succeed(&db, &unarmed.execution_id, 20);
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    let active_task = task(&db, "active");
    let active = reserve(&db, &active_task, "op-active");
    db.arm_connector_terminal_continuation(&active.execution_id, 21)
        .unwrap();
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    let terminal = succeed(&db, &active.execution_id, 22);
    assert_eq!(
        terminal.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Unclaimed
    );
    let claim = db
        .claim_next_terminal_continuation()
        .unwrap()
        .expect("armed terminal unclaimed execution must be claimable");
    assert_eq!(claim.execution.execution_id, active.execution_id);
    assert_eq!(
        claim.execution.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
    assert!(claim.claim_fence.starts_with("wc_claim_"));
    assert!(claim.claim_fence.len() <= 80);
    assert_eq!(
        stored_fence(&db, &active.execution_id).as_deref(),
        Some(claim.claim_fence.as_str())
    );
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());
}

#[test]
fn concurrent_database_handles_create_only_one_live_claim() {
    let (_temp, path, db) = database();
    let terminal = armed_terminal(&db, "concurrent", "op-concurrent", 20, 21);
    let execution_id = terminal.execution_id.clone();
    drop(db);

    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let db = Database::open(&path).unwrap();
            barrier.wait();
            db.claim_next_terminal_continuation()
                .map(|claim| claim.map(|claim| (claim.execution.execution_id, claim.claim_fence)))
                .map_err(|error| error.to_string())
        }));
    }
    barrier.wait();
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_some()).count(),
        1
    );
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_none()).count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .flatten()
            .next()
            .map(|claim| claim.0.as_str()),
        Some(execution_id.as_str())
    );

    let reopened = Database::open(&path).unwrap();
    let durable = reopened.connector_execution(&execution_id).unwrap();
    assert_eq!(
        durable.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
}

#[test]
fn release_rotates_fence_and_stale_claimant_cannot_mutate_new_claim() {
    let (_temp, _path, db) = database();
    let terminal = armed_terminal(&db, "stale", "op-stale", 20, 21);
    let first = db.claim_next_terminal_continuation().unwrap().unwrap();
    assert_eq!(first.execution.execution_id, terminal.execution_id);

    let released = db
        .release_terminal_continuation_claim(&terminal.execution_id, &first.claim_fence)
        .unwrap();
    assert_eq!(
        released.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Unclaimed
    );
    assert_eq!(stored_fence(&db, &terminal.execution_id), None);

    let second = db.claim_next_terminal_continuation().unwrap().unwrap();
    assert_ne!(second.claim_fence, first.claim_fence);
    assert_stale(
        db.release_terminal_continuation_claim(&terminal.execution_id, &first.claim_fence),
    );
    assert_stale(
        db.begin_terminal_continuation_dispatch(&terminal.execution_id, &first.claim_fence),
    );
    assert_stale(
        db.complete_terminal_continuation_delivery(&terminal.execution_id, &first.claim_fence),
    );
    assert_stale(
        db.mark_terminal_continuation_delivery_unknown(&terminal.execution_id, &first.claim_fence),
    );

    let dispatching = db
        .begin_terminal_continuation_dispatch(&terminal.execution_id, &second.claim_fence)
        .unwrap();
    assert_eq!(
        dispatching.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Dispatching
    );
    assert_eq!(
        stored_fence(&db, &terminal.execution_id).as_deref(),
        Some(second.claim_fence.as_str())
    );
    assert!(matches!(
        db.release_terminal_continuation_claim(&terminal.execution_id, &second.claim_fence),
        Err(ConnectorTaskStoreError::InvalidState(_))
    ));
}

#[test]
fn delivered_and_delivery_unknown_are_terminal_delivery_truths() {
    let (_temp, path, db) = database();
    let delivered_execution = armed_terminal(&db, "delivered", "op-delivered", 20, 21);
    let delivered_claim = db.claim_next_terminal_continuation().unwrap().unwrap();
    db.begin_terminal_continuation_dispatch(
        &delivered_execution.execution_id,
        &delivered_claim.claim_fence,
    )
    .unwrap();
    let delivered = db
        .complete_terminal_continuation_delivery(
            &delivered_execution.execution_id,
            &delivered_claim.claim_fence,
        )
        .unwrap();
    assert_eq!(
        delivered.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Delivered
    );
    assert_eq!(stored_fence(&db, &delivered.execution_id), None);
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    let unknown_execution = armed_terminal(&db, "unknown", "op-unknown", 30, 31);
    let unknown_claim = db.claim_next_terminal_continuation().unwrap().unwrap();
    db.begin_terminal_continuation_dispatch(
        &unknown_execution.execution_id,
        &unknown_claim.claim_fence,
    )
    .unwrap();
    let unknown = db
        .mark_terminal_continuation_delivery_unknown(
            &unknown_execution.execution_id,
            &unknown_claim.claim_fence,
        )
        .unwrap();
    assert_eq!(
        unknown.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::DeliveryUnknown
    );
    assert_eq!(stored_fence(&db, &unknown.execution_id), None);
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    let delivered_id = delivered.execution_id.clone();
    let unknown_id = unknown.execution_id.clone();
    drop(db);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened
            .connector_execution(&delivered_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Delivered
    );
    assert_eq!(
        reopened
            .connector_execution(&unknown_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::DeliveryUnknown
    );
    reopened
        .reconcile_connector_startup("wc_proj_delivery", 40)
        .unwrap();
    assert_eq!(
        reopened
            .connector_execution(&delivered_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Delivered
    );
    assert_eq!(
        reopened
            .connector_execution(&unknown_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::DeliveryUnknown
    );
    assert!(reopened
        .claim_next_terminal_continuation()
        .unwrap()
        .is_none());
}

#[test]
fn reopen_preserves_claim_and_dispatch_uncertainty_fence() {
    let (_temp, path, db) = database();
    let terminal = armed_terminal(&db, "reopen", "op-reopen", 20, 21);
    let claim = db.claim_next_terminal_continuation().unwrap().unwrap();
    let execution_id = terminal.execution_id.clone();
    let fence = claim.claim_fence.clone();
    drop(db);

    let reopened = Database::open(&path).unwrap();
    let claimed = reopened.connector_execution(&execution_id).unwrap();
    assert_eq!(
        claimed.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
    assert_eq!(
        stored_fence(&reopened, &execution_id).as_deref(),
        Some(fence.as_str())
    );
    reopened
        .begin_terminal_continuation_dispatch(&execution_id, &fence)
        .unwrap();
    drop(reopened);

    let reopened = Database::open(&path).unwrap();
    let dispatching = reopened.connector_execution(&execution_id).unwrap();
    assert_eq!(
        dispatching.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Dispatching
    );
    assert_eq!(
        stored_fence(&reopened, &execution_id).as_deref(),
        Some(fence.as_str())
    );
}

#[test]
fn startup_reconciliation_releases_claimed_but_quarantines_dispatching() {
    let (_temp, _path, db) = database();
    let claimed_execution = armed_terminal(&db, "restart-claimed", "op-restart-claimed", 20, 21);
    let dispatching_execution =
        armed_terminal(&db, "restart-dispatching", "op-restart-dispatching", 30, 31);

    let claimed = db.claim_next_terminal_continuation().unwrap().unwrap();
    assert_eq!(
        claimed.execution.execution_id,
        claimed_execution.execution_id
    );
    let dispatching = db.claim_next_terminal_continuation().unwrap().unwrap();
    assert_eq!(
        dispatching.execution.execution_id,
        dispatching_execution.execution_id
    );
    db.begin_terminal_continuation_dispatch(
        &dispatching_execution.execution_id,
        &dispatching.claim_fence,
    )
    .unwrap();

    db.reconcile_connector_executions("wc_proj_delivery", 39)
        .unwrap();
    assert_eq!(
        db.connector_execution(&claimed_execution.execution_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
    assert_eq!(
        db.connector_execution(&dispatching_execution.execution_id)
            .unwrap()
            .continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Dispatching
    );

    db.reconcile_connector_startup("wc_proj_delivery", 40)
        .unwrap();

    let safely_released = db
        .connector_execution(&claimed_execution.execution_id)
        .unwrap();
    assert_eq!(safely_released.state, ConnectorExecutionState::Succeeded);
    assert_eq!(
        safely_released.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Unclaimed
    );
    assert_eq!(stored_fence(&db, &claimed_execution.execution_id), None);

    let uncertain = db
        .connector_execution(&dispatching_execution.execution_id)
        .unwrap();
    assert_eq!(uncertain.state, ConnectorExecutionState::Succeeded);
    assert_eq!(
        uncertain.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::DeliveryUnknown
    );
    assert_eq!(stored_fence(&db, &dispatching_execution.execution_id), None);

    let reclaimed = db.claim_next_terminal_continuation().unwrap().unwrap();
    assert_eq!(
        reclaimed.execution.execution_id,
        claimed_execution.execution_id
    );
    assert_ne!(reclaimed.claim_fence, claimed.claim_fence);
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());
}

#[test]
fn inconsistent_delivery_state_and_fence_fail_closed() {
    let (_temp, _path, db) = database();
    let terminal = armed_terminal(&db, "inconsistent", "op-inconsistent", 20, 21);

    db.conn_for_tests()
        .execute_batch(&format!(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE wc_executions
             SET terminal_continuation_claim_fence = 'orphan-fence'
             WHERE id = '{}';
             PRAGMA ignore_check_constraints = OFF;",
            terminal.execution_id
        ))
        .unwrap();
    assert!(db.terminal_ready_connector_executions().unwrap().is_empty());
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    db.conn_for_tests()
        .execute_batch(&format!(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE wc_executions
             SET terminal_continuation_delivery_state = 'claimed',
                 terminal_continuation_claim_fence = NULL
             WHERE id = '{}';
             PRAGMA ignore_check_constraints = OFF;",
            terminal.execution_id
        ))
        .unwrap();
    let active_task = task(&db, "inconsistent-active");
    let active = reserve(&db, &active_task, "op-inconsistent-active");
    db.arm_connector_terminal_continuation(&active.execution_id, 22)
        .unwrap();
    db.conn_for_tests()
        .execute(
            "UPDATE wc_executions
             SET terminal_continuation_delivery_state = 'claimed',
                 terminal_continuation_claim_fence = 'corrupt-active-fence'
             WHERE id = ?1",
            rusqlite::params![active.execution_id],
        )
        .unwrap();

    db.reconcile_connector_startup("wc_proj_delivery", 30)
        .unwrap();
    let stranded = db.connector_execution(&terminal.execution_id).unwrap();
    assert_eq!(
        stranded.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
    assert_eq!(stored_fence(&db, &terminal.execution_id), None);
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());

    let recovered_active = db.connector_execution(&active.execution_id).unwrap();
    assert_eq!(recovered_active.state, ConnectorExecutionState::Interrupted);
    assert_eq!(
        recovered_active.continuation_delivery_state,
        ConnectorTerminalContinuationDeliveryState::Claimed
    );
    assert_eq!(
        stored_fence(&db, &active.execution_id).as_deref(),
        Some("corrupt-active-fence")
    );
    assert!(db.claim_next_terminal_continuation().unwrap().is_none());
}

#[test]
fn unknown_persisted_delivery_state_fails_closed_and_is_not_claimable() {
    let (_temp, _path, db) = database();
    let terminal = armed_terminal(&db, "corrupt", "op-corrupt", 20, 21);
    db.conn_for_tests()
        .execute_batch(&format!(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE wc_executions
             SET terminal_continuation_delivery_state = 'future_state'
             WHERE id = '{}';
             PRAGMA ignore_check_constraints = OFF;",
            terminal.execution_id
        ))
        .unwrap();

    assert!(db.claim_next_terminal_continuation().unwrap().is_none());
    assert!(db.connector_execution(&terminal.execution_id).is_err());
}
