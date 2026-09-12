use super::{
    AgentWakeEnvelope, AgentWakeState, CommunicationPrincipal, Database, NewAgentEndpoint,
    NewAgentIdentity, NewConversation, NewConversationMessage,
};
use crate::agent_wake::{
    dispatch_next_agent_wake, AgentWakeDispatchReport, ContinuationAdapter,
    ContinuationDispatchOutcome, ContinuationPreflight, ContinuationPreflightError,
};
use std::sync::Mutex;

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
) -> super::AgentEndpointRecord {
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

#[derive(Debug)]
struct FakeContinuationAdapter {
    preflight_error: Option<&'static str>,
    outcome: ContinuationDispatchOutcome,
    preflights: Mutex<Vec<ContinuationPreflight>>,
    envelopes: Mutex<Vec<AgentWakeEnvelope>>,
}

impl FakeContinuationAdapter {
    fn new(preflight_error: Option<&'static str>, outcome: ContinuationDispatchOutcome) -> Self {
        Self {
            preflight_error,
            outcome,
            preflights: Mutex::new(Vec::new()),
            envelopes: Mutex::new(Vec::new()),
        }
    }

    fn dispatch_count(&self) -> usize {
        self.envelopes.lock().unwrap().len()
    }

    fn latest_envelope(&self) -> AgentWakeEnvelope {
        self.envelopes.lock().unwrap().last().unwrap().clone()
    }
}

impl ContinuationAdapter for FakeContinuationAdapter {
    fn adapter_kind(&self) -> &'static str {
        "deterministic_fake"
    }

    fn preflight(
        &self,
        continuation: &ContinuationPreflight,
    ) -> Result<(), ContinuationPreflightError> {
        self.preflights.lock().unwrap().push(continuation.clone());
        match self.preflight_error {
            Some(kind) => Err(ContinuationPreflightError::new(kind)),
            None => Ok(()),
        }
    }

    fn dispatch(&self, envelope: &AgentWakeEnvelope) -> ContinuationDispatchOutcome {
        self.envelopes.lock().unwrap().push(envelope.clone());
        self.outcome
    }
}

#[test]
fn fake_adapter_recovers_same_wake_before_fence_and_exact_consume_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adapter.db");
    let db = Database::open(&path).unwrap();
    let fixture = create_fixture(&db, '3');
    let secret_body = "secret-message-body-must-not-enter-wake-payload";
    post_to_receiver(&db, &fixture, secret_body, "secret-message-idempotency-key");
    let logical_wake_id = wake_id_for(&db, &fixture.receiver_agent_id);
    let generation_one = attach_wake_endpoint(&db, &fixture, "host-one", "adapter-one");

    let rejected = FakeContinuationAdapter::new(
        Some("host_bridge_unavailable"),
        ContinuationDispatchOutcome::Delivered,
    );
    let report = dispatch_next_agent_wake(
        &db,
        &fixture.owner,
        &fixture.receiver_agent_id,
        &generation_one.endpoint_id,
        generation_one.controller_generation,
        &rejected,
    )
    .unwrap();
    match report {
        AgentWakeDispatchReport::ReleasedBeforeDispatch {
            wake,
            adapter_error_kind,
        } => {
            assert_eq!(wake.wake_id, logical_wake_id);
            assert_eq!(wake.state, AgentWakeState::Pending);
            assert_eq!(adapter_error_kind, "host_bridge_unavailable");
        }
        other => panic!("unexpected pre-dispatch report: {other:?}"),
    }
    assert_eq!(rejected.dispatch_count(), 0);

    let generation_two = attach_wake_endpoint(&db, &fixture, "host-two", "adapter-two");
    let delivered = FakeContinuationAdapter::new(None, ContinuationDispatchOutcome::Delivered);
    let report = dispatch_next_agent_wake(
        &db,
        &fixture.owner,
        &fixture.receiver_agent_id,
        &generation_two.endpoint_id,
        generation_two.controller_generation,
        &delivered,
    )
    .unwrap();
    match report {
        AgentWakeDispatchReport::Delivered { wake } => {
            assert_eq!(wake.wake_id, logical_wake_id);
            assert_eq!(wake.state, AgentWakeState::Delivered);
        }
        other => panic!("unexpected delivery report: {other:?}"),
    }
    let envelope = delivered.latest_envelope();
    assert_eq!(envelope.wake_id, logical_wake_id);
    assert_eq!(envelope.agent_id, fixture.receiver_agent_id);
    assert_eq!(envelope.endpoint_id, generation_two.endpoint_id);
    assert_eq!(envelope.controller_generation, 2);
    assert!(envelope
        .resume_hint
        .contains("read the authoritative Agent Inbox"));
    assert!(envelope
        .resume_hint
        .contains("authoritative current work for this resumed turn"));
    assert!(envelope
        .resume_hint
        .contains("user-visible final response reflect the actual work/result"));
    assert!(envelope
        .resume_hint
        .contains("Do not merely repeat this continuation contract"));
    assert!(!envelope.resume_hint.contains(secret_body));
    assert!(!envelope
        .resume_hint
        .contains("receiver durable description"));
    assert!(!envelope.resume_hint.contains("receiver-specialty"));
    assert!(!envelope
        .resume_hint
        .contains("secret-message-idempotency-key"));
    assert!(!envelope.resume_hint.contains(&fixture.owner.digest));

    let first_consume = db
        .consume_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            &logical_wake_id,
            &envelope.consume_token,
        )
        .unwrap();
    assert!(first_consume.state_changed);
    assert!(!first_consume.already_consumed);
    let retry_consume = db
        .consume_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            &logical_wake_id,
            &envelope.consume_token,
        )
        .unwrap();
    assert!(!retry_consume.state_changed);
    assert!(retry_consume.already_consumed);
    assert_eq!(
        retry_consume.consumed_at_unix_ms,
        first_consume.consumed_at_unix_ms
    );
    assert_eq!(
        queued_delivery_ids(&db, &fixture.receiver_agent_id).len(),
        1
    );

    drop(db);
    let reopened = Database::open(&path).unwrap();
    let after_restart = reopened
        .consume_agent_wake(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            &logical_wake_id,
            &envelope.consume_token,
        )
        .unwrap();
    assert!(after_restart.already_consumed);
    assert!(!after_restart.state_changed);

    let generation_three = attach_wake_endpoint(&reopened, &fixture, "host-three", "adapter-three");
    assert_eq!(generation_three.controller_generation, 3);
    assert_eq!(
        reopened
            .consume_agent_wake(
                &fixture.owner,
                &fixture.receiver_agent_id,
                &generation_two.endpoint_id,
                generation_two.controller_generation,
                &logical_wake_id,
                &envelope.consume_token,
            )
            .unwrap_err()
            .code(),
        "endpoint_expired"
    );

    let delivery_ids = queued_delivery_ids(&reopened, &fixture.receiver_agent_id);
    reopened
        .consume_agent_deliveries(
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_three.endpoint_id,
            generation_three.controller_generation,
            delivery_ids,
        )
        .unwrap();
    assert_eq!(
        reopened
            .agent_wake(&logical_wake_id)
            .unwrap()
            .unwrap()
            .state,
        AgentWakeState::Consumed,
        "Inbox consumption must not rewrite consumed Wake state"
    );
}

#[test]
fn dispatch_uncertainty_blocks_duplicate_and_preserves_successor_across_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("unknown.db");
    let db = Database::open(&path).unwrap();
    let fixture = create_fixture(&db, '4');
    post_to_receiver(&db, &fixture, "first durable work", "unknown-one");
    let first_wake_id = wake_id_for(&db, &fixture.receiver_agent_id);
    let generation_one =
        attach_wake_endpoint(&db, &fixture, "unknown-host-one", "unknown-endpoint-one");
    let unknown = FakeContinuationAdapter::new(None, ContinuationDispatchOutcome::OutcomeUnknown);
    match dispatch_next_agent_wake(
        &db,
        &fixture.owner,
        &fixture.receiver_agent_id,
        &generation_one.endpoint_id,
        generation_one.controller_generation,
        &unknown,
    )
    .unwrap()
    {
        AgentWakeDispatchReport::DeliveryUnknown { wake } => {
            assert_eq!(wake.wake_id, first_wake_id);
            assert_eq!(wake.state, AgentWakeState::DeliveryUnknown);
        }
        other => panic!("unexpected unknown report: {other:?}"),
    }
    assert_eq!(unknown.dispatch_count(), 1);

    post_to_receiver(&db, &fixture, "successor durable work", "unknown-two");
    let wake_states: Vec<(String, String)> = {
        let conn = db.conn_for_tests();
        let mut statement = conn
            .prepare(
                "SELECT wake_id, state FROM wc_agent_wakes
                 WHERE target_agent_id = ?1 ORDER BY created_at_unix_ms, wake_id",
            )
            .unwrap();
        statement
            .query_map([&fixture.receiver_agent_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(wake_states.len(), 2);
    assert!(wake_states
        .iter()
        .any(|(wake_id, state)| wake_id == &first_wake_id && state == "delivery_unknown"));
    assert!(wake_states.iter().any(|(_, state)| state == "pending"));
    assert_eq!(
        queued_delivery_ids(&db, &fixture.receiver_agent_id).len(),
        2
    );

    assert!(matches!(
        dispatch_next_agent_wake(
            &db,
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_one.endpoint_id,
            generation_one.controller_generation,
            &unknown,
        )
        .unwrap(),
        AgentWakeDispatchReport::NoPendingWake
    ));
    assert_eq!(
        unknown.dispatch_count(),
        1,
        "unknown dispatch must not be retried"
    );

    drop(db);
    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened.agent_wake(&first_wake_id).unwrap().unwrap().state,
        AgentWakeState::DeliveryUnknown
    );
    let generation_two = attach_wake_endpoint(
        &reopened,
        &fixture,
        "unknown-host-two",
        "unknown-endpoint-two",
    );
    let replacement_adapter =
        FakeContinuationAdapter::new(None, ContinuationDispatchOutcome::Delivered);
    assert!(matches!(
        dispatch_next_agent_wake(
            &reopened,
            &fixture.owner,
            &fixture.receiver_agent_id,
            &generation_two.endpoint_id,
            generation_two.controller_generation,
            &replacement_adapter,
        )
        .unwrap(),
        AgentWakeDispatchReport::NoPendingWake
    ));
    assert_eq!(replacement_adapter.dispatch_count(), 0);
    assert_eq!(
        reopened.agent_wake_attempts(&first_wake_id).unwrap().len(),
        1
    );
}
