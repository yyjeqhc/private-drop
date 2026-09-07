use crate::*;
use serde_json::{json, Value};

fn session_tool_contract(tool_name: &str) -> SessionToolContract {
    let advances_context_checkpoint = matches!(
        tool_name,
        "apply_text_edits" | "run_process" | "workspace_checkpoint_create"
    );
    SessionToolContract {
        risk_class: if advances_context_checkpoint {
            "write"
        } else {
            "read"
        },
        read_like: !advances_context_checkpoint,
        write_like: advances_context_checkpoint,
        shell_like: tool_name == "run_process",
        git_like: false,
        change_summary_like: false,
        project_write: tool_name == "apply_text_edits",
        path_hint: SessionPathHint::None,
        accepts_context_ack: true,
        advances_context_checkpoint,
    }
}

fn project_edit_contract(path_hint: SessionPathHint) -> SessionToolContract {
    SessionToolContract {
        risk_class: "write",
        read_like: false,
        write_like: true,
        shell_like: false,
        git_like: false,
        change_summary_like: false,
        project_write: true,
        path_hint,
        accepts_context_ack: false,
        advances_context_checkpoint: true,
    }
}

fn record_model_facing_result(
    store: &SessionStore,
    session_id: &str,
    tool_name: &str,
    ack: SessionContextRevisionAck,
    success: bool,
    output: Value,
) -> RecordedModelFacingToolCall {
    let arguments = json!({"project": "proj"});
    let start = store
        .record_tool_call_started_with_metadata(
            Some(session_id),
            SessionTransport::Mcp,
            tool_name,
            &arguments,
            Some("proj".to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: ack,
                ..Default::default()
            },
            session_tool_contract(tool_name),
        )
        .expect("recorded call start");
    store
        .record_model_facing_tool_call_finished(
            Some(start),
            success,
            &output,
            (!success).then_some("business failure"),
            (!success).then_some("business_failure"),
        )
        .expect("recorded model-facing result")
}

#[test]
fn dry_run_project_edits_do_not_record_session_changed_paths() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("dry run paths".to_string()));

    let dry_text_start = store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "apply_text_edits",
            &json!({
                "project": "proj",
                "dry_run": true,
                "changes": [{"kind": "create", "path": "src/would_only.rs", "content": "x"}]
            }),
            project_edit_contract(SessionPathHint::PathList),
        )
        .expect("dry-run apply_text_edits start");
    assert!(dry_text_start.changed_paths.is_empty());
    store
        .record_tool_call_finished(
            Some(dry_text_start),
            true,
            &json!({
                "dry_run": true,
                "state_changed": false,
                "changed_paths": ["src/would_only.rs"]
            }),
            None,
            None,
        )
        .expect("dry-run apply_text_edits finish");

    let dry_patch_start = store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "apply_patch",
            &json!({"project": "proj", "dry_run": true, "patch": "*** Begin Patch\n*** End Patch"}),
            project_edit_contract(SessionPathHint::Patch),
        )
        .expect("dry-run apply_patch start");
    store
        .record_tool_call_finished(
            Some(dry_patch_start),
            true,
            &json!({
                "dry_run": true,
                "state_changed": false,
                "changed_paths": ["src/would_patch.rs"]
            }),
            None,
            None,
        )
        .expect("dry-run apply_patch finish");

    let live_start = store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "apply_text_edits",
            &json!({
                "project": "proj",
                "dry_run": false,
                "changes": [{"kind": "create", "path": "src/live.rs", "content": "x"}]
            }),
            project_edit_contract(SessionPathHint::PathList),
        )
        .expect("live apply_text_edits start");
    assert_eq!(live_start.changed_paths, vec!["src/live.rs"]);
    store
        .record_tool_call_finished(
            Some(live_start),
            true,
            &json!({"dry_run": false, "state_changed": true}),
            None,
            None,
        )
        .expect("live apply_text_edits finish");

    let summary = store
        .summary(&session.session_id, Some(100))
        .expect("session summary");
    let finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 3);
    assert!(finished[0].changed_paths.is_empty());
    assert!(finished[1].changed_paths.is_empty());
    assert_eq!(finished[2].changed_paths, vec!["src/live.rs"]);
}

#[test]
fn session_context_revision_exact_missing_stale_invalid_future_and_failure_semantics() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("continuity".to_string()));

    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.pre_call_context_revision, 0);
    assert_eq!(first.pre_response_context_revision, 0);
    assert_eq!(first.context_revision, 1);
    assert!(first.checkpoint_advanced);

    let second = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(second.pre_call_context_revision, 1);
    assert_eq!(second.pre_response_context_revision, 1);
    assert_eq!(second.context_revision, 1);
    assert!(!second.checkpoint_advanced);
    assert!(second.recovery_events.is_empty());
    assert!(!second.history_lost);

    let missing = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(missing.context_revision, 2);
    assert!(
        missing.recovery_events.is_empty(),
        "unknown caller state must not be treated as an acknowledged revision-zero delta"
    );
    assert!(!missing.history_lost);

    let stale = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(stale.pre_call_context_revision, 2);
    assert_eq!(stale.pre_response_context_revision, 2);
    assert_eq!(stale.context_revision, 2);
    assert!(!stale.checkpoint_advanced);
    assert_eq!(
        stale
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![2]
    );

    let future = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(999),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(future.context_revision, 2);
    assert_eq!(future.pre_call_context_revision, 2);
    assert!(future.recovery_events.is_empty());
    assert!(!future.history_lost);

    let invalid = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Invalid,
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(invalid.context_revision, 2);
    assert_eq!(invalid.pre_call_context_revision, 2);
    assert!(invalid.recovery_events.is_empty());
    assert!(!invalid.history_lost);

    let failed = record_model_facing_result(
        &store,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(2),
        false,
        json!({"failure_kind": "process_failed", "command_started": true, "command_completed": true}),
    );
    assert_eq!(failed.pre_call_context_revision, 2);
    assert_eq!(failed.pre_response_context_revision, 2);
    assert_eq!(failed.context_revision, 3);
    assert!(failed.checkpoint_advanced);
    assert_eq!(store.context_revision(&session.session_id), Some(3));
}

#[test]
fn non_capable_model_facing_result_advances_cross_surface_recovery_watermark() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("cross surface".to_string()));
    let seed = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(seed.context_revision, 1);

    let non_capable = record_model_facing_result(
        &store,
        &session.session_id,
        "workspace_checkpoint_create",
        SessionContextRevisionAck::Unsupported,
        true,
        json!({
            "checkpoint_id": "wc_ckpt_cross_surface",
            "branch": "feature",
            "status_summary": {"modified": 3},
            "state_changed": true
        }),
    );
    assert_eq!(non_capable.pre_call_context_revision, 1);
    assert_eq!(non_capable.context_revision, 2);
    assert!(non_capable.recovery_events.is_empty());
    assert!(!non_capable.history_lost);
    assert_eq!(store.context_revision(&session.session_id), Some(2));

    let resumed = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(resumed.pre_call_context_revision, 2);
    assert_eq!(resumed.pre_response_context_revision, 2);
    assert_eq!(resumed.context_revision, 2);
    assert!(!resumed.checkpoint_advanced);
    assert_eq!(
        resumed
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let checkpoint = &resumed.recovery_events[0];
    assert_eq!(checkpoint.tool_name, "workspace_checkpoint_create");
    let consequence = checkpoint.context_result_summary.as_ref().unwrap();
    assert_eq!(consequence["checkpoint_id"], "wc_ckpt_cross_surface");
    assert_eq!(consequence["status_summary"]["modified"], 3);
}

#[test]
fn session_context_revision_retention_reports_history_loss_without_counter_regression() {
    let store = SessionStore::new(10, 4);
    let session = store.start_session(Some("proj".to_string()), Some("retention".to_string()));
    let mut latest = 0;
    for _ in 0..6 {
        let recorded = record_model_facing_result(
            &store,
            &session.session_id,
            "apply_text_edits",
            SessionContextRevisionAck::Revision(latest),
            true,
            json!({"state_changed": true}),
        );
        latest = recorded.context_revision;
    }
    assert_eq!(latest, 6);
    assert_eq!(store.context_revision(&session.session_id), Some(6));

    let stale = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(stale.pre_call_context_revision, 6);
    assert_eq!(stale.pre_response_context_revision, 6);
    assert_eq!(stale.context_revision, 6);
    assert!(!stale.checkpoint_advanced);
    assert!(stale.history_lost);
    assert!(stale.recovery_events.len() < 5);
}

#[test]
fn generic_session_events_do_not_advance_model_context_revision() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("background".to_string()));
    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);

    let generic_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_test",
        &json!({"project": "proj"}),
        session_tool_contract("cargo_test"),
    );
    assert!(store
        .record_tool_call_finished(
            generic_start,
            true,
            &json!({"tests_run": 1, "passed": true}),
            None,
            None,
        )
        .is_some());
    assert_eq!(store.context_revision(&session.session_id), Some(1));

    let exact = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(exact.pre_call_context_revision, 1);
    assert_eq!(exact.pre_response_context_revision, 1);
    assert_eq!(exact.context_revision, 1);
    assert!(!exact.checkpoint_advanced);
    assert!(exact.recovery_events.is_empty());
}

#[test]
fn simultaneous_no_checkpoint_results_leave_revision_unchanged() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("read concurrency".to_string()),
    );
    let seed = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(seed.context_revision, 1);

    let make_start = |tool_name: &str| {
        store
            .record_tool_call_started_with_metadata(
                Some(&session.session_id),
                SessionTransport::Mcp,
                tool_name,
                &json!({"project": "proj"}),
                Some("proj".to_string()),
                ToolCallRecorderMetadata {
                    ack_session_context_revision: SessionContextRevisionAck::Revision(1),
                    ..Default::default()
                },
                session_tool_contract(tool_name),
            )
            .unwrap()
    };
    let read_start = make_start("read_file");
    let search_start = make_start("search_project_texts");
    assert_eq!(read_start.pre_call_context_revision, 1);
    assert_eq!(search_start.pre_call_context_revision, 1);

    let a_store = store.clone();
    let b_store = store.clone();
    let read = std::thread::spawn(move || {
        a_store
            .record_model_facing_tool_call_finished(
                Some(read_start),
                true,
                &json!({"content": "read"}),
                None,
                None,
            )
            .unwrap()
    });
    let search = std::thread::spawn(move || {
        b_store
            .record_model_facing_tool_call_finished(
                Some(search_start),
                true,
                &json!({"matches": []}),
                None,
                None,
            )
            .unwrap()
    });
    for recorded in [read.join().unwrap(), search.join().unwrap()] {
        assert_eq!(recorded.context_revision, 1);
        assert_eq!(recorded.pre_response_context_revision, 1);
        assert!(!recorded.checkpoint_advanced);
        assert_eq!(
            recorded.ack_session_context_revision,
            SessionContextRevisionAck::Revision(1)
        );
    }
    assert_eq!(store.context_revision(&session.session_id), Some(1));
}

#[test]
fn concurrent_checkpoint_and_raw_result_advance_once() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("mixed concurrency".to_string()),
    );
    let seed = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(seed.context_revision, 1);

    let read_start = store
        .record_tool_call_started_with_metadata(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "read_file",
            &json!({"project": "proj"}),
            Some("proj".to_string()),
            ToolCallRecorderMetadata::default(),
            session_tool_contract("read_file"),
        )
        .unwrap();
    let checkpoint_start = store
        .record_tool_call_started_with_metadata(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "run_process",
            &json!({"project": "proj"}),
            Some("proj".to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: SessionContextRevisionAck::Revision(1),
                ..Default::default()
            },
            session_tool_contract("run_process"),
        )
        .unwrap();
    assert_eq!(read_start.pre_call_context_revision, 1);
    assert_eq!(checkpoint_start.pre_call_context_revision, 1);

    let a_store = store.clone();
    let b_store = store.clone();
    let read = std::thread::spawn(move || {
        a_store
            .record_model_facing_tool_call_finished(
                Some(read_start),
                true,
                &json!({"content": "read"}),
                None,
                None,
            )
            .unwrap()
    });
    let checkpoint = std::thread::spawn(move || {
        b_store
            .record_model_facing_tool_call_finished(
                Some(checkpoint_start),
                true,
                &json!({"command_started": true, "command_completed": true, "exit_code": 0}),
                None,
                None,
            )
            .unwrap()
    });
    let read = read.join().unwrap();
    let checkpoint = checkpoint.join().unwrap();
    assert!(!read.checkpoint_advanced);
    assert!(matches!(read.context_revision, 1 | 2));
    assert!(checkpoint.checkpoint_advanced);
    assert_eq!(checkpoint.context_revision, 2);
    assert_eq!(store.context_revision(&session.session_id), Some(2));
}

#[test]
fn session_context_revision_survives_persistence_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("restart".to_string()));
    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    let raw = record_model_facing_result(
        &store,
        &session.session_id,
        "read_file",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"content": "persisted raw observation"}),
    );
    assert_eq!(raw.context_revision, 1);
    assert!(!raw.checkpoint_advanced);
    assert_eq!(store.context_revision(&session.session_id), Some(1));
    store.flush_persistence();
    drop(store);

    let restored = SessionStore::with_persistence(&ledger, 10, 100);
    assert_eq!(restored.context_revision(&session.session_id), Some(1));
    let second = record_model_facing_result(
        &restored,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(second.pre_call_context_revision, 1);
    assert_eq!(second.pre_response_context_revision, 1);
    assert_eq!(second.context_revision, 2);
    assert!(second.checkpoint_advanced);
    assert!(second.recovery_events.is_empty());
}

#[test]
fn existing_persisted_context_watermark_is_not_renumbered() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("high watermark".to_string()));
    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    store.flush_persistence();
    drop(store);

    let mut persisted: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    persisted["sessions"][0]["context_revision"] = json!(847);
    std::fs::write(&ledger, serde_json::to_vec(&persisted).unwrap()).unwrap();

    let restored = SessionStore::with_persistence(&ledger, 10, 100);
    assert_eq!(restored.context_revision(&session.session_id), Some(847));
    let read = record_model_facing_result(
        &restored,
        &session.session_id,
        "read_file",
        SessionContextRevisionAck::Revision(847),
        true,
        json!({"content": "still 847"}),
    );
    assert_eq!(read.context_revision, 847);
    assert!(!read.checkpoint_advanced);
    let next = record_model_facing_result(
        &restored,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(847),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(next.context_revision, 848);
}

#[test]
fn session_context_revision_restore_revalidates_bounded_context_result_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("context sanitize".to_string()),
    );
    let recorded = record_model_facing_result(
        &store,
        &session.session_id,
        "workspace_checkpoint_create",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({
            "checkpoint_id": "wc_ckpt_demo",
            "branch": "b".repeat(500),
            "status_summary": {
                "modified": 21,
                "token": "wc_pat_must_not_survive"
            }
        }),
    );
    assert_eq!(recorded.context_revision, 1);
    let live = store.summary(&session.session_id, Some(20)).unwrap();
    let live_summary = live
        .events
        .iter()
        .find(|event| event.context_revision == Some(1))
        .and_then(|event| event.context_result_summary.as_ref())
        .unwrap();
    assert_eq!(live_summary["status_summary"]["modified"], 21);
    assert_eq!(live_summary["status_summary"]["token"], "[redacted]");
    let live_branch = live_summary["branch"].as_str().unwrap();
    assert!(live_branch.chars().count() <= 123);
    assert!(live_branch.ends_with("..."));

    store.flush_persistence();
    drop(store);
    let mut persisted: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    let events = persisted["sessions"][0]["events"].as_array_mut().unwrap();
    let finished = events
        .iter_mut()
        .find(|event| event["context_revision"] == 1)
        .unwrap();
    finished["context_result_summary"] = json!({
        "checkpoint_id": "wc_ckpt_demo",
        "branch": "x".repeat(500),
        "status_summary": {
            "modified": 21,
            "token": "wc_pat_corrupt_ledger_secret"
        },
        "arbitrary_untrusted_body": "must not survive restore"
    });
    std::fs::write(&ledger, serde_json::to_vec(&persisted).unwrap()).unwrap();

    let restored = SessionStore::with_persistence(&ledger, 10, 100);
    assert_eq!(restored.context_revision(&session.session_id), Some(1));
    let restored_summary = restored.summary(&session.session_id, Some(20)).unwrap();
    let context = restored_summary
        .events
        .iter()
        .find(|event| event.context_revision == Some(1))
        .and_then(|event| event.context_result_summary.as_ref())
        .unwrap();
    assert!(context.get("arbitrary_untrusted_body").is_none());
    assert_eq!(context["status_summary"]["modified"], 21);
    assert_eq!(context["status_summary"]["token"], "[redacted]");
    let restored_branch = context["branch"].as_str().unwrap();
    assert!(restored_branch.chars().count() <= 123);
    assert!(restored_branch.ends_with("..."));
}
