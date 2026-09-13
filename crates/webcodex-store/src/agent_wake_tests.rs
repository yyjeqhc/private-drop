use super::agent_wake::{AgentWakeAttemptState, AgentWakeState};
use super::communication::{
    CommunicationPrincipal, ConversationAccess, NewAgentEndpoint, NewAgentIdentity,
    NewConversation, NewConversationMessage,
};
use super::Database;
use rusqlite::params;

#[derive(Clone)]
struct Fixture {
    owner: CommunicationPrincipal,
    #[allow(dead_code)]
    sender_agent_id: String,
    receiver_agent_id: String,
    conversation_id: String,
}

fn principal(hex: char) -> CommunicationPrincipal {
    CommunicationPrincipal {
        kind: "user".to_string(),
        digest: format!("wc_commprincipal_{}", hex.to_string().repeat(64)),
    }
}

fn create_fixture(db: &Database, hex: char) -> Fixture {
    let owner = principal(hex);
    let sender = db
        .create_agent_identity(
            &owner,
            NewAgentIdentity {
                handle: "wake-sender".to_string(),
                display_name: "Wake Sender".to_string(),
                description: "sender durable description".to_string(),
                specialty_labels: vec!["sender-specialty".to_string()],
                idempotency_key: "wake-sender-agent".to_string(),
            },
        )
        .unwrap()
        .agent;
    let receiver = db
        .create_agent_identity(
            &owner,
            NewAgentIdentity {
                handle: "wake-receiver".to_string(),
                display_name: "Wake Receiver".to_string(),
                description: "receiver durable description".to_string(),
                specialty_labels: vec!["receiver-specialty".to_string()],
                idempotency_key: "wake-receiver-agent".to_string(),
            },
        )
        .unwrap()
        .agent;
    let conversation_id = db
        .create_conversation(
            &owner,
            NewConversation {
                title: Some("Wake architecture room".to_string()),
                agent_ids: vec![sender.agent_id.clone(), receiver.agent_id.clone()],
                idempotency_key: "wake-conversation".to_string(),
            },
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    Fixture {
        owner,
        sender_agent_id: sender.agent_id,
        receiver_agent_id: receiver.agent_id,
        conversation_id,
    }
}

fn post_to_receiver(db: &Database, fixture: &Fixture, body: &str, key: &str) -> String {
    db.post_conversation_message(
        &fixture.owner,
        NewConversationMessage {
            conversation_id: fixture.conversation_id.clone(),
            body: body.to_string(),
            author_agent_id: None,
            endpoint_id: None,
            expected_controller_generation: None,
            recipient_agent_ids: Some(vec![fixture.receiver_agent_id.clone()]),
            reply_to: None,
            idempotency_key: Some(key.to_string()),
            wake_reply_id: None,
            reply_operation_index: None,
        },
    )
    .unwrap()
    .message
    .message_id
}

fn attach_wake_endpoint(
    db: &Database,
    fixture: &Fixture,
    host: &str,
    key: &str,
) -> super::communication::AgentEndpointRecord {
    db.attach_agent_endpoint(
        &fixture.owner,
        NewAgentEndpoint {
            agent_id: fixture.receiver_agent_id.clone(),
            host: host.to_string(),
            client_attachment_id: Some(format!("attachment-{key}")),
            wake_capable: true,
            idempotency_key: key.to_string(),
        },
    )
    .unwrap()
    .endpoint
}

fn wake_id_for(db: &Database, agent_id: &str) -> String {
    db.conn_for_tests()
        .query_row(
            "SELECT wake_id FROM wc_agent_wakes
             WHERE target_agent_id = ?1
             ORDER BY created_at_unix_ms, wake_id LIMIT 1",
            [agent_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn queued_delivery_ids(db: &Database, agent_id: &str) -> Vec<String> {
    let conn = db.conn_for_tests();
    let mut statement = conn
        .prepare(
            "SELECT delivery_id FROM wc_agent_deliveries
             WHERE recipient_agent_id = ?1 AND state = 'queued'
             ORDER BY delivery_order",
        )
        .unwrap();
    statement
        .query_map([agent_id], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

#[test]
fn withdrawing_wake_capability_reconciles_exact_endpoint_attempts() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("wake-capability-withdrawal.db")).unwrap();
    let fixture = create_fixture(&db, 'c');
    post_to_receiver(&db, &fixture, "withdraw capability", "withdraw-message");
    let endpoint = attach_wake_endpoint(&db, &fixture, "host", "withdraw-endpoint");
    let wake_id = wake_id_for(&db, &fixture.receiver_agent_id);

    let first_claimed = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "host_adapter",
        )
        .unwrap()
        .unwrap();
    let withdrawn = db
        .set_agent_endpoint_wake_capability(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            false,
        )
        .unwrap();
    assert!(!withdrawn.wake_capable);
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Pending
    );
    assert_eq!(
        db.agent_wake_attempts(&wake_id)
            .unwrap()
            .into_iter()
            .find(|attempt| attempt.attempt_id == first_claimed.attempt.attempt_id)
            .unwrap()
            .state,
        AgentWakeAttemptState::Revoked
    );

    db.set_agent_endpoint_wake_capability(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        true,
    )
    .unwrap();
    let claimed = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "host_adapter",
        )
        .unwrap()
        .unwrap();
    db.prepare_agent_wake_dispatch(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &wake_id,
        &claimed.attempt.attempt_id,
        &claimed.claim_fence,
        &claimed.consume_token,
    )
    .unwrap();
    db.set_agent_endpoint_wake_capability(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        false,
    )
    .unwrap();
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::DeliveryUnknown
    );
    assert_eq!(
        db.agent_wake_attempts(&wake_id)
            .unwrap()
            .into_iter()
            .find(|attempt| attempt.attempt_id == claimed.attempt.attempt_id)
            .unwrap()
            .state,
        AgentWakeAttemptState::DeliveryUnknown
    );
}

#[test]
fn offline_fifty_message_burst_preserves_facts_and_coalesces_wake() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("coalescing.db")).unwrap();
    let fixture = create_fixture(&db, '1');

    for index in 0..50 {
        post_to_receiver(
            &db,
            &fixture,
            &format!("offline durable message {index}"),
            &format!("burst-message-{index}"),
        );
    }

    let (message_count, delivery_count, wake_count): (i64, i64, i64) = {
        let conn = db.conn_for_tests();
        (
            conn.query_row("SELECT COUNT(*) FROM wc_conversation_messages", [], |row| {
                row.get(0)
            })
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_agent_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap(),
            conn.query_row("SELECT COUNT(*) FROM wc_agent_wakes", [], |row| row.get(0))
                .unwrap(),
        )
    };
    assert_eq!((message_count, delivery_count, wake_count), (50, 50, 1));

    let wake_id = wake_id_for(&db, &fixture.receiver_agent_id);
    let wake = db.agent_wake(&wake_id).unwrap().unwrap();
    assert_eq!(wake.state, AgentWakeState::Pending);
    assert_eq!(wake.queued_delivery_count_snapshot, Some(50));
    assert!(wake.inbox_high_watermark.is_some_and(|value| value >= 50));
    assert_ne!(
        wake.first_triggering_delivery_id,
        wake.latest_triggering_delivery_id
    );

    let agent = db
        .list_agent_identities(&fixture.owner, Some(&fixture.receiver_agent_id), 0, 10)
        .unwrap()
        .agents
        .pop()
        .unwrap();
    assert_eq!(agent.active_endpoint_count, 0);
    assert_eq!(agent.queued_delivery_count, 50);
    assert_eq!(agent.unresolved_wake_count, 1);
    assert_eq!(agent.latest_wake_id.as_deref(), Some(wake_id.as_str()));
    assert_eq!(agent.latest_wake_state.as_deref(), Some("pending"));

    let endpoint = attach_wake_endpoint(&db, &fixture, "fake-host", "burst-endpoint");
    let inbox = db
        .list_agent_inbox(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            0,
            100,
        )
        .unwrap();
    assert_eq!(inbox.total_queued_count, 50);
    assert_eq!(inbox.deliveries.len(), 50);

    let first_delivery_id = inbox.deliveries[0].delivery_id.clone();
    db.consume_agent_deliveries(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        vec![first_delivery_id],
    )
    .unwrap();
    assert_eq!(
        db.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Pending,
        "Delivery consume must not consume or rewrite the logical Wake"
    );
    let transcript = db
        .read_conversation(
            &fixture.owner,
            &ConversationAccess::Human,
            &fixture.conversation_id,
            0,
            100,
        )
        .unwrap();
    assert_eq!(transcript.messages.len(), 50);
    assert_eq!(transcript.conversation.message_count, 50);
}

#[test]
fn replacement_generation_fences_old_claim_dispatch_wake_consume_and_inbox_consume() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("generation.db")).unwrap();
    let fixture = create_fixture(&db, '2');
    post_to_receiver(
        &db,
        &fixture,
        "generation-fenced work",
        "generation-message",
    );
    let delivery_id = queued_delivery_ids(&db, &fixture.receiver_agent_id)
        .pop()
        .unwrap();

    let generation_one = attach_wake_endpoint(&db, &fixture, "host-one", "endpoint-one");
    assert_eq!(generation_one.controller_generation, 1);
    let claim_one = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            "deterministic_fake",
        )
        .unwrap()
        .unwrap();

    let generation_two = attach_wake_endpoint(&db, &fixture, "host-two", "endpoint-two");
    assert_eq!(generation_two.controller_generation, 2);
    let lifecycle: String = db
        .conn_for_tests()
        .query_row(
            "SELECT lifecycle FROM wc_agent_endpoints WHERE endpoint_id = ?1",
            [&generation_one.endpoint_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lifecycle, "expired");
    assert_eq!(
        db.agent_wake(&claim_one.wake.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Pending
    );
    assert_eq!(
        db.agent_wake_attempts(&claim_one.wake.wake_id).unwrap()[0].state,
        AgentWakeAttemptState::Revoked
    );

    let stale_claim = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            "deterministic_fake",
        )
        .unwrap_err();
    assert_eq!(stale_claim.code(), "endpoint_expired");
    assert_eq!(
        db.prepare_agent_wake_dispatch(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            &claim_one.wake.wake_id,
            &claim_one.attempt.attempt_id,
            &claim_one.claim_fence,
            &claim_one.consume_token,
        )
        .unwrap_err()
        .code(),
        "endpoint_expired"
    );
    assert_eq!(
        db.consume_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            &claim_one.wake.wake_id,
            &claim_one.consume_token,
        )
        .unwrap_err()
        .code(),
        "endpoint_expired"
    );
    assert_eq!(
        db.consume_agent_deliveries(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            vec![delivery_id],
        )
        .unwrap_err()
        .code(),
        "endpoint_expired"
    );

    let claim_two = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            "deterministic_fake",
        )
        .unwrap()
        .unwrap();
    assert_eq!(claim_two.wake.wake_id, claim_one.wake.wake_id);
    db.prepare_agent_wake_dispatch(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &generation_two.endpoint_id,
        generation_two.controller_generation,
        &claim_two.wake.wake_id,
        &claim_two.attempt.attempt_id,
        &claim_two.claim_fence,
        &claim_two.consume_token,
    )
    .unwrap();
    let generation_three = attach_wake_endpoint(&db, &fixture, "host-three", "endpoint-three");
    assert_eq!(generation_three.controller_generation, 3);
    assert_eq!(
        db.verify_agent_wake_dispatch_binding(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            &claim_two.wake.wake_id,
            &claim_two.attempt.attempt_id,
            &claim_two.claim_fence,
        )
        .unwrap_err()
        .code(),
        "endpoint_expired",
        "a callback reaching the controller after replacement is fenced before Host invocation"
    );
    assert_eq!(
        db.agent_wake(&claim_two.wake.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::DeliveryUnknown
    );
}

#[test]
fn generic_reopen_preserves_live_pre_dispatch_claim_without_takeover() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("offline-restart.db");
    let db = Database::open(&path).unwrap();
    let fixture = create_fixture(&db, '5');
    post_to_receiver(
        &db,
        &fixture,
        "offline before attachment",
        "offline-message",
    );
    let wake_id = wake_id_for(&db, &fixture.receiver_agent_id);
    let endpoint = attach_wake_endpoint(&db, &fixture, "restart-host", "restart-endpoint");
    let first_claim = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "deterministic_fake",
        )
        .unwrap()
        .unwrap();
    assert_eq!(first_claim.wake.wake_id, wake_id);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        queued_delivery_ids(&reopened, &fixture.receiver_agent_id).len(),
        1
    );
    assert_eq!(
        reopened.agent_wake(&wake_id).unwrap().unwrap().state,
        AgentWakeState::Claimed,
        "opening the database must not assert that a live Wake owner died"
    );
    let attempts = reopened.agent_wake_attempts(&wake_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, AgentWakeAttemptState::Claimed);
    assert_eq!(attempts[0].attempt_id, first_claim.attempt.attempt_id);
}

#[test]
fn generic_reopen_preserves_prepared_dispatch_without_inventing_takeover() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("prepared-restart.db");
    let db = Database::open(&path).unwrap();
    let fixture = create_fixture(&db, '6');
    post_to_receiver(&db, &fixture, "prepared before restart", "prepared-message");
    let endpoint = attach_wake_endpoint(&db, &fixture, "prepared-host", "prepared-endpoint");
    let claim = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "deterministic_fake",
        )
        .unwrap()
        .unwrap();
    db.prepare_agent_wake_dispatch(
        &fixture.owner,
        &fixture.receiver_agent_id,
        &endpoint.endpoint_id,
        endpoint.controller_generation,
        &claim.wake.wake_id,
        &claim.attempt.attempt_id,
        &claim.claim_fence,
        &claim.consume_token,
    )
    .unwrap();

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened
            .agent_wake(&claim.wake.wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Prepared
    );
    assert_eq!(
        reopened.agent_wake_attempts(&claim.wake.wake_id).unwrap()[0].state,
        AgentWakeAttemptState::Prepared
    );
    assert!(
        reopened
            .claim_next_agent_wake(
                &fixture.owner,
                &fixture.receiver_agent_id,
                &endpoint.endpoint_id,
                endpoint.controller_generation,
                "deterministic_fake",
            )
            .unwrap()
            .is_none(),
        "a prepared Wake remains fenced until explicit authoritative recovery"
    );
}

#[test]
fn wake_storage_keeps_stable_refs_and_hashes_without_communication_payload() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("privacy.db")).unwrap();
    let fixture = create_fixture(&db, '7');
    let body = "private-body-not-a-wake-fact";
    post_to_receiver(&db, &fixture, body, "private-idempotency-key");
    let endpoint = attach_wake_endpoint(&db, &fixture, "privacy-host", "privacy-endpoint");
    let claim = db
        .claim_next_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &endpoint.endpoint_id,
            endpoint.controller_generation,
            "deterministic_fake",
        )
        .unwrap()
        .unwrap();

    let columns: Vec<String> = {
        let conn = db.conn_for_tests();
        let mut statement = conn.prepare("PRAGMA table_info(wc_agent_wakes)").unwrap();
        statement
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    for forbidden in [
        "body",
        "description",
        "specialty_labels",
        "principal_digest",
        "idempotency_key",
        "wake_payload",
        "credential",
    ] {
        assert!(!columns.iter().any(|column| column == forbidden));
    }

    let (claim_fence_hash, consume_token_hash): (String, String) = db
        .conn_for_tests()
        .query_row(
            "SELECT claim_fence_hash, consume_token_hash
             FROM wc_agent_wake_attempts WHERE attempt_id = ?1",
            params![claim.attempt.attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_ne!(claim_fence_hash, claim.claim_fence);
    assert_ne!(consume_token_hash, claim.consume_token);
    assert!(!claim_fence_hash.contains(&claim.claim_fence));
    assert!(!consume_token_hash.contains(&claim.consume_token));
    assert_eq!(claim.wake.trigger_kind, "inbox_changed");
    assert_eq!(claim.wake.state.as_str(), "claimed");
    assert!(!format!("{:?}", claim.wake).contains(body));
}

#[test]
fn a4b_wake_schema_migration_preserves_inbox_wake_and_rebuilds_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("wake-a4b-migration.db");
    let db = Database::open(&path).unwrap();
    let fixture = create_fixture(&db, 'e');
    post_to_receiver(
        &db,
        &fixture,
        "persist across migration",
        "migration-message",
    );
    let wake_id = wake_id_for(&db, &fixture.receiver_agent_id);

    {
        let conn = db.conn_for_tests();
        conn.execute_batch(
            "
            PRAGMA foreign_keys = OFF;
            DROP TABLE wc_agent_task_endpoint_executions;
            DROP INDEX IF EXISTS idx_wc_agent_wakes_target_state;
            DROP INDEX IF EXISTS idx_wc_agent_wakes_one_queueable_inbox;
            DROP INDEX IF EXISTS idx_wc_agent_wakes_task_attempt;
            DROP INDEX IF EXISTS idx_wc_agent_wakes_one_dispatched;
            DROP INDEX IF EXISTS idx_wc_agent_wake_attempts_wake;
            DROP INDEX IF EXISTS idx_wc_agent_wake_attempts_endpoint;
            ALTER TABLE wc_agent_wake_attempts RENAME TO wc_agent_wake_attempts_current;
            ALTER TABLE wc_agent_wakes RENAME TO wc_agent_wakes_current;

            CREATE TABLE wc_agent_wakes (
                wake_id TEXT PRIMARY KEY,
                target_agent_id TEXT NOT NULL,
                trigger_kind TEXT NOT NULL,
                first_triggering_delivery_id TEXT NOT NULL,
                latest_triggering_delivery_id TEXT NOT NULL,
                latest_conversation_id TEXT NOT NULL,
                latest_message_id TEXT NOT NULL,
                inbox_high_watermark INTEGER NOT NULL,
                queued_delivery_count_snapshot INTEGER NOT NULL,
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
            INSERT INTO wc_agent_wakes (
                wake_id, target_agent_id, trigger_kind,
                first_triggering_delivery_id, latest_triggering_delivery_id,
                latest_conversation_id, latest_message_id,
                inbox_high_watermark, queued_delivery_count_snapshot,
                state, revision, created_at_unix_ms, updated_at_unix_ms,
                claimed_attempt_id, claimed_endpoint_id,
                claimed_controller_generation, claim_lease_expires_at_unix_ms,
                consumed_at_unix_ms, consumed_by_endpoint_id,
                consumed_controller_generation
            )
            SELECT wake_id, target_agent_id, trigger_kind,
                   first_triggering_delivery_id, latest_triggering_delivery_id,
                   latest_conversation_id, latest_message_id,
                   inbox_high_watermark, queued_delivery_count_snapshot,
                   state, revision, created_at_unix_ms, updated_at_unix_ms,
                   claimed_attempt_id, claimed_endpoint_id,
                   claimed_controller_generation, claim_lease_expires_at_unix_ms,
                   consumed_at_unix_ms, consumed_by_endpoint_id,
                   consumed_controller_generation
            FROM wc_agent_wakes_current;

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
            );
            INSERT INTO wc_agent_wake_attempts
                SELECT * FROM wc_agent_wake_attempts_current;
            DROP TABLE wc_agent_wake_attempts_current;
            DROP TABLE wc_agent_wakes_current;

            CREATE INDEX idx_wc_agent_wakes_target_state
                ON wc_agent_wakes(target_agent_id, state, created_at_unix_ms, wake_id);
            CREATE UNIQUE INDEX idx_wc_agent_wakes_one_queueable
                ON wc_agent_wakes(target_agent_id)
                WHERE state IN ('pending', 'claimed');
            CREATE UNIQUE INDEX idx_wc_agent_wakes_one_dispatched
                ON wc_agent_wakes(target_agent_id)
                WHERE state IN ('prepared', 'delivered', 'delivery_unknown');
            CREATE INDEX idx_wc_agent_wake_attempts_wake
                ON wc_agent_wake_attempts(wake_id, claimed_at_unix_ms, attempt_id);
            CREATE INDEX idx_wc_agent_wake_attempts_endpoint
                ON wc_agent_wake_attempts(endpoint_id, controller_generation, state);
            PRAGMA foreign_keys = ON;
            ",
        )
        .unwrap();
    }
    drop(db);

    let reopened = Database::open(&path).unwrap();
    let wake = reopened.agent_wake(&wake_id).unwrap().unwrap();
    assert_eq!(wake.trigger_kind, "inbox_changed");
    assert_eq!(wake.state, AgentWakeState::Pending);
    assert_eq!(wake.source_task_id, None);
    assert_eq!(wake.source_task_attempt_id, None);
    assert_eq!(
        wake.latest_conversation_id.as_deref(),
        Some(fixture.conversation_id.as_str())
    );

    let indexes: Vec<String> = {
        let conn = reopened.conn_for_tests();
        let mut statement = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index'
                   AND tbl_name IN ('wc_agent_wakes', 'wc_agent_wake_attempts')
                 ORDER BY name",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    for expected in [
        "idx_wc_agent_wakes_target_state",
        "idx_wc_agent_wakes_one_queueable_inbox",
        "idx_wc_agent_wakes_task_attempt",
        "idx_wc_agent_wakes_one_dispatched",
        "idx_wc_agent_wake_attempts_wake",
        "idx_wc_agent_wake_attempts_endpoint",
    ] {
        assert!(
            indexes.iter().any(|name| name == expected),
            "migration must rebuild {expected}: {indexes:?}"
        );
    }
    assert!(
        !indexes
            .iter()
            .any(|name| name == "idx_wc_agent_wakes_one_queueable"),
        "legacy queueable index must be replaced by the inbox-only index"
    );

    post_to_receiver(
        &reopened,
        &fixture,
        "coalesce after migration",
        "migration-message-2",
    );
    assert_eq!(
        wake_id_for(&reopened, &fixture.receiver_agent_id),
        wake_id,
        "existing inbox Wake must remain the coalescing target after migration"
    );
}
