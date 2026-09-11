use crate::model::SESSION_LEDGER_VERSION;
use crate::{
    ListSessionMessagesFilter, PostSessionMessageInput, SessionCloseError, SessionGuards,
    SessionLifecycle, SessionMessageError, SessionMessageKind, SessionMessagePriority,
    SessionPathHint, SessionStore, SessionToolContract, SessionTransport, ToolCallRecorderMetadata,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use webcodex_core::workflow_session_contract::SessionMode;

fn persistent_store(path: PathBuf) -> SessionStore {
    SessionStore::with_persistence(path, 10, 10)
}

fn flush_and_restore(store: &SessionStore, path: PathBuf) -> SessionStore {
    store.flush_persistence();
    SessionStore::with_persistence(path, 10, 10)
}

fn post_message(store: &SessionStore, session_id: &str, kind: SessionMessageKind, message: &str) {
    store
        .post_message(PostSessionMessageInput {
            session_id: session_id.to_string(),
            kind,
            message: message.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
}

fn session_tool_contract(tool_name: &str) -> SessionToolContract {
    let (read_like, write_like, shell_like, path_hint) = match tool_name {
        "read_file" => (true, false, false, SessionPathHint::SinglePath),
        "session_summary" => (true, false, false, SessionPathHint::None),
        "run_shell" => (false, true, true, SessionPathHint::None),
        "write_project_file" | "post_session_message" | "memory_set" | "close_session" => {
            (false, true, false, SessionPathHint::None)
        }
        other => panic!("unexpected synthetic lifecycle tool contract: {other}"),
    };
    SessionToolContract {
        risk_class: if write_like { "write" } else { "read" },
        read_like,
        write_like,
        shell_like,
        git_like: false,
        change_summary_like: false,
        project_write: write_like,
        path_hint,
        accepts_context_ack: false,
        advances_context_checkpoint: false,
    }
}

#[test]
fn ledger_round_trip_preserves_session_state_events_and_messages() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 50);
    let session = store.start_session_with_guards(
        Some("proj".to_string()),
        Some("persist".to_string()),
        SessionMode::ReadOnly,
        SessionGuards::default(),
    );
    let start = store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "proj", "path": "src/lib.rs"}),
            session_tool_contract("read_file"),
        )
        .unwrap();
    store.record_tool_call_finished(Some(start), true, &json!({}), None, None);
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Guidance,
        "keep OpenAPI operation count stable",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "checkpoint",
    );

    store.flush_persistence();
    let restored = SessionStore::with_persistence(&ledger, 10, 50);
    let summary = restored.summary(&session.session_id, Some(20)).unwrap();
    assert_eq!(summary.project.as_deref(), Some("proj"));
    assert_eq!(summary.title.as_deref(), Some("persist"));
    assert_eq!(summary.mode, SessionMode::ReadOnly);
    assert!(summary.guards.deny_write_tools);
    assert!(summary.guards.deny_shell_tools);
    assert_eq!(summary.lifecycle, SessionLifecycle::Active);
    assert_eq!(summary.counts.tool_calls, 1);
    assert_eq!(summary.counts.succeeded, 1);
    assert!(summary
        .events
        .iter()
        .any(|event| event.kind == "tool_call_finished" && event.tool_name == "read_file"));

    let messages = restored
        .list_messages(&session.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message, "checkpoint");
    assert_eq!(messages[1].kind, SessionMessageKind::Guidance);
    let discussion = restored
        .discussion_summary(&session.session_id, Some(10))
        .unwrap();
    assert_eq!(discussion.counts.total, 2);
    assert_eq!(discussion.counts.guidance, 1);
    assert_eq!(discussion.counts.progress, 1);
}

// --- Workflow Session lifecycle: explicit Active/Closed authority ---

#[test]
fn new_session_defaults_lifecycle_to_active() {
    let store = SessionStore::default();
    let summary = store.start_session(None, Some("lifecycle".to_string()));
    assert_eq!(summary.lifecycle, SessionLifecycle::Active);

    // Summary JSON exposes lifecycle for observability.
    let value = serde_json::to_value(&summary).unwrap();
    assert_eq!(value["lifecycle"], "active");
}

#[test]
fn persisted_ledger_writes_and_reads_lifecycle_active() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(Some("proj".to_string()), Some("with lifecycle".to_string()));
    assert_eq!(session.lifecycle, SessionLifecycle::Active);

    store.flush_persistence();
    let raw = std::fs::read_to_string(&ledger).unwrap();
    let ledger_value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(ledger_value["version"], SESSION_LEDGER_VERSION);
    assert_eq!(ledger_value["sessions"][0]["lifecycle"], "active");
    assert_eq!(
        ledger_value["sessions"][0]["session_id"].as_str().unwrap(),
        session.session_id
    );

    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    assert_eq!(summary.lifecycle, SessionLifecycle::Active);
}

#[test]
fn pre_current_ledger_version_is_rejected_without_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    let store = persistent_store(ledger_path.clone());
    let session = store.start_session(Some("proj".to_string()), Some("current".to_string()));
    store.flush_persistence();
    drop(store);

    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    ledger["version"] = json!(1);
    std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();
    let before = std::fs::read(&ledger_path).unwrap();

    let restored = persistent_store(ledger_path.clone());
    assert_eq!(restored.status().restored_sessions, 0);
    assert!(restored.summary(&session.session_id, None).is_none());
    assert!(restored
        .status()
        .last_persist_error
        .as_deref()
        .is_some_and(|error| error.contains("unsupported session ledger version 1")));
    assert_eq!(std::fs::read(&ledger_path).unwrap(), before);
}

#[test]
fn legacy_inspect_v2_row_is_discarded_without_affecting_valid_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    let store = persistent_store(ledger_path.clone());
    let retired = store.start_session(Some("proj".to_string()), Some("legacy inspect".to_string()));
    let valid = store.start_session(Some("proj".to_string()), Some("normal".to_string()));
    store.flush_persistence();
    drop(store);

    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    let retired_record = ledger["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["session_id"] == retired.session_id)
        .unwrap();
    retired_record["mode"] = json!("inspect");
    std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();

    let restored = persistent_store(ledger_path);
    assert_eq!(restored.status().restored_sessions, 1);
    assert!(restored.summary(&retired.session_id, None).is_none());
    let valid_summary = restored.summary(&valid.session_id, None).unwrap();
    assert_eq!(valid_summary.mode, SessionMode::Normal);
    assert_eq!(restored.status().last_persist_error, None);
}

#[test]
fn v2_missing_assignment_metadata_discards_only_that_row() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    let store = persistent_store(ledger_path.clone());
    let bad = store.start_session(Some("proj".to_string()), Some("bad".to_string()));
    let good = store.start_session(Some("proj".to_string()), Some("good".to_string()));
    store.flush_persistence();
    drop(store);

    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    let bad_record = ledger["sessions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["session_id"] == bad.session_id)
        .unwrap()
        .as_object_mut()
        .unwrap();
    bad_record.remove("assignment_history_tracking_complete");
    std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();

    let restored = persistent_store(ledger_path);
    assert_eq!(restored.status().restored_sessions, 1);
    assert!(restored.summary(&bad.session_id, None).is_none());
    assert!(restored.summary(&good.session_id, None).is_some());
    assert_eq!(restored.status().last_persist_error, None);
}

#[test]
fn v2_event_and_message_shape_corruption_discards_only_affected_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    let store = persistent_store(ledger_path.clone());
    let bad_missing = store.start_session(
        Some("proj".to_string()),
        Some("missing correlation key".to_string()),
    );
    let bad_retired = store.start_session(
        Some("proj".to_string()),
        Some("retired event field".to_string()),
    );
    let bad_noncanonical_correlation = store.start_session(
        Some("proj".to_string()),
        Some("noncanonical correlation values".to_string()),
    );
    let bad_message = store.start_session(
        Some("proj".to_string()),
        Some("unknown message field".to_string()),
    );
    let good = store.start_session(Some("proj".to_string()), Some("good".to_string()));

    for session_id in [
        &bad_missing.session_id,
        &bad_retired.session_id,
        &bad_noncanonical_correlation.session_id,
    ] {
        let mut metadata = ToolCallRecorderMetadata::default();
        metadata.assign_logical_invocation();
        let start = store.record_tool_call_started_with_metadata(
            Some(session_id),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "proj", "path": "src/lib.rs"}),
            Some("proj".to_string()),
            metadata,
            session_tool_contract("read_file"),
        );
        store.record_tool_call_finished(start, true, &json!({"content": "omitted"}), None, None);
    }
    store
        .post_message(PostSessionMessageInput {
            session_id: bad_message.session_id.clone(),
            kind: SessionMessageKind::Note,
            message: "message row".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    store.flush_persistence();
    drop(store);

    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    let records = ledger["sessions"].as_array_mut().unwrap();
    let missing_record = records
        .iter_mut()
        .find(|record| record["session_id"] == bad_missing.session_id)
        .unwrap();
    let missing_event = missing_record["events"][0].as_object_mut().unwrap();
    assert!(missing_event.contains_key("logical_invocation_id"));
    assert!(missing_event.contains_key("logical_invocation_role"));
    missing_event.remove("logical_invocation_role");

    let retired_record = records
        .iter_mut()
        .find(|record| record["session_id"] == bad_retired.session_id)
        .unwrap();
    retired_record["events"][0]
        .as_object_mut()
        .unwrap()
        .insert("allow_cross_project_session".to_string(), Value::Bool(true));

    let noncanonical_record = records
        .iter_mut()
        .find(|record| record["session_id"] == bad_noncanonical_correlation.session_id)
        .unwrap();
    let noncanonical_event = noncanonical_record["events"][0].as_object_mut().unwrap();
    noncanonical_event.insert("logical_invocation_id".to_string(), Value::Null);
    noncanonical_event.insert("logical_invocation_role".to_string(), Value::Null);

    let message_record = records
        .iter_mut()
        .find(|record| record["session_id"] == bad_message.session_id)
        .unwrap();
    message_record["messages"][0]
        .as_object_mut()
        .unwrap()
        .insert(
            "unsupported_development_field".to_string(),
            Value::Bool(true),
        );

    std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();
    let restored = persistent_store(ledger_path);
    assert_eq!(restored.status().restored_sessions, 1);
    assert!(restored.summary(&bad_missing.session_id, None).is_none());
    assert!(restored.summary(&bad_retired.session_id, None).is_none());
    assert!(restored
        .summary(&bad_noncanonical_correlation.session_id, None)
        .is_none());
    assert!(restored.summary(&bad_message.session_id, None).is_none());
    assert!(restored.summary(&good.session_id, None).is_some());
    assert_eq!(restored.status().last_persist_error, None);
}

#[test]
fn current_ledger_rejects_retired_top_level_binding_member() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    let store = persistent_store(ledger_path.clone());
    let session = store.start_session(Some("proj".to_string()), Some("canonical".to_string()));
    store.flush_persistence();
    drop(store);

    let mut ledger: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
    ledger["durable_current_bindings"] = json!([]);
    std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();

    let restored = persistent_store(ledger_path);
    assert_eq!(restored.status().restored_sessions, 0);
    assert!(restored.summary(&session.session_id, None).is_none());
    assert!(restored
        .status()
        .last_persist_error
        .as_deref()
        .is_some_and(|error| error.contains("invalid v2 session ledger")));
}

#[test]
fn v2_missing_or_removed_lifecycle_discards_only_that_row() {
    for lifecycle in [None, Some(json!("archived"))] {
        let tmp = tempfile::tempdir().unwrap();
        let ledger_path = tmp.path().join("sessions.json");
        let store = persistent_store(ledger_path.clone());
        let bad = store.start_session(Some("proj".to_string()), Some("bad lifecycle".to_string()));
        let good =
            store.start_session(Some("proj".to_string()), Some("good lifecycle".to_string()));
        store.flush_persistence();
        drop(store);

        let mut ledger: Value =
            serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
        let bad_record = ledger["sessions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|record| record["session_id"] == bad.session_id)
            .unwrap()
            .as_object_mut()
            .unwrap();
        match lifecycle.clone() {
            Some(value) => {
                bad_record.insert("lifecycle".to_string(), value);
            }
            None => {
                bad_record.remove("lifecycle");
            }
        }
        std::fs::write(&ledger_path, serde_json::to_vec_pretty(&ledger).unwrap()).unwrap();

        let restored = persistent_store(ledger_path);
        assert_eq!(restored.status().restored_sessions, 1);
        assert!(restored.summary(&bad.session_id, None).is_none());
        assert!(restored.summary(&good.session_id, None).is_some());
    }
}

#[test]
fn session_lifecycle_wire_values_are_snake_case() {
    assert_eq!(
        serde_json::to_value(SessionLifecycle::Active).unwrap(),
        json!("active")
    );
    assert_eq!(
        serde_json::to_value(SessionLifecycle::Closed).unwrap(),
        json!("closed")
    );
    assert_eq!(
        serde_json::from_value::<SessionLifecycle>(json!("active")).unwrap(),
        SessionLifecycle::Active
    );
}

// --- Workflow session lifecycle (Phase 2: explicit close) ---

#[test]
fn active_to_closed_succeeds_and_emits_session_closed_event() {
    let store = SessionStore::default();
    let session = store.start_session(None, Some("close me".to_string()));
    assert_eq!(session.lifecycle, SessionLifecycle::Active);

    let outcome = store.close_session(&session.session_id).unwrap();
    assert!(!outcome.already_closed);
    assert_eq!(outcome.summary.lifecycle, SessionLifecycle::Closed);
    assert_eq!(outcome.summary.session_id, session.session_id);

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    assert_eq!(summary.lifecycle, SessionLifecycle::Closed);
    let closed_events: Vec<_> = summary
        .events
        .iter()
        .filter(|event| event.kind == "session_closed")
        .collect();
    assert_eq!(closed_events.len(), 1);
    assert_eq!(closed_events[0].tool_name, "close_session");
    assert_eq!(closed_events[0].status.as_deref(), Some("succeeded"));
}

#[test]
fn closed_session_coldifies_payload_and_queries_without_reheating() {
    let store = SessionStore::default();
    let session = store.start_session(None, Some("cold history".to_string()));
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Todo,
        "retained closed message",
    );
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_file",
        &json!({"project": "demo", "path": "src/lib.rs"}),
        session_tool_contract("read_file"),
    );
    store.record_tool_call_finished(start, true, &json!({"content": "retained"}), None, None);
    assert!(store
        .hot_payload_entry_count_for_test(&session.session_id)
        .is_some_and(|entries| entries > 0));

    let outcome = store.close_session(&session.session_id).unwrap();
    assert!(!outcome.already_closed);
    assert_eq!(
        store.hot_payload_entry_count_for_test(&session.session_id),
        None
    );
    assert!(store
        .cold_payload_bytes_for_test(&session.session_id)
        .is_some_and(|bytes| bytes > 0));

    let other = store.start_session(None, Some("other cold history".to_string()));
    store.close_session(&other.session_id).unwrap();
    assert!(store
        .cold_payload_bytes_for_test(&other.session_id)
        .is_some());

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    assert_eq!(summary.lifecycle, SessionLifecycle::Closed);
    assert!(summary
        .events
        .iter()
        .any(|event| event.kind == "session_closed"));
    let messages = store
        .list_messages(&session.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message, "retained closed message");
    assert_eq!(
        store.hot_payload_entry_count_for_test(&session.session_id),
        None,
        "cold queries must not install a hot SessionRecord"
    );
    assert_eq!(
        store.hot_payload_entry_count_for_test(&other.session_id),
        None,
        "querying one cold Session must not heat another historical Session"
    );

    let query_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "session_summary",
        &json!({"session_id": session.session_id.clone()}),
        session_tool_contract("session_summary"),
    );
    store.record_tool_call_finished(query_start, true, &json!({"success": true}), None, None);
    assert_eq!(
        store.hot_payload_entry_count_for_test(&session.session_id),
        None,
        "closed recorder events must rewrite the cold payload in place"
    );

    let repeated = store.close_session(&session.session_id).unwrap();
    assert!(repeated.already_closed);
    let summary = store.summary(&session.session_id, Some(100)).unwrap();
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| event.kind == "session_closed")
            .count(),
        1
    );
}

#[test]
fn close_cleanup_evidence_rewrites_cold_payload_without_reheating() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("cold close evidence".to_string()));
    store.close_session(&session.session_id).unwrap();
    assert!(store
        .cold_payload_bytes_for_test(&session.session_id)
        .is_some());

    store.record_session_close_persistent_shell_evidence(
        &session.session_id,
        "wc_shell_cold_close",
        "closed",
        "completed",
        None,
        false,
    );
    assert_eq!(
        store.hot_payload_entry_count_for_test(&session.session_id),
        None
    );

    let restored = flush_and_restore(&store, ledger);
    assert!(restored
        .cold_payload_bytes_for_test(&session.session_id)
        .is_some());
    let summary = restored.summary(&session.session_id, Some(50)).unwrap();
    let evidence = summary
        .events
        .iter()
        .find(|event| event.kind == "session_closed")
        .and_then(|event| event.persistent_shell.as_ref())
        .expect("close cleanup evidence must survive cold persistence");
    assert_eq!(evidence.shell_id.as_deref(), Some("wc_shell_cold_close"));
    assert_eq!(evidence.shell_state.as_deref(), Some("closed"));
    assert_eq!(evidence.execution_state.as_deref(), Some("completed"));
}

#[test]
fn closed_cold_round_trip_preserves_evidence_and_active_stays_hot() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 20);
    let closed = store.start_session(Some("proj".to_string()), Some("historical".to_string()));
    post_message(
        &store,
        &closed.session_id,
        SessionMessageKind::Note,
        "durable historical message",
    );
    let start = store.record_tool_call_started(
        Some(&closed.session_id),
        SessionTransport::Api,
        "read_file",
        &json!({"project": "proj", "path": "history.rs"}),
        session_tool_contract("read_file"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({"content": "history evidence"}),
        None,
        None,
    );
    store.close_session(&closed.session_id).unwrap();

    let active = store.start_session(Some("proj".to_string()), Some("active".to_string()));
    post_message(
        &store,
        &active.session_id,
        SessionMessageKind::Progress,
        "active message",
    );
    assert_eq!(
        store.hot_payload_entry_count_for_test(&closed.session_id),
        None
    );
    assert!(store
        .hot_payload_entry_count_for_test(&active.session_id)
        .is_some());
    assert_eq!(store.cold_payload_bytes_for_test(&active.session_id), None);

    let before_summary = store.summary(&closed.session_id, Some(100)).unwrap();
    let before_messages = store
        .list_messages(&closed.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    store.flush_persistence();
    let raw = std::fs::read_to_string(&ledger).unwrap();
    let ledger_value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(ledger_value["version"], SESSION_LEDGER_VERSION);
    let closed_json = ledger_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["session_id"] == closed.session_id)
        .unwrap();
    assert_eq!(closed_json["lifecycle"], "closed");
    assert!(closed_json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["message"] == "durable historical message"));

    drop(store);
    let restored = SessionStore::with_persistence(&ledger, 10, 20);
    assert_eq!(
        restored.hot_payload_entry_count_for_test(&closed.session_id),
        None,
        "restored closed session must not keep the parsed event/message graph"
    );
    assert!(restored
        .cold_payload_bytes_for_test(&closed.session_id)
        .is_some_and(|bytes| bytes > 0));
    assert!(restored
        .hot_payload_entry_count_for_test(&active.session_id)
        .is_some());
    assert_eq!(
        restored.cold_payload_bytes_for_test(&active.session_id),
        None
    );

    let after_summary = restored.summary(&closed.session_id, Some(100)).unwrap();
    let after_messages = restored
        .list_messages(&closed.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    assert_eq!(
        serde_json::to_value(&after_summary).unwrap(),
        serde_json::to_value(&before_summary).unwrap()
    );
    assert_eq!(after_messages.len(), before_messages.len());
    assert_eq!(after_messages[0].message, before_messages[0].message);
    assert_eq!(after_summary.lifecycle, SessionLifecycle::Closed);
    assert_eq!(
        restored.hot_payload_entry_count_for_test(&closed.session_id),
        None,
        "restart query must materialize only a temporary target record"
    );
    assert_eq!(
        restored
            .summary(&active.session_id, Some(10))
            .unwrap()
            .lifecycle,
        SessionLifecycle::Active
    );
}

#[test]
fn cold_session_query_touch_preserves_lru_capacity_order() {
    let store = SessionStore::new(2, 10);
    let first = store.start_session(None, Some("cold survivor".to_string()));
    store.close_session(&first.session_id).unwrap();
    let second = store.start_session(None, Some("old active".to_string()));

    assert!(store
        .cold_payload_bytes_for_test(&first.session_id)
        .is_some());
    store.summary(&first.session_id, Some(10)).unwrap();
    let third = store.start_session(None, Some("new active".to_string()));

    assert!(store.contains_session(&first.session_id));
    assert!(!store.contains_session(&second.session_id));
    assert!(store.contains_session(&third.session_id));
    assert_eq!(
        store.lifecycle_state(&first.session_id),
        Some(SessionLifecycle::Closed)
    );
    assert!(store
        .cold_payload_bytes_for_test(&first.session_id)
        .is_some());
    assert!(store
        .hot_payload_entry_count_for_test(&third.session_id)
        .is_some());
}

#[test]
fn closed_lifecycle_persists_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(Some("proj".to_string()), Some("close persist".to_string()));
    store.close_session(&session.session_id).unwrap();

    store.flush_persistence();
    let raw = std::fs::read_to_string(&ledger).unwrap();
    let ledger_value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(ledger_value["version"], SESSION_LEDGER_VERSION);
    assert_eq!(ledger_value["sessions"][0]["lifecycle"], "closed");

    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(20)).unwrap();
    assert_eq!(summary.lifecycle, SessionLifecycle::Closed);
    assert!(summary
        .events
        .iter()
        .any(|event| event.kind == "session_closed"));
}

#[test]
fn closed_session_denies_mutation_tools_allows_query() {
    let store = SessionStore::default();
    let session = store.start_session(None, Some("closed query".to_string()));
    store.close_session(&session.session_id).unwrap();

    // Write / shell blocked.
    let write_denial = store
        .lifecycle_denial(
            &session.session_id,
            "write_project_file",
            session_tool_contract("write_project_file"),
        )
        .expect("write denied on closed");
    assert_eq!(write_denial.lifecycle, SessionLifecycle::Closed);
    let shell_denial = store
        .lifecycle_denial(
            &session.session_id,
            "run_shell",
            session_tool_contract("run_shell"),
        )
        .expect("shell denied on closed");
    assert_eq!(shell_denial.lifecycle, SessionLifecycle::Closed);
    assert!(store
        .lifecycle_denial(
            &session.session_id,
            "post_session_message",
            session_tool_contract("post_session_message")
        )
        .is_some());
    assert!(store
        .lifecycle_denial(
            &session.session_id,
            "memory_set",
            session_tool_contract("memory_set")
        )
        .is_some());

    // Query / pure read still allowed; close remains idempotent path.
    assert!(store
        .lifecycle_denial(
            &session.session_id,
            "read_file",
            session_tool_contract("read_file")
        )
        .is_none());
    assert!(store
        .lifecycle_denial(
            &session.session_id,
            "session_summary",
            session_tool_contract("session_summary")
        )
        .is_none());
    assert!(store
        .lifecycle_denial(
            &session.session_id,
            "close_session",
            session_tool_contract("close_session")
        )
        .is_none());

    // Message board mutations fail with SessionClosed; list still works.
    let post = store.post_message(PostSessionMessageInput {
        session_id: session.session_id.clone(),
        kind: SessionMessageKind::Note,
        message: "after close".to_string(),
        tags: Vec::new(),
        reply_to: None,
        priority: SessionMessagePriority::Normal,
    });
    assert!(matches!(
        post,
        Err(SessionMessageError::SessionClosed {
            lifecycle: SessionLifecycle::Closed
        })
    ));
    let listed = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: None,
                status: None,
                message_id: None,
                reply_to: None,
                limit: None,
            },
        )
        .unwrap();
    assert!(listed.is_empty());

    let summary = store.summary(&session.session_id, Some(10)).unwrap();
    assert_eq!(summary.lifecycle, SessionLifecycle::Closed);
    assert_eq!(summary.session_id, session.session_id);
}

#[test]
fn repeated_close_is_idempotent_without_duplicate_events() {
    let store = SessionStore::default();
    let session = store.start_session(None, Some("idempotent".to_string()));
    let first = store.close_session(&session.session_id).unwrap();
    assert!(!first.already_closed);
    let second = store.close_session(&session.session_id).unwrap();
    assert!(second.already_closed);
    assert_eq!(second.summary.lifecycle, SessionLifecycle::Closed);

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    let closed_count = summary
        .events
        .iter()
        .filter(|event| event.kind == "session_closed")
        .count();
    assert_eq!(
        closed_count, 1,
        "repeat close must not append another event"
    );
}

#[test]
fn unknown_session_close_fails_without_create() {
    let store = SessionStore::default();
    let missing = "wc_sess_missingclose01";
    let err = store.close_session(missing).unwrap_err();
    assert_eq!(err, SessionCloseError::UnknownSession);
    assert!(!store.contains_session(missing));
    assert!(store.summary(missing, None).is_none());
}

#[test]
fn eviction_does_not_produce_closed_lifecycle() {
    // Capacity eviction removes the record; it is not a Closed transition.
    let store = SessionStore::new(1, 10);
    let first = store.start_session(None, Some("evict me".to_string()));
    let _second = store.start_session(None, Some("survivor".to_string()));
    assert!(!store.contains_session(&first.session_id));
    assert!(store.summary(&first.session_id, None).is_none());
    // Evicted id is unknown, not Closed — close must not invent a session.
    assert_eq!(
        store.close_session(&first.session_id).unwrap_err(),
        SessionCloseError::UnknownSession
    );
    assert!(!store.contains_session(&first.session_id));
}

#[test]
fn closed_session_does_not_reopen() {
    let store = SessionStore::default();
    let session = store.start_session(None, Some("no reopen".to_string()));
    store.close_session(&session.session_id).unwrap();
    // Only path that could "reopen" would be inventing Active; close stays Closed.
    let again = store.close_session(&session.session_id).unwrap();
    assert!(again.already_closed);
    assert_eq!(again.summary.lifecycle, SessionLifecycle::Closed);
    assert!(!again.summary.lifecycle.allows_mutation());
}
