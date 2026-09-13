use super::agent_task::*;
use super::agent_wake::AgentWakeState;
use super::communication::{
    CommunicationPrincipal, NewAgentEndpoint, NewAgentIdentity, NewConversation,
    NewConversationMessage, COMMUNICATION_PRINCIPAL_DIGEST_PREFIX,
};
use super::Database;
use rusqlite::params;
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use webcodex_core::coding_agent::{CodingAgentExecutionState, CodingAgentRunState};

const T0: i64 = 1_000_000;

fn wall_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock after Unix epoch")
        .as_millis()
        .try_into()
        .expect("wall-clock milliseconds fit i64")
}

fn principal(hex: char) -> CommunicationPrincipal {
    CommunicationPrincipal {
        kind: "user".to_string(),
        digest: format!(
            "{COMMUNICATION_PRINCIPAL_DIGEST_PREFIX}{}",
            hex.to_string().repeat(64)
        ),
    }
}

fn agent(db: &Database, owner: &CommunicationPrincipal, label: &str) -> String {
    db.create_agent_identity(
        owner,
        NewAgentIdentity {
            handle: label.to_string(),
            display_name: label.to_string(),
            description: String::new(),
            specialty_labels: Vec::new(),
            idempotency_key: format!("create-{label}"),
        },
    )
    .unwrap()
    .agent
    .agent_id
}

fn task_input(assignee_agent_id: Option<String>, key: &str) -> NewAgentTask {
    NewAgentTask {
        title: "Durable work".to_string(),
        instruction: "Perform bounded durable work without assuming a window or Endpoint."
            .to_string(),
        assignee_agent_id,
        source_conversation_id: None,
        source_message_id: None,
        referenced_project_id: Some("agent:special:reference-only".to_string()),
        idempotency_key: key.to_string(),
    }
}

fn create_assigned_task(
    db: &Database,
    owner: &CommunicationPrincipal,
    assignee: &str,
    key: &str,
) -> String {
    db.create_agent_task_at(owner, task_input(Some(assignee.to_string()), key), T0)
        .unwrap()
        .task
        .summary
        .task_id
}

fn start(
    db: &Database,
    owner: &CommunicationPrincipal,
    task_id: &str,
    assignee: &str,
    key: &str,
    now: i64,
) -> AgentTaskAttemptStartMutation {
    db.start_agent_task_attempt_at(owner, task_id, assignee, key, now)
        .unwrap()
}

fn coding_binding_intent(label: &str) -> AgentTaskCodingRunBindingIntent {
    AgentTaskCodingRunBindingIntent {
        run_id: format!("wc_agent_run_{label}"),
        runtime_project_id: "agent:special:reference-only".to_string(),
        provider_id: "codex".to_string(),
        provider_instance_id: format!("provider-instance-{label}"),
        authority_fingerprint: format!("auth_{label}"),
        coding_agent_intent_fingerprint: format!("coding-intent-{label}"),
        binding_intent_fingerprint: format!("binding-intent-{label}"),
    }
}

fn coding_observation(
    intent: &AgentTaskCodingRunBindingIntent,
    run_state: &str,
    execution_state: &str,
    revision: i64,
) -> AgentTaskCodingRunObservation {
    AgentTaskCodingRunObservation {
        run_id: intent.run_id.clone(),
        runtime_project_id: intent.runtime_project_id.clone(),
        provider_id: intent.provider_id.clone(),
        provider_instance_id: intent.provider_instance_id.clone(),
        authority_fingerprint: intent.authority_fingerprint.clone(),
        coding_agent_intent_fingerprint: intent.coding_agent_intent_fingerprint.clone(),
        run_state: CodingAgentRunState::from_str(run_state).expect("valid test run state"),
        execution_state: CodingAgentExecutionState::from_str(execution_state)
            .expect("valid test execution state"),
        observation_revision: revision,
        terminal_stop_reason: match (run_state, execution_state) {
            ("completed", _) => Some("end_turn".to_string()),
            ("cancelled", "completed") => Some("cancelled".to_string()),
            _ => None,
        },
        terminal_error_code: match run_state {
            "failed" => Some("provider_failed".to_string()),
            "lost" => Some("coding_agent_transport_lost".to_string()),
            _ => None,
        },
        terminal_message: matches!(run_state, "completed" | "failed" | "cancelled" | "lost")
            .then(|| format!("terminal-{run_state}")),
        completed_at_unix: matches!(run_state, "completed" | "failed" | "cancelled" | "lost")
            .then_some(1234),
    }
}

fn prepare_coding_binding(
    db: &Database,
    owner: &CommunicationPrincipal,
    task_id: &str,
    assignee: &str,
    started: &AgentTaskAttemptStartMutation,
    intent: &AgentTaskCodingRunBindingIntent,
    now: i64,
) -> AgentTaskCodingRunPrepared {
    db.prepare_agent_task_coding_run_at(
        owner,
        "agent:special:reference-only",
        task_id,
        &started.attempt.attempt_id,
        assignee,
        &started.attempt_fence,
        started.attempt.attempt_controller_generation,
        intent,
        now,
    )
    .unwrap()
}

#[test]
fn coding_run_binding_is_unique_replayable_and_fenced_before_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-binding.db")).unwrap();
    let owner = principal('1');
    let assignee = agent(&db, &owner, "coding-binding-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-binding-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "coding-binding-start",
        now,
    );

    let context = db
        .agent_task_coding_run_start_context_at(
            &owner,
            "agent:special:reference-only",
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap();
    assert_eq!(
        context.task.instruction,
        task_input(None, "ignored").instruction
    );
    assert_eq!(context.attempt.attempt_id, started.attempt.attempt_id);
    let project_error = db
        .agent_task_coding_run_start_context_at(
            &owner,
            "agent:special:wrong-project",
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap_err();
    assert_eq!(project_error.code(), "agent_task_project_mismatch");

    let intent = coding_binding_intent("binding-one");
    let prepared =
        prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 2);
    assert_eq!(
        prepared.binding.dispatch_state,
        AgentTaskCodingRunDispatchState::Prepared
    );
    assert!(!prepared.replayed);
    assert_eq!(
        db.read_agent_task(&owner, &task_id)
            .unwrap()
            .summary
            .execution_kind,
        Some(AgentTaskExecutionKind::CodingAgentRun)
    );

    let replay =
        prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 3);
    assert!(replay.replayed);
    assert_eq!(replay.binding.run_id, intent.run_id);

    let mut changed = intent.clone();
    changed.provider_id = "other-provider".to_string();
    changed.binding_intent_fingerprint = "binding-intent-changed".to_string();
    let conflict = db
        .prepare_agent_task_coding_run_at(
            &owner,
            "agent:special:reference-only",
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &changed,
            now + 4,
        )
        .unwrap_err();
    assert_eq!(conflict.code(), "agent_task_coding_run_binding_conflict");

    let first_claim = db
        .claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
    assert!(first_claim.may_dispatch);
    assert_eq!(
        first_claim.binding.dispatch_state,
        AgentTaskCodingRunDispatchState::OutcomeUnknown
    );
    let retry_claim = db
        .claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
    assert!(!retry_claim.may_dispatch);
    assert_eq!(retry_claim.binding.run_id, intent.run_id);
    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_coding_runs",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        1
    );
}

#[test]
fn definitive_not_started_releases_backend_fence_after_lease_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-not-started.db")).unwrap();
    let owner = principal('2');
    let agent_a = agent(&db, &owner, "coding-not-started-a");
    let agent_b = agent(&db, &owner, "coding-not-started-b");
    let now = wall_now_ms();

    let task_id = create_assigned_task(&db, &owner, &agent_a, "coding-not-started-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &agent_a,
        "coding-not-started-start",
        now,
    );
    let intent = coding_binding_intent("not-started-one");
    prepare_coding_binding(&db, &owner, &task_id, &agent_a, &started, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &agent_a,
        &started.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();
    let not_started = db
        .record_agent_task_coding_run_not_started(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &intent.run_id,
        )
        .unwrap();
    assert_eq!(
        not_started.dispatch_state,
        AgentTaskCodingRunDispatchState::NotStarted
    );
    let second = db
        .start_agent_task_attempt_at(
            &owner,
            &task_id,
            &agent_a,
            "coding-not-started-second",
            started.attempt.lease_expires_at_unix_ms,
        )
        .unwrap();
    assert_eq!(second.attempt.attempt_number, 2);

    let reassign_task =
        create_assigned_task(&db, &owner, &agent_a, "coding-not-started-reassign-task");
    let reassign_started = start(
        &db,
        &owner,
        &reassign_task,
        &agent_a,
        "coding-not-started-reassign-start",
        now + 10,
    );
    let reassign_intent = coding_binding_intent("not-started-reassign");
    prepare_coding_binding(
        &db,
        &owner,
        &reassign_task,
        &agent_a,
        &reassign_started,
        &reassign_intent,
        now + 11,
    );
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &reassign_task,
        &reassign_started.attempt.attempt_id,
        &agent_a,
        &reassign_started.attempt_fence,
        1,
        &reassign_intent.binding_intent_fingerprint,
    )
    .unwrap();
    db.record_agent_task_coding_run_not_started(
        &owner,
        &reassign_task,
        &reassign_started.attempt.attempt_id,
        &reassign_intent.run_id,
    )
    .unwrap();
    let reassigned = db
        .assign_agent_task_at(
            &owner,
            &reassign_task,
            &agent_b,
            reassign_started.attempt.lease_expires_at_unix_ms,
        )
        .unwrap();
    assert_eq!(
        reassigned.task.summary.assignee_agent_id.as_deref(),
        Some(agent_b.as_str())
    );
}

#[test]
fn unresolved_coding_execution_blocks_replacement_and_reassignment_after_lease_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-blocking.db")).unwrap();
    let owner = principal('3');
    let agent_a = agent(&db, &owner, "coding-block-a");
    let agent_b = agent(&db, &owner, "coding-block-b");
    let now = wall_now_ms();

    for (index, run_state, execution_state, expected_error) in [
        (0_i64, None, None, "agent_task_execution_outcome_unknown"),
        (
            1,
            Some("running"),
            Some("started"),
            "agent_task_execution_active",
        ),
        (
            2,
            Some("waiting_permission"),
            Some("started"),
            "agent_task_execution_active",
        ),
        (
            3,
            Some("lost"),
            Some("outcome_unknown"),
            "agent_task_execution_outcome_unknown",
        ),
    ] {
        let task_id =
            create_assigned_task(&db, &owner, &agent_a, &format!("coding-block-task-{index}"));
        let started = start(
            &db,
            &owner,
            &task_id,
            &agent_a,
            &format!("coding-block-start-{index}"),
            now + index * 20,
        );
        let intent = coding_binding_intent(&format!("blocking-{index}"));
        prepare_coding_binding(
            &db,
            &owner,
            &task_id,
            &agent_a,
            &started,
            &intent,
            now + index * 20 + 1,
        );
        db.claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &agent_a,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
        if let (Some(run_state), Some(execution_state)) = (run_state, execution_state) {
            db.record_agent_task_coding_run_observation(
                &owner,
                &task_id,
                &started.attempt.attempt_id,
                &coding_observation(&intent, run_state, execution_state, 1),
            )
            .unwrap();
        }
        let replacement = db
            .start_agent_task_attempt_at(
                &owner,
                &task_id,
                &agent_a,
                &format!("coding-block-replacement-{index}"),
                started.attempt.lease_expires_at_unix_ms,
            )
            .unwrap_err();
        assert_eq!(replacement.code(), expected_error, "case {index}");
        let reassignment = db
            .assign_agent_task_at(
                &owner,
                &task_id,
                &agent_b,
                started.attempt.lease_expires_at_unix_ms + 1,
            )
            .unwrap_err();
        assert_eq!(reassignment.code(), expected_error, "case {index}");
        let task = db
            .read_agent_task_at(
                &owner,
                &task_id,
                started.attempt.lease_expires_at_unix_ms + 2,
            )
            .unwrap();
        assert_eq!(task.summary.state, AgentTaskState::Active);
        assert!(task.summary.execution_bound);
    }
}

#[test]
fn backend_terminal_truth_reconciles_exact_attempt_after_ordinary_lease_expiry() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-terminal.db")).unwrap();
    let owner = principal('4');
    let assignee = agent(&db, &owner, "coding-terminal-agent");
    let now = wall_now_ms();
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-terminal-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "coding-terminal-start",
        now,
    );
    let intent = coding_binding_intent("terminal-completed");
    prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();
    let completed = coding_observation(&intent, "completed", "completed", 7);
    db.record_agent_task_coding_run_observation(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &completed,
    )
    .unwrap();

    let stale_generic = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("stale browser completion"),
            None,
            "stale-generic-completion",
            started.attempt.lease_expires_at_unix_ms,
        )
        .unwrap_err();
    assert_eq!(stale_generic.code(), "agent_task_attempt_stale");

    let reconciled = db
        .terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed,
            Some("bounded coding result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
    assert!(reconciled.state_changed);
    assert_eq!(reconciled.task.state, AgentTaskState::Succeeded);
    assert_eq!(reconciled.attempt.state, AgentTaskAttemptState::Succeeded);
    assert_eq!(
        reconciled.binding.dispatch_state,
        AgentTaskCodingRunDispatchState::Terminal
    );

    let replay_observation = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed,
        )
        .unwrap();
    assert_eq!(
        replay_observation.dispatch_state,
        AgentTaskCodingRunDispatchState::Terminal
    );
    let replay = db
        .terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed,
            Some("bounded coding result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
    assert!(!replay.state_changed);
}

#[test]
fn coding_run_observation_merge_preserves_newer_terminal_truth() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-monotonic.db")).unwrap();
    let owner = principal('a');
    let assignee = agent(&db, &owner, "coding-monotonic-agent");
    let now = wall_now_ms();
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-monotonic-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "coding-monotonic-start",
        now,
    );
    let intent = coding_binding_intent("monotonic-run");
    prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();

    let running_rev1 = coding_observation(&intent, "running", "started", 1);
    db.record_agent_task_coding_run_observation(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &running_rev1,
    )
    .unwrap();

    let mut completed_rev2 = coding_observation(&intent, "completed", "completed", 2);
    completed_rev2.terminal_message = Some("authoritative rev2 completion".to_string());
    completed_rev2.completed_at_unix = Some(2222);
    db.record_agent_task_coding_run_observation(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &completed_rev2,
    )
    .unwrap();
    let terminal = db
        .terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
            Some("authoritative result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
    assert_eq!(terminal.task.state, AgentTaskState::Succeeded);
    assert_eq!(terminal.attempt.state, AgentTaskAttemptState::Succeeded);

    let stale = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &running_rev1,
        )
        .unwrap();
    assert_eq!(
        stale.dispatch_state,
        AgentTaskCodingRunDispatchState::Terminal
    );
    assert_eq!(stale.last_observation_revision, Some(2));
    assert_eq!(
        stale.last_observed_run_state,
        Some(CodingAgentRunState::Completed)
    );
    assert_eq!(
        stale.last_observed_execution_state,
        Some(CodingAgentExecutionState::Completed)
    );
    assert_eq!(stale.terminal_stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(stale.terminal_error_code, None);
    assert_eq!(
        stale.terminal_message.as_deref(),
        Some("authoritative rev2 completion")
    );
    assert_eq!(stale.completed_at_unix, Some(2222));

    let exact_replay = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
        )
        .unwrap();
    assert_eq!(exact_replay, stale);
    let replay_terminal = db
        .terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
            Some("authoritative result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
    assert!(!replay_terminal.state_changed);

    let running_rev3 = coding_observation(&intent, "running", "started", 3);
    let regression = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &running_rev3,
        )
        .unwrap_err();
    assert_eq!(
        regression.code(),
        "agent_task_coding_run_observation_conflict"
    );
    let unchanged = db
        .read_agent_task_coding_run_binding(&owner, &task_id, &started.attempt.attempt_id)
        .unwrap();
    assert_eq!(unchanged, stale);
    let task = db.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(task.summary.state, AgentTaskState::Succeeded);
    assert_eq!(
        task.summary.latest_attempt.unwrap().state,
        AgentTaskAttemptState::Succeeded
    );
}

#[test]
fn coding_run_observation_equal_revision_conflict_keeps_stored_truth() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-revision-conflict.db")).unwrap();
    let owner = principal('b');
    let assignee = agent(&db, &owner, "coding-revision-conflict-agent");
    let now = wall_now_ms();
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-revision-conflict-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "coding-revision-conflict-start",
        now,
    );
    let intent = coding_binding_intent("revision-conflict-run");
    prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();

    let running_rev2 = coding_observation(&intent, "running", "started", 2);
    let stored = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &running_rev2,
        )
        .unwrap();
    let completed_rev2 = coding_observation(&intent, "completed", "completed", 2);
    let conflict = db
        .record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
        )
        .unwrap_err();
    assert_eq!(
        conflict.code(),
        "agent_task_coding_run_observation_conflict"
    );
    let unchanged = db
        .read_agent_task_coding_run_binding(&owner, &task_id, &started.attempt.attempt_id)
        .unwrap();
    assert_eq!(unchanged, stored);
    assert_eq!(unchanged.last_observation_revision, Some(2));
    assert_eq!(
        unchanged.last_observed_run_state,
        Some(CodingAgentRunState::Running)
    );
    assert_eq!(unchanged.terminal_stop_reason, None);
    assert_eq!(unchanged.terminal_message, None);
}

#[test]
fn coding_run_terminal_observation_stays_monotonic_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-coding-monotonic-restart.db");
    let owner = principal('c');
    let now = wall_now_ms();
    let (task_id, attempt_id, running_rev1, completed_rev2) = {
        let db = Database::open(&path).unwrap();
        let assignee = agent(&db, &owner, "coding-monotonic-restart-agent");
        let task_id = create_assigned_task(&db, &owner, &assignee, "coding-monotonic-restart-task");
        let started = start(
            &db,
            &owner,
            &task_id,
            &assignee,
            "coding-monotonic-restart-start",
            now,
        );
        let intent = coding_binding_intent("monotonic-restart-run");
        prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
        db.claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
        let running_rev1 = coding_observation(&intent, "running", "started", 1);
        db.record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &running_rev1,
        )
        .unwrap();
        let mut completed_rev2 = coding_observation(&intent, "completed", "completed", 2);
        completed_rev2.terminal_message = Some("restart rev2 completion".to_string());
        completed_rev2.completed_at_unix = Some(3333);
        db.record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
        )
        .unwrap();
        db.terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &completed_rev2,
            Some("restart result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
        (
            task_id,
            started.attempt.attempt_id,
            running_rev1,
            completed_rev2,
        )
    };

    let reopened = Database::open(&path).unwrap();
    let stale = reopened
        .record_agent_task_coding_run_observation(&owner, &task_id, &attempt_id, &running_rev1)
        .unwrap();
    assert_eq!(
        stale.dispatch_state,
        AgentTaskCodingRunDispatchState::Terminal
    );
    assert_eq!(stale.last_observation_revision, Some(2));
    assert_eq!(
        stale.last_observed_run_state,
        Some(CodingAgentRunState::Completed)
    );
    assert_eq!(
        stale.terminal_message.as_deref(),
        completed_rev2.terminal_message.as_deref()
    );
    assert_eq!(stale.completed_at_unix, Some(3333));
    let task = reopened.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(task.summary.state, AgentTaskState::Succeeded);
    assert_eq!(
        task.summary.latest_attempt.unwrap().state,
        AgentTaskAttemptState::Succeeded
    );
}

#[test]
fn failed_cancelled_and_lost_have_bounded_exact_terminal_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-terminal-states.db")).unwrap();
    let owner = principal('5');
    let assignee = agent(&db, &owner, "coding-terminal-states-agent");
    let now = wall_now_ms();

    for (index, run_state, reason) in [
        (0_i64, "failed", "provider_failed"),
        (1_i64, "cancelled", "coding_agent_cancelled"),
    ] {
        let task_id = create_assigned_task(
            &db,
            &owner,
            &assignee,
            &format!("coding-terminal-state-task-{index}"),
        );
        let started = start(
            &db,
            &owner,
            &task_id,
            &assignee,
            &format!("coding-terminal-state-start-{index}"),
            now + index * 20,
        );
        let intent = coding_binding_intent(&format!("terminal-state-{index}"));
        prepare_coding_binding(
            &db,
            &owner,
            &task_id,
            &assignee,
            &started,
            &intent,
            now + index * 20 + 1,
        );
        db.claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
        let observation = coding_observation(&intent, run_state, "completed", 3);
        db.record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &observation,
        )
        .unwrap();
        let mutation = db
            .terminalize_agent_task_coding_run(
                &owner,
                &task_id,
                &started.attempt.attempt_id,
                &observation,
                Some("bounded terminal result"),
                Some(reason),
            )
            .unwrap();
        assert_eq!(mutation.task.state, AgentTaskState::Failed);
        assert_eq!(mutation.attempt.state, AgentTaskAttemptState::Failed);
        assert_eq!(mutation.attempt.terminal_reason.as_deref(), Some(reason));
    }

    let lost_task = create_assigned_task(&db, &owner, &assignee, "coding-lost-task");
    let lost_started = start(
        &db,
        &owner,
        &lost_task,
        &assignee,
        "coding-lost-start",
        now + 50,
    );
    let lost_intent = coding_binding_intent("lost-terminal-state");
    prepare_coding_binding(
        &db,
        &owner,
        &lost_task,
        &assignee,
        &lost_started,
        &lost_intent,
        now + 51,
    );
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &lost_task,
        &lost_started.attempt.attempt_id,
        &assignee,
        &lost_started.attempt_fence,
        1,
        &lost_intent.binding_intent_fingerprint,
    )
    .unwrap();
    let lost = coding_observation(&lost_intent, "lost", "outcome_unknown", 4);
    let binding = db
        .record_agent_task_coding_run_observation(
            &owner,
            &lost_task,
            &lost_started.attempt.attempt_id,
            &lost,
        )
        .unwrap();
    assert_eq!(
        binding.dispatch_state,
        AgentTaskCodingRunDispatchState::OutcomeUnknown
    );
    let terminal_error = db
        .terminalize_agent_task_coding_run(
            &owner,
            &lost_task,
            &lost_started.attempt.attempt_id,
            &lost,
            Some("must not commit"),
            Some("must not commit"),
        )
        .unwrap_err();
    assert_eq!(terminal_error.code(), "agent_task_coding_run_not_terminal");
    assert_eq!(
        db.read_agent_task(&owner, &lost_task)
            .unwrap()
            .summary
            .state,
        AgentTaskState::Active
    );
    let completed_after_lost = coding_observation(&lost_intent, "completed", "completed", 6);
    let recovered = db
        .terminalize_agent_task_coding_run(
            &owner,
            &lost_task,
            &lost_started.attempt.attempt_id,
            &completed_after_lost,
            Some("recovered definitive result"),
            Some("coding_agent_completed"),
        )
        .unwrap();
    assert_eq!(recovered.task.state, AgentTaskState::Succeeded);
    assert_eq!(recovered.attempt.state, AgentTaskAttemptState::Succeeded);
    assert_eq!(
        recovered.binding.dispatch_state,
        AgentTaskCodingRunDispatchState::Terminal
    );
    assert_eq!(recovered.binding.last_observation_revision, Some(6));
    assert_eq!(
        recovered.binding.last_observed_run_state,
        Some(CodingAgentRunState::Completed)
    );
}

#[test]
fn old_coding_run_cannot_terminalize_task_after_later_attempt_exists() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-later-attempt.db")).unwrap();
    let owner = principal('6');
    let assignee = agent(&db, &owner, "coding-later-agent");
    let now = wall_now_ms();
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-later-task");
    let first = start(&db, &owner, &task_id, &assignee, "coding-later-first", now);
    let intent = coding_binding_intent("later-attempt-old-run");
    prepare_coding_binding(&db, &owner, &task_id, &assignee, &first, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &first.attempt.attempt_id,
        &assignee,
        &first.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();
    db.record_agent_task_coding_run_not_started(
        &owner,
        &task_id,
        &first.attempt.attempt_id,
        &intent.run_id,
    )
    .unwrap();
    let second = db
        .start_agent_task_attempt_at(
            &owner,
            &task_id,
            &assignee,
            "coding-later-second",
            first.attempt.lease_expires_at_unix_ms,
        )
        .unwrap();
    assert_eq!(second.attempt.attempt_number, 2);

    let old_terminal = coding_observation(&intent, "completed", "completed", 9);
    db.record_agent_task_coding_run_observation(
        &owner,
        &task_id,
        &first.attempt.attempt_id,
        &old_terminal,
    )
    .unwrap();
    let stale = db
        .terminalize_agent_task_coding_run(
            &owner,
            &task_id,
            &first.attempt.attempt_id,
            &old_terminal,
            Some("old result"),
            Some("coding_agent_completed"),
        )
        .unwrap_err();
    assert_eq!(stale.code(), "agent_task_attempt_stale");
    let task = db.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(task.summary.state, AgentTaskState::Active);
    assert_eq!(
        task.summary.latest_attempt.unwrap().attempt_id,
        second.attempt.attempt_id
    );
}

#[test]
fn coding_run_binding_survives_restart_without_endpoint_or_window_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-coding-restart.db");
    let owner = principal('7');
    let foreign = principal('8');
    let now = wall_now_ms();
    let (task_id, attempt_id, run_id) = {
        let db = Database::open(&path).unwrap();
        let assignee = agent(&db, &owner, "coding-restart-agent");
        let task_id = create_assigned_task(&db, &owner, &assignee, "coding-restart-task");
        let started = start(
            &db,
            &owner,
            &task_id,
            &assignee,
            "coding-restart-start",
            now,
        );
        let intent = coding_binding_intent("restart-run");
        prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
        db.claim_agent_task_coding_run_dispatch(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            &intent.binding_intent_fingerprint,
        )
        .unwrap();
        db.record_agent_task_coding_run_observation(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &coding_observation(&intent, "running", "started", 11),
        )
        .unwrap();
        (task_id, started.attempt.attempt_id, intent.run_id)
    };

    let reopened = Database::open(&path).unwrap();
    let binding = reopened
        .read_agent_task_coding_run_binding(&owner, &task_id, &attempt_id)
        .unwrap();
    assert_eq!(binding.run_id, run_id);
    assert_eq!(binding.runtime_project_id, "agent:special:reference-only");
    assert_eq!(binding.provider_id, "codex");
    assert_eq!(
        binding.dispatch_state,
        AgentTaskCodingRunDispatchState::Bound
    );
    assert_eq!(
        binding.last_observed_run_state,
        Some(CodingAgentRunState::Running)
    );
    assert_eq!(binding.last_observation_revision, Some(11));
    let task = reopened.read_agent_task(&owner, &task_id).unwrap();
    assert!(task.summary.execution_bound);
    assert_eq!(
        task.summary.execution_status,
        Some(AgentTaskExecutionStatus::Active)
    );
    assert_eq!(
        task.summary.recovery_kind,
        AgentTaskExecutionRecoveryKind::Observe
    );

    let foreign_error = reopened
        .read_agent_task_coding_run_binding(&foreign, &task_id, &attempt_id)
        .unwrap_err();
    assert_eq!(foreign_error.code(), "agent_task_not_found");
}

#[test]
fn corrupt_coding_run_observation_states_fail_closed_on_binding_load() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-coding-corrupt-observation.db")).unwrap();
    let owner = principal('0');
    let assignee = agent(&db, &owner, "coding-corrupt-observation-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "coding-corrupt-observation-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "coding-corrupt-observation-start",
        now,
    );
    let intent = coding_binding_intent("corrupt-observation-run");
    prepare_coding_binding(&db, &owner, &task_id, &assignee, &started, &intent, now + 1);
    db.claim_agent_task_coding_run_dispatch(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        1,
        &intent.binding_intent_fingerprint,
    )
    .unwrap();
    db.record_agent_task_coding_run_observation(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &coding_observation(&intent, "running", "started", 1),
    )
    .unwrap();

    db.conn_for_tests()
        .execute(
            "UPDATE wc_agent_task_coding_runs
             SET last_observed_run_state = 'future_state'
             WHERE task_id = ?1 AND attempt_id = ?2",
            params![task_id, started.attempt.attempt_id],
        )
        .unwrap();
    assert!(db
        .read_agent_task_coding_run_binding(&owner, &task_id, &started.attempt.attempt_id)
        .is_err());

    db.conn_for_tests()
        .execute(
            "UPDATE wc_agent_task_coding_runs
             SET last_observed_run_state = 'running',
                 last_observed_execution_state = 'future_state'
             WHERE task_id = ?1 AND attempt_id = ?2",
            params![task_id, started.attempt.attempt_id],
        )
        .unwrap();
    assert!(db
        .read_agent_task_coding_run_binding(&owner, &task_id, &started.attempt.attempt_id)
        .is_err());
}

#[test]
fn agent_task_create_is_keyed_bounded_and_restart_durable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-create.db");
    let db = Database::open(&path).unwrap();
    let owner = principal('1');
    let assignee = agent(&db, &owner, "creator-agent");
    let created = db
        .create_agent_task_at(
            &owner,
            task_input(Some(assignee.clone()), "task-create-key"),
            T0,
        )
        .unwrap();
    assert!(created.created);
    assert!(!created.replayed);
    assert!(created.state_changed);
    assert!(created
        .task
        .summary
        .task_id
        .starts_with(AGENT_TASK_ID_PREFIX));
    assert_eq!(created.task.summary.state, AgentTaskState::Ready);
    assert_eq!(
        created.task.summary.assignee_agent_id.as_deref(),
        Some(assignee.as_str())
    );
    assert_eq!(
        created.task.summary.referenced_project_id.as_deref(),
        Some("agent:special:reference-only")
    );
    assert!(created.task.summary.latest_attempt.is_none());

    let replay = db
        .create_agent_task_at(
            &owner,
            task_input(Some(assignee.clone()), "task-create-key"),
            T0 + 1,
        )
        .unwrap();
    assert_eq!(replay.task.summary.task_id, created.task.summary.task_id);
    assert!(replay.replayed);
    assert!(!replay.state_changed);

    let mut changed = task_input(Some(assignee), "task-create-key");
    changed.instruction.push_str(" changed");
    let conflict = db
        .create_agent_task_at(&owner, changed, T0 + 2)
        .unwrap_err();
    assert_eq!(conflict.code(), "communication_idempotency_conflict");

    let task_id = created.task.summary.task_id.clone();
    drop(db);
    let reopened = Database::open(&path).unwrap();
    let recovered = reopened.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(recovered.summary.task_id, task_id);
    assert_eq!(recovered.summary.state, AgentTaskState::Ready);
    assert_eq!(recovered.instruction, created.task.instruction);
}

#[test]
fn unassigned_and_foreign_assignee_tasks_fail_closed_until_explicit_assignment() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-assignment.db")).unwrap();
    let owner = principal('2');
    let foreign = principal('3');
    let owned_agent = agent(&db, &owner, "owned-assignee");
    let foreign_agent = agent(&db, &foreign, "foreign-assignee");

    let unassigned = db
        .create_agent_task_at(&owner, task_input(None, "unassigned"), T0)
        .unwrap();
    let task_id = unassigned.task.summary.task_id;
    let error = db
        .start_agent_task_attempt_at(
            &owner,
            &task_id,
            &owned_agent,
            "start-before-assign",
            T0 + 10,
        )
        .unwrap_err();
    assert_eq!(error.code(), "agent_task_unassigned");

    let foreign_error = db
        .assign_agent_task_at(&owner, &task_id, &foreign_agent, T0 + 11)
        .unwrap_err();
    assert_eq!(foreign_error.code(), "agent_not_found");

    let foreign_create = db
        .create_agent_task_at(
            &owner,
            task_input(Some(foreign_agent), "foreign-create-assignee"),
            T0 + 11,
        )
        .unwrap_err();
    assert_eq!(foreign_create.code(), "agent_not_found");

    let assigned = db
        .assign_agent_task_at(&owner, &task_id, &owned_agent, T0 + 12)
        .unwrap();
    assert!(assigned.state_changed);
    assert_eq!(
        assigned.task.summary.assignee_agent_id.as_deref(),
        Some(owned_agent.as_str())
    );
    let attempt = start(
        &db,
        &owner,
        &task_id,
        &owned_agent,
        "start-after-assign",
        T0 + 13,
    );
    assert_eq!(attempt.attempt.attempt_number, 1);
}

#[test]
fn concurrent_attempt_start_creates_exactly_one_authoritative_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-race.db");
    let setup = Database::open(&path).unwrap();
    let owner = principal('4');
    let assignee = agent(&setup, &owner, "race-agent");
    let task_id = create_assigned_task(&setup, &owner, &assignee, "race-task");
    drop(setup);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for suffix in ["a", "b"] {
        let path = path.clone();
        let barrier = barrier.clone();
        let owner = owner.clone();
        let assignee = assignee.clone();
        let task_id = task_id.clone();
        handles.push(std::thread::spawn(move || {
            let db = Database::open(&path).unwrap();
            barrier.wait();
            db.start_agent_task_attempt_at(
                &owner,
                &task_id,
                &assignee,
                &format!("race-start-{suffix}"),
                T0 + 100,
            )
        }));
    }
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .map(|error| error.code())
            .collect::<Vec<_>>(),
        vec!["agent_task_attempt_active"]
    );

    let reopened = Database::open(&path).unwrap();
    let count: i64 = reopened
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_agent_task_attempts WHERE task_id = ?1",
            [task_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn live_coding_dispatch_mutations_sample_server_time_after_serialization_wait() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-coding-serialized-clock.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let owner = principal('d');
    let assignee = agent(&db, &owner, "coding-serialized-clock-agent");
    let now = wall_now_ms();

    let prepare_task =
        create_assigned_task(&db, &owner, &assignee, "coding-serialized-prepare-task");
    let prepare_attempt = start(
        &db,
        &owner,
        &prepare_task,
        &assignee,
        "coding-serialized-prepare-start",
        now,
    );
    let prepare_intent = coding_binding_intent("serialized-prepare");

    let claim_task = create_assigned_task(&db, &owner, &assignee, "coding-serialized-claim-task");
    let claim_attempt = start(
        &db,
        &owner,
        &claim_task,
        &assignee,
        "coding-serialized-claim-start",
        now + 1,
    );
    let claim_intent = coding_binding_intent("serialized-claim");
    prepare_coding_binding(
        &db,
        &owner,
        &claim_task,
        &assignee,
        &claim_attempt,
        &claim_intent,
        now + 2,
    );

    // Hold the same Database mutex/transaction boundary used by live A4a mutations.
    // Both workers enter their production wrappers before the lease is shortened, then
    // wait until after expiry. A pre-serialization clock sample would incorrectly let
    // prepare create a binding or claim cross the external-dispatch fence.
    let guard = db.conn_for_tests();
    let (ready_tx, ready_rx) = mpsc::channel();

    let prepare_db = Arc::clone(&db);
    let prepare_owner = owner.clone();
    let prepare_assignee = assignee.clone();
    let prepare_task_id = prepare_task.clone();
    let prepare_attempt_id = prepare_attempt.attempt.attempt_id.clone();
    let prepare_fence = prepare_attempt.attempt_fence.clone();
    let prepare_intent_for_thread = prepare_intent.clone();
    let prepare_ready = ready_tx.clone();
    let prepare_handle = std::thread::spawn(move || {
        prepare_ready.send(()).unwrap();
        prepare_db.prepare_agent_task_coding_run(
            &prepare_owner,
            "agent:special:reference-only",
            &prepare_task_id,
            &prepare_attempt_id,
            &prepare_assignee,
            &prepare_fence,
            1,
            &prepare_intent_for_thread,
        )
    });

    let claim_db = Arc::clone(&db);
    let claim_owner = owner.clone();
    let claim_assignee = assignee.clone();
    let claim_task_id = claim_task.clone();
    let claim_attempt_id = claim_attempt.attempt.attempt_id.clone();
    let claim_fence = claim_attempt.attempt_fence.clone();
    let claim_fingerprint = claim_intent.binding_intent_fingerprint.clone();
    let claim_ready = ready_tx.clone();
    let claim_handle = std::thread::spawn(move || {
        claim_ready.send(()).unwrap();
        claim_db.claim_agent_task_coding_run_dispatch(
            &claim_owner,
            &claim_task_id,
            &claim_attempt_id,
            &claim_assignee,
            &claim_fence,
            1,
            &claim_fingerprint,
        )
    });

    ready_rx.recv().unwrap();
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let expires_at = wall_now_ms().saturating_add(200);
    guard
        .execute(
            "UPDATE wc_agent_task_attempts
             SET lease_expires_at_unix_ms = ?1
             WHERE attempt_id IN (?2, ?3)",
            params![
                expires_at,
                prepare_attempt.attempt.attempt_id,
                claim_attempt.attempt.attempt_id,
            ],
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(guard);

    let prepare_error = prepare_handle.join().unwrap().unwrap_err();
    assert_eq!(prepare_error.code(), "agent_task_attempt_stale");
    let claim_error = claim_handle.join().unwrap().unwrap_err();
    assert_eq!(claim_error.code(), "agent_task_attempt_stale");

    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_coding_runs WHERE attempt_id = ?1",
                [prepare_attempt.attempt.attempt_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "expired prepare must not create a durable backend binding",
    );
    assert_eq!(
        db.read_agent_task_coding_run_binding(
            &owner,
            &claim_task,
            &claim_attempt.attempt.attempt_id,
        )
        .unwrap()
        .dispatch_state,
        AgentTaskCodingRunDispatchState::Prepared,
        "expired claim must not cross the durable outcome-unknown dispatch fence",
    );
}

#[test]
fn live_lease_mutations_sample_server_time_after_serialization_wait() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-serialized-clock.db");
    let db = Arc::new(Database::open(&path).unwrap());
    let owner = principal('e');
    let assignee = agent(&db, &owner, "serialized-clock-agent");

    let heartbeat_task = create_assigned_task(&db, &owner, &assignee, "heartbeat-clock-task");
    let heartbeat_attempt = start(
        &db,
        &owner,
        &heartbeat_task,
        &assignee,
        "heartbeat-clock-start",
        T0 + 15,
    );
    let completion_task = create_assigned_task(&db, &owner, &assignee, "completion-clock-task");
    let completion_attempt = start(
        &db,
        &owner,
        &completion_task,
        &assignee,
        "completion-clock-start",
        T0 + 16,
    );

    // Hold the same Database mutex that production mutations serialize through.
    // Workers announce immediately before entering the public mutation wrapper. The
    // lease is then moved to a point after any pre-lock clock sample but before the
    // mutex is released. A wrapper that sampled time before serialization would
    // incorrectly renew/complete; the production path must sample after it acquires
    // the authoritative transaction.
    let guard = db.conn_for_tests();
    let (ready_tx, ready_rx) = mpsc::channel();

    let heartbeat_db = Arc::clone(&db);
    let heartbeat_owner = owner.clone();
    let heartbeat_assignee = assignee.clone();
    let heartbeat_task_id = heartbeat_task.clone();
    let heartbeat_attempt_id = heartbeat_attempt.attempt.attempt_id.clone();
    let heartbeat_fence = heartbeat_attempt.attempt_fence.clone();
    let heartbeat_ready = ready_tx.clone();
    let heartbeat_handle = std::thread::spawn(move || {
        heartbeat_ready.send(()).unwrap();
        heartbeat_db.heartbeat_agent_task_attempt(
            &heartbeat_owner,
            &heartbeat_task_id,
            &heartbeat_attempt_id,
            &heartbeat_assignee,
            &heartbeat_fence,
            1,
        )
    });

    let completion_db = Arc::clone(&db);
    let completion_owner = owner.clone();
    let completion_assignee = assignee.clone();
    let completion_task_id = completion_task.clone();
    let completion_attempt_id = completion_attempt.attempt.attempt_id.clone();
    let completion_fence = completion_attempt.attempt_fence.clone();
    let completion_ready = ready_tx.clone();
    let completion_handle = std::thread::spawn(move || {
        completion_ready.send(()).unwrap();
        completion_db.complete_agent_task_attempt(
            &completion_owner,
            &completion_task_id,
            &completion_attempt_id,
            &completion_assignee,
            &completion_fence,
            1,
            AgentTaskState::Succeeded,
            Some("late result"),
            None,
            "completion-clock-finish",
        )
    });

    ready_rx.recv().unwrap();
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let expires_at = wall_now_ms().saturating_add(200);
    guard
        .execute(
            "UPDATE wc_agent_task_attempts
             SET lease_expires_at_unix_ms = ?1
             WHERE attempt_id IN (?2, ?3)",
            params![
                expires_at,
                heartbeat_attempt.attempt.attempt_id,
                completion_attempt.attempt.attempt_id,
            ],
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(guard);

    let heartbeat_error = heartbeat_handle.join().unwrap().unwrap_err();
    assert_eq!(heartbeat_error.code(), "agent_task_attempt_stale");
    let completion_error = completion_handle.join().unwrap().unwrap_err();
    assert_eq!(completion_error.code(), "agent_task_attempt_stale");
}

#[test]
fn exact_start_retry_returns_same_attempt_and_fence_even_after_later_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-start-replay.db")).unwrap();
    let owner = principal('5');
    let assignee = agent(&db, &owner, "retry-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "retry-task");
    let first = start(&db, &owner, &task_id, &assignee, "start-one", T0 + 10);
    assert!(first
        .attempt
        .attempt_id
        .starts_with(AGENT_TASK_ATTEMPT_ID_PREFIX));
    assert!(first
        .attempt_fence
        .starts_with(AGENT_TASK_ATTEMPT_FENCE_PREFIX));
    let direct_replay = start(&db, &owner, &task_id, &assignee, "start-one", T0 + 11);
    assert_eq!(direct_replay.attempt.attempt_id, first.attempt.attempt_id);
    assert_eq!(direct_replay.attempt_fence, first.attempt_fence);
    assert!(direct_replay.replayed);

    let second = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "start-two",
        first.attempt.lease_expires_at_unix_ms,
    );
    assert_eq!(second.attempt.attempt_number, 2);
    assert_ne!(second.attempt.attempt_id, first.attempt.attempt_id);
    assert_ne!(second.attempt_fence, first.attempt_fence);

    let old_replay = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "start-one",
        second.attempt.started_at_unix_ms + 1,
    );
    assert_eq!(old_replay.attempt.attempt_id, first.attempt.attempt_id);
    assert_eq!(old_replay.attempt_fence, first.attempt_fence);
    assert_eq!(old_replay.attempt.state, AgentTaskAttemptState::Expired);
    assert_eq!(
        old_replay.task.latest_attempt.unwrap().attempt_id,
        second.attempt.attempt_id
    );
}

#[test]
fn lease_expiry_permanently_fences_old_attempt_and_never_transfers_assignment() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-lease.db")).unwrap();
    let owner = principal('6');
    let agent_a = agent(&db, &owner, "lease-a");
    let agent_b = agent(&db, &owner, "lease-b");
    let task_id = create_assigned_task(&db, &owner, &agent_a, "lease-task");
    let first = start(&db, &owner, &task_id, &agent_a, "lease-start-1", T0 + 20);
    let before_expiry = first.attempt.lease_expires_at_unix_ms - 1;
    let heartbeat = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &first.attempt.attempt_id,
            &agent_a,
            &first.attempt_fence,
            first.attempt.attempt_controller_generation,
            before_expiry,
        )
        .unwrap();
    assert!(heartbeat.attempt.lease_active);
    let extended_expiry = heartbeat.attempt.lease_expires_at_unix_ms;
    assert!(extended_expiry > first.attempt.lease_expires_at_unix_ms);

    let stale_heartbeat = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &first.attempt.attempt_id,
            &agent_a,
            &first.attempt_fence,
            first.attempt.attempt_controller_generation,
            extended_expiry,
        )
        .unwrap_err();
    assert_eq!(stale_heartbeat.code(), "agent_task_attempt_stale");
    let stale_completion = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &first.attempt.attempt_id,
            &agent_a,
            &first.attempt_fence,
            first.attempt.attempt_controller_generation,
            AgentTaskState::Succeeded,
            Some("late"),
            None,
            "late-completion",
            extended_expiry,
        )
        .unwrap_err();
    assert_eq!(stale_completion.code(), "agent_task_attempt_stale");

    let wrong_agent = db
        .start_agent_task_attempt_at(
            &owner,
            &task_id,
            &agent_b,
            "wrong-agent-start",
            extended_expiry + 1,
        )
        .unwrap_err();
    assert_eq!(wrong_agent.code(), "agent_task_assignee_mismatch");

    let second = start(
        &db,
        &owner,
        &task_id,
        &agent_a,
        "lease-start-2",
        extended_expiry + 2,
    );
    assert_eq!(second.attempt.attempt_number, 2);
    assert_ne!(second.attempt_fence, first.attempt_fence);

    let permanently_stale = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &first.attempt.attempt_id,
            &agent_a,
            &first.attempt_fence,
            first.attempt.attempt_controller_generation,
            extended_expiry + 3,
        )
        .unwrap_err();
    assert_eq!(permanently_stale.code(), "agent_task_attempt_stale");
}

#[test]
fn explicit_reassignment_after_expiry_changes_only_durable_assignee() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-reassign.db")).unwrap();
    let owner = principal('7');
    let agent_a = agent(&db, &owner, "reassign-a");
    let agent_b = agent(&db, &owner, "reassign-b");
    let task_id = create_assigned_task(&db, &owner, &agent_a, "reassign-task");
    let first = start(&db, &owner, &task_id, &agent_a, "reassign-start-1", T0 + 30);

    let active_reassign = db
        .assign_agent_task_at(
            &owner,
            &task_id,
            &agent_b,
            first.attempt.lease_expires_at_unix_ms - 1,
        )
        .unwrap_err();
    assert_eq!(active_reassign.code(), "agent_task_attempt_active");

    let reassigned = db
        .assign_agent_task_at(
            &owner,
            &task_id,
            &agent_b,
            first.attempt.lease_expires_at_unix_ms,
        )
        .unwrap();
    assert!(reassigned.state_changed);
    assert_eq!(
        reassigned.task.summary.assignee_agent_id.as_deref(),
        Some(agent_b.as_str())
    );
    let second = start(
        &db,
        &owner,
        &task_id,
        &agent_b,
        "reassign-start-2",
        first.attempt.lease_expires_at_unix_ms + 1,
    );
    assert_eq!(second.attempt.assignee_agent_id, agent_b);
    assert_eq!(second.attempt.attempt_number, 2);
}

#[test]
fn controller_generation_fences_replaced_carrier_without_creating_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-controller.db")).unwrap();
    let owner = principal('8');
    let assignee = agent(&db, &owner, "controller-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "controller-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "controller-start",
        T0 + 40,
    );
    assert_eq!(started.attempt.attempt_controller_generation, 1);

    let replaced = db
        .replace_agent_task_attempt_controller_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            T0 + 41,
        )
        .unwrap();
    assert_eq!(replaced.attempt_id, started.attempt.attempt_id);
    assert_eq!(replaced.attempt_number, 1);
    assert_eq!(replaced.attempt_controller_generation, 2);

    let stale_generation = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            T0 + 42,
        )
        .unwrap_err();
    assert_eq!(stale_generation.code(), "agent_task_attempt_stale");
    let stale_completion = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("stale controller must not complete"),
            None,
            "stale-controller-completion",
            T0 + 42,
        )
        .unwrap_err();
    assert_eq!(stale_completion.code(), "agent_task_attempt_stale");
    db.heartbeat_agent_task_attempt_at(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        2,
        T0 + 42,
    )
    .unwrap();

    let count: i64 = db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_agent_task_attempts WHERE task_id = ?1",
            [task_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn completion_is_fenced_terminal_and_exactly_replayable() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-completion.db")).unwrap();
    let owner = principal('9');
    let assignee = agent(&db, &owner, "completion-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "completion-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "completion-start",
        T0 + 50,
    );

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("done"),
            Some("bounded result"),
            "completion-key",
            T0 + 51,
        )
        .unwrap();
    assert_eq!(completed.task.state, AgentTaskState::Succeeded);
    assert_eq!(completed.attempt.state, AgentTaskAttemptState::Succeeded);
    assert_eq!(completed.attempt.terminal_result.as_deref(), Some("done"));
    assert!(completed.state_changed);

    let replay = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("done"),
            Some("bounded result"),
            "completion-key",
            T0 + 99,
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(!replay.state_changed);
    assert_eq!(replay.attempt.attempt_id, completed.attempt.attempt_id);

    let changed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("changed"),
            Some("bounded result"),
            "completion-key",
            T0 + 100,
        )
        .unwrap_err();
    assert_eq!(changed.code(), "communication_idempotency_conflict");

    let terminal_start = db
        .start_agent_task_attempt_at(&owner, &task_id, &assignee, "after-terminal", T0 + 101)
        .unwrap_err();
    assert_eq!(terminal_start.code(), "agent_task_terminal");
}

#[test]
fn restart_preserves_attempt_number_fence_generation_and_replay() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-restart.db");
    let owner = principal('a');
    let (task_id, assignee, first_fence, second_attempt_id, second_fence) = {
        let db = Database::open(&path).unwrap();
        let assignee = agent(&db, &owner, "restart-agent");
        let task_id = create_assigned_task(&db, &owner, &assignee, "restart-task");
        let first = start(&db, &owner, &task_id, &assignee, "restart-start-1", T0 + 60);
        let second = start(
            &db,
            &owner,
            &task_id,
            &assignee,
            "restart-start-2",
            first.attempt.lease_expires_at_unix_ms,
        );
        let replaced = db
            .replace_agent_task_attempt_controller_at(
                &owner,
                &task_id,
                &second.attempt.attempt_id,
                &assignee,
                &second.attempt_fence,
                1,
                second.attempt.started_at_unix_ms + 1,
            )
            .unwrap();
        assert_eq!(replaced.attempt_controller_generation, 2);
        (
            task_id,
            assignee,
            first.attempt_fence,
            second.attempt.attempt_id,
            second.attempt_fence,
        )
    };

    let reopened = Database::open(&path).unwrap();
    let task = reopened.read_agent_task(&owner, &task_id).unwrap();
    let latest = task.summary.latest_attempt.unwrap();
    assert_eq!(latest.attempt_id, second_attempt_id);
    assert_eq!(latest.attempt_number, 2);
    assert_eq!(latest.attempt_controller_generation, 2);

    let replay = reopened
        .start_agent_task_attempt_at(&owner, &task_id, &assignee, "restart-start-2", T0 + 61)
        .unwrap();
    assert_eq!(replay.attempt.attempt_id, second_attempt_id);
    assert_eq!(replay.attempt_fence, second_fence);
    assert_eq!(replay.attempt.attempt_controller_generation, 2);
    assert_ne!(replay.attempt_fence, first_fence);
}

#[test]
fn restart_preserves_terminal_replay_and_never_revives_expired_attempts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-task-restart-terminal.db");
    let owner = principal('1');
    let (
        assignee,
        completed_task_id,
        completed_attempt_id,
        completed_fence,
        expiring_task_id,
        expiring_attempt_id,
        expiring_fence,
        expiring_lease,
    ) = {
        let db = Database::open(&path).unwrap();
        let assignee = agent(&db, &owner, "restart-terminal-agent");

        let completed_task_id =
            create_assigned_task(&db, &owner, &assignee, "restart-terminal-task");
        let completed = start(
            &db,
            &owner,
            &completed_task_id,
            &assignee,
            "restart-terminal-start",
            T0 + 70,
        );
        db.complete_agent_task_attempt_at(
            &owner,
            &completed_task_id,
            &completed.attempt.attempt_id,
            &assignee,
            &completed.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("persisted terminal result"),
            Some("persisted terminal reason"),
            "restart-terminal-completion",
            T0 + 71,
        )
        .unwrap();

        let expiring_task_id =
            create_assigned_task(&db, &owner, &assignee, "restart-expiring-task");
        let expiring = start(
            &db,
            &owner,
            &expiring_task_id,
            &assignee,
            "restart-expiring-start",
            T0 + 80,
        );
        (
            assignee,
            completed_task_id,
            completed.attempt.attempt_id,
            completed.attempt_fence,
            expiring_task_id,
            expiring.attempt.attempt_id,
            expiring.attempt_fence,
            expiring.attempt.lease_expires_at_unix_ms,
        )
    };

    let reopened = Database::open(&path).unwrap();
    let completed = reopened
        .read_agent_task_at(&owner, &completed_task_id, T0 + 100)
        .unwrap();
    let completed_attempt = completed.summary.latest_attempt.unwrap();
    assert_eq!(completed.summary.state, AgentTaskState::Succeeded);
    assert_eq!(completed_attempt.attempt_id, completed_attempt_id);
    assert_eq!(
        completed_attempt.terminal_result.as_deref(),
        Some("persisted terminal result")
    );
    assert_eq!(
        completed_attempt.terminal_reason.as_deref(),
        Some("persisted terminal reason")
    );

    let completion_replay = reopened
        .complete_agent_task_attempt_at(
            &owner,
            &completed_task_id,
            &completed_attempt_id,
            &assignee,
            &completed_fence,
            1,
            AgentTaskState::Succeeded,
            Some("persisted terminal result"),
            Some("persisted terminal reason"),
            "restart-terminal-completion",
            T0 + 101,
        )
        .unwrap();
    assert!(completion_replay.replayed);
    assert!(!completion_replay.state_changed);
    assert_eq!(
        completion_replay.attempt.terminal_result.as_deref(),
        Some("persisted terminal result")
    );

    let expired = reopened
        .read_agent_task_at(&owner, &expiring_task_id, expiring_lease)
        .unwrap();
    assert_eq!(expired.summary.state, AgentTaskState::Ready);
    let expired_attempt = expired.summary.latest_attempt.unwrap();
    assert_eq!(expired_attempt.attempt_id, expiring_attempt_id);
    assert_eq!(expired_attempt.state, AgentTaskAttemptState::Expired);
    assert!(!expired_attempt.lease_active);

    let stale_after_restart = reopened
        .heartbeat_agent_task_attempt_at(
            &owner,
            &expiring_task_id,
            &expiring_attempt_id,
            &assignee,
            &expiring_fence,
            1,
            expiring_lease,
        )
        .unwrap_err();
    assert_eq!(stale_after_restart.code(), "agent_task_attempt_stale");
    let start_replay = reopened
        .start_agent_task_attempt_at(
            &owner,
            &expiring_task_id,
            &assignee,
            "restart-expiring-start",
            expiring_lease + 1,
        )
        .unwrap();
    assert!(start_replay.replayed);
    assert_eq!(start_replay.attempt.attempt_id, expiring_attempt_id);
    assert_eq!(start_replay.attempt_fence, expiring_fence);
    assert_eq!(start_replay.attempt.state, AgentTaskAttemptState::Expired);
}

#[test]
fn endpoint_detach_never_deletes_or_controls_agent_task_attempt_truth() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint.db")).unwrap();
    let owner = principal('b');
    let assignee = agent(&db, &owner, "endpoint-independent-agent");
    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "test-host".to_string(),
                client_attachment_id: Some("browser-window".to_string()),
                wake_capable: false,
                idempotency_key: "task-endpoint".to_string(),
            },
        )
        .unwrap()
        .endpoint;
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-task");
    let started = start(&db, &owner, &task_id, &assignee, "endpoint-start", T0 + 70);
    db.detach_agent_endpoint(&owner, &endpoint.endpoint_id)
        .unwrap();

    let task = db.read_agent_task(&owner, &task_id).unwrap();
    let attempt = task.summary.latest_attempt.unwrap();
    assert_eq!(attempt.attempt_id, started.attempt.attempt_id);
    assert_eq!(attempt.attempt_number, 1);
    assert_eq!(attempt.assignee_agent_id, assignee);
}

#[test]
fn source_conversation_is_correlation_only_and_foreign_exact_ids_are_existence_hidden() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-privacy.db")).unwrap();
    let owner = principal('c');
    let foreign = principal('d');
    let assignee = agent(&db, &owner, "privacy-agent");
    let conversation = db
        .create_conversation(
            &owner,
            NewConversation {
                title: Some("source".to_string()),
                agent_ids: vec![assignee.clone()],
                idempotency_key: "source-conversation".to_string(),
            },
        )
        .unwrap();
    let conversation_id = conversation.conversation.conversation.conversation_id;
    let message = db
        .post_conversation_message(
            &owner,
            NewConversationMessage {
                conversation_id: conversation_id.clone(),
                body: "Explicit source message, not an implicit Task.".to_string(),
                author_agent_id: None,
                endpoint_id: None,
                expected_controller_generation: None,
                recipient_agent_ids: Some(Vec::new()),
                reply_to: None,
                idempotency_key: Some("source-message".to_string()),
                wake_reply_id: None,
                reply_operation_index: None,
            },
        )
        .unwrap();
    let message_id = message.message.message_id;
    let mut input = task_input(Some(assignee.clone()), "source-task");
    input.source_conversation_id = Some(conversation_id.clone());
    input.source_message_id = Some(message_id.clone());
    let created = db.create_agent_task_at(&owner, input, T0 + 80).unwrap();
    let task_id = created.task.summary.task_id.clone();
    assert_eq!(
        created.task.summary.source_conversation_id.as_deref(),
        Some(conversation_id.as_str())
    );
    assert_eq!(
        created.task.summary.source_message_id.as_deref(),
        Some(message_id.as_str())
    );
    assert_eq!(
        created.task.instruction,
        "Perform bounded durable work without assuming a window or Endpoint."
    );
    assert_ne!(
        created.task.instruction,
        "Explicit source message, not an implicit Task."
    );

    let owner_page = db.list_agent_tasks(&owner, None, 0, 10).unwrap();
    assert_eq!(owner_page.total_count, 1);
    assert_eq!(owner_page.tasks.len(), 1);
    let foreign_page = db.list_agent_tasks(&foreign, None, 0, 10).unwrap();
    assert_eq!(foreign_page.total_count, 0);
    assert!(foreign_page.tasks.is_empty());

    // Give the foreign principal Conversation participation directly. This is
    // test-only setup proving that communication membership does not become
    // AgentTask authority.
    db.conn_for_tests()
        .execute(
            "INSERT INTO wc_conversation_participants (
                participant_id, conversation_id, participant_kind, agent_id,
                principal_kind, principal_digest, joined_at_unix_ms
             ) VALUES (?1, ?2, 'human', NULL, ?3, ?4, ?5)",
            params![
                format!("wc_participant_{}", "e".repeat(32)),
                conversation_id,
                foreign.kind,
                foreign.digest,
                T0 + 81,
            ],
        )
        .unwrap();
    let foreign_error = db.read_agent_task(&foreign, &task_id).unwrap_err();
    let missing_id = format!("{AGENT_TASK_ID_PREFIX}{}", "f".repeat(32));
    let missing_error = db.read_agent_task(&foreign, &missing_id).unwrap_err();
    assert_eq!(foreign_error.code(), "agent_task_not_found");
    assert_eq!(foreign_error.code(), missing_error.code());
    assert_eq!(foreign_error.message(), missing_error.message());

    let started = start(&db, &owner, &task_id, &assignee, "privacy-start", T0 + 82);
    let foreign_attempt = db
        .heartbeat_agent_task_attempt_at(
            &foreign,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            T0 + 83,
        )
        .unwrap_err();
    assert_eq!(foreign_attempt.code(), "agent_task_not_found");
}

#[test]
fn random_fence_and_stale_controller_generation_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-fence.db")).unwrap();
    let owner = principal('e');
    let assignee = agent(&db, &owner, "fence-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "fence-task");
    let started = start(&db, &owner, &task_id, &assignee, "fence-start", T0 + 90);
    let random_fence = format!("{AGENT_TASK_ATTEMPT_FENCE_PREFIX}{}", "0".repeat(32));
    let fence_error = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &random_fence,
            1,
            T0 + 91,
        )
        .unwrap_err();
    assert_eq!(fence_error.code(), "agent_task_attempt_stale");

    let generation_error = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            2,
            T0 + 91,
        )
        .unwrap_err();
    assert_eq!(generation_error.code(), "agent_task_attempt_stale");
}

#[test]
fn replay_record_and_effect_commit_atomically_for_create_start_and_completion() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-atomicity.db")).unwrap();
    let owner = principal('f');
    let assignee = agent(&db, &owner, "atomic-agent");

    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER fail_task_create_replay
             BEFORE INSERT ON wc_communication_idempotency
             WHEN NEW.operation = 'create_agent_task'
             BEGIN SELECT RAISE(ABORT, 'forced task create replay failure'); END;",
        )
        .unwrap();
    let failed_create = db
        .create_agent_task_at(
            &owner,
            task_input(Some(assignee.clone()), "atomic-create"),
            T0 + 100,
        )
        .unwrap_err();
    assert_eq!(failed_create.code(), "communication_store_unavailable");
    assert_eq!(
        db.conn_for_tests()
            .query_row("SELECT COUNT(*) FROM wc_agent_tasks", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.conn_for_tests()
        .execute_batch("DROP TRIGGER fail_task_create_replay;")
        .unwrap();

    let task_id = create_assigned_task(&db, &owner, &assignee, "atomic-task");
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER fail_task_start_replay
             BEFORE INSERT ON wc_communication_idempotency
             WHEN NEW.operation = 'start_agent_task_attempt'
             BEGIN SELECT RAISE(ABORT, 'forced task start replay failure'); END;",
        )
        .unwrap();
    let failed_start = db
        .start_agent_task_attempt_at(&owner, &task_id, &assignee, "atomic-start", T0 + 101)
        .unwrap_err();
    assert_eq!(failed_start.code(), "communication_store_unavailable");
    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_task_attempts WHERE task_id = ?1",
                [task_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        db.read_agent_task(&owner, &task_id).unwrap().summary.state,
        AgentTaskState::Ready
    );
    db.conn_for_tests()
        .execute_batch("DROP TRIGGER fail_task_start_replay;")
        .unwrap();

    let started = start(&db, &owner, &task_id, &assignee, "atomic-start", T0 + 102);
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER fail_task_completion_replay
             BEFORE INSERT ON wc_communication_idempotency
             WHEN NEW.operation = 'complete_agent_task_attempt'
             BEGIN SELECT RAISE(ABORT, 'forced task completion replay failure'); END;",
        )
        .unwrap();
    let failed_completion = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("would have completed"),
            None,
            "atomic-completion",
            T0 + 103,
        )
        .unwrap_err();
    assert_eq!(failed_completion.code(), "communication_store_unavailable");
    let still_active = db.read_agent_task_at(&owner, &task_id, T0 + 103).unwrap();
    assert_eq!(still_active.summary.state, AgentTaskState::Active);
    assert_eq!(
        still_active.summary.latest_attempt.unwrap().state,
        AgentTaskAttemptState::Active
    );
    db.conn_for_tests()
        .execute_batch("DROP TRIGGER fail_task_completion_replay;")
        .unwrap();
    db.complete_agent_task_attempt_at(
        &owner,
        &task_id,
        &started.attempt.attempt_id,
        &assignee,
        &started.attempt_fence,
        1,
        AgentTaskState::Succeeded,
        Some("committed"),
        None,
        "atomic-completion",
        T0 + 104,
    )
    .unwrap();
}

#[test]
fn endpoint_continuation_start_is_endpoint_independent_replay_safe_and_payload_free() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-start.db")).unwrap();
    let owner = principal('e');
    let assignee = agent(&db, &owner, "endpoint-start-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-start-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-start-attempt",
        now,
    );
    let before_messages: i64 = db
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM wc_conversation_messages", [], |row| {
            row.get(0)
        })
        .unwrap();
    let before_deliveries: i64 = db
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM wc_agent_deliveries", [], |row| {
            row.get(0)
        })
        .unwrap();

    let first = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            started.attempt.attempt_controller_generation,
            now + 1,
        )
        .unwrap();
    assert!(first.state_changed);
    assert!(!first.replayed);
    assert_eq!(first.execution.wake_state, AgentWakeState::Pending);
    assert_eq!(first.execution.endpoint_id, None);
    assert_eq!(first.execution.endpoint_controller_generation, None);
    let projected = db.read_agent_task(&owner, &task_id).unwrap();
    assert!(projected.summary.execution_bound);
    assert_eq!(
        projected.summary.execution_kind,
        Some(AgentTaskExecutionKind::AgentEndpoint)
    );
    assert_eq!(
        projected.summary.execution_status,
        Some(AgentTaskExecutionStatus::NotStarted)
    );
    assert_eq!(
        projected.summary.recovery_kind,
        AgentTaskExecutionRecoveryKind::None
    );
    assert_eq!(
        db.conn_for_tests()
            .query_row("SELECT COUNT(*) FROM wc_conversation_messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        before_messages,
        "Task-origin execution must not synthesize a Conversation Message",
    );
    assert_eq!(
        db.conn_for_tests()
            .query_row("SELECT COUNT(*) FROM wc_agent_deliveries", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        before_deliveries,
        "Task-origin execution must not synthesize an Inbox Delivery",
    );

    let replay = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            started.attempt.attempt_controller_generation,
            now + 2,
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(!replay.state_changed);
    assert_eq!(replay.execution.wake_id, first.execution.wake_id);

    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "task-host".to_string(),
                client_attachment_id: Some("task-host-view".to_string()),
                wake_capable: true,
                idempotency_key: "task-host-endpoint".to_string(),
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
            "task_host_adapter",
        )
        .unwrap()
        .unwrap();
    assert_eq!(claim.wake.wake_id, first.execution.wake_id);
    assert_eq!(claim.wake.trigger_kind, "agent_task_attempt");
    assert_eq!(claim.wake.latest_conversation_id, None);
    assert_eq!(claim.wake.queued_delivery_count_snapshot, None);

    let task_after_claim = db.read_agent_task(&owner, &task_id).unwrap();
    let attempt_after_claim = task_after_claim.summary.latest_attempt.unwrap();
    assert_eq!(attempt_after_claim.attempt_controller_generation, 2);

    let replay_after_claim = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 3,
        )
        .unwrap();
    assert!(replay_after_claim.replayed);
    assert_eq!(
        replay_after_claim.execution.wake_state,
        AgentWakeState::Claimed
    );
    assert_eq!(
        replay_after_claim.execution.endpoint_id.as_deref(),
        Some(endpoint.endpoint_id.as_str())
    );
    assert_eq!(
        db.conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM wc_agent_wakes WHERE source_task_attempt_id = ?1",
                [started.attempt.attempt_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "exact start replay must never mint a second Task Wake",
    );
    let prepared = db
        .prepare_agent_wake_dispatch(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            &claim.wake.wake_id,
            &claim.attempt.attempt_id,
            &claim.claim_fence,
            &claim.consume_token,
        )
        .unwrap();
    let heartbeat_pos = prepared
        .envelope
        .resume_hint
        .find("First call heartbeat_agent_task_attempt")
        .expect("Task continuation must require heartbeat before business work");
    let bootstrap_pos = prepared
        .envelope
        .resume_hint
        .find("bootstrap_agent_conversation")
        .expect("Task continuation must still bootstrap the exact Wake");
    assert!(heartbeat_pos < bootstrap_pos);
    assert!(!prepared.envelope.resume_hint.contains("Durable work"));
    assert!(!prepared
        .envelope
        .resume_hint
        .contains("Perform bounded durable work without assuming a window or Endpoint."));
}

#[test]
fn endpoint_continuation_pre_dispatch_carrier_replacement_advances_attempt_generation() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-carrier.db")).unwrap();
    let owner = principal('f');
    let assignee = agent(&db, &owner, "endpoint-carrier-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-carrier-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-carrier-attempt",
        now,
    );
    let execution = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap();
    let endpoint_one = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "task-host-one".to_string(),
                client_attachment_id: None,
                wake_capable: true,
                idempotency_key: "task-endpoint-one".to_string(),
            },
        )
        .unwrap()
        .endpoint;
    let first_claim = db
        .claim_next_agent_wake(
            &owner,
            &assignee,
            &endpoint_one.endpoint_id,
            endpoint_one.controller_generation,
            "task_host_adapter",
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        db.read_agent_task(&owner, &task_id)
            .unwrap()
            .summary
            .latest_attempt
            .unwrap()
            .attempt_controller_generation,
        2
    );

    db.set_agent_endpoint_wake_capability(
        &owner,
        &assignee,
        &endpoint_one.endpoint_id,
        endpoint_one.controller_generation,
        false,
    )
    .unwrap();
    let released = db
        .agent_wake(&execution.execution.wake_id)
        .unwrap()
        .unwrap();
    assert_eq!(released.state, AgentWakeState::Pending);
    let replay_after_loss = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 2,
        )
        .unwrap();
    assert_eq!(replay_after_loss.execution.endpoint_id, None);

    let endpoint_two = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "task-host-two".to_string(),
                client_attachment_id: None,
                wake_capable: true,
                idempotency_key: "task-endpoint-two".to_string(),
            },
        )
        .unwrap()
        .endpoint;
    let second_claim = db
        .claim_next_agent_wake(
            &owner,
            &assignee,
            &endpoint_two.endpoint_id,
            endpoint_two.controller_generation,
            "task_host_adapter",
        )
        .unwrap()
        .unwrap();
    assert_eq!(second_claim.wake.wake_id, first_claim.wake.wake_id);
    assert_eq!(
        db.read_agent_task(&owner, &task_id)
            .unwrap()
            .summary
            .latest_attempt
            .unwrap()
            .attempt_controller_generation,
        3,
        "each new pre-dispatch carrier claim must fence the previous Attempt carrier generation",
    );
}

#[test]
fn endpoint_continuation_post_fence_endpoint_loss_fences_old_attempt_controller() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-post-fence-loss.db")).unwrap();
    let owner = principal('8');
    let assignee = agent(&db, &owner, "endpoint-post-fence-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-post-fence-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-post-fence-attempt",
        now,
    );
    let execution = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap();
    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "post-fence-host".to_string(),
                client_attachment_id: None,
                wake_capable: true,
                idempotency_key: "post-fence-endpoint".to_string(),
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
            "task_host_adapter",
        )
        .unwrap()
        .unwrap();
    db.prepare_agent_wake_dispatch(
        &owner,
        &assignee,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
        &claim.consume_token,
    )
    .unwrap();
    let before_loss = db
        .read_agent_task(&owner, &task_id)
        .unwrap()
        .summary
        .latest_attempt
        .unwrap()
        .attempt_controller_generation;

    db.detach_agent_endpoint(&owner, &endpoint.endpoint_id)
        .unwrap();

    let wake = db
        .agent_wake(&execution.execution.wake_id)
        .unwrap()
        .unwrap();
    assert_eq!(wake.state, AgentWakeState::DeliveryUnknown);
    let task = db.read_agent_task(&owner, &task_id).unwrap();
    let after_loss = task
        .summary
        .latest_attempt
        .unwrap()
        .attempt_controller_generation;
    assert_eq!(after_loss, before_loss + 1);
    let stale = db
        .heartbeat_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            before_loss,
            now + 2,
        )
        .unwrap_err();
    assert_eq!(stale.code(), "agent_task_attempt_stale");
}

#[test]
fn endpoint_continuation_retires_when_attempt_expires_before_host_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-expiry.db")).unwrap();
    let owner = principal('9');
    let assignee = agent(&db, &owner, "endpoint-expiry-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-expiry-task");
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-expiry-attempt",
        T0 + 10,
    );
    let execution = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            T0 + 11,
        )
        .unwrap();
    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "late-task-host".to_string(),
                client_attachment_id: None,
                wake_capable: true,
                idempotency_key: "late-task-endpoint".to_string(),
            },
        )
        .unwrap()
        .endpoint;
    assert!(
        db.claim_next_agent_wake(
            &owner,
            &assignee,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "task_host_adapter",
        )
        .unwrap()
        .is_none(),
        "expired Task Wake must retire before any Host carrier can claim it",
    );
    assert_eq!(
        db.agent_wake(&execution.execution.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Retired
    );
    let task = db.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(task.summary.state, AgentTaskState::Ready);
    assert_eq!(
        task.summary.latest_attempt.as_ref().unwrap().state,
        AgentTaskAttemptState::Expired
    );

    let replay = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            wall_now_ms(),
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.execution.wake_state, AgentWakeState::Retired);

    let second = db
        .start_agent_task_attempt(
            &owner,
            &task_id,
            &assignee,
            "endpoint-expiry-second-attempt",
        )
        .unwrap();
    assert_eq!(second.attempt.attempt_number, 2);
    assert_ne!(second.attempt.attempt_id, started.attempt.attempt_id);
}

#[test]
fn endpoint_continuation_and_coding_run_backends_are_mutually_exclusive() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-backend-exclusion.db")).unwrap();
    let owner = principal('a');
    let assignee = agent(&db, &owner, "backend-exclusion-agent");
    let now = wall_now_ms();

    let endpoint_task = create_assigned_task(&db, &owner, &assignee, "endpoint-first-task");
    let endpoint_attempt = start(
        &db,
        &owner,
        &endpoint_task,
        &assignee,
        "endpoint-first-attempt",
        now,
    );
    db.start_agent_task_endpoint_continuation_at(
        &owner,
        &endpoint_task,
        &endpoint_attempt.attempt.attempt_id,
        &assignee,
        &endpoint_attempt.attempt_fence,
        1,
        now + 1,
    )
    .unwrap();
    let coding_after_endpoint = db
        .agent_task_coding_run_start_context_at(
            &owner,
            "agent:special:reference-only",
            &endpoint_task,
            &endpoint_attempt.attempt.attempt_id,
            &assignee,
            &endpoint_attempt.attempt_fence,
            1,
            now + 2,
        )
        .unwrap_err();
    assert_eq!(
        coding_after_endpoint.code(),
        "agent_task_execution_backend_conflict"
    );

    let coding_task = create_assigned_task(&db, &owner, &assignee, "coding-first-task");
    let coding_attempt = start(
        &db,
        &owner,
        &coding_task,
        &assignee,
        "coding-first-attempt",
        now + 3,
    );
    let intent = coding_binding_intent("backend-exclusion-coding");
    prepare_coding_binding(
        &db,
        &owner,
        &coding_task,
        &assignee,
        &coding_attempt,
        &intent,
        now + 4,
    );
    let endpoint_after_coding = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &coding_task,
            &coding_attempt.attempt.attempt_id,
            &assignee,
            &coding_attempt.attempt_fence,
            1,
            now + 5,
        )
        .unwrap_err();
    assert_eq!(
        endpoint_after_coding.code(),
        "agent_task_execution_backend_conflict"
    );
}

#[test]
fn endpoint_continuation_terminal_completion_retires_pre_dispatch_wake() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-terminal.db")).unwrap();
    let owner = principal('b');
    let assignee = agent(&db, &owner, "endpoint-terminal-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-terminal-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-terminal-attempt",
        now,
    );
    let execution = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap();

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            AgentTaskState::Succeeded,
            Some("completed without Host dispatch"),
            None,
            "endpoint-terminal-completion",
            now + 2,
        )
        .unwrap();
    assert_eq!(completed.task.state, AgentTaskState::Succeeded);
    assert_eq!(completed.attempt.state, AgentTaskAttemptState::Succeeded);
    assert_eq!(
        db.agent_wake(&execution.execution.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Retired
    );
    let replay = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 3,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.execution.wake_state, AgentWakeState::Retired);
}

#[test]
fn endpoint_continuation_consumed_wake_stays_consumed_after_task_completion_but_execution_is_terminal(
) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("agent-task-endpoint-consumed-terminal.db")).unwrap();
    let owner = principal('c');
    let assignee = agent(&db, &owner, "endpoint-consumed-terminal-agent");
    let task_id = create_assigned_task(&db, &owner, &assignee, "endpoint-consumed-terminal-task");
    let now = wall_now_ms();
    let started = start(
        &db,
        &owner,
        &task_id,
        &assignee,
        "endpoint-consumed-terminal-attempt",
        now,
    );
    let execution = db
        .start_agent_task_endpoint_continuation_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            1,
            now + 1,
        )
        .unwrap();
    let endpoint = db
        .attach_agent_endpoint(
            &owner,
            NewAgentEndpoint {
                agent_id: assignee.clone(),
                host: "consumed-task-host".to_string(),
                client_attachment_id: None,
                wake_capable: true,
                idempotency_key: "consumed-task-endpoint".to_string(),
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
            "task_host_adapter",
        )
        .unwrap()
        .unwrap();
    db.prepare_agent_wake_dispatch(
        &owner,
        &assignee,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
        &claim.consume_token,
    )
    .unwrap();
    db.complete_agent_wake_delivery(
        &owner,
        &assignee,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
    )
    .unwrap();
    db.consume_agent_wake(
        &owner,
        &assignee,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &claim.wake.wake_id,
        &claim.consume_token,
    )
    .unwrap();

    let completed = db
        .complete_agent_task_attempt_at(
            &owner,
            &task_id,
            &started.attempt.attempt_id,
            &assignee,
            &started.attempt_fence,
            2,
            AgentTaskState::Succeeded,
            Some("completed after consumed Host continuation"),
            None,
            "endpoint-consumed-terminal-completion",
            now + 2,
        )
        .unwrap();
    assert_eq!(
        completed.task.execution_status,
        Some(AgentTaskExecutionStatus::Terminal)
    );
    assert_eq!(completed.attempt.state, AgentTaskAttemptState::Succeeded);
    assert_eq!(
        db.agent_wake(&execution.execution.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Consumed,
        "Task completion must not rewrite a post-dispatch consumed Wake into pre-dispatch retirement"
    );
    let read = db.read_agent_task(&owner, &task_id).unwrap();
    assert_eq!(read.summary.state, AgentTaskState::Succeeded);
    assert_eq!(
        read.summary.execution_status,
        Some(AgentTaskExecutionStatus::Terminal)
    );
}
