use crate::workspace::{LocalResultDecision, WorkspaceManager};
use crate::{ConnectorContext, ConnectorRuntime};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use webcodex_runner_registry::RunnerRegistry;
use webcodex_store::{
    ConnectorBinding, ConnectorResultDecisionRecoveryState, ConnectorResultDecisionStatus,
    ConnectorRunState, ConnectorTaskMode, ConnectorTaskResult, ConnectorTaskState,
    ConnectorTaskStoreError, Database, NewConnectorResult, NewConnectorTask,
};

const TASK_ID: &str = "wc_task_f123456789abcdef0123456789abcdef";
const RESULT_ID: &str = "wc_result_f123456789abcdef";
const SUBJECT: &str = "user:owner";

struct Fixture {
    temp: tempfile::TempDir,
    context: ConnectorContext,
    db: Database,
}

impl Fixture {
    fn decide(
        &self,
        result_id: Option<&str>,
        decision: LocalResultDecision,
        now: i64,
    ) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
        decide(&self.db, &self.context, result_id, decision, now)
    }
}

fn fixture(finish: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    git(&root, &["config", "core.longpaths", "true"]);
    fs::write(root.join("README.md"), "before\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(
        &root,
        &[
            "-c",
            "user.name=WebCodex Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    );
    let state = temp.path().join("state");
    let context = ConnectorContext {
        project_id: "wc_proj_1234567890".to_string(),
        project_name: "project".to_string(),
        workspace_id: "wc_ws_1234567890".to_string(),
        executor_project: "agent:hosted:project".to_string(),
        executor_root: root.to_string_lossy().into_owned(),
        runs_root: state.join("runs").to_string_lossy().into_owned(),
        results_root: state.join("results").to_string_lossy().into_owned(),
        project_registry_dir: state
            .join("agent/project-registry")
            .to_string_lossy()
            .into_owned(),
        profile: "personal".to_string(),
        project_grant_id: "wc_pgrant_1111111111111111".to_string(),
    };
    let db = Database::open(&temp.path().join("connector.db")).unwrap();
    db.ensure_connector_binding(ConnectorBinding {
        project_id: &context.project_id,
        project_name: &context.project_name,
        workspace_id: &context.workspace_id,
        executor_ref: &context.executor_project,
        subject_id: SUBJECT,
        profile: "personal",
        now: 1,
    })
    .unwrap();
    let manager = WorkspaceManager::new(&context).unwrap();
    let prepared = manager
        .prepare(
            &context,
            TASK_ID,
            "wc_run_f123456789abcdef0123456789abcdef",
            false,
        )
        .unwrap();
    let task = db
        .start_connector_task(NewConnectorTask {
            task_id: TASK_ID,
            run_id: &prepared.run_id,
            project_id: &context.project_id,
            workspace_id: &context.workspace_id,
            subject_id: SUBJECT,
            goal: "update readme",
            mode: "normal",
            target_executor_ref: &context.executor_project,
            execution_executor_ref: &prepared.execution_executor_ref,
            target_root: &context.executor_root,
            execution_root: &prepared.execution_root,
            baseline_commit: prepared.baseline_commit.as_deref(),
            baseline_tree: prepared.baseline_tree.as_deref(),
            isolated: true,
            now: 2,
        })
        .unwrap();
    if finish {
        fs::write(Path::new(&task.execution_root).join("README.md"), "after\n").unwrap();
        let captured = manager.capture_result(&task).unwrap();
        db.finish_connector_task(
            TASK_ID,
            &context.project_id,
            SUBJECT,
            NewConnectorResult {
                result_id: RESULT_ID,
                summary: "updated readme",
                patch_artifact: captured.patch_artifact.as_deref(),
                patch_sha256: captured.patch_sha256.as_deref(),
                patch_bytes: captured.patch_bytes,
                changed_paths: &captured.changed_paths,
                validation: &serde_json::json!({"status": "not_run"}),
                warnings: &captured.warnings,
            },
            3,
        )
        .unwrap();
    }
    Fixture { temp, context, db }
}

fn decide(
    db: &Database,
    context: &ConnectorContext,
    result_id: Option<&str>,
    decision: LocalResultDecision,
    now: i64,
) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
    WorkspaceManager::decide_connector_result_local(
        db,
        &context.project_id,
        TASK_ID,
        result_id,
        Path::new(&context.executor_root),
        decision,
        "local_test",
        None,
        now,
    )
}

fn assert_decision_error(
    result: Result<ConnectorTaskResult, ConnectorTaskStoreError>,
    expected: &str,
) {
    assert!(matches!(
        result,
        Err(ConnectorTaskStoreError::Decision(code, _)) if code == expected
    ));
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn target(context: &ConnectorContext) -> PathBuf {
    Path::new(&context.executor_root).join("README.md")
}

fn reopen_runtime(
    context: ConnectorContext,
    db: Arc<Database>,
) -> Result<ConnectorRuntime, String> {
    ConnectorRuntime::new(Arc::new(RunnerRegistry::default()), db, context)
}

#[test]
fn queue_filters_completed_history_before_limit() {
    let fx = fixture(false);
    fx.db
        .conn_for_tests()
        .execute_batch(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 81)
         INSERT INTO wc_tasks
             (id, project_id, owner_subject_id, goal, mode, status, created_at, updated_at)
         SELECT printf('wc_task_history_%04d', x), 'wc_proj_1234567890', 'user:owner',
                'closed history', 'normal', 'ready_for_review', 100 + x, 100 + x FROM n;
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 81)
         INSERT INTO wc_runs (id, task_id, workspace_id, status, started_at, finished_at)
         SELECT printf('wc_run_history_%04d', x), printf('wc_task_history_%04d', x),
                'wc_ws_1234567890', 'completed', 100 + x, 100 + x FROM n;
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 81)
         INSERT INTO wc_task_results
             (id, task_id, run_id, summary, patch_bytes, changed_paths_json,
              validation_json, warnings_json, decision_status, created_at)
         SELECT printf('wc_result_history_%04d', x), printf('wc_task_history_%04d', x),
                printf('wc_run_history_%04d', x), 'closed', 0, '[]',
                '{\"status\":\"passed\"}', '[]', 'accepted', 100 + x FROM n;",
        )
        .unwrap();
    let rows = fx
        .db
        .local_reviewable_tasks(&fx.context.project_id, false, 20)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task_id, TASK_ID);
}

#[test]
fn local_decision_binds_result_and_exact_retry() {
    let fx = fixture(true);
    let cursor = fx
        .db
        .local_connector_task(TASK_ID, &fx.context.project_id)
        .unwrap()
        .event_cursor;
    assert_decision_error(
        fx.decide(Some("wc_result_stale"), LocalResultDecision::Accept, 4),
        "result_changed",
    );
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "before\n");
    assert_eq!(
        fx.db
            .local_connector_task(TASK_ID, &fx.context.project_id)
            .unwrap()
            .event_cursor,
        cursor
    );
    fx.decide(Some(RESULT_ID), LocalResultDecision::Accept, 5)
        .unwrap();
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "after\n");
    assert_decision_error(
        fx.decide(Some(RESULT_ID), LocalResultDecision::Accept, 6),
        "result_already_decided",
    );
}

#[test]
fn local_accept_preserves_workspace_preconditions() {
    for case in ["head", "artifact"] {
        let fx = fixture(true);
        if case == "head" {
            git(
                Path::new(&fx.context.executor_root),
                &[
                    "-c",
                    "user.name=WebCodex Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "moved",
                ],
            );
        } else {
            let result = fx
                .db
                .local_connector_task_result(TASK_ID, &fx.context.project_id)
                .unwrap()
                .unwrap();
            fs::write(result.patch_artifact.unwrap(), "tampered\n").unwrap();
        }
        let error = fx
            .decide(Some(RESULT_ID), LocalResultDecision::Accept, 4)
            .unwrap_err();
        assert!(matches!(
            error,
            ConnectorTaskStoreError::Decision(
                "target_checkout_changed" | "result_precondition_failed",
                _
            )
        ));
    }
}

#[test]
fn finalization_failure_is_recovered_once_after_reopen() {
    let fx = fixture(true);
    fx.db
        .conn_for_tests()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_result_finalize
             BEFORE UPDATE OF decision_status ON wc_task_results
             WHEN NEW.decision_status = 'accepted'
             BEGIN SELECT RAISE(FAIL, 'injected finalization failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        fx.decide(Some(RESULT_ID), LocalResultDecision::Accept, 4),
        Err(ConnectorTaskStoreError::Storage(_))
    ));
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "after\n");
    assert_eq!(
        fx.db
            .local_connector_task_result(TASK_ID, &fx.context.project_id)
            .unwrap()
            .unwrap()
            .decision_status,
        ConnectorResultDecisionStatus::Pending
    );
    let Fixture { temp, context, db } = fx;
    drop(db);
    let reopened = Database::open(&temp.path().join("connector.db")).unwrap();
    assert_eq!(
        WorkspaceManager::recover_result_decisions(
            &reopened,
            &context.project_id,
            Path::new(&context.executor_root),
            5,
        )
        .unwrap(),
        1
    );
    assert_eq!(
        reopened
            .local_connector_task_result(TASK_ID, &context.project_id)
            .unwrap()
            .unwrap()
            .decision_status,
        ConnectorResultDecisionStatus::Accepted
    );
    assert_eq!(fs::read_to_string(target(&context)).unwrap(), "after\n");
    assert_eq!(
        WorkspaceManager::recover_result_decisions(
            &reopened,
            &context.project_id,
            Path::new(&context.executor_root),
            6,
        )
        .unwrap(),
        0
    );
}

#[test]
fn unrecoverable_accept_is_quarantined_while_other_intents_recover_and_runtime_starts() {
    const SECOND_TASK_ID: &str = "wc_task_e123456789abcdef0123456789abcdef";
    const SECOND_RUN_ID: &str = "wc_run_e123456789abcdef0123456789abcdef";
    const SECOND_RESULT_ID: &str = "wc_result_e123456789abcdef";

    let fx = fixture(true);
    {
        let conn = fx.db.conn_for_tests();
        conn.execute(
            "INSERT INTO wc_tasks
                (id, project_id, owner_subject_id, goal, mode, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'second result', 'normal', 'ready_for_review', 3, 3)",
            rusqlite::params![SECOND_TASK_ID, fx.context.project_id, SUBJECT],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wc_runs
                (id, task_id, workspace_id, status, started_at, finished_at)
             VALUES (?1, ?2, ?3, 'completed', 2, 3)",
            rusqlite::params![SECOND_RUN_ID, SECOND_TASK_ID, fx.context.workspace_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wc_run_contexts
                (run_id, target_executor_ref, execution_executor_ref, target_root,
                 execution_root, baseline_commit, baseline_tree, isolated, created_at)
             VALUES (?1, ?2, ?2, ?3, ?3, NULL, NULL, 0, 2)",
            rusqlite::params![
                SECOND_RUN_ID,
                fx.context.executor_project,
                fx.context.executor_root
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wc_task_results
                (id, task_id, run_id, summary, patch_bytes, changed_paths_json,
                 validation_json, warnings_json, decision_status, created_at)
             VALUES (?1, ?2, ?3, 'no changes', 0, '[]',
                     '{\"status\":\"not_run\"}', '[]', 'pending', 3)",
            rusqlite::params![SECOND_RESULT_ID, SECOND_TASK_ID, SECOND_RUN_ID],
        )
        .unwrap();
    }
    fx.db
        .begin_connector_result_decision(
            TASK_ID,
            &fx.context.project_id,
            RESULT_ID,
            "accepted",
            "local_test",
            4,
        )
        .unwrap();
    fx.db
        .begin_connector_result_decision(
            SECOND_TASK_ID,
            &fx.context.project_id,
            SECOND_RESULT_ID,
            "rejected",
            "local_test",
            5,
        )
        .unwrap();
    git(
        Path::new(&fx.context.executor_root),
        &[
            "-c",
            "user.name=WebCodex Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "moved target",
        ],
    );

    let Fixture { temp, context, db } = fx;
    drop(db);
    let reopened = Arc::new(Database::open(&temp.path().join("connector.db")).unwrap());
    let runtime = reopen_runtime(context.clone(), reopened.clone())
        .expect("one stale intent must not block ConnectorRuntime startup");

    let bad = reopened
        .local_connector_task_result(TASK_ID, &context.project_id)
        .unwrap()
        .unwrap();
    let recovery = bad
        .recovery
        .as_ref()
        .expect("stale intent must be observable");
    assert_eq!(bad.decision_status, ConnectorResultDecisionStatus::Pending);
    assert_eq!(
        recovery.state,
        ConnectorResultDecisionRecoveryState::NeedsAttention
    );
    assert_eq!(
        recovery.error_code.as_deref(),
        Some("target_checkout_changed")
    );
    assert_eq!(
        super::result_projection(&bad)["recovery"]["state"],
        "needs_attention"
    );
    assert_eq!(
        reopened
            .local_connector_task_result(SECOND_TASK_ID, &context.project_id)
            .unwrap()
            .unwrap()
            .decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
    let queue = reopened
        .local_reviewable_tasks(&context.project_id, false, 20)
        .unwrap();
    assert!(queue
        .iter()
        .any(|task| task.task_id == TASK_ID
            && task.task_status == ConnectorTaskState::NeedsAttention));
    assert_decision_error(
        WorkspaceManager::decide_connector_result_local(
            &reopened,
            &context.project_id,
            TASK_ID,
            Some(RESULT_ID),
            Path::new(&context.executor_root),
            LocalResultDecision::Accept,
            "local_test",
            None,
            9,
        ),
        "result_decision_in_progress",
    );

    drop(runtime);
    reopen_runtime(context.clone(), reopened.clone())
        .expect("quarantined intent must remain non-blocking on later restarts");
    let event_count: i64 = reopened
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_task_events
             WHERE task_id = ?1 AND kind = 'result_recovery_needs_attention'",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 1, "restart must not repeat quarantine effects");

    let rejected = WorkspaceManager::decide_connector_result_local(
        &reopened,
        &context.project_id,
        TASK_ID,
        Some(RESULT_ID),
        Path::new(&context.executor_root),
        LocalResultDecision::Reject,
        "local_test",
        None,
        10,
    )
    .unwrap();
    assert_eq!(
        rejected.decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
    assert!(rejected.recovery.is_none());
    let task = reopened
        .local_connector_task(TASK_ID, &context.project_id)
        .unwrap();
    assert_eq!(task.task_status, ConnectorTaskState::Rejected);
    assert_eq!(task.run_status, ConnectorRunState::Completed);
    let (stored_task_status, stored_run_status, finished_at): (String, String, Option<i64>) =
        reopened
            .conn_for_tests()
            .query_row(
                "SELECT t.status, r.status, r.finished_at
                 FROM wc_tasks t JOIN wc_runs r ON r.task_id = t.id
                 WHERE t.id = ?1",
                [TASK_ID],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(stored_task_status, "rejected");
    assert_eq!(stored_run_status, "completed");
    assert!(finished_at.is_some());
    let intent_count: i64 = reopened
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_result_decision_intents WHERE task_id = ?1",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(intent_count, 0);
    assert!(!Path::new(&context.runs_root)
        .join(".write-slot-01.lease.json")
        .exists());
    assert!(reopened
        .connector_preserved_workspaces(&context.project_id)
        .unwrap()
        .iter()
        .all(|workspace| workspace.task_id != TASK_ID));

    let retry = WorkspaceManager::decide_connector_result_local(
        &reopened,
        &context.project_id,
        TASK_ID,
        Some(RESULT_ID),
        Path::new(&context.executor_root),
        LocalResultDecision::Reject,
        "local_test",
        None,
        11,
    )
    .unwrap();
    assert_eq!(retry, rejected);

    reopen_runtime(context.clone(), reopened.clone())
        .expect("completed Reject must stay terminal on later restarts");
    let event_counts: (i64, i64) = reopened
        .conn_for_tests()
        .query_row(
            "SELECT
                SUM(CASE WHEN kind = 'result_recovery_needs_attention' THEN 1 ELSE 0 END),
                SUM(CASE WHEN kind = 'task_rejected' THEN 1 ELSE 0 END)
             FROM wc_task_events WHERE task_id = ?1",
            [TASK_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(event_counts, (1, 1));
    assert_eq!(
        WorkspaceManager::recover_result_decisions(
            reopened.as_ref(),
            &context.project_id,
            Path::new(&context.executor_root),
            12,
        )
        .unwrap(),
        0
    );
}

#[test]
fn concurrent_decisions_apply_once() {
    let fx = fixture(true);
    let db = Arc::new(fx.db);
    let barrier = Arc::new(Barrier::new(3));
    let threads = [4, 5].map(|now| {
        let (db, context, barrier) = (db.clone(), fx.context.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            decide(
                &db,
                &context,
                Some(RESULT_ID),
                LocalResultDecision::Accept,
                now,
            )
        })
    });
    barrier.wait();
    let outcomes = threads.map(|thread| thread.join().unwrap());
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "after\n");
}

#[test]
fn interrupted_no_result_reject_is_the_only_identity_exception() {
    let fx = fixture(false);
    fx.db
        .reconcile_connector_executions(&fx.context.project_id, 4)
        .unwrap();
    assert_decision_error(
        fx.decide(Some("wc_result_claimed"), LocalResultDecision::Reject, 5),
        "result_changed",
    );
    fx.decide(None, LocalResultDecision::Reject, 6).unwrap();
    assert_eq!(
        fx.db
            .local_connector_task_result(TASK_ID, &fx.context.project_id)
            .unwrap()
            .unwrap()
            .decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
}

#[test]
fn persisted_read_only_isolated_result_cannot_be_accepted() {
    let fx = fixture(true);
    fx.db
        .conn_for_tests()
        .execute(
            "UPDATE wc_tasks SET mode = 'read_only' WHERE id = ?1",
            [TASK_ID],
        )
        .unwrap();
    let malformed = fx
        .db
        .local_connector_task(TASK_ID, &fx.context.project_id)
        .unwrap();
    assert_eq!(malformed.mode, ConnectorTaskMode::ReadOnly);
    assert!(malformed.isolated);

    assert_decision_error(
        fx.decide(Some(RESULT_ID), LocalResultDecision::Accept, 4),
        "result_precondition_failed",
    );
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "before\n");
    assert_eq!(
        fx.db
            .local_connector_task_result(TASK_ID, &fx.context.project_id)
            .unwrap()
            .unwrap()
            .decision_status,
        ConnectorResultDecisionStatus::Pending
    );
}

#[test]
fn legacy_inspect_interrupted_task_can_be_rejected_but_never_accepted() {
    let fx = fixture(false);
    fx.db
        .conn_for_tests()
        .execute(
            "UPDATE wc_tasks SET mode = 'inspect' WHERE id = ?1",
            [TASK_ID],
        )
        .unwrap();
    fx.db
        .reconcile_connector_executions(&fx.context.project_id, 4)
        .unwrap();

    assert_decision_error(
        fx.decide(None, LocalResultDecision::Accept, 5),
        "inspect_mode_retired",
    );
    let rejected = fx
        .decide(None, LocalResultDecision::Reject, 6)
        .expect("legacy inspect cleanup must remain rejectable");
    assert_eq!(
        rejected.decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
    let task = fx
        .db
        .local_connector_task(TASK_ID, &fx.context.project_id)
        .unwrap();
    assert_eq!(task.mode, ConnectorTaskMode::InspectLegacy);
    assert_eq!(task.task_status, ConnectorTaskState::Rejected);
    assert_eq!(fs::read_to_string(target(&fx.context)).unwrap(), "before\n");
}

#[test]
fn rejected_cleanup_can_be_retried() {
    let fx = fixture(true);
    let lease = Path::new(&fx.context.runs_root).join(".write-slot-01.lease.json");
    fs::write(&lease, "broken").unwrap();
    let first = fx
        .decide(Some(RESULT_ID), LocalResultDecision::Reject, 4)
        .unwrap();
    assert!(first.cleanup_warning.is_some());
    fs::remove_file(lease).unwrap();
    let retry = fx
        .decide(Some(RESULT_ID), LocalResultDecision::Reject, 5)
        .unwrap();
    assert!(retry.cleanup_warning.is_none());
}

#[test]
fn validate_path_rejects_parent_traversal() {
    use crate::projections::validate_path;

    // Leading `/` and NUL were already rejected. Parent traversal was not, so a
    // connector-surface path could resolve outside the granted project once the
    // agent joined it against the project root.
    for path in [
        "../outside.txt",
        "src/../../outside.txt",
        "a/b/../../../etc/passwd",
        "..",
    ] {
        assert!(
            validate_path(path).is_err(),
            "validate_path accepted traversal {path:?}"
        );
    }

    for path in ["src/main.rs", "a/b/c.txt", "README.md", "./src/lib.rs"] {
        assert!(
            validate_path(path).is_ok(),
            "validate_path rejected legitimate path {path:?}"
        );
    }
}

#[test]
fn reject_reason_reaches_the_model_as_guidance_once() {
    let fixture = fixture(true);
    let rejected = WorkspaceManager::decide_connector_result_local(
        &fixture.db,
        &fixture.context.project_id,
        TASK_ID,
        Some(RESULT_ID),
        Path::new(&fixture.context.executor_root),
        LocalResultDecision::Reject,
        "local_test",
        Some("  the tests were not actually run  "),
        5,
    )
    .unwrap();
    assert_eq!(
        rejected.decision_status,
        ConnectorResultDecisionStatus::Rejected
    );
    let payload: String = fixture
        .db
        .conn_for_tests()
        .query_row(
            "SELECT payload_json FROM wc_task_events
             WHERE task_id = ?1 AND kind = 'human_guidance'",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let message = payload["message"].as_str().unwrap();
    assert!(message.contains("rejected"));
    assert!(
        message.ends_with("the tests were not actually run"),
        "reason must be trimmed into the message: {message}"
    );
    assert_eq!(payload["source"], "host_reject");
    assert_eq!(payload["result_id"], RESULT_ID);

    // An idempotent re-reject reports the same terminal decision and must not
    // append a second guidance message with a different meaning.
    WorkspaceManager::decide_connector_result_local(
        &fixture.db,
        &fixture.context.project_id,
        TASK_ID,
        Some(RESULT_ID),
        Path::new(&fixture.context.executor_root),
        LocalResultDecision::Reject,
        "local_test",
        Some("a different reason"),
        6,
    )
    .unwrap();
    let count: i64 = fixture
        .db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_task_events
             WHERE task_id = ?1 AND kind = 'human_guidance'",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "re-reject must not duplicate guidance");
}

#[test]
fn reject_without_reason_records_no_guidance() {
    let fixture = fixture(true);
    decide(
        &fixture.db,
        &fixture.context,
        Some(RESULT_ID),
        LocalResultDecision::Reject,
        5,
    )
    .unwrap();
    let count: i64 = fixture
        .db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_task_events
             WHERE task_id = ?1 AND kind = 'human_guidance'",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "a silent reject must stay silent");
}

#[test]
fn connector_tasks_for_subject_scopes_to_the_owner() {
    let fixture = fixture(true);
    let mine = fixture
        .db
        .connector_tasks_for_subject(&fixture.context.project_id, SUBJECT, 10)
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].task_id, TASK_ID);
    assert_eq!(mine[0].task_status, ConnectorTaskState::ReadyForReview);
    assert_eq!(mine[0].next_action, "review_and_accept");
    assert_eq!(mine[0].unread_guidance, 0, "no guidance recorded yet");

    // Ownership is the visibility boundary: another subject sees nothing,
    // not a filtered view of someone else's history.
    let strangers = fixture
        .db
        .connector_tasks_for_subject(&fixture.context.project_id, "user:someone_else", 10)
        .unwrap();
    assert!(strangers.is_empty());
}

#[test]
fn reviewable_tasks_report_unread_guidance_until_the_model_claims_it() {
    use serde_json::json;
    let fixture = fixture(true);

    // Guidance is durable state in the event log; write the row directly so
    // this test is independent of the task-state guard that `append` enforces
    // for in-progress capability calls. A fresh guidance event sits above the
    // zero watermark, so the work queue flags it as unread.
    let conn = fixture.db.conn_for_tests();
    let next_sequence: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM wc_task_events WHERE task_id = ?1",
            [TASK_ID],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO wc_task_events (id, task_id, run_id, sequence, kind, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, 'human_guidance', ?5, 200)",
        rusqlite::params![
            "wc_evt_unread_test",
            TASK_ID,
            "wc_run_f123456789abcdef0123456789abcdef",
            next_sequence,
            json!({ "message": "look at the lexer", "source": "host" }).to_string(),
        ],
    )
    .unwrap();
    drop(conn);

    let listed = fixture
        .db
        .local_reviewable_tasks(&fixture.context.project_id, false, 20)
        .unwrap();
    let mine = fixture
        .db
        .connector_tasks_for_subject(&fixture.context.project_id, SUBJECT, 10)
        .unwrap();
    assert_eq!(listed[0].unread_guidance, 1);
    assert_eq!(mine[0].unread_guidance, 1);

    // Once the model claims it, the watermark advances and the queue no longer
    // flags the task — the same read-state the timeline renders with.
    fixture
        .db
        .claim_pending_connector_guidance(TASK_ID, &fixture.context.project_id, SUBJECT, 16)
        .unwrap();
    let listed = fixture
        .db
        .local_reviewable_tasks(&fixture.context.project_id, false, 20)
        .unwrap();
    assert_eq!(
        listed[0].unread_guidance, 0,
        "claimed guidance is no longer unread"
    );
}
