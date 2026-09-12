use super::communication::*;
use super::Database;

fn principal(kind: &str, hex: char) -> CommunicationPrincipal {
    CommunicationPrincipal {
        kind: kind.to_string(),
        digest: format!("wc_commprincipal_{}", hex.to_string().repeat(64)),
    }
}

fn missing_id(prefix: &str, hex: char) -> String {
    format!("{prefix}{}", hex.to_string().repeat(32))
}

fn assert_same_private_not_found(
    foreign: CommunicationStoreError,
    missing: CommunicationStoreError,
    expected_code: &str,
) {
    assert_eq!(foreign.code(), expected_code);
    assert_eq!(missing.code(), expected_code);
    assert_eq!(foreign.message(), missing.message());
    assert_eq!(
        foreign.current_profile_revision(),
        missing.current_profile_revision()
    );
}

fn new_agent(handle: &str, display_name: &str, key: &str) -> NewAgentIdentity {
    NewAgentIdentity {
        handle: handle.to_string(),
        display_name: display_name.to_string(),
        description: format!("{display_name} durable profile"),
        specialty_labels: vec!["architecture".to_string(), "rust".to_string()],
        idempotency_key: key.to_string(),
    }
}

fn endpoint(agent_id: &str, host: &str, key: &str) -> NewAgentEndpoint {
    NewAgentEndpoint {
        agent_id: agent_id.to_string(),
        host: host.to_string(),
        client_attachment_id: Some(format!("attachment-{key}")),
        wake_capable: false,
        idempotency_key: key.to_string(),
    }
}

fn conversation(agent_ids: Vec<String>, key: &str) -> NewConversation {
    NewConversation {
        title: Some("Architecture room".to_string()),
        agent_ids,
        idempotency_key: key.to_string(),
    }
}

fn human_message(
    conversation_id: &str,
    body: &str,
    recipient_agent_ids: Option<Vec<String>>,
    reply_to: Option<String>,
    key: &str,
) -> NewConversationMessage {
    NewConversationMessage {
        conversation_id: conversation_id.to_string(),
        body: body.to_string(),
        author_agent_id: None,
        endpoint_id: None,
        expected_controller_generation: None,
        recipient_agent_ids,
        reply_to,
        idempotency_key: Some(key.to_string()),
        wake_reply_id: None,
        reply_operation_index: None,
    }
}

fn agent_message(
    conversation_id: &str,
    agent_id: &str,
    endpoint_id: &str,
    expected_controller_generation: i64,
    body: &str,
    recipient_agent_ids: Option<Vec<String>>,
    reply_to: Option<String>,
    key: &str,
) -> NewConversationMessage {
    NewConversationMessage {
        conversation_id: conversation_id.to_string(),
        body: body.to_string(),
        author_agent_id: Some(agent_id.to_string()),
        endpoint_id: Some(endpoint_id.to_string()),
        expected_controller_generation: Some(expected_controller_generation),
        recipient_agent_ids,
        reply_to,
        idempotency_key: Some(key.to_string()),
        wake_reply_id: None,
        reply_operation_index: None,
    }
}

#[test]
fn communication_schema_migrates_existing_endpoint_table_with_recovery_fingerprint() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("communication-schema-migration.db");
    let db = Database::open(&path).unwrap();
    db.conn_for_tests()
        .execute(
            "ALTER TABLE wc_agent_endpoints DROP COLUMN mcp_app_recovery_fingerprint",
            [],
        )
        .unwrap();
    drop(db);

    let reopened = Database::open(&path).unwrap();
    let column_count: i64 = reopened
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('wc_agent_endpoints')
             WHERE name = 'mcp_app_recovery_fingerprint'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(column_count, 1);
}

#[test]
fn durable_agent_identity_profile_collision_owner_and_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("communication.db");
    let owner = principal("user", 'a');
    let other = principal("user", 'b');
    let db = Database::open(&path).unwrap();

    let first = db
        .create_agent_identity(&owner, new_agent("reviewer", "Reviewer", "agent-a"))
        .unwrap();
    assert!(first.created && first.state_changed && !first.replayed);
    assert!(first.agent.agent_id.starts_with(DURABLE_AGENT_ID_PREFIX));
    assert_eq!(first.agent.profile_revision, 1);

    let replay = db
        .create_agent_identity(&owner, new_agent("reviewer", "Reviewer", "agent-a"))
        .unwrap();
    assert!(replay.replayed && !replay.created && !replay.state_changed);
    assert_eq!(replay.agent.agent_id, first.agent.agent_id);

    let conflict = db
        .create_agent_identity(&owner, new_agent("builder", "Builder", "agent-a"))
        .unwrap_err();
    assert_eq!(conflict.code(), "communication_idempotency_conflict");

    // Self-description collisions never define canonical identity.
    let second = db
        .create_agent_identity(&owner, new_agent("reviewer", "Reviewer", "agent-b"))
        .unwrap();
    assert_ne!(second.agent.agent_id, first.agent.agent_id);
    assert_eq!(second.agent.handle, first.agent.handle);
    assert_eq!(second.agent.display_name, first.agent.display_name);

    let updated = db
        .update_agent_identity(
            &owner,
            &first.agent.agent_id,
            1,
            AgentProfilePatch {
                display_name: Some("Architecture Reviewer".to_string()),
                description: Some("Reviews durable boundaries".to_string()),
                specialty_labels: Some(vec!["review".to_string(), "sqlite".to_string()]),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap();
    assert!(updated.state_changed);
    assert_eq!(updated.agent.agent_id, first.agent.agent_id);
    assert_eq!(updated.agent.profile_revision, 2);
    assert_eq!(updated.agent.display_name, "Architecture Reviewer");

    assert_eq!(
        db.update_agent_identity(
            &owner,
            &first.agent.agent_id,
            1,
            AgentProfilePatch {
                description: Some("stale update".to_string()),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap_err()
        .code(),
        "agent_profile_changed"
    );
    assert_eq!(
        db.update_agent_identity(
            &other,
            &first.agent.agent_id,
            2,
            AgentProfilePatch {
                description: Some("not allowed".to_string()),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap_err()
        .code(),
        "agent_not_found"
    );

    assert_eq!(
        db.list_agent_identities(&owner, None, 0, 10)
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(
        db.list_agent_identities(&other, None, 0, 10)
            .unwrap()
            .total_count,
        0
    );
    assert!(db
        .list_agent_identities(&other, Some(&first.agent.agent_id), 0, 10)
        .unwrap()
        .agents
        .is_empty());

    drop(db);
    let reopened = Database::open(&path).unwrap();
    let durable = reopened
        .list_agent_identities(&owner, Some(&first.agent.agent_id), 0, 10)
        .unwrap()
        .agents
        .pop()
        .unwrap();
    assert_eq!(durable.agent_id, first.agent.agent_id);
    assert_eq!(durable.profile_revision, 2);
    assert_eq!(durable.display_name, "Architecture Reviewer");
}

#[test]
fn endpoint_attachment_is_principal_bound_and_detach_preserves_agent() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("endpoint.db")).unwrap();
    let owner = principal("shared-key", 'c');
    let other = principal("shared-key", 'd');
    let agent = db
        .create_agent_identity(&owner, new_agent("worker", "Worker", "agent"))
        .unwrap()
        .agent;

    assert_eq!(
        db.attach_agent_endpoint(&other, endpoint(&agent.agent_id, "ChatGPT", "wrong"))
            .unwrap_err()
            .code(),
        "agent_not_found"
    );
    let attached = db
        .attach_agent_endpoint(&owner, endpoint(&agent.agent_id, "ChatGPT", "window-a"))
        .unwrap();
    assert!(attached.created && attached.state_changed && !attached.replayed);
    assert!(attached
        .endpoint
        .endpoint_id
        .starts_with(AGENT_ENDPOINT_ID_PREFIX));
    assert_eq!(attached.endpoint.controller_generation, 1);
    assert_eq!(
        attached.endpoint.lifecycle,
        AgentEndpointLifecycle::Attached
    );
    assert_eq!(
        serde_json::to_value(&attached.endpoint).unwrap()["lifecycle"],
        "attached"
    );
    assert!(attached.endpoint.lease_expires_at_unix_ms > attached.endpoint.attached_at_unix_ms);

    let replay = db
        .attach_agent_endpoint(&owner, endpoint(&agent.agent_id, "ChatGPT", "window-a"))
        .unwrap();
    assert!(replay.replayed && !replay.state_changed);
    assert_eq!(replay.endpoint.endpoint_id, attached.endpoint.endpoint_id);

    assert_eq!(
        db.detach_agent_endpoint(&other, &attached.endpoint.endpoint_id)
            .unwrap_err()
            .code(),
        "endpoint_not_found"
    );
    let detached = db
        .detach_agent_endpoint(&owner, &attached.endpoint.endpoint_id)
        .unwrap();
    assert!(detached.state_changed);
    assert_eq!(
        detached.endpoint.lifecycle,
        AgentEndpointLifecycle::Detached
    );
    assert!(detached.endpoint.detached_at_unix_ms.is_some());
    let desired_state_retry = db
        .detach_agent_endpoint(&owner, &attached.endpoint.endpoint_id)
        .unwrap();
    assert!(!desired_state_retry.state_changed);

    let after_detach = db
        .list_agent_identities(&owner, Some(&agent.agent_id), 0, 10)
        .unwrap()
        .agents
        .pop()
        .unwrap();
    assert_eq!(after_detach.agent_id, agent.agent_id);
    assert_eq!(after_detach.active_endpoint_count, 0);

    let replacement = db
        .attach_agent_endpoint(&owner, endpoint(&agent.agent_id, "ChatGPT", "window-b"))
        .unwrap();
    assert_ne!(
        replacement.endpoint.endpoint_id,
        attached.endpoint.endpoint_id
    );
    assert_eq!(replacement.endpoint.agent_id, agent.agent_id);
    assert_eq!(replacement.endpoint.controller_generation, 2);
    assert_eq!(
        replacement.endpoint.lifecycle,
        AgentEndpointLifecycle::Attached
    );
    let after_replacement = db
        .list_agent_identities(&owner, Some(&agent.agent_id), 0, 10)
        .unwrap()
        .agents
        .pop()
        .unwrap();
    assert_eq!(after_replacement.current_controller_generation, 2);
    assert_eq!(after_replacement.active_endpoint_count, 1);
}

#[test]
fn corrupt_endpoint_lifecycle_fails_closed_in_authority_load_path() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("corrupt-endpoint.db")).unwrap();
    let owner = principal("user", '7');
    let agent = db
        .create_agent_identity(&owner, new_agent("corrupt", "Corrupt", "agent"))
        .unwrap()
        .agent;
    let attached = db
        .attach_agent_endpoint(&owner, endpoint(&agent.agent_id, "ChatGPT", "endpoint"))
        .unwrap()
        .endpoint;
    let conn = db.conn_for_tests();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    conn.execute(
        "UPDATE wc_agent_endpoints SET lifecycle = 'future_state' WHERE endpoint_id = ?1",
        [&attached.endpoint_id],
    )
    .unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
        .unwrap();
    drop(conn);

    assert!(db
        .detach_agent_endpoint(&owner, &attached.endpoint_id)
        .is_err());
}

#[test]
fn conversation_transcript_delivery_replay_offline_and_restart_are_durable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("conversation.db");
    let owner = principal("user", 'e');
    let other = principal("user", 'f');
    let db = Database::open(&path).unwrap();
    let agent_a = db
        .create_agent_identity(&owner, new_agent("agent-a", "Agent A", "agent-a"))
        .unwrap()
        .agent;
    let agent_b = db
        .create_agent_identity(&owner, new_agent("agent-b", "Agent B", "agent-b"))
        .unwrap()
        .agent;

    assert_eq!(
        db.create_conversation(
            &other,
            conversation(vec![agent_a.agent_id.clone()], "foreign-room")
        )
        .unwrap_err()
        .code(),
        "agent_not_found"
    );

    let created = db
        .create_conversation(
            &owner,
            conversation(
                vec![agent_b.agent_id.clone(), agent_a.agent_id.clone()],
                "room",
            ),
        )
        .unwrap();
    assert!(created.created && created.state_changed && !created.replayed);
    assert!(created
        .conversation
        .conversation
        .conversation_id
        .starts_with(CONVERSATION_ID_PREFIX));
    assert_eq!(created.conversation.participants.len(), 3);
    assert_eq!(
        created.conversation.conversation.lifecycle,
        ConversationLifecycle::Open
    );
    assert_eq!(
        serde_json::to_value(&created.conversation.conversation).unwrap()["lifecycle"],
        "open"
    );
    let conversation_id = created.conversation.conversation.conversation_id.clone();

    let room_replay = db
        .create_conversation(
            &owner,
            conversation(
                vec![agent_a.agent_id.clone(), agent_b.agent_id.clone()],
                "room",
            ),
        )
        .unwrap();
    assert!(room_replay.replayed && !room_replay.state_changed);
    assert_eq!(
        room_replay.conversation.conversation.conversation_id,
        conversation_id
    );

    let human = db
        .post_conversation_message(
            &owner,
            human_message(&conversation_id, "Human to both Agents", None, None, "m1"),
        )
        .unwrap();
    assert_eq!(human.message.seq, 1);
    assert_eq!(human.message.author.participant_kind, "human");
    assert_eq!(human.message.deliveries.len(), 2);
    assert!(human
        .message
        .deliveries
        .iter()
        .all(|delivery| delivery.state == MessageDeliveryState::Queued));
    assert_eq!(
        serde_json::to_value(&human.message.deliveries[0]).unwrap()["state"],
        "queued"
    );

    let exact_retry = db
        .post_conversation_message(
            &owner,
            human_message(&conversation_id, "Human to both Agents", None, None, "m1"),
        )
        .unwrap();
    assert!(exact_retry.replayed && !exact_retry.state_changed);
    assert_eq!(exact_retry.message.message_id, human.message.message_id);
    assert_eq!(
        db.post_conversation_message(
            &owner,
            human_message(&conversation_id, "Changed payload", None, None, "m1"),
        )
        .unwrap_err()
        .code(),
        "communication_idempotency_conflict"
    );

    let endpoint_a = db
        .attach_agent_endpoint(&owner, endpoint(&agent_a.agent_id, "ChatGPT", "endpoint-a"))
        .unwrap()
        .endpoint;
    let endpoint_b = db
        .attach_agent_endpoint(&owner, endpoint(&agent_b.agent_id, "ChatGPT", "endpoint-b"))
        .unwrap()
        .endpoint;

    let from_a = db
        .post_conversation_message(
            &owner,
            agent_message(
                &conversation_id,
                &agent_a.agent_id,
                &endpoint_a.endpoint_id,
                endpoint_a.controller_generation,
                "Agent A to Agent B",
                Some(vec![agent_b.agent_id.clone()]),
                Some(human.message.message_id.clone()),
                "m2",
            ),
        )
        .unwrap();
    assert_eq!(from_a.message.seq, 2);
    assert_eq!(
        from_a.message.author.agent_id.as_deref(),
        Some(agent_a.agent_id.as_str())
    );
    assert_eq!(from_a.message.deliveries.len(), 1);

    let from_b_to_room = db
        .post_conversation_message(
            &owner,
            agent_message(
                &conversation_id,
                &agent_b.agent_id,
                &endpoint_b.endpoint_id,
                endpoint_b.controller_generation,
                "Agent B to Human / room",
                Some(Vec::new()),
                Some(from_a.message.message_id.clone()),
                "m3",
            ),
        )
        .unwrap();
    assert_eq!(from_b_to_room.message.seq, 3);
    assert!(from_b_to_room.message.deliveries.is_empty());

    let transcript = db
        .read_conversation(&owner, &ConversationAccess::Human, &conversation_id, 0, 10)
        .unwrap();
    assert_eq!(
        transcript
            .messages
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(transcript.conversation.message_count, 3);
    assert_eq!(transcript.conversation.last_seq, 3);

    let inbox_a = db
        .list_agent_inbox(
            &owner,
            &agent_a.agent_id,
            &endpoint_a.endpoint_id,
            endpoint_a.controller_generation,
            0,
            10,
        )
        .unwrap();
    assert_eq!(inbox_a.total_queued_count, 1);
    assert_eq!(
        inbox_a.deliveries[0].message.message_id,
        human.message.message_id
    );

    let inbox_b = db
        .list_agent_inbox(
            &owner,
            &agent_b.agent_id,
            &endpoint_b.endpoint_id,
            endpoint_b.controller_generation,
            0,
            10,
        )
        .unwrap();
    assert_eq!(inbox_b.total_queued_count, 2);
    assert_eq!(
        inbox_b
            .deliveries
            .iter()
            .map(|item| item.message.seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(inbox_b.deliveries[0].message.deliveries[0].delivery_order > 0);
    assert!(inbox_b
        .deliveries
        .iter()
        .all(|item| item.state == MessageDeliveryState::Queued));

    let b_delivery_ids = inbox_b
        .deliveries
        .iter()
        .map(|item| item.delivery_id.clone())
        .collect::<Vec<_>>();
    let mut expected_b_delivery_ids = b_delivery_ids.clone();
    expected_b_delivery_ids.sort();
    let consumed = db
        .consume_agent_deliveries(
            &owner,
            &agent_b.agent_id,
            &endpoint_b.endpoint_id,
            endpoint_b.controller_generation,
            b_delivery_ids.clone(),
        )
        .unwrap();
    assert!(consumed.state_changed);
    assert_eq!(consumed.consumed_delivery_ids, expected_b_delivery_ids);
    let consumed_transcript = db
        .read_conversation(&owner, &ConversationAccess::Human, &conversation_id, 0, 10)
        .unwrap();
    assert!(consumed_transcript.messages.iter().any(|message| {
        message.deliveries.iter().any(|delivery| {
            delivery.recipient_agent_id == agent_b.agent_id
                && delivery.state == MessageDeliveryState::Consumed
        })
    }));
    let consume_retry = db
        .consume_agent_deliveries(
            &owner,
            &agent_b.agent_id,
            &endpoint_b.endpoint_id,
            endpoint_b.controller_generation,
            b_delivery_ids.clone(),
        )
        .unwrap();
    assert!(!consume_retry.state_changed);
    assert_eq!(
        consume_retry.already_consumed_delivery_ids,
        expected_b_delivery_ids
    );
    assert_eq!(
        db.list_agent_inbox(
            &owner,
            &agent_a.agent_id,
            &endpoint_a.endpoint_id,
            endpoint_a.controller_generation,
            0,
            10,
        )
        .unwrap()
        .total_queued_count,
        1,
        "Agent B consumption must not mutate Agent A delivery state"
    );

    db.detach_agent_endpoint(&owner, &endpoint_b.endpoint_id)
        .unwrap();
    let offline = db
        .post_conversation_message(
            &owner,
            human_message(
                &conversation_id,
                "Queued while Agent B is offline",
                Some(vec![agent_b.agent_id.clone()]),
                None,
                "m4",
            ),
        )
        .unwrap();
    assert_eq!(offline.message.seq, 4);
    assert_eq!(offline.message.deliveries.len(), 1);

    let replacement_b = db
        .attach_agent_endpoint(
            &owner,
            endpoint(&agent_b.agent_id, "ChatGPT", "endpoint-b2"),
        )
        .unwrap()
        .endpoint;
    let recovered_inbox = db
        .list_agent_inbox(
            &owner,
            &agent_b.agent_id,
            &replacement_b.endpoint_id,
            replacement_b.controller_generation,
            0,
            10,
        )
        .unwrap();
    assert_eq!(recovered_inbox.total_queued_count, 1);
    assert_eq!(
        recovered_inbox.deliveries[0].message.message_id,
        offline.message.message_id
    );

    let other_conversation = db
        .create_conversation(
            &owner,
            conversation(vec![agent_a.agent_id.clone()], "other-room"),
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    assert_eq!(
        db.post_conversation_message(
            &owner,
            human_message(
                &other_conversation,
                "Invalid cross-room reply",
                None,
                Some(human.message.message_id.clone()),
                "cross-reply",
            ),
        )
        .unwrap_err()
        .code(),
        "reply_message_not_found"
    );

    drop(db);
    let reopened = Database::open(&path).unwrap();
    let durable = reopened
        .read_conversation(&owner, &ConversationAccess::Human, &conversation_id, 0, 10)
        .unwrap();
    assert_eq!(durable.conversation.message_count, 4);
    assert_eq!(
        durable.messages.last().unwrap().message_id,
        offline.message.message_id
    );
    let agent_view = reopened
        .read_conversation(
            &owner,
            &ConversationAccess::Agent {
                agent_id: agent_b.agent_id.clone(),
                endpoint_id: replacement_b.endpoint_id.clone(),
                expected_controller_generation: replacement_b.controller_generation,
            },
            &conversation_id,
            0,
            10,
        )
        .unwrap();
    assert_eq!(agent_view.conversation.queued_delivery_count, Some(1));
}

#[test]
fn exact_message_replay_survives_endpoint_detach_without_duplicate_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("detached-replay.db")).unwrap();
    let owner = principal("user", '8');
    let agent_a = db
        .create_agent_identity(&owner, new_agent("author", "Author", "author"))
        .unwrap()
        .agent;
    let agent_b = db
        .create_agent_identity(&owner, new_agent("recipient", "Recipient", "recipient"))
        .unwrap()
        .agent;
    let conversation_id = db
        .create_conversation(
            &owner,
            conversation(
                vec![agent_a.agent_id.clone(), agent_b.agent_id.clone()],
                "room",
            ),
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    let endpoint_a = db
        .attach_agent_endpoint(
            &owner,
            endpoint(&agent_a.agent_id, "ChatGPT", "author-endpoint"),
        )
        .unwrap()
        .endpoint;

    let first = db
        .post_conversation_message(
            &owner,
            agent_message(
                &conversation_id,
                &agent_a.agent_id,
                &endpoint_a.endpoint_id,
                endpoint_a.controller_generation,
                "Committed before the response was lost",
                Some(vec![agent_b.agent_id.clone()]),
                None,
                "detached-replay-message",
            ),
        )
        .unwrap();
    assert_eq!(first.message.seq, 1);
    assert_eq!(first.message.deliveries.len(), 1);

    db.detach_agent_endpoint(&owner, &endpoint_a.endpoint_id)
        .unwrap();
    let replay = db
        .post_conversation_message(
            &owner,
            agent_message(
                &conversation_id,
                &agent_a.agent_id,
                &endpoint_a.endpoint_id,
                endpoint_a.controller_generation,
                "Committed before the response was lost",
                Some(vec![agent_b.agent_id.clone()]),
                None,
                "detached-replay-message",
            ),
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(!replay.state_changed);
    assert_eq!(replay.message.message_id, first.message.message_id);
    assert_eq!(replay.message.deliveries, first.message.deliveries);

    assert_eq!(
        db.post_conversation_message(
            &owner,
            agent_message(
                &conversation_id,
                &agent_a.agent_id,
                &endpoint_a.endpoint_id,
                endpoint_a.controller_generation,
                "Changed request must not bypass detached Endpoint validation",
                Some(vec![agent_b.agent_id.clone()]),
                None,
                "detached-replay-message",
            ),
        )
        .unwrap_err()
        .code(),
        "communication_idempotency_conflict"
    );

    let transcript = db
        .read_conversation(&owner, &ConversationAccess::Human, &conversation_id, 0, 10)
        .unwrap();
    assert_eq!(transcript.conversation.message_count, 1);
    assert_eq!(transcript.messages.len(), 1);
    assert_eq!(transcript.messages[0].message_id, first.message.message_id);
    let recipient = db
        .list_agent_identities(&owner, Some(&agent_b.agent_id), 0, 10)
        .unwrap()
        .agents
        .pop()
        .unwrap();
    assert_eq!(recipient.active_endpoint_count, 0);
    assert_eq!(recipient.queued_delivery_count, 1);
}

#[test]
fn message_deliveries_and_wake_commit_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("atomic.db")).unwrap();
    let owner = principal("user", '9');
    let agent = db
        .create_agent_identity(&owner, new_agent("atomic", "Atomic", "agent"))
        .unwrap()
        .agent;
    let conversation_id = db
        .create_conversation(&owner, conversation(vec![agent.agent_id.clone()], "room"))
        .unwrap()
        .conversation
        .conversation
        .conversation_id;

    let idempotency_count_before: i64 = db
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM wc_communication_idempotency",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER fail_wake_insert
             BEFORE INSERT ON wc_agent_wakes
             BEGIN SELECT RAISE(ABORT, 'forced wake failure'); END;",
        )
        .unwrap();
    assert_eq!(
        db.post_conversation_message(
            &owner,
            human_message(&conversation_id, "must rollback", None, None, "message"),
        )
        .unwrap_err()
        .code(),
        "communication_store_unavailable"
    );
    {
        let conn = db.conn_for_tests();
        let message_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM wc_conversation_messages", [], |row| {
                row.get(0)
            })
            .unwrap();
        let delivery_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM wc_agent_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        let wake_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM wc_agent_wakes", [], |row| row.get(0))
            .unwrap();
        let idempotency_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wc_communication_idempotency",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let next_seq: i64 = conn
            .query_row(
                "SELECT next_seq FROM wc_conversations WHERE conversation_id = ?1",
                [&conversation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (message_count, delivery_count, wake_count, next_seq),
            (0, 0, 0, 1)
        );
        assert_eq!(idempotency_count, idempotency_count_before);
    }
    db.conn_for_tests()
        .execute_batch("DROP TRIGGER fail_wake_insert;")
        .unwrap();

    let retry = db
        .post_conversation_message(
            &owner,
            human_message(&conversation_id, "must rollback", None, None, "message"),
        )
        .unwrap();
    assert_eq!(retry.message.seq, 1);
    assert_eq!(retry.message.deliveries.len(), 1);
    let conn = db.conn_for_tests();
    let wake_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM wc_agent_wakes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(wake_count, 1);
}

#[test]
fn foreign_exact_communication_resources_match_missing_ids() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(&temp.path().join("principal-privacy.db")).unwrap();
    let alice = principal("user", '1');
    let bob = principal("user", '2');

    let alice_agent = db
        .create_agent_identity(
            &alice,
            new_agent("alice-agent", "Alice Agent", "alice-agent"),
        )
        .unwrap()
        .agent;
    let alice_endpoint = db
        .attach_agent_endpoint(
            &alice,
            endpoint(&alice_agent.agent_id, "alice-host", "alice-endpoint"),
        )
        .unwrap()
        .endpoint;
    let alice_conversation = db
        .create_conversation(
            &alice,
            conversation(vec![alice_agent.agent_id.clone()], "alice-conversation"),
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    let alice_seed = db
        .post_conversation_message(
            &alice,
            human_message(
                &alice_conversation,
                "Alice seed message",
                None,
                None,
                "alice-seed-message",
            ),
        )
        .unwrap();
    let alice_delivery_id = alice_seed.message.deliveries[0].delivery_id.clone();
    let alice_other_agent = db
        .create_agent_identity(
            &alice,
            new_agent("alice-other", "Alice Other Agent", "alice-other-agent"),
        )
        .unwrap()
        .agent;
    let alice_other_conversation = db
        .create_conversation(
            &alice,
            conversation(
                vec![alice_other_agent.agent_id.clone()],
                "alice-other-conversation",
            ),
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    let alice_other_delivery_id = db
        .post_conversation_message(
            &alice,
            human_message(
                &alice_other_conversation,
                "Same principal, different Agent Inbox",
                None,
                None,
                "alice-other-seed-message",
            ),
        )
        .unwrap()
        .message
        .deliveries[0]
        .delivery_id
        .clone();

    let bob_agent = db
        .create_agent_identity(&bob, new_agent("bob-agent", "Bob Agent", "bob-agent"))
        .unwrap()
        .agent;
    let bob_endpoint = db
        .attach_agent_endpoint(
            &bob,
            endpoint(&bob_agent.agent_id, "bob-private-host", "bob-endpoint"),
        )
        .unwrap()
        .endpoint;
    let bob_conversation = db
        .create_conversation(
            &bob,
            conversation(vec![bob_agent.agent_id.clone()], "bob-conversation"),
        )
        .unwrap()
        .conversation
        .conversation
        .conversation_id;
    let bob_seed = db
        .post_conversation_message(
            &bob,
            human_message(
                &bob_conversation,
                "Bob private message",
                None,
                None,
                "bob-seed-message",
            ),
        )
        .unwrap();
    let bob_message_id = bob_seed.message.message_id.clone();
    let bob_delivery_id = bob_seed.message.deliveries[0].delivery_id.clone();

    let missing_agent = missing_id(DURABLE_AGENT_ID_PREFIX, '9');
    let missing_endpoint = missing_id(AGENT_ENDPOINT_ID_PREFIX, '8');
    let missing_conversation = missing_id(CONVERSATION_ID_PREFIX, '7');
    let missing_message = missing_id(CONVERSATION_MESSAGE_ID_PREFIX, '6');
    let missing_delivery = missing_id(AGENT_DELIVERY_ID_PREFIX, '5');

    assert_same_private_not_found(
        db.update_agent_identity(
            &alice,
            &bob_agent.agent_id,
            1,
            AgentProfilePatch {
                description: Some("must stay private".to_string()),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap_err(),
        db.update_agent_identity(
            &alice,
            &missing_agent,
            1,
            AgentProfilePatch {
                description: Some("must stay private".to_string()),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap_err(),
        "agent_not_found",
    );

    assert_same_private_not_found(
        db.attach_agent_endpoint(
            &alice,
            endpoint(&bob_agent.agent_id, "alice-host", "foreign-agent-attach"),
        )
        .unwrap_err(),
        db.attach_agent_endpoint(
            &alice,
            endpoint(&missing_agent, "alice-host", "missing-agent-attach"),
        )
        .unwrap_err(),
        "agent_not_found",
    );

    assert_same_private_not_found(
        db.create_conversation(
            &alice,
            conversation(
                vec![bob_agent.agent_id.clone()],
                "foreign-agent-conversation",
            ),
        )
        .unwrap_err(),
        db.create_conversation(
            &alice,
            conversation(vec![missing_agent.clone()], "missing-agent-conversation"),
        )
        .unwrap_err(),
        "agent_not_found",
    );

    assert_same_private_not_found(
        db.detach_agent_endpoint(&alice, &bob_endpoint.endpoint_id)
            .unwrap_err(),
        db.detach_agent_endpoint(&alice, &missing_endpoint)
            .unwrap_err(),
        "endpoint_not_found",
    );

    assert_same_private_not_found(
        db.list_conversations(
            &alice,
            &ConversationAccess::Agent {
                agent_id: alice_agent.agent_id.clone(),
                endpoint_id: bob_endpoint.endpoint_id.clone(),
                expected_controller_generation: bob_endpoint.controller_generation,
            },
            0,
            10,
        )
        .unwrap_err(),
        db.list_conversations(
            &alice,
            &ConversationAccess::Agent {
                agent_id: alice_agent.agent_id.clone(),
                endpoint_id: missing_endpoint.clone(),
                expected_controller_generation: bob_endpoint.controller_generation,
            },
            0,
            10,
        )
        .unwrap_err(),
        "endpoint_not_found",
    );

    assert_same_private_not_found(
        db.list_agent_inbox(
            &alice,
            &alice_agent.agent_id,
            &bob_endpoint.endpoint_id,
            bob_endpoint.controller_generation,
            0,
            10,
        )
        .unwrap_err(),
        db.list_agent_inbox(
            &alice,
            &alice_agent.agent_id,
            &missing_endpoint,
            bob_endpoint.controller_generation,
            0,
            10,
        )
        .unwrap_err(),
        "endpoint_not_found",
    );

    assert_same_private_not_found(
        db.read_conversation(&alice, &ConversationAccess::Human, &bob_conversation, 0, 10)
            .unwrap_err(),
        db.read_conversation(
            &alice,
            &ConversationAccess::Human,
            &missing_conversation,
            0,
            10,
        )
        .unwrap_err(),
        "conversation_not_found",
    );

    assert_same_private_not_found(
        db.read_conversation(
            &alice,
            &ConversationAccess::Agent {
                agent_id: alice_agent.agent_id.clone(),
                endpoint_id: alice_endpoint.endpoint_id.clone(),
                expected_controller_generation: alice_endpoint.controller_generation,
            },
            &bob_conversation,
            0,
            10,
        )
        .unwrap_err(),
        db.read_conversation(
            &alice,
            &ConversationAccess::Agent {
                agent_id: alice_agent.agent_id.clone(),
                endpoint_id: alice_endpoint.endpoint_id.clone(),
                expected_controller_generation: alice_endpoint.controller_generation,
            },
            &missing_conversation,
            0,
            10,
        )
        .unwrap_err(),
        "conversation_not_found",
    );

    assert_same_private_not_found(
        db.post_conversation_message(
            &alice,
            human_message(
                &bob_conversation,
                "Alice must not learn whether Bob room is open",
                None,
                None,
                "foreign-open-conversation",
            ),
        )
        .unwrap_err(),
        db.post_conversation_message(
            &alice,
            human_message(
                &missing_conversation,
                "Alice must not learn whether Bob room is open",
                None,
                None,
                "missing-open-conversation",
            ),
        )
        .unwrap_err(),
        "conversation_not_found",
    );

    db.conn_for_tests()
        .execute(
            "UPDATE wc_conversations SET lifecycle = 'closed' WHERE conversation_id = ?1",
            [&bob_conversation],
        )
        .unwrap();
    assert_same_private_not_found(
        db.post_conversation_message(
            &alice,
            human_message(
                &bob_conversation,
                "Alice must not learn that Bob room is closed",
                None,
                None,
                "foreign-closed-conversation",
            ),
        )
        .unwrap_err(),
        db.post_conversation_message(
            &alice,
            human_message(
                &missing_conversation,
                "Alice must not learn that Bob room is closed",
                None,
                None,
                "missing-closed-conversation",
            ),
        )
        .unwrap_err(),
        "conversation_not_found",
    );
    assert_eq!(
        db.post_conversation_message(
            &bob,
            human_message(
                &bob_conversation,
                "Bob can still observe own closed state",
                None,
                None,
                "bob-authorized-closed",
            ),
        )
        .unwrap_err()
        .code(),
        "conversation_closed"
    );

    assert_same_private_not_found(
        db.post_conversation_message(
            &alice,
            human_message(
                &alice_conversation,
                "Cross-room reply must not prove Bob message exists",
                None,
                Some(bob_message_id),
                "foreign-reply-target",
            ),
        )
        .unwrap_err(),
        db.post_conversation_message(
            &alice,
            human_message(
                &alice_conversation,
                "Cross-room reply must not prove Bob message exists",
                None,
                Some(missing_message),
                "missing-reply-target",
            ),
        )
        .unwrap_err(),
        "reply_message_not_found",
    );

    assert_same_private_not_found(
        db.consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![bob_delivery_id],
        )
        .unwrap_err(),
        db.consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![missing_delivery.clone()],
        )
        .unwrap_err(),
        "delivery_not_found",
    );

    assert_same_private_not_found(
        db.consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![alice_other_delivery_id],
        )
        .unwrap_err(),
        db.consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![missing_delivery],
        )
        .unwrap_err(),
        "delivery_not_found",
    );

    let first_consume = db
        .consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![alice_delivery_id.clone()],
        )
        .unwrap();
    assert!(first_consume.state_changed);
    assert_eq!(
        first_consume.consumed_delivery_ids,
        vec![alice_delivery_id.clone()]
    );
    let consume_retry = db
        .consume_agent_deliveries(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            vec![alice_delivery_id.clone()],
        )
        .unwrap();
    assert!(!consume_retry.state_changed);
    assert_eq!(
        consume_retry.already_consumed_delivery_ids,
        vec![alice_delivery_id]
    );

    assert_eq!(
        db.update_agent_identity(
            &alice,
            &alice_agent.agent_id,
            2,
            AgentProfilePatch {
                description: Some("stale revision remains diagnosable".to_string()),
                ..AgentProfilePatch::default()
            },
        )
        .unwrap_err()
        .code(),
        "agent_profile_changed"
    );

    db.detach_agent_endpoint(&alice, &alice_endpoint.endpoint_id)
        .unwrap();
    assert_eq!(
        db.list_agent_inbox(
            &alice,
            &alice_agent.agent_id,
            &alice_endpoint.endpoint_id,
            alice_endpoint.controller_generation,
            0,
            10,
        )
        .unwrap_err()
        .code(),
        "endpoint_detached"
    );
}
