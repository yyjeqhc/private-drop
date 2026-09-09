use super::*;
use serde_json::json;

fn database() -> (tempfile::TempDir, Database) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("task-kernel.db")).unwrap();
    (temp, db)
}

fn bind(db: &Database, subject: &str) {
    db.ensure_connector_binding(ConnectorBinding {
        project_id: "wc_proj_demo",
        project_name: "demo",
        workspace_id: "wc_ws_demo",
        executor_ref: "agent:hosted:demo",
        subject_id: subject,
        profile: "personal",
        now: 100,
    })
    .unwrap();
}

fn start(db: &Database, subject: &str, goal: &str) -> ConnectorTaskSnapshot {
    let task_id = new_id("wc_task");
    let run_id = new_id("wc_run");
    db.start_connector_task(NewConnectorTask {
        task_id: &task_id,
        run_id: &run_id,
        project_id: "wc_proj_demo",
        workspace_id: "wc_ws_demo",
        subject_id: subject,
        goal,
        mode: "normal",
        target_executor_ref: "agent:hosted:demo",
        execution_executor_ref: "agent:hosted:run",
        target_root: "/workspace/demo",
        execution_root: "/workspace/runs/one",
        baseline_commit: Some("0123456789abcdef"),
        baseline_tree: Some("fedcba9876543210"),
        isolated: true,
        now: 101,
    })
    .unwrap()
}

fn fingerprint(root_sha256: &str, target_path: &str) -> ProjectContextFingerprint {
    ProjectContextFingerprint {
        schema_version: 2,
        project_root_sha256: root_sha256.to_string(),
        target_directory: target_path.to_string(),
        git: webcodex_core::project_context_contract::GitContextFingerprint {
            available: true,
            branch: Some("main".to_string()),
            head: Some("0123456789abcdef".to_string()),
            worktree_sha256: Some("f".repeat(64)),
            dirty: Some(false),
        },
        rules: Vec::new(),
        manifests: Vec::new(),
        completeness: webcodex_core::project_context_contract::FingerprintCompleteness::default(),
    }
}

fn fail_window_context_inserts(db: &Database) {
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER wc_test_fail_window_context_insert
             BEFORE INSERT ON wc_window_project_contexts
             BEGIN
               SELECT RAISE(ABORT, 'injected window binding failure');
             END;",
        )
        .unwrap();
}

fn fail_window_context_updates(db: &Database) {
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER wc_test_fail_window_context_update
             BEFORE UPDATE ON wc_window_project_contexts
             BEGIN
               SELECT RAISE(ABORT, 'injected window binding failure');
             END;",
        )
        .unwrap();
}

#[test]
fn start_creates_task_run_and_first_monotonic_event() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    assert!(task.task_id.starts_with("wc_task_"));
    assert!(task.run_id.starts_with("wc_run_"));
    assert_eq!(task.event_cursor, 1);

    let cursor = db
        .append_connector_task_event(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "files_read",
            &serde_json::json!({ "ok": true, "file_count": 2 }),
            102,
        )
        .unwrap();
    assert_eq!(cursor, 2);
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn failed_initial_window_binding_rolls_back_task_run_and_event() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    fail_window_context_inserts(&db);
    let task_id = new_id("wc_task");
    let run_id = new_id("wc_run");
    let root_sha256 = "a".repeat(64);
    let fingerprint = fingerprint(&root_sha256, "");

    let result = db.start_connector_task_and_bind(
        NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: "wc_proj_demo",
            workspace_id: "wc_ws_demo",
            subject_id: "user:one",
            goal: "must roll back",
            mode: "read_only",
            target_executor_ref: "agent:hosted:demo",
            execution_executor_ref: "agent:hosted:demo",
            target_root: "/workspace/demo",
            execution_root: "/workspace/demo",
            baseline_commit: None,
            baseline_tree: None,
            isolated: false,
            now: 101,
        },
        ConnectorWindowBinding {
            window_key: "mcp:window-one",
            window_source: "mcp_session",
            project_root_sha256: &root_sha256,
            target_path: "",
            fingerprint: &fingerprint,
            now: 101,
        },
    );
    assert!(result.is_err());

    let conn = db.conn_for_tests();
    for table in [
        "wc_tasks",
        "wc_runs",
        "wc_run_contexts",
        "wc_task_events",
        "wc_window_project_contexts",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table} retained a partial start");
    }
}

#[test]
fn failed_continuation_binding_rolls_back_mode_workspace_and_instruction() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task_id = new_id("wc_task");
    let run_id = new_id("wc_run");
    let root_sha256 = "b".repeat(64);
    let fingerprint = fingerprint(&root_sha256, "src");
    let task = db
        .start_connector_task(NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: "wc_proj_demo",
            workspace_id: "wc_ws_demo",
            subject_id: "user:one",
            goal: "inspect first",
            mode: "read_only",
            target_executor_ref: "agent:hosted:demo",
            execution_executor_ref: "agent:hosted:demo",
            target_root: "/workspace/demo",
            execution_root: "/workspace/demo",
            baseline_commit: None,
            baseline_tree: None,
            isolated: false,
            now: 101,
        })
        .unwrap();
    db.bind_connector_window_context(
        "mcp:window-one",
        "mcp_session",
        "wc_proj_demo",
        "user:one",
        &root_sha256,
        &task.task_id,
        "src",
        &fingerprint,
        102,
    )
    .unwrap();
    fail_window_context_updates(&db);

    let result = db.continue_connector_task_and_bind(
        ConnectorTaskContinuation {
            task_id: &task.task_id,
            project_id: "wc_proj_demo",
            subject_id: "user:one",
            instruction: "upgrade atomically",
            mode: "normal",
            workspace: Some(ConnectorWorkspaceTransition {
                target_executor_ref: "agent:hosted:demo",
                execution_executor_ref: "agent:hosted:run",
                target_root: "/workspace/demo",
                execution_root: "/workspace/runs/upgraded",
                baseline_commit: "0123456789abcdef",
                baseline_tree: "fedcba9876543210",
            }),
            now: 103,
        },
        ConnectorWindowBinding {
            window_key: "mcp:window-one",
            window_source: "mcp_session",
            project_root_sha256: &root_sha256,
            target_path: "src",
            fingerprint: &fingerprint,
            now: 103,
        },
    );
    assert!(result.is_err());

    let restored = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(restored.mode, ConnectorTaskMode::ReadOnly);
    assert!(!restored.isolated);
    assert_eq!(restored.execution_root, "/workspace/demo");
    assert_eq!(restored.event_cursor, 1);
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 10)
        .unwrap();
    assert_eq!(events.len(), 1);
    let context = db
        .connector_window_context("mcp:window-one", "wc_proj_demo", "user:one", &root_sha256)
        .unwrap()
        .unwrap();
    assert_eq!(context.updated_at, 102);
}

#[test]
fn normal_task_cannot_downgrade_to_read_only() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "writable task");
    let root_sha256 = "c".repeat(64);
    let fingerprint = fingerprint(&root_sha256, "");

    let error = db
        .continue_connector_task_and_bind(
            ConnectorTaskContinuation {
                task_id: &task.task_id,
                project_id: "wc_proj_demo",
                subject_id: "user:one",
                instruction: "analyze without changing mode",
                mode: "read_only",
                workspace: None,
                now: 102,
            },
            ConnectorWindowBinding {
                window_key: "mcp:normal-downgrade",
                window_source: "mcp_session",
                project_root_sha256: &root_sha256,
                target_path: "",
                fingerprint: &fingerprint,
                now: 102,
            },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ConnectorTaskStoreError::InvalidState(message)
            if message.contains("normal tasks cannot transition to read_only")
    ));
    let restored = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(restored.mode, ConnectorTaskMode::Normal);
    assert!(restored.isolated);
    assert_eq!(restored.execution_root, task.execution_root);
    assert_eq!(restored.event_cursor, 1);
    assert!(db
        .connector_window_context(
            "mcp:normal-downgrade",
            "wc_proj_demo",
            "user:one",
            &root_sha256,
        )
        .unwrap()
        .is_none());
}

#[test]
fn malformed_persisted_read_only_isolated_task_cannot_continue() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "historical malformed task");
    db.conn_for_tests()
        .execute(
            "UPDATE wc_tasks SET mode = 'read_only' WHERE id = ?1",
            [&task.task_id],
        )
        .unwrap();
    let root_sha256 = "d".repeat(64);
    let fingerprint = fingerprint(&root_sha256, "");

    let error = db
        .continue_connector_task_and_bind(
            ConnectorTaskContinuation {
                task_id: &task.task_id,
                project_id: "wc_proj_demo",
                subject_id: "user:one",
                instruction: "must remain fail closed",
                mode: "read_only",
                workspace: None,
                now: 102,
            },
            ConnectorWindowBinding {
                window_key: "mcp:malformed-read-only",
                window_source: "mcp_session",
                project_root_sha256: &root_sha256,
                target_path: "",
                fingerprint: &fingerprint,
                now: 102,
            },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ConnectorTaskStoreError::InvalidState(message)
            if message == "read_only tasks must use the target workspace without isolation"
    ));
    let restored = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(restored.mode, ConnectorTaskMode::ReadOnly);
    assert!(restored.isolated);
    assert_eq!(restored.execution_root, task.execution_root);
    assert_eq!(restored.event_cursor, 1);
}

#[test]
fn new_inspect_task_is_rejected_before_persistence() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task_id = new_id("wc_task");
    let run_id = new_id("wc_run");
    let error = db
        .start_connector_task(NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: "wc_proj_demo",
            workspace_id: "wc_ws_demo",
            subject_id: "user:one",
            goal: "legacy inspect request",
            mode: "inspect",
            target_executor_ref: "agent:hosted:demo",
            execution_executor_ref: "agent:hosted:demo",
            target_root: "/workspace/demo",
            execution_root: "/workspace/demo",
            baseline_commit: None,
            baseline_tree: None,
            isolated: false,
            now: 101,
        })
        .unwrap_err();

    assert!(matches!(
        error,
        ConnectorTaskStoreError::InvalidState(message)
            if message == "task mode must be normal or read_only"
    ));
    assert!(matches!(
        db.connector_task(&task_id, "wc_proj_demo", "user:one"),
        Err(ConnectorTaskStoreError::NotFound)
    ));
}

#[test]
fn edit_operation_is_durable_idempotency_authority() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "edit atomically");
    let begin = |operation_id: &str, request_sha256: &str| {
        db.begin_connector_edit_operation(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            operation_id,
            request_sha256,
            102,
        )
        .unwrap()
    };

    assert_eq!(
        begin("edit-1", &"a".repeat(64)),
        ConnectorEditOperationGate::Started
    );
    assert_eq!(
        begin("edit-1", &"a".repeat(64)),
        ConnectorEditOperationGate::Pending
    );
    let result = serde_json::json!({"changed": true, "changed_paths": ["src/lib.rs"]});
    db.complete_connector_edit_operation(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        "edit-1",
        &"a".repeat(64),
        &result,
        103,
    )
    .unwrap();
    assert_eq!(
        begin("edit-1", &"a".repeat(64)),
        ConnectorEditOperationGate::Replay(result)
    );
    assert_eq!(
        begin("edit-1", &"b".repeat(64)),
        ConnectorEditOperationGate::Conflict
    );

    assert_eq!(
        begin("edit-2", &"c".repeat(64)),
        ConnectorEditOperationGate::Started
    );
    db.fail_connector_edit_operation(&task.task_id, "edit-2", &"c".repeat(64), 103)
        .unwrap();
    assert_eq!(
        begin("edit-2", &"d".repeat(64)),
        ConnectorEditOperationGate::Conflict
    );
    assert_eq!(
        begin("edit-2", &"c".repeat(64)),
        ConnectorEditOperationGate::Started
    );
}

#[test]
fn task_access_is_subject_and_project_scoped() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    bind(&db, "user:two");
    let task = start(&db, "user:one", "private task");
    assert!(matches!(
        db.connector_task(&task.task_id, "wc_proj_demo", "user:two"),
        Err(ConnectorTaskStoreError::NotFound)
    ));
    assert!(matches!(
        db.connector_task(&task.task_id, "wc_proj_other", "user:one"),
        Err(ConnectorTaskStoreError::NotFound)
    ));
}

#[test]
fn finish_is_atomic_and_prevents_more_events() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "finish me");
    let changed_paths = vec!["src/lib.rs".to_string()];
    let warnings = Vec::new();
    let cursor = db
        .finish_connector_task(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            NewConnectorResult {
                result_id: "wc_result_0123456789abcdef",
                summary: "done",
                patch_artifact: Some("/state/results/task.patch"),
                patch_sha256: Some("abc123"),
                patch_bytes: 42,
                changed_paths: &changed_paths,
                validation: &serde_json::json!({"checks": []}),
                warnings: &warnings,
            },
            102,
        )
        .unwrap();
    assert_eq!(cursor, 2);
    let snapshot = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(snapshot.task_status, ConnectorTaskState::ReadyForReview);
    assert_eq!(snapshot.run_status, ConnectorRunState::Completed);
    let result = db
        .connector_task_result(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap()
        .unwrap();
    assert_eq!(result.changed_paths, changed_paths);
    assert_eq!(
        result.decision_status,
        ConnectorResultDecisionStatus::Pending
    );
    assert!(matches!(
        db.append_connector_task_event(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "files_read",
            &serde_json::json!({}),
            103,
        ),
        Err(ConnectorTaskStoreError::InvalidState(_))
    ));
}

#[test]
fn task_and_run_lifecycle_contracts_are_distinct_from_effective_state() {
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::Active,
            ConnectorRunLifecycle::Running,
            None,
            false,
        ),
        ConnectorTaskState::Active
    );
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::ReadyForReview,
            ConnectorRunLifecycle::Completed,
            Some(ConnectorResultDecisionStatus::Pending),
            false,
        ),
        ConnectorTaskState::ReadyForReview
    );
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::ReadyForReview,
            ConnectorRunLifecycle::Completed,
            Some(ConnectorResultDecisionStatus::Accepted),
            false,
        ),
        ConnectorTaskState::Accepted
    );
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::ReadyForReview,
            ConnectorRunLifecycle::Completed,
            Some(ConnectorResultDecisionStatus::Rejected),
            false,
        ),
        ConnectorTaskState::Rejected
    );
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::Active,
            ConnectorRunLifecycle::Interrupted,
            None,
            false,
        ),
        ConnectorTaskState::NeedsAttention
    );
    assert_eq!(
        effective_task_state(
            ConnectorTaskLifecycle::Rejected,
            ConnectorRunLifecycle::Interrupted,
            Some(ConnectorResultDecisionStatus::Accepted),
            true,
        ),
        ConnectorTaskState::Cancelled,
        "cancellation must retain precedence over result and interruption projections"
    );
    for (mode, db) in [
        (ConnectorTaskMode::Normal, "normal"),
        (ConnectorTaskMode::ReadOnly, "read_only"),
        (ConnectorTaskMode::InspectLegacy, "inspect"),
    ] {
        assert_eq!(mode.as_db(), db);
        assert_eq!(ConnectorTaskMode::from_db(db, 0).unwrap(), mode);
    }
    assert!(ConnectorTaskMode::from_db("future_mode", 0).is_err());
    assert!(ConnectorTaskMode::requested("inspect").is_err());

    assert_eq!(
        effective_run_state(ConnectorRunLifecycle::Completed, true),
        ConnectorRunState::Cancelled
    );

    for (lifecycle, db) in [
        (ConnectorTaskLifecycle::Active, "active"),
        (ConnectorTaskLifecycle::ReadyForReview, "ready_for_review"),
        (ConnectorTaskLifecycle::Accepted, "accepted"),
        (ConnectorTaskLifecycle::Rejected, "rejected"),
    ] {
        assert_eq!(lifecycle.as_db(), db);
        assert_eq!(ConnectorTaskLifecycle::from_db(db, 0).unwrap(), lifecycle);
    }
    assert!(ConnectorTaskLifecycle::from_db("cancelled", 0).is_err());
    assert!(ConnectorTaskLifecycle::from_db("needs_attention", 0).is_err());

    for (lifecycle, db) in [
        (ConnectorRunLifecycle::Running, "running"),
        (ConnectorRunLifecycle::Completed, "completed"),
        (ConnectorRunLifecycle::Interrupted, "interrupted"),
    ] {
        assert_eq!(lifecycle.as_db(), db);
        assert_eq!(ConnectorRunLifecycle::from_db(db, 0).unwrap(), lifecycle);
    }
    assert!(ConnectorRunLifecycle::from_db("cancelled", 0).is_err());

    for (status, db) in [
        (ConnectorResultDecisionStatus::Pending, "pending"),
        (ConnectorResultDecisionStatus::Accepted, "accepted"),
        (ConnectorResultDecisionStatus::Rejected, "rejected"),
    ] {
        assert_eq!(status.as_db(), db);
        assert_eq!(
            ConnectorResultDecisionStatus::from_db(db, 0).unwrap(),
            status
        );
    }
    assert!(ConnectorResultDecisionStatus::from_db("future_state", 0).is_err());

    for (decision, db) in [
        (ConnectorResultDecision::Accepted, "accepted"),
        (ConnectorResultDecision::Rejected, "rejected"),
    ] {
        assert_eq!(decision.as_db(), db);
        assert_eq!(ConnectorResultDecision::from_db(db, 0).unwrap(), decision);
    }
    assert!(ConnectorResultDecision::from_db("pending", 0).is_err());

    for (state, db) in [
        (ConnectorResultDecisionRecoveryState::Pending, "pending"),
        (
            ConnectorResultDecisionRecoveryState::NeedsAttention,
            "needs_attention",
        ),
    ] {
        assert_eq!(state.as_db(), db);
        assert_eq!(
            ConnectorResultDecisionRecoveryState::from_db(db, 0).unwrap(),
            state
        );
    }
    assert!(ConnectorResultDecisionRecoveryState::from_db("future_state", 0).is_err());

    for (state, db) in [
        (ConnectorApprovalState::Pending, "pending"),
        (ConnectorApprovalState::Approved, "approved"),
        (ConnectorApprovalState::Denied, "denied"),
        (ConnectorApprovalState::Consumed, "consumed"),
        (ConnectorApprovalState::Expired, "expired"),
    ] {
        assert_eq!(state.as_db(), db);
        assert_eq!(ConnectorApprovalState::from_db(db, 0).unwrap(), state);
    }
    assert!(ConnectorApprovalState::from_db("future_state", 0).is_err());

    for (state, db) in [
        (ConnectorEditOperationState::Pending, "pending"),
        (ConnectorEditOperationState::Completed, "completed"),
        (ConnectorEditOperationState::Failed, "failed"),
    ] {
        assert_eq!(ConnectorEditOperationState::from_db(db, 0).unwrap(), state);
    }
    assert!(ConnectorEditOperationState::from_db("future_state", 0).is_err());
}

#[test]
fn cancellation_projection_does_not_pollute_persisted_task_or_run_lifecycle() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "project cancellation separately");
    db.finish_connector_task(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        NewConnectorResult {
            result_id: "wc_result_cancel_projection",
            summary: "done",
            patch_artifact: None,
            patch_sha256: None,
            patch_bytes: 0,
            changed_paths: &[],
            validation: &json!({"status": "recorded"}),
            warnings: &[],
        },
        102,
    )
    .unwrap();
    {
        let conn = db.conn_for_tests();
        conn.execute(
            "UPDATE wc_tasks SET status = 'accepted' WHERE id = ?1",
            [&task.task_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE wc_task_results SET decision_status = 'accepted' WHERE task_id = ?1",
            [&task.task_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wc_task_events
                 (id, task_id, run_id, sequence, kind, payload_json, created_at)
             VALUES (?1, ?2, ?3, 3, 'task_cancelled', '{}', 103)",
            rusqlite::params![new_id("wc_evt"), task.task_id, task.run_id],
        )
        .unwrap();
    }

    let projected = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(projected.task_lifecycle, ConnectorTaskLifecycle::Accepted);
    assert_eq!(projected.run_lifecycle, ConnectorRunLifecycle::Completed);
    assert_eq!(projected.task_status, ConnectorTaskState::Cancelled);
    assert_eq!(projected.run_status, ConnectorRunState::Cancelled);
    let reviewable = db.local_reviewable_tasks("wc_proj_demo", true, 20).unwrap();
    assert_eq!(reviewable.len(), 1);
    assert_eq!(reviewable[0].task_status, ConnectorTaskState::Cancelled);
    let mine = db
        .connector_tasks_for_subject("wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].task_status, ConnectorTaskState::Cancelled);
}

#[test]
fn task_snapshot_json_preserves_legacy_strings_and_hides_persisted_lifecycle_fields() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "serialize compatibly");
    let value = serde_json::to_value(&task).unwrap();
    assert_eq!(value["mode"], "normal");
    assert_eq!(value["task_status"], "active");
    assert_eq!(value["run_status"], "running");
    assert!(value.get("task_lifecycle").is_none());
    assert!(value.get("run_lifecycle").is_none());
    assert_eq!(
        serde_json::to_value(ConnectorApprovalState::Pending).unwrap(),
        "pending"
    );
    assert_eq!(
        serde_json::to_value(ConnectorResultDecisionRecoveryState::NeedsAttention).unwrap(),
        "needs_attention"
    );
}

#[test]
fn corrupt_persisted_task_and_run_lifecycles_fail_closed_on_load() {
    for target in ["task", "run"] {
        let (_temp, db) = database();
        bind(&db, "user:one");
        let task = start(&db, "user:one", "reject future state");
        let conn = db.conn_for_tests();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        match target {
            "task" => {
                conn.execute(
                    "UPDATE wc_tasks SET status = 'future_state' WHERE id = ?1",
                    [&task.task_id],
                )
                .unwrap();
            }
            "run" => {
                conn.execute(
                    "UPDATE wc_runs SET status = 'future_state' WHERE id = ?1",
                    [&task.run_id],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .unwrap();
        drop(conn);
        assert!(
            db.connector_task(&task.task_id, "wc_proj_demo", "user:one")
                .is_err(),
            "{target} future state was accepted"
        );
    }
}

#[test]
fn raw_command_approval_is_exact_and_consumed_once() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "run a generator");
    let pending = db
        .request_or_consume_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "commands_run",
            "exact-action-hash",
            "raw project command (20 bytes)",
            102,
            200,
        )
        .unwrap();
    let ConnectorApprovalGate::Pending(approval) = pending else {
        panic!("first exact action must wait for local approval");
    };
    let approved = db
        .decide_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            &approval.approval_id,
            true,
            "local_cli",
            None,
            103,
        )
        .unwrap();
    assert_eq!(approved.state, ConnectorApprovalState::Approved);

    let authorized = db
        .request_or_consume_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "commands_run",
            "exact-action-hash",
            "raw project command (20 bytes)",
            104,
            200,
        )
        .unwrap();
    assert!(matches!(authorized, ConnectorApprovalGate::Authorized(_)));
    let replay = db
        .request_or_consume_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "commands_run",
            "exact-action-hash",
            "raw project command (20 bytes)",
            105,
            200,
        )
        .unwrap();
    assert!(matches!(replay, ConnectorApprovalGate::Consumed(_)));
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            "task_started",
            "approval_requested",
            "approval_granted",
            "approval_consumed"
        ]
    );
}

#[test]
fn finishing_task_expires_unconsumed_command_approval() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "finish safely");
    let pending = db
        .request_or_consume_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "commands_run",
            "unconsumed-action",
            "raw project command",
            102,
            200,
        )
        .unwrap();
    let ConnectorApprovalGate::Pending(approval) = pending else {
        panic!("approval must initially be pending");
    };
    db.finish_connector_task(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        NewConnectorResult {
            result_id: "wc_result_2123456789abcdef",
            summary: "finished without the command",
            patch_artifact: None,
            patch_sha256: None,
            patch_bytes: 0,
            changed_paths: &[],
            validation: &serde_json::json!({"status": "not_run"}),
            warnings: &[],
        },
        103,
    )
    .unwrap();
    let stored = db
        .local_connector_task_approvals(&task.task_id, "wc_proj_demo")
        .unwrap();
    assert_eq!(stored[0].state, ConnectorApprovalState::Expired);
    assert!(matches!(
        db.decide_connector_approval(
            &task.task_id,
            "wc_proj_demo",
            &approval.approval_id,
            true,
            "local_cli",
            None,
            104
        ),
        Err(ConnectorTaskStoreError::InvalidState(_))
    ));
}

#[test]
fn restart_marks_unfinished_runs_for_attention() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "survive a restart");
    let recovery = db
        .reconcile_connector_executions("wc_proj_demo", 102)
        .unwrap();
    assert_eq!(recovery, (1, 0));
    let recovered = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(recovered.task_status, ConnectorTaskState::NeedsAttention);
    assert_eq!(recovered.run_status, ConnectorRunState::Interrupted);
    let preserved = db.connector_preserved_workspaces("wc_proj_demo").unwrap();
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0].task_id, task.task_id);
    assert_eq!(preserved[0].run_id, task.run_id);
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(events.last().unwrap().kind, "run_interrupted");
    let resumed = db
        .resume_connector_task(&task.task_id, "wc_proj_demo", "local_cli", 103)
        .unwrap();
    assert_eq!(resumed.task_status, ConnectorTaskState::Active);
    assert_eq!(resumed.run_status, ConnectorRunState::Running);
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(events.last().unwrap().kind, "run_resumed");
}

#[test]
fn interrupted_task_can_be_abandoned_without_capturing_workspace_changes() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "abandon after restart");
    db.reconcile_connector_executions("wc_proj_demo", 102)
        .unwrap();

    let result = db
        .abandon_interrupted_connector_task(&task.task_id, "wc_proj_demo", "local_cli", 103)
        .unwrap();
    assert_eq!(
        result.decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
    assert_eq!(result.patch_bytes, 0);
    assert_eq!(result.validation["status"], "not_run");
    assert!(db
        .connector_preserved_workspaces("wc_proj_demo")
        .unwrap()
        .is_empty());
    let decided = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(decided.task_status, ConnectorTaskState::Rejected);
    let cursor = db
        .record_connector_workspace_release(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            true,
            None,
            104,
        )
        .unwrap();
    assert_eq!(cursor, 4);
    let events = db
        .connector_task_events(&task.task_id, "wc_proj_demo", "user:one", 20)
        .unwrap();
    assert_eq!(events[2].kind, "task_abandoned");
    assert_eq!(events[3].kind, "workspace_release");
}

#[test]
fn local_result_decision_becomes_canonical_task_status() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "finish and accept");
    let changed_paths = vec!["src/lib.rs".to_string()];
    db.finish_connector_task(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        NewConnectorResult {
            result_id: "wc_result_1123456789abcdef",
            summary: "done",
            patch_artifact: None,
            patch_sha256: None,
            patch_bytes: 0,
            changed_paths: &changed_paths,
            validation: &serde_json::json!({"status": "recorded"}),
            warnings: &[],
        },
        102,
    )
    .unwrap();
    let release_cursor = db
        .record_connector_workspace_release(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            false,
            Some("slot cleanup needs retry"),
            103,
        )
        .unwrap();
    assert_eq!(release_cursor, 3);
    let result_id = "wc_result_1123456789abcdef";
    db.begin_connector_result_decision(
        &task.task_id,
        "wc_proj_demo",
        result_id,
        "accepted",
        "local_cli",
        104,
    )
    .unwrap();
    let result = db
        .finalize_connector_result_decision(&task.task_id, "wc_proj_demo", result_id, None, 104)
        .unwrap();
    assert_eq!(
        result.decision_status,
        ConnectorResultDecisionStatus::Accepted
    );
    assert_eq!(
        result.cleanup_warning.as_deref(),
        Some("slot cleanup needs retry")
    );
    let decided = db
        .connector_task(&task.task_id, "wc_proj_demo", "user:one")
        .unwrap();
    assert_eq!(decided.task_status, ConnectorTaskState::Accepted);
}
// -----------------------------------------------------------------------
// Guidance claim
// -----------------------------------------------------------------------

fn guide(db: &Database, task: &ConnectorTaskSnapshot, message: &str) -> i64 {
    db.append_connector_task_event(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        "human_guidance",
        &json!({ "message": message, "source": "host" }),
        200,
    )
    .unwrap()
}

fn claim(db: &Database, task: &ConnectorTaskSnapshot) -> Vec<ConnectorTaskEvent> {
    db.claim_pending_connector_guidance(&task.task_id, "wc_proj_demo", "user:one", 16)
        .unwrap()
}

#[test]
fn concurrent_capability_responses_claim_guidance_once() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    guide(&db, &task, "focus on the parser first");

    // Two capability responses racing for the same message: the claim and
    // the watermark advance share a transaction, so the loser sees nothing
    // rather than delivering a duplicate.
    let db = std::sync::Arc::new(db);
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let task = task.clone();
            std::thread::spawn(move || claim(&db, &task))
        })
        .collect();
    let claimed: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    let total: usize = claimed.iter().map(Vec::len).sum();
    assert_eq!(
        total, 1,
        "guidance was delivered {total} times: {claimed:?}"
    );
}

#[test]
fn guidance_survives_more_than_fifty_unrelated_events() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    let sequence = guide(&db, &task, "look at the lexer");

    // Far more than the timeline window, so a "recent events" scan would
    // no longer see the guidance at all.
    for index in 0..60 {
        db.append_connector_task_event(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "files_read",
            &json!({ "index": index }),
            300,
        )
        .unwrap();
    }

    let claimed = claim(&db, &task);
    assert_eq!(claimed.len(), 1, "{claimed:?}");
    assert_eq!(claimed[0].sequence, sequence);
    assert_eq!(claimed[0].payload["message"], "look at the lexer");
}

#[test]
fn multiple_guidance_messages_are_delivered_in_sequence() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    for message in ["first", "second", "third"] {
        guide(&db, &task, message);
    }

    let claimed = claim(&db, &task);
    let messages: Vec<&str> = claimed
        .iter()
        .map(|event| event.payload["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages, ["first", "second", "third"]);
    assert!(
        claimed
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence),
        "guidance is not in sequence order: {claimed:?}"
    );
    // Claimed once, and a message recorded afterwards is still delivered.
    assert!(claim(&db, &task).is_empty());
    guide(&db, &task, "fourth");
    let later = claim(&db, &task);
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].payload["message"], "fourth");
}

#[test]
fn failed_claim_does_not_advance_the_watermark() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    bind(&db, "user:two");
    let task = start(&db, "user:one", "fix the parser");
    guide(&db, &task, "still pending");

    // A claim for the wrong subject fails and must not consume anything.
    let denied = db.claim_pending_connector_guidance(&task.task_id, "wc_proj_demo", "user:two", 16);
    assert!(matches!(denied, Err(ConnectorTaskStoreError::NotFound)));

    let claimed = claim(&db, &task);
    assert_eq!(claimed.len(), 1, "the failed claim swallowed the guidance");
    assert_eq!(claimed[0].payload["message"], "still pending");
}

#[test]
fn read_state_reports_pending_and_claimed_guidance_without_consuming() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");

    // No guidance yet: the watermark is zero and nothing is pending.
    let state = db
        .connector_guidance_read_state(&task.task_id, "wc_proj_demo")
        .unwrap()
        .expect("task exists");
    assert_eq!(state.seen_seq, 0);
    assert_eq!(state.last_pending_seq, None);

    let first = guide(&db, &task, "first");
    let second = guide(&db, &task, "second");

    // Both pending: the newest pending sequence is reported while the
    // watermark stays at zero.
    let state = db
        .connector_guidance_read_state(&task.task_id, "wc_proj_demo")
        .unwrap()
        .unwrap();
    assert_eq!(state.seen_seq, 0);
    assert_eq!(state.last_pending_seq, Some(second));

    // The read-only query did not advance the watermark: a real claim
    // still sees both messages.
    let claimed = claim(&db, &task);
    assert_eq!(claimed.len(), 2);
    assert_eq!(claimed[0].sequence, first);
    assert_eq!(claimed[1].sequence, second);

    // After the claim the watermark catches up and nothing is pending.
    let state = db
        .connector_guidance_read_state(&task.task_id, "wc_proj_demo")
        .unwrap()
        .unwrap();
    assert_eq!(state.seen_seq, second);
    assert_eq!(state.last_pending_seq, None);

    // A guidance that arrives later is reported as pending above the
    // advanced watermark, while the claimed one stays read.
    let third = guide(&db, &task, "third");
    let state = db
        .connector_guidance_read_state(&task.task_id, "wc_proj_demo")
        .unwrap()
        .unwrap();
    assert_eq!(state.seen_seq, second);
    assert_eq!(state.last_pending_seq, Some(third));
}

#[test]
fn read_state_returns_none_for_missing_task() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    assert!(db
        .connector_guidance_read_state("wc_task_missing", "wc_proj_demo")
        .unwrap()
        .is_none());
}

// -----------------------------------------------------------------------
// Applied paths
// -----------------------------------------------------------------------

fn applied(db: &Database, task: &ConnectorTaskSnapshot, cap: usize) -> AppliedPaths {
    db.connector_task_applied_paths(&task.task_id, "wc_proj_demo", "user:one", cap)
        .unwrap()
}

fn edits(db: &Database, task: &ConnectorTaskSnapshot, payload: Value) {
    db.append_connector_task_event(
        &task.task_id,
        "wc_proj_demo",
        "user:one",
        "edits_apply",
        &payload,
        400,
    )
    .unwrap();
}

#[test]
fn active_review_keeps_paths_older_than_fifty_events() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    edits(
        &db,
        &task,
        json!({ "ok": true, "dry_run": false, "changed_paths": ["src/early.rs"] }),
    );
    for index in 0..60 {
        db.append_connector_task_event(
            &task.task_id,
            "wc_proj_demo",
            "user:one",
            "files_read",
            &json!({ "index": index }),
            401,
        )
        .unwrap();
    }

    let applied = applied(&db, &task, 200);
    assert_eq!(applied.paths, vec!["src/early.rs".to_string()]);
    assert!(applied.complete);
}

#[test]
fn applied_paths_ignore_failed_and_dry_run_edits() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    edits(
        &db,
        &task,
        json!({ "ok": true, "dry_run": true, "changed_paths": ["dry.rs"] }),
    );
    edits(
        &db,
        &task,
        json!({ "ok": false, "dry_run": false, "changed_paths": ["failed.rs"] }),
    );
    edits(
        &db,
        &task,
        json!({ "ok": true, "dry_run": false, "changed_paths": ["real.rs"] }),
    );

    let applied = applied(&db, &task, 200);
    assert_eq!(applied.paths, vec!["real.rs".to_string()]);
    assert!(applied.complete);
}

#[test]
fn renames_include_source_and_destination() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    // The recorded payload carries both sides of a rename.
    edits(
        &db,
        &task,
        json!({
            "ok": true,
            "dry_run": false,
            "changed_paths": ["src/old.rs", "src/new.rs"]
        }),
    );
    let applied = applied(&db, &task, 200);
    assert_eq!(
        applied.paths,
        vec!["src/old.rs".to_string(), "src/new.rs".to_string()]
    );
}

#[test]
fn applied_paths_are_distinct_and_stably_ordered() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    for paths in [
        json!(["a.rs", "b.rs"]),
        json!(["b.rs", "c.rs"]),
        json!(["a.rs"]),
    ] {
        edits(
            &db,
            &task,
            json!({ "ok": true, "dry_run": false, "changed_paths": paths }),
        );
    }
    let applied = applied(&db, &task, 200);
    // First-seen order, each path once.
    assert_eq!(
        applied.paths,
        vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()]
    );
    assert_eq!(applied.total, 3);
    assert!(applied.complete);
}

#[test]
fn a_truncated_applied_path_list_never_claims_to_be_complete() {
    let (_temp, db) = database();
    bind(&db, "user:one");
    let task = start(&db, "user:one", "fix the parser");
    edits(
        &db,
        &task,
        json!({
            "ok": true,
            "dry_run": false,
            "changed_paths": ["a.rs", "b.rs", "c.rs"]
        }),
    );
    edits(
        &db,
        &task,
        json!({
            "ok": true,
            "dry_run": false,
            "changed_paths": ["c.rs", "d.rs", "c.rs"]
        }),
    );
    let applied = applied(&db, &task, 2);
    assert_eq!(applied.paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    assert_eq!(
        applied.total, 4,
        "duplicates beyond the returned cap must not inflate the distinct total"
    );
    assert!(!applied.complete, "a truncated list claimed completeness");
}
