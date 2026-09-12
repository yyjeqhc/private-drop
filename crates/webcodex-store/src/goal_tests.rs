use super::communication::{CommunicationPrincipal, COMMUNICATION_PRINCIPAL_DIGEST_PREFIX};
use super::goal::*;
use super::Database;

const T0: i64 = 10_000;

fn principal(hex: char) -> CommunicationPrincipal {
    CommunicationPrincipal {
        kind: "user".to_string(),
        digest: format!(
            "{COMMUNICATION_PRINCIPAL_DIGEST_PREFIX}{}",
            hex.to_string().repeat(64)
        ),
    }
}

fn input(key: &str) -> NewGoal {
    NewGoal {
        title: "Ship durable Goal foundation".to_string(),
        objective: "Preserve high-level durable intent without granting execution authority."
            .to_string(),
        idempotency_key: key.to_string(),
    }
}

#[test]
fn create_read_list_update_and_terminal_replay_are_durable_and_revisioned() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("goals.db");
    let owner = principal('a');
    let goal_id = {
        let db = Database::open(&path).unwrap();
        let created = db.create_goal_at(&owner, input("create-goal"), T0).unwrap();
        assert!(created.created);
        assert!(!created.replayed);
        assert!(created.state_changed);
        assert!(created.goal.summary.goal_id.starts_with(GOAL_ID_PREFIX));
        assert_eq!(created.goal.summary.lifecycle, GoalLifecycle::Active);
        assert_eq!(created.goal.summary.revision, 1);
        assert!(created.goal.correlations.is_empty());

        let replay = db
            .create_goal_at(&owner, input("create-goal"), T0 + 1)
            .unwrap();
        assert!(replay.replayed);
        assert!(!replay.state_changed);
        assert_eq!(replay.goal.summary.goal_id, created.goal.summary.goal_id);

        let mut changed_create = input("create-goal");
        changed_create.objective.push_str(" Changed.");
        let conflict = db
            .create_goal_at(&owner, changed_create, T0 + 2)
            .unwrap_err();
        assert_eq!(conflict.code(), "goal_idempotency_conflict");

        let updated = db
            .update_goal_at(
                &owner,
                &created.goal.summary.goal_id,
                1,
                GoalPatch {
                    title: Some("Ship Goal foundation".to_string()),
                    objective: None,
                    lifecycle: None,
                    terminal_reason: None,
                },
                "update-title",
                T0 + 3,
            )
            .unwrap();
        assert!(updated.state_changed);
        assert_eq!(updated.goal.summary.revision, 2);
        assert_eq!(updated.goal.summary.lifecycle, GoalLifecycle::Active);

        let changed_update_reuse = db
            .update_goal_at(
                &owner,
                &created.goal.summary.goal_id,
                1,
                GoalPatch {
                    title: Some("Different title under same key".to_string()),
                    objective: None,
                    lifecycle: None,
                    terminal_reason: None,
                },
                "update-title",
                T0 + 3,
            )
            .unwrap_err();
        assert_eq!(changed_update_reuse.code(), "goal_idempotency_conflict");

        let terminal = db
            .update_goal_at(
                &owner,
                &created.goal.summary.goal_id,
                2,
                GoalPatch {
                    title: None,
                    objective: None,
                    lifecycle: Some(GoalLifecycle::Completed),
                    terminal_reason: Some("Phase 1 accepted".to_string()),
                },
                "complete-goal",
                T0 + 4,
            )
            .unwrap();
        assert_eq!(terminal.goal.summary.lifecycle, GoalLifecycle::Completed);
        assert_eq!(terminal.goal.summary.revision, 3);
        assert_eq!(terminal.goal.summary.terminal_at_unix_ms, Some(T0 + 4));
        assert_eq!(
            terminal.goal.terminal_reason.as_deref(),
            Some("Phase 1 accepted")
        );

        let terminal_replay = db
            .update_goal_at(
                &owner,
                &created.goal.summary.goal_id,
                2,
                GoalPatch {
                    title: None,
                    objective: None,
                    lifecycle: Some(GoalLifecycle::Completed),
                    terminal_reason: Some("Phase 1 accepted".to_string()),
                },
                "complete-goal",
                T0 + 5,
            )
            .unwrap();
        assert!(terminal_replay.replayed);
        assert!(!terminal_replay.state_changed);
        assert_eq!(terminal_replay.goal.summary.revision, 3);

        let page = db
            .list_goals(&owner, Some(GoalLifecycle::Completed), 0, 10)
            .unwrap();
        assert_eq!(page.total_count, 1);
        assert_eq!(page.goals[0].goal_id, created.goal.summary.goal_id);
        assert_eq!(page.goals[0].agent_task_count, 0);
        assert_eq!(page.goals[0].workflow_session_count, 0);
        created.goal.summary.goal_id
    };

    let reopened = Database::open(&path).unwrap();
    let goal = reopened.read_goal(&owner, &goal_id).unwrap();
    assert_eq!(goal.summary.lifecycle, GoalLifecycle::Completed);
    assert_eq!(goal.summary.revision, 3);
    assert_eq!(goal.terminal_reason.as_deref(), Some("Phase 1 accepted"));
}

#[test]
fn exact_read_hides_foreign_existence_and_updates_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("goal-auth.db")).unwrap();
    let owner = principal('b');
    let foreign = principal('c');
    let created = db
        .create_goal_at(&owner, input("owner-create"), T0)
        .unwrap();
    let goal_id = created.goal.summary.goal_id;

    let foreign_error = db.read_goal(&foreign, &goal_id).unwrap_err();
    assert_eq!(foreign_error.code(), "goal_not_found");
    let missing = format!("{GOAL_ID_PREFIX}{}", "f".repeat(32));
    let missing_error = db.read_goal(&foreign, &missing).unwrap_err();
    assert_eq!(missing_error.code(), "goal_not_found");

    let stale = db
        .update_goal_at(
            &owner,
            &goal_id,
            99,
            GoalPatch {
                title: Some("stale".to_string()),
                ..GoalPatch::default()
            },
            "stale-update",
            T0 + 1,
        )
        .unwrap_err();
    assert_eq!(stale.code(), "goal_revision_changed");
    assert_eq!(stale.current_revision(), Some(1));

    let terminal = db
        .update_goal_at(
            &owner,
            &goal_id,
            1,
            GoalPatch {
                lifecycle: Some(GoalLifecycle::Cancelled),
                ..GoalPatch::default()
            },
            "cancel-goal",
            T0 + 2,
        )
        .unwrap();
    assert_eq!(terminal.goal.summary.lifecycle, GoalLifecycle::Cancelled);
    let immutable = db
        .update_goal_at(
            &owner,
            &goal_id,
            2,
            GoalPatch {
                title: Some("cannot mutate".to_string()),
                ..GoalPatch::default()
            },
            "terminal-mutate",
            T0 + 3,
        )
        .unwrap_err();
    assert_eq!(immutable.code(), "goal_terminal");
}

#[test]
fn correlations_are_bounded_explicit_identity_only_and_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("goal-correlations.db")).unwrap();
    let owner = principal('d');
    let created = db.create_goal_at(&owner, input("corr-create"), T0).unwrap();
    let goal_id = created.goal.summary.goal_id;
    let task_id = format!("wc_agent_task_{}", "1".repeat(32));
    let session_id = format!("wc_sess_{}", "2".repeat(32));

    let task_link = db
        .associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::AgentTask,
            &task_id,
            "task-link",
            T0 + 1,
        )
        .unwrap();
    assert!(task_link.state_changed);
    assert_eq!(task_link.goal.summary.revision, 2);
    assert_eq!(task_link.goal.summary.agent_task_count, 1);

    let task_replay = db
        .associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::AgentTask,
            &task_id,
            "task-link",
            T0 + 2,
        )
        .unwrap();
    assert!(task_replay.replayed);
    assert!(!task_replay.state_changed);

    let session_link = db
        .associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::WorkflowSession,
            &session_id,
            "session-link",
            T0 + 3,
        )
        .unwrap();
    assert_eq!(session_link.goal.summary.revision, 3);
    assert_eq!(session_link.goal.summary.workflow_session_count, 1);
    assert_eq!(session_link.goal.correlations.len(), 2);

    let changed_reuse = db
        .associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::WorkflowSession,
            &format!("wc_sess_{}", "3".repeat(32)),
            "session-link",
            T0 + 4,
        )
        .unwrap_err();
    assert_eq!(changed_reuse.code(), "goal_idempotency_conflict");

    for ordinal in 0..(MAX_GOAL_CORRELATIONS - 2) {
        let reference_id = format!("wc_agent_task_{ordinal:032x}");
        let key = format!("capacity-link-{ordinal}");
        db.associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::AgentTask,
            &reference_id,
            &key,
            T0 + 10 + ordinal,
        )
        .unwrap();
    }
    let full = db.read_goal(&owner, &goal_id).unwrap();
    assert_eq!(full.correlations.len(), MAX_GOAL_CORRELATIONS as usize);
    let overflow = db
        .associate_goal_reference_at(
            &owner,
            &goal_id,
            GoalCorrelationKind::AgentTask,
            &format!("wc_agent_task_{}", "f".repeat(32)),
            "capacity-overflow",
            T0 + 100,
        )
        .unwrap_err();
    assert_eq!(overflow.code(), "goal_correlation_capacity_exceeded");
}

#[test]
fn bounds_and_unknown_persisted_lifecycle_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("goal-bounds.db")).unwrap();
    let owner = principal('e');

    let mut oversized = input("oversized-title");
    oversized.title = "x".repeat(MAX_GOAL_TITLE_CHARS + 1);
    assert_eq!(
        db.create_goal_at(&owner, oversized, T0).unwrap_err().code(),
        "invalid_goal_title"
    );
    let mut oversized = input("oversized-objective");
    oversized.objective = "x".repeat(MAX_GOAL_OBJECTIVE_BYTES + 1);
    assert_eq!(
        db.create_goal_at(&owner, oversized, T0).unwrap_err().code(),
        "invalid_goal_objective"
    );
    assert_eq!(
        db.list_goals(&owner, None, 0, MAX_GOAL_LIST_LIMIT + 1)
            .unwrap_err()
            .code(),
        "invalid_goal_list_limit"
    );
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        db.list_goals(&owner, None, (i64::MAX as usize) + 1, 1)
            .unwrap_err()
            .code(),
        "invalid_goal_list_offset"
    );

    let oversized_persisted = db
        .create_goal_at(&owner, input("corrupt-objective-create"), T0)
        .unwrap();
    {
        let conn = db.conn_for_tests();
        conn.execute(
            "UPDATE wc_goals SET objective = ?2 WHERE goal_id = ?1",
            rusqlite::params![
                oversized_persisted.goal.summary.goal_id,
                "x".repeat(MAX_GOAL_OBJECTIVE_BYTES + 1)
            ],
        )
        .unwrap();
    }
    let error = db
        .read_goal(&owner, &oversized_persisted.goal.summary.goal_id)
        .unwrap_err();
    assert_eq!(error.code(), "goal_store_unavailable");

    let overlinked = db
        .create_goal_at(&owner, input("corrupt-correlations-create"), T0)
        .unwrap();
    {
        let conn = db.conn_for_tests();
        for ordinal in 0..=MAX_GOAL_CORRELATIONS {
            conn.execute(
                "INSERT INTO wc_goal_correlations (
                    goal_id, kind, reference_id, created_at_unix_ms
                 ) VALUES (?1, 'agent_task', ?2, ?3)",
                rusqlite::params![
                    overlinked.goal.summary.goal_id,
                    format!("wc_agent_task_{ordinal:032x}"),
                    T0 + ordinal,
                ],
            )
            .unwrap();
        }
    }
    let error = db
        .read_goal(&owner, &overlinked.goal.summary.goal_id)
        .unwrap_err();
    assert_eq!(error.code(), "goal_store_unavailable");

    let created = db
        .create_goal_at(&owner, input("corrupt-create"), T0)
        .unwrap();
    let goal_id = created.goal.summary.goal_id;
    {
        let conn = db.conn_for_tests();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "UPDATE wc_goals SET lifecycle = 'waiting_validation' WHERE goal_id = ?1",
            [goal_id.as_str()],
        )
        .unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .unwrap();
    }
    let error = db.read_goal(&owner, &goal_id).unwrap_err();
    assert_eq!(error.code(), "goal_store_unavailable");
}
