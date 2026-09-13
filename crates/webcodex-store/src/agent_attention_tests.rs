use super::agent_attention::{
    attention_events_for_attempt, AGENT_ATTENTION_EVENT_KIND_AGENT_TASK_TERMINAL,
};
use super::agent_task::{AgentTaskState, NewAgentTask};
use super::agent_wake::{AgentWakeState, AGENT_WAKE_ID_PREFIX, WAKE_TRIGGER_ATTENTION_EVENT};
use super::communication::{
    CommunicationPrincipal, NewAgentEndpoint, NewAgentIdentity,
    COMMUNICATION_PRINCIPAL_DIGEST_PREFIX,
};
use super::goal::{GoalCorrelationKind, GoalLifecycle, GoalPatch, NewGoal, MAX_GOAL_CORRELATIONS};
use super::Database;
use rusqlite::params;

const T0: i64 = 20_000_000;

fn principal(hex: char) -> CommunicationPrincipal {
    CommunicationPrincipal {
        kind: "user".to_string(),
        digest: format!(
            "{COMMUNICATION_PRINCIPAL_DIGEST_PREFIX}{}",
            hex.to_string().repeat(64)
        ),
    }
}

fn create_agent(db: &Database, owner: &CommunicationPrincipal, key: &str) -> String {
    db.create_agent_identity(
        owner,
        NewAgentIdentity {
            handle: key.to_string(),
            display_name: key.to_string(),
            description: String::new(),
            specialty_labels: Vec::new(),
            idempotency_key: format!("agent-{key}"),
        },
    )
    .unwrap()
    .agent
    .agent_id
}

fn create_task_and_attempt(
    db: &Database,
    owner: &CommunicationPrincipal,
    assignee: &str,
    key: &str,
    now: i64,
) -> (String, String, String) {
    let task_id = db
        .create_agent_task_at(
            owner,
            NewAgentTask {
                title: format!("Task {key}"),
                instruction: "Perform one bounded durable work unit.".to_string(),
                assignee_agent_id: Some(assignee.to_string()),
                source_conversation_id: None,
                source_message_id: None,
                referenced_project_id: None,
                idempotency_key: format!("task-{key}"),
            },
            now,
        )
        .unwrap()
        .task
        .summary
        .task_id;
    let started = db
        .start_agent_task_attempt_at(
            owner,
            &task_id,
            assignee,
            &format!("attempt-{key}"),
            now + 1,
        )
        .unwrap();
    (task_id, started.attempt.attempt_id, started.attempt_fence)
}

fn create_goal_and_link(
    db: &Database,
    owner: &CommunicationPrincipal,
    task_id: &str,
    key: &str,
    now: i64,
) -> String {
    let goal_id = db
        .create_goal_at(
            owner,
            NewGoal {
                title: format!("Goal {key}"),
                objective: "Keep high-level intent separate from task execution truth.".to_string(),
                idempotency_key: format!("goal-{key}"),
            },
            now,
        )
        .unwrap()
        .goal
        .summary
        .goal_id;
    let linked = db
        .associate_goal_reference_at(
            owner,
            &goal_id,
            GoalCorrelationKind::AgentTask,
            task_id,
            &format!("link-{key}"),
            now + 1,
        )
        .unwrap();
    assert_eq!(linked.goal.summary.revision, 2);
    goal_id
}

fn events(
    db: &Database,
    attempt_id: &str,
) -> Vec<super::agent_attention::AgentAttentionEventRecord> {
    let conn = db.conn_for_tests();
    attention_events_for_attempt(&conn, attempt_id).unwrap()
}

fn attention_wake_count(db: &Database, attempt_id: &str) -> i64 {
    db.conn_for_tests()
        .query_row(
            "SELECT COUNT(*)
             FROM wc_agent_wakes w
             JOIN wc_agent_attention_events e ON e.event_id = w.source_event_id
             WHERE w.trigger_kind = 'attention_event' AND e.task_attempt_id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn wake_id_for_event(db: &Database, event_id: &str) -> String {
    db.conn_for_tests()
        .query_row(
            "SELECT wake_id FROM wc_agent_wakes
             WHERE trigger_kind = 'attention_event' AND source_event_id = ?1",
            [event_id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn uncorrelated_terminal_task_creates_no_attention_fact_or_wake() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("uncorrelated.db")).unwrap();
    let owner = principal('1');
    let assignee = create_agent(&db, &owner, "uncorrelated-worker");
    let (task_id, attempt_id, fence) =
        create_task_and_attempt(&db, &owner, &assignee, "uncorrelated", T0);

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("bounded result"),
            None,
            "complete-uncorrelated",
            T0 + 2,
        )
        .unwrap();
    assert_eq!(completed.task.state, AgentTaskState::Succeeded);
    assert_eq!(completed.attention_event_count, 0);
    assert!(events(&db, &attempt_id).is_empty());
    assert_eq!(attention_wake_count(&db, &attempt_id), 0);
}

#[test]
fn correlated_success_is_atomic_replay_safe_and_never_mutates_goal() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("success.db")).unwrap();
    let owner = principal('2');
    let foreign = principal('3');
    let assignee = create_agent(&db, &owner, "success-worker");
    let (task_id, attempt_id, fence) =
        create_task_and_attempt(&db, &owner, &assignee, "success", T0);
    let goal_id = create_goal_and_link(&db, &owner, &task_id, "success", T0 + 2);

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("PRIVATE_RESULT_MUST_NOT_BE_COPIED"),
            None,
            "complete-success",
            T0 + 4,
        )
        .unwrap();
    assert_eq!(completed.attention_event_count, 1);
    let first_events = events(&db, &attempt_id);
    assert_eq!(first_events.len(), 1);
    let event = &first_events[0];
    assert_eq!(event.kind, AGENT_ATTENTION_EVENT_KIND_AGENT_TASK_TERMINAL);
    assert_eq!(event.goal_id, goal_id);
    assert_eq!(event.task_id, task_id);
    assert_eq!(event.task_attempt_id, attempt_id);
    assert_eq!(event.target_agent_id, assignee);
    assert_eq!(event.terminal_task_state, AgentTaskState::Succeeded);
    let wake_id = wake_id_for_event(&db, &event.event_id);
    let wake = db.agent_wake(&wake_id).unwrap().unwrap();
    assert_eq!(wake.trigger_kind, WAKE_TRIGGER_ATTENTION_EVENT);
    assert_eq!(
        wake.source_event_id.as_deref(),
        Some(event.event_id.as_str())
    );
    assert_eq!(wake.state, AgentWakeState::Pending);
    assert_eq!(attention_wake_count(&db, &attempt_id), 1);

    let goal = db.read_goal(&owner, &goal_id).unwrap();
    assert_eq!(goal.summary.lifecycle, GoalLifecycle::Active);
    assert_eq!(goal.summary.revision, 2);
    assert_eq!(
        db.read_goal(&foreign, &goal_id).unwrap_err().code(),
        "goal_not_found"
    );
    assert_eq!(
        db.read_agent_task(&foreign, &task_id).unwrap_err().code(),
        "agent_task_not_found"
    );

    let replay = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("PRIVATE_RESULT_MUST_NOT_BE_COPIED"),
            None,
            "complete-success",
            T0 + 5,
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(!replay.state_changed);
    assert_eq!(replay.attention_event_count, 0);
    assert_eq!(events(&db, &attempt_id), first_events);
    assert_eq!(attention_wake_count(&db, &attempt_id), 1);

    let conflict = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("changed result"),
            None,
            "complete-success",
            T0 + 6,
        )
        .unwrap_err();
    assert_eq!(conflict.code(), "communication_idempotency_conflict");
    assert_eq!(events(&db, &attempt_id), first_events);
    assert_eq!(attention_wake_count(&db, &attempt_id), 1);

    let columns = {
        let conn = db.conn_for_tests();
        let mut statement = conn
            .prepare("SELECT name FROM pragma_table_info('wc_agent_attention_events') ORDER BY cid")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    for forbidden in [
        "instruction",
        "objective",
        "terminal_result",
        "attempt_fence",
        "endpoint_id",
        "controller_generation",
        "project_id",
        "session_id",
        "payload",
    ] {
        assert!(
            !columns.iter().any(|column| column == forbidden),
            "{forbidden}"
        );
    }
}

#[test]
fn failed_task_fans_out_to_every_active_goal_and_survives_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("multi-goal.db");
    let owner = principal('4');
    let (attempt_id, active_goal_ids) = {
        let db = Database::open(&path).unwrap();
        let assignee = create_agent(&db, &owner, "multi-worker");
        let (task_id, attempt_id, fence) =
            create_task_and_attempt(&db, &owner, &assignee, "multi", T0);
        let goal_a = create_goal_and_link(&db, &owner, &task_id, "multi-a", T0 + 2);
        let goal_b = create_goal_and_link(&db, &owner, &task_id, "multi-b", T0 + 4);
        let terminal_goal = create_goal_and_link(&db, &owner, &task_id, "multi-terminal", T0 + 6);
        let terminal = db
            .update_goal_at(
                &owner,
                &terminal_goal,
                2,
                GoalPatch {
                    title: None,
                    objective: None,
                    lifecycle: Some(GoalLifecycle::Completed),
                    terminal_reason: Some(
                        "Completed independently before Task terminalized".to_string(),
                    ),
                },
                "complete-independent-goal",
                T0 + 8,
            )
            .unwrap();
        assert_eq!(terminal.goal.summary.revision, 3);

        let completed = db
            .complete_agent_task_attempt_at(
                &owner,
                &task_id,
                &attempt_id,
                &assignee,
                &fence,
                1,
                AgentTaskState::Failed,
                None,
                Some("bounded failure"),
                "complete-multi",
                T0 + 9,
            )
            .unwrap();
        assert_eq!(completed.attention_event_count, 2);
        let emitted = events(&db, &attempt_id);
        assert_eq!(emitted.len(), 2);
        assert!(emitted
            .iter()
            .all(|event| event.terminal_task_state == AgentTaskState::Failed));
        let emitted_goals = emitted
            .iter()
            .map(|event| event.goal_id.clone())
            .collect::<Vec<_>>();
        let mut expected = vec![goal_a.clone(), goal_b.clone()];
        expected.sort();
        assert_eq!(emitted_goals, expected);
        assert_eq!(attention_wake_count(&db, &attempt_id), 2);
        assert_eq!(db.read_goal(&owner, &goal_a).unwrap().summary.revision, 2);
        assert_eq!(db.read_goal(&owner, &goal_b).unwrap().summary.revision, 2);
        assert_eq!(
            db.read_goal(&owner, &terminal_goal)
                .unwrap()
                .summary
                .revision,
            3
        );
        (attempt_id, vec![goal_a, goal_b])
    };

    let reopened = Database::open(&path).unwrap();
    let emitted = events(&reopened, &attempt_id);
    assert_eq!(emitted.len(), 2);
    assert_eq!(attention_wake_count(&reopened, &attempt_id), 2);
    for goal_id in active_goal_ids {
        let goal = reopened.read_goal(&owner, &goal_id).unwrap();
        assert_eq!(goal.summary.lifecycle, GoalLifecycle::Active);
        assert_eq!(goal.summary.revision, 2);
    }
}

#[test]
fn attention_write_failure_rolls_back_task_attempt_event_wake_and_replay_record() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("atomic.db")).unwrap();
    let owner = principal('5');
    let assignee = create_agent(&db, &owner, "atomic-worker");
    let (task_id, attempt_id, fence) =
        create_task_and_attempt(&db, &owner, &assignee, "atomic", T0);
    create_goal_and_link(&db, &owner, &task_id, "atomic", T0 + 2);
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER fail_attention_wake
             BEFORE INSERT ON wc_agent_wakes
             WHEN NEW.trigger_kind = 'attention_event'
             BEGIN
                 SELECT RAISE(ABORT, 'forced attention Wake failure');
             END;",
        )
        .unwrap();

    assert!(db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("result"),
            None,
            "atomic-completion",
            T0 + 4,
        )
        .is_err());
    let states: (String, String) = db
        .conn_for_tests()
        .query_row(
            "SELECT t.state, a.state
             FROM wc_agent_tasks t JOIN wc_agent_task_attempts a ON a.task_id = t.task_id
             WHERE t.task_id = ?1 AND a.attempt_id = ?2",
            params![task_id, attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(states, ("active".to_string(), "active".to_string()));
    assert!(events(&db, &attempt_id).is_empty());
    assert_eq!(attention_wake_count(&db, &attempt_id), 0);

    db.conn_for_tests()
        .execute_batch("DROP TRIGGER fail_attention_wake;")
        .unwrap();
    let retry = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("result"),
            None,
            "atomic-completion",
            T0 + 5,
        )
        .unwrap();
    assert!(!retry.replayed);
    assert_eq!(retry.attention_event_count, 1);
}

#[test]
fn stale_controller_or_expired_attempt_cannot_generate_attention() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("stale.db")).unwrap();
    let owner = principal('6');
    let assignee = create_agent(&db, &owner, "stale-worker");
    let (task_id, attempt_id, fence) = create_task_and_attempt(&db, &owner, &assignee, "stale", T0);
    create_goal_and_link(&db, &owner, &task_id, "stale", T0 + 2);

    let stale_generation = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            2,
            AgentTaskState::Succeeded,
            None,
            None,
            "stale-generation",
            T0 + 4,
        )
        .unwrap_err();
    assert_eq!(stale_generation.code(), "agent_task_attempt_stale");
    assert!(events(&db, &attempt_id).is_empty());
    assert_eq!(attention_wake_count(&db, &attempt_id), 0);

    let expired = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            None,
            None,
            "expired-attempt",
            T0 + 10_000_000,
        )
        .unwrap_err();
    assert_eq!(expired.code(), "agent_task_attempt_stale");
    assert!(events(&db, &attempt_id).is_empty());
    assert_eq!(attention_wake_count(&db, &attempt_id), 0);
}

#[test]
fn terminal_attention_uses_continuation_without_requiring_live_task_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("carrier.db")).unwrap();
    let owner = principal('7');
    let assignee = create_agent(&db, &owner, "carrier-worker");
    let (task_id, attempt_id, fence) =
        create_task_and_attempt(&db, &owner, &assignee, "carrier", T0);
    let goal_id = create_goal_and_link(&db, &owner, &task_id, "carrier", T0 + 2);
    db.complete_agent_task_attempt_at(
        &owner,
        &task_id,
        &attempt_id,
        &assignee,
        &fence,
        1,
        AgentTaskState::Succeeded,
        Some("PRIVATE_RESULT_MUST_NOT_BE_IN_ENVELOPE"),
        None,
        "carrier-completion",
        T0 + 4,
    )
    .unwrap();
    let event = events(&db, &attempt_id).pop().unwrap();
    let wake_id = wake_id_for_event(&db, &event.event_id);

    let terminal_goal = db
        .update_goal_at(
            &owner,
            &goal_id,
            2,
            GoalPatch {
                title: None,
                objective: None,
                lifecycle: Some(GoalLifecycle::Completed),
                terminal_reason: Some("Another turn completed the Goal first".to_string()),
            },
            "carrier-goal-complete",
            T0 + 5,
        )
        .unwrap();
    assert_eq!(
        terminal_goal.goal.summary.lifecycle,
        GoalLifecycle::Completed
    );

    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "ChatGPT".to_string(),
                client_attachment_id: Some("attention-carrier".to_string()),
                wake_capable: true,
                idempotency_key: "attention-carrier-endpoint".to_string(),
            },
        )
        .unwrap()
        .endpoint;
    let claim = db
        .claim_next_agent_wake(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "mcp_app",
        )
        .unwrap()
        .unwrap();
    assert_eq!(claim.wake.wake_id, wake_id);
    assert_eq!(claim.wake.trigger_kind, WAKE_TRIGGER_ATTENTION_EVENT);

    let prepared = db
        .prepare_agent_wake_dispatch(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            &wake_id,
            &claim.attempt.attempt_id,
            &claim.claim_fence,
            &claim.consume_token,
        )
        .unwrap();
    assert_eq!(prepared.envelope.queued_delivery_count, 0);
    assert_eq!(prepared.envelope.inbox_high_watermark, 0);
    assert!(prepared.envelope.resume_hint.contains(&event.event_id));
    assert!(prepared.envelope.resume_hint.contains(&goal_id));
    assert!(prepared.envelope.resume_hint.contains(&task_id));
    assert!(prepared.envelope.resume_hint.contains(&attempt_id));
    assert!(prepared
        .envelope
        .resume_hint
        .contains(&format!("agent_id={assignee}")));
    assert!(prepared
        .envelope
        .resume_hint
        .contains(&format!("endpoint_id={}", endpoint.endpoint_id)));
    assert!(prepared.envelope.resume_hint.contains(&format!(
        "controller_generation={}",
        endpoint.controller_generation
    )));
    assert!(prepared
        .envelope
        .resume_hint
        .contains(&format!("wake_id={wake_id}")));
    assert!(prepared
        .envelope
        .resume_hint
        .contains(&format!("consume_token={}", claim.consume_token)));
    assert!(prepared
        .envelope
        .resume_hint
        .contains("terminal_task_state=succeeded"));
    for required_semantic in [
        "Bootstrap this agent/endpoint generation/wake first",
        "require the returned Wake to remain attention_event",
        "replay/retry keeps these identities and consume_token unchanged",
        "consume this exact Wake",
        "independently get_goal(goal_id) and read_agent_task(task_id)",
        "grants no Goal, Task, Project, Runner, filesystem, Conversation, or Workflow Session authority",
        "never repeat an already-terminal Task",
        "never reopen a completed/cancelled Goal",
        "make an explicit decision through ordinary authorized tools",
        "does not auto-complete Goals or auto-create successor Tasks",
        "report the actual decision/result/blocker",
    ] {
        assert!(prepared.envelope.resume_hint.contains(required_semantic));
    }
    assert!(prepared.envelope.resume_hint.len() < 2_000);
    assert!(!prepared
        .envelope
        .resume_hint
        .contains("PRIVATE_RESULT_MUST_NOT_BE_IN_ENVELOPE"));

    let bootstrap = db
        .bootstrap_agent_conversation(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            None,
            Some(&wake_id),
        )
        .unwrap();
    let projected = bootstrap.wake.unwrap();
    assert_eq!(projected.trigger_kind, WAKE_TRIGGER_ATTENTION_EVENT);
    assert_eq!(projected.event_id.as_deref(), Some(event.event_id.as_str()));
    assert_eq!(projected.goal_id.as_deref(), Some(goal_id.as_str()));
    assert_eq!(projected.task_id.as_deref(), Some(task_id.as_str()));
    assert_eq!(
        projected.task_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );

    let bogus_event_id = format!("wc_attention_event_{}", "f".repeat(32));
    db.conn_for_tests()
        .execute(
            "UPDATE wc_agent_wakes SET source_event_id = ?2 WHERE wake_id = ?1",
            params![wake_id, bogus_event_id],
        )
        .unwrap();
    assert_eq!(
        db.bootstrap_agent_conversation(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            None,
            Some(&wake_id),
        )
        .unwrap_err()
        .code(),
        "agent_attention_wake_invariant"
    );
    db.conn_for_tests()
        .execute(
            "UPDATE wc_agent_wakes SET source_event_id = ?2 WHERE wake_id = ?1",
            params![wake_id, event.event_id],
        )
        .unwrap();

    let consumed = db
        .consume_agent_wake(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            &wake_id,
            &claim.consume_token,
        )
        .unwrap();
    assert_eq!(consumed.state, AgentWakeState::Consumed);
    assert_eq!(events(&db, &attempt_id).len(), 1);
    assert_eq!(
        db.read_agent_task(&owner, &task_id).unwrap().summary.state,
        AgentTaskState::Succeeded
    );
    assert_eq!(
        db.read_goal(&owner, &goal_id).unwrap().summary.lifecycle,
        GoalLifecycle::Completed
    );
}

#[test]
fn active_goal_fanout_is_bounded_before_completion_and_maximum_fanout_is_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("fanout-bound.db")).unwrap();
    let owner = principal('9');
    let assignee = create_agent(&db, &owner, "fanout-worker");
    let (task_id, attempt_id, fence) =
        create_task_and_attempt(&db, &owner, &assignee, "fanout", T0);

    for index in 0..MAX_GOAL_CORRELATIONS {
        create_goal_and_link(
            &db,
            &owner,
            &task_id,
            &format!("fanout-{index:02}"),
            T0 + 10 + index * 2,
        );
    }
    let overflow_goal_id = db
        .create_goal_at(
            &owner,
            NewGoal {
                title: "Fanout overflow".to_string(),
                objective: "Must fail before creating an unbounded terminal attention fanout."
                    .to_string(),
                idempotency_key: "goal-fanout-overflow".to_string(),
            },
            T0 + 1_000,
        )
        .unwrap()
        .goal
        .summary
        .goal_id;
    let overflow = db
        .associate_goal_reference_at(
            &owner,
            &overflow_goal_id,
            GoalCorrelationKind::AgentTask,
            &task_id,
            "link-fanout-overflow",
            T0 + 1_001,
        )
        .unwrap_err();
    assert_eq!(overflow.code(), "goal_agent_task_fanout_capacity_exceeded");
    assert_eq!(
        db.read_goal(&owner, &overflow_goal_id)
            .unwrap()
            .summary
            .revision,
        1
    );

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &attempt_id,
            &assignee,
            &fence,
            1,
            AgentTaskState::Succeeded,
            Some("bounded fanout result"),
            None,
            "complete-fanout-bound",
            T0 + 1_002,
        )
        .unwrap();
    assert_eq!(
        completed.attention_event_count as i64,
        MAX_GOAL_CORRELATIONS
    );
    let emitted = events(&db, &attempt_id);
    assert_eq!(emitted.len() as i64, MAX_GOAL_CORRELATIONS);
    assert!(emitted
        .windows(2)
        .all(|pair| pair[0].goal_id < pair[1].goal_id));
    assert_eq!(
        attention_wake_count(&db, &attempt_id),
        MAX_GOAL_CORRELATIONS
    );
    let pending_attention_wakes: i64 = db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_agent_wakes w
             JOIN wc_agent_attention_events e ON e.event_id = w.source_event_id
             WHERE e.task_attempt_id = ?1
               AND w.trigger_kind = 'attention_event' AND w.state = 'pending'",
            [&attempt_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_attention_wakes, MAX_GOAL_CORRELATIONS);
}

#[test]
fn legacy_wake_schema_migration_preserves_existing_task_wake() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy-wake.db");
    let owner = principal('8');
    let legacy_wake_id = format!("{AGENT_WAKE_ID_PREFIX}{}", "a".repeat(32));
    let (assignee, task_id, attempt_id) = {
        let db = Database::open(&path).unwrap();
        let assignee = create_agent(&db, &owner, "legacy-worker");
        let (task_id, attempt_id, _fence) =
            create_task_and_attempt(&db, &owner, &assignee, "legacy", T0);
        let conn = db.conn_for_tests();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(
            "DROP TABLE wc_agent_wake_attempts;
             DROP TABLE wc_agent_wakes;
             CREATE TABLE wc_agent_wakes (
                wake_id TEXT PRIMARY KEY,
                target_agent_id TEXT NOT NULL,
                trigger_kind TEXT NOT NULL,
                first_triggering_delivery_id TEXT,
                latest_triggering_delivery_id TEXT,
                latest_conversation_id TEXT,
                latest_message_id TEXT,
                inbox_high_watermark INTEGER,
                queued_delivery_count_snapshot INTEGER,
                source_task_id TEXT,
                source_task_attempt_id TEXT,
                state TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                claimed_attempt_id TEXT,
                claimed_endpoint_id TEXT,
                claimed_controller_generation INTEGER,
                claim_lease_expires_at_unix_ms INTEGER,
                consumed_at_unix_ms INTEGER,
                consumed_by_endpoint_id TEXT,
                consumed_controller_generation INTEGER
             );
             CREATE TABLE wc_agent_wake_attempts (
                attempt_id TEXT PRIMARY KEY,
                wake_id TEXT NOT NULL,
                endpoint_id TEXT NOT NULL,
                controller_generation INTEGER NOT NULL,
                adapter_kind TEXT NOT NULL,
                state TEXT NOT NULL,
                claim_fence_hash TEXT NOT NULL,
                consume_token_hash TEXT NOT NULL,
                claimed_at_unix_ms INTEGER NOT NULL,
                claim_lease_expires_at_unix_ms INTEGER NOT NULL,
                prepared_at_unix_ms INTEGER,
                delivered_at_unix_ms INTEGER,
                delivery_unknown_at_unix_ms INTEGER,
                revoked_at_unix_ms INTEGER,
                consumed_at_unix_ms INTEGER
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wc_agent_wakes (
                wake_id, target_agent_id, trigger_kind,
                first_triggering_delivery_id, latest_triggering_delivery_id,
                latest_conversation_id, latest_message_id,
                inbox_high_watermark, queued_delivery_count_snapshot,
                source_task_id, source_task_attempt_id,
                state, revision, created_at_unix_ms, updated_at_unix_ms,
                claimed_attempt_id, claimed_endpoint_id,
                claimed_controller_generation, claim_lease_expires_at_unix_ms,
                consumed_at_unix_ms, consumed_by_endpoint_id,
                consumed_controller_generation
             ) VALUES (?1, ?2, 'agent_task_attempt', NULL, NULL, NULL, NULL, NULL, NULL,
                       ?3, ?4, 'pending', 1, ?5, ?5, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
            params![legacy_wake_id, assignee, task_id, attempt_id, T0 + 2],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        drop(conn);
        (assignee, task_id, attempt_id)
    };

    let reopened = Database::open(&path).unwrap();
    let wake = reopened.agent_wake(&legacy_wake_id).unwrap().unwrap();
    assert_eq!(wake.target_agent_id, assignee);
    assert_eq!(wake.trigger_kind, "agent_task_attempt");
    assert_eq!(wake.source_task_id.as_deref(), Some(task_id.as_str()));
    assert_eq!(
        wake.source_task_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert_eq!(wake.source_event_id, None);
    assert_eq!(wake.state, AgentWakeState::Pending);
    let has_source_event: bool = reopened
        .conn_for_tests()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('wc_agent_wakes') WHERE name = 'source_event_id')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_source_event);
}
