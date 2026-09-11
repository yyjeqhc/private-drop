use crate::model::MESSAGE_ID_PREFIX;
use crate::*;
use serde_json::{json, Value};
use std::path::PathBuf;
use webcodex_core::workflow_session_contract::{ExecutionShell, SessionMode};

fn post_message(
    store: &SessionStore,
    session_id: &str,
    kind: SessionMessageKind,
    message: &str,
) -> SessionMessage {
    store
        .post_message(PostSessionMessageInput {
            session_id: session_id.to_string(),
            kind,
            message: message.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap()
}

fn session_tool_contract(tool_name: &str) -> SessionToolContract {
    let (read_like, write_like, shell_like, path_hint) = match tool_name {
        "read_file" => (true, false, false, SessionPathHint::SinglePath),
        "write_project_file" => (false, true, false, SessionPathHint::SinglePath),
        "run_shell" | "session_shell_exec" | "cargo_check" => {
            (false, false, true, SessionPathHint::None)
        }
        _ => (true, false, false, SessionPathHint::None),
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
fn session_store_bounds_event_limit() {
    let store = SessionStore::new(10, 3);
    let summary = store.start_session(None, None);
    for idx in 0..5 {
        let args = json!({"project": "demo", "path": format!("file{idx}.rs")});
        let start = store.record_tool_call_started(
            Some(&summary.session_id),
            SessionTransport::Api,
            "write_project_file",
            &args,
            session_tool_contract("write_project_file"),
        );
        store.record_tool_call_finished(start, true, &json!({}), None, None);
    }
    let summary = store.summary(&summary.session_id, Some(50)).unwrap();
    assert_eq!(summary.events.len(), 3);
    assert_eq!(summary.counts.tool_calls, 2);
}

#[test]
fn input_summary_redacts_sensitive_keys() {
    let store = SessionStore::default();
    let summary = store.start_session(None, None);
    store.record_tool_call_started(
        Some(&summary.session_id),
        SessionTransport::Api,
        "runtime_status",
        &json!({
            "token": "super-secret-token",
            "command": "curl -H 'Authorization: Bearer wc_pat_never_store'"
        }),
        session_tool_contract("runtime_status"),
    );
    let summary = store.summary(&summary.session_id, Some(10)).unwrap();
    assert_eq!(
        summary.events[0].input_summary.as_ref().unwrap()["token"],
        "[redacted]"
    );
    assert_eq!(
        summary.events[0].input_summary.as_ref().unwrap()["command"],
        "[redacted]"
    );
}

#[test]
fn coding_instruction_redacts_reusable_credentials_without_truncating_normal_goals() {
    assert_eq!(
        super::util::redact_and_bound_instruction(
            "continue with wc_pat_never_persist_this_value",
            super::model::MAX_CODING_INSTRUCTION_CHARS,
        ),
        "[redacted]"
    );
    let goal = "x".repeat(1_000);
    assert_eq!(
        super::util::redact_and_bound_instruction(
            &goal,
            super::model::MAX_CODING_INSTRUCTION_CHARS,
        ),
        goal
    );
}

fn persistent_store(path: PathBuf) -> SessionStore {
    SessionStore::with_persistence(path, 10, 10)
}

/// Flush deferred ledger writes, then open a fresh store from the same path.
/// Required because persistent stores write on a background thread.
fn flush_and_restore(store: &SessionStore, path: PathBuf) -> SessionStore {
    store.flush_persistence();
    SessionStore::with_persistence(path, 10, 10)
}

#[test]
fn session_store_persists_and_restores_basic_session() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(
        Some("agent:oe:private-drop".to_string()),
        Some("persistent work".to_string()),
    );

    store.flush_persistence();
    let raw = std::fs::read_to_string(&ledger).unwrap();
    assert!(
        !raw.contains('\n'),
        "production ledger should use compact JSON"
    );
    let restored = SessionStore::with_persistence(ledger, 10, 10);
    let status = restored.status();
    assert_eq!(status.persistence, "enabled");
    assert_eq!(status.restored_sessions, 1);
    assert_eq!(status.last_persist_error, None);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    assert_eq!(summary.session_id, session.session_id);
    assert_eq!(summary.project.as_deref(), Some("agent:oe:private-drop"));
    assert_eq!(summary.title.as_deref(), Some("persistent work"));
    assert_eq!(summary.lifecycle, SessionLifecycle::Active);
    assert_eq!(
        summary.execution_context,
        SessionExecutionContext::default()
    );
}

#[test]
fn session_execution_context_persistence_matrix() {
    let cases = [
        (
            "local-normalized",
            SessionExecutionContext {
                default_cwd: Some("frontend/./src".to_string()),
                default_shell: Some(ExecutionShell::Bash),
                resource: None,
            },
            SessionExecutionContext {
                default_cwd: Some("frontend/src".to_string()),
                default_shell: Some(ExecutionShell::Bash),
                resource: None,
            },
            json!({"default_cwd": "frontend/src", "default_shell": "bash"}),
        ),
        (
            "remote-resource",
            SessionExecutionContext {
                default_cwd: Some("/opt/webcodex-edge".to_string()),
                default_shell: None,
                resource: Some("tmp".to_string()),
            },
            SessionExecutionContext {
                default_cwd: Some("/opt/webcodex-edge".to_string()),
                default_shell: None,
                resource: Some("tmp".to_string()),
            },
            json!({"default_cwd": "/opt/webcodex-edge", "resource": "tmp"}),
        ),
    ];

    for (label, input, expected, persisted_context) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join("sessions.json");
        let store = persistent_store(ledger.clone());
        let session = store
            .start_session_with_options(
                SessionCreateOptions::new(
                    Some("agent:oe:private-drop".to_string()),
                    Some(format!("persistent context {label}")),
                    SessionMode::Normal,
                    SessionGuards::default(),
                )
                .with_execution_context(input),
            )
            .unwrap();
        assert_eq!(session.execution_context, expected, "{label}");

        store.flush_persistence();
        let value: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(
            value["sessions"][0]["execution_context"], persisted_context,
            "{label}"
        );
        let restored = SessionStore::with_persistence(ledger.clone(), 10, 10);
        assert_eq!(
            restored
                .summary(&session.session_id, None)
                .unwrap()
                .execution_context,
            expected,
            "{label}"
        );
    }
}

#[test]
fn coding_session_context_precommit_failure_leaves_memory_unchanged() {
    fn request(
        resume_session_id: Option<String>,
        execution_context: Option<SessionExecutionContext>,
    ) -> CodingSessionRequest {
        CodingSessionRequest {
            project: "agent:oe:private-drop".to_string(),
            authority_fingerprint: TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT.to_string(),
            resume_session_id,
            instruction: Some("continue".to_string()),
            mode: SessionMode::Normal,
            guards: SessionGuards::default(),
            execution_context,
            project_instructions: None,
            transport: SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        }
    }

    let store = SessionStore::default();
    let initial = SessionExecutionContext {
        default_cwd: Some("frontend".to_string()),
        default_shell: Some(ExecutionShell::Bash),
        resource: None,
    };
    let created = store
        .ensure_coding_session(request(None, Some(initial.clone())))
        .unwrap();
    assert!(!created.reused);
    assert!(created.execution_context_changed);
    assert_eq!(created.summary.execution_context, initial);

    let session_id = created.summary.session_id.clone();
    let preserved = store
        .ensure_coding_session(request(Some(session_id.clone()), None))
        .unwrap();
    assert!(preserved.reused);
    assert!(!preserved.execution_context_changed);
    assert_eq!(preserved.summary.execution_context, initial);

    let replacement = SessionExecutionContext {
        default_cwd: Some("backend".to_string()),
        default_shell: Some(ExecutionShell::Sh),
        resource: None,
    };
    let replaced = store
        .ensure_coding_session(request(Some(session_id.clone()), Some(replacement.clone())))
        .unwrap();
    assert!(replaced.execution_context_changed);
    assert_eq!(replaced.summary.execution_context, replacement);
    let event = replaced.summary.events.last().unwrap();
    assert_eq!(event.execution_context, Some(replacement.clone()));
    assert_eq!(event.previous_execution_context, Some(initial.clone()));

    let event_count = replaced.summary.events_total;
    store.fail_next_coding_continuity_precommit_for_test();
    let error = store
        .ensure_coding_session(request(
            Some(session_id.clone()),
            Some(SessionExecutionContext::default()),
        ))
        .unwrap_err();
    assert_eq!(error, CodingSessionError::CommitFailed);
    let after_failure = store.summary(&session_id, None).unwrap();
    assert_eq!(after_failure.execution_context, replacement);
    assert_eq!(after_failure.events_total, event_count);
}

#[test]
fn update_session_execution_context_sets_clears_and_rejects_invalid_states() {
    let store = SessionStore::default();
    let session = store.start_session(
        Some("agent:oe:private-drop".to_string()),
        Some("context update".to_string()),
    );
    let replacement = SessionExecutionContext {
        default_cwd: Some("frontend".to_string()),
        default_shell: Some(ExecutionShell::Bash),
        resource: None,
    };
    let set = store
        .update_execution_context(
            &session.session_id,
            replacement.clone(),
            SessionTransport::Api,
        )
        .unwrap();
    assert!(set.changed);
    assert_eq!(
        set.previous_execution_context,
        SessionExecutionContext::default()
    );
    assert_eq!(set.summary.execution_context, replacement);
    assert_eq!(
        set.summary.events.last().unwrap().kind,
        "session_execution_context_updated"
    );

    let cleared = store
        .update_execution_context(
            &session.session_id,
            SessionExecutionContext::default(),
            SessionTransport::Mcp,
        )
        .unwrap();
    assert!(cleared.changed);
    assert_eq!(
        cleared.summary.execution_context,
        SessionExecutionContext::default()
    );

    let before_invalid = cleared.summary.events_total;
    let invalid = store
        .update_execution_context(
            &session.session_id,
            SessionExecutionContext {
                default_cwd: Some("../outside".to_string()),
                default_shell: None,
                resource: None,
            },
            SessionTransport::Api,
        )
        .unwrap_err();
    assert!(matches!(
        invalid,
        SessionExecutionContextUpdateError::InvalidExecutionContext(_)
    ));
    assert_eq!(
        store
            .summary(&session.session_id, None)
            .unwrap()
            .events_total,
        before_invalid
    );

    assert_eq!(
        store
            .update_execution_context(
                "wc_sess_missingcontext01",
                SessionExecutionContext::default(),
                SessionTransport::Api,
            )
            .unwrap_err(),
        SessionExecutionContextUpdateError::UnknownSession
    );
    let unscoped = store.start_session(None, Some("unscoped".to_string()));
    assert_eq!(
        store
            .update_execution_context(
                &unscoped.session_id,
                SessionExecutionContext::default(),
                SessionTransport::Api,
            )
            .unwrap_err(),
        SessionExecutionContextUpdateError::SessionHasNoProject
    );
    store.close_session(&session.session_id).unwrap();
    assert_eq!(
        store
            .update_execution_context(
                &session.session_id,
                SessionExecutionContext::default(),
                SessionTransport::Api,
            )
            .unwrap_err(),
        SessionExecutionContextUpdateError::SessionNotActive {
            lifecycle: SessionLifecycle::Closed
        }
    );
}

#[test]
fn persistent_shell_evidence_survives_restore_without_command_or_output() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("persistent shell evidence".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "session_shell_exec",
        &json!({
            "project": "agent:oe:private-drop",
            "session_id": session.session_id.clone(),
            "shell_id": "wc_shell_evidence",
            "command": "export PRIVATE_LEDGER_VALUE=secret",
            "command_summary": "export PRIVATE_LEDGER_VALUE=secret",
            "command_present": true
        }),
        session_tool_contract("session_shell_exec"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "shell_id": "wc_shell_evidence",
            "shell_state": "running",
            "execution_state": "completed",
            "command_started": true,
            "command_completed": true,
            "exit_code": 0,
            "stdout": "PRIVATE_LEDGER_VALUE=secret",
            "stderr": ""
        }),
        None,
        None,
    );
    store.flush_persistence();
    let persisted = std::fs::read_to_string(&ledger).unwrap();
    assert!(!persisted.contains("PRIVATE_LEDGER_VALUE"));
    assert!(!persisted.contains("\"stdout\""));
    assert!(!persisted.contains("\"stderr\""));

    let restored = SessionStore::with_persistence(ledger, 10, 10);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    let evidence = summary.events[1].persistent_shell.as_ref().unwrap();
    assert_eq!(evidence.action, "exec");
    assert_eq!(evidence.shell_id.as_deref(), Some("wc_shell_evidence"));
    assert_eq!(evidence.shell_state.as_deref(), Some("running"));
    assert_eq!(evidence.execution_state.as_deref(), Some("completed"));
    assert_eq!(evidence.command_started, Some(true));
    assert_eq!(evidence.command_completed, Some(true));
}

#[test]
fn validation_output_summary_survives_restore_sanitized() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("validation output".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(
        start,
        false,
        &json!({
            "exit_code": 101,
            "stdout": "full stdout body must not persist",
            "stderr": "full stderr body must not persist",
            "stdout_tail": "token=supersecret\nsafe stdout line\n",
            "stderr_tail": "Authorization: Bearer supersecret\nerror[E0308]: mismatched types\n --> src/lib.rs:12:5\n",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
        Some("tool failed"),
        None,
    );

    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    let output_summary = finished.validation_output_summary.as_ref().unwrap();
    let stdout_excerpt = output_summary["stdout_tail_excerpt"].as_str().unwrap();
    let stderr_excerpt = output_summary["stderr_tail_excerpt"].as_str().unwrap();

    assert_eq!(output_summary["tool_name"], "cargo_check");
    assert!(stdout_excerpt.contains("safe stdout line"));
    assert!(stderr_excerpt.contains("error[E0308]"));
    assert!(stderr_excerpt.contains("--> src/lib.rs:12:5"));
    for leaked in [
        "full stdout body must not persist",
        "full stderr body must not persist",
        "token=supersecret",
        "Authorization: Bearer supersecret",
    ] {
        assert!(
            !serde_json::to_string(output_summary)
                .unwrap()
                .contains(leaked),
            "restored validation_output_summary leaked {leaked}: {output_summary}"
        );
    }
    assert!(stdout_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
    assert!(stderr_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
    assert_eq!(output_summary["stdout_truncated"], true);
    assert_eq!(output_summary["stderr_truncated"], true);
}

#[test]
fn malicious_persisted_validation_output_summary_is_resanitized_on_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("malicious validation".to_string()));
    for tool_name in ["cargo_check", "run_shell"] {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool_name,
            &json!({"project": "agent:eval:demo"}),
            session_tool_contract(tool_name),
        );
        store.record_tool_call_finished(
            start,
            false,
            &json!({"exit_code": 101}),
            Some("tool failed"),
            None,
        );
    }

    store.flush_persistence();
    let mut ledger_value: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let events = ledger_value["sessions"][0]["events"]
        .as_array_mut()
        .unwrap();
    for event in events {
        if event["kind"] != "tool_call_finished" {
            continue;
        }
        let tool_name = event["tool_name"].clone();
        event["validation_output_summary"] = json!({
            "tool_name": tool_name,
            "stdout_tail_excerpt": format!(
                "token=abc\nsecret=abc\npassword=abc\napi_key=abc\n{}STDOUT_SAFE_END",
                "x".repeat(MAX_VALIDATION_EXCERPT_CHARS + 64)
            ),
            "stderr_tail_excerpt": format!(
                "authorization: basic abc\nbearer abc\nprivate key abc\naccess key abc\n{}STDERR_SAFE_END",
                "y".repeat(MAX_VALIDATION_EXCERPT_CHARS + 64)
            ),
            "stdout_truncated": false,
            "stderr_truncated": false,
            "max_excerpt_chars": 999999,
        });
    }
    std::fs::write(&ledger, serde_json::to_vec_pretty(&ledger_value).unwrap()).unwrap();

    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    let cargo_finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "cargo_check")
        .unwrap();
    let run_shell_finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "run_shell")
        .unwrap();
    for output_summary in [
        cargo_finished.validation_output_summary.as_ref().unwrap(),
        run_shell_finished
            .validation_output_summary
            .as_ref()
            .unwrap(),
    ] {
        let stdout_excerpt = output_summary["stdout_tail_excerpt"].as_str().unwrap();
        let stderr_excerpt = output_summary["stderr_tail_excerpt"].as_str().unwrap();
        let serialized = serde_json::to_string(output_summary).unwrap();

        assert!(stdout_excerpt.contains("STDOUT_SAFE_END"));
        assert!(stderr_excerpt.contains("STDERR_SAFE_END"));
        assert!(stdout_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
        assert!(stderr_excerpt.chars().count() <= MAX_VALIDATION_EXCERPT_CHARS);
        assert_eq!(
            output_summary["max_excerpt_chars"],
            MAX_VALIDATION_EXCERPT_CHARS
        );
        assert_eq!(output_summary["stdout_truncated"], true);
        assert_eq!(output_summary["stderr_truncated"], true);
        for leaked in [
            "token=abc",
            "secret=abc",
            "password=abc",
            "api_key=abc",
            "authorization: basic abc",
            "bearer abc",
            "private key abc",
            "access key abc",
        ] {
            assert!(
                !serialized.contains(leaked),
                "restored validation_output_summary leaked {leaked}: {serialized}"
            );
        }
    }
}

#[test]
fn tool_call_start_and_finish_share_one_call_id() {
    let store = SessionStore::new_in_memory(10, 20);
    let session = store.start_session(
        Some("agent:eval:demo".to_string()),
        Some("correlation".to_string()),
    );
    let start = store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "agent:eval:demo", "path": "src/lib.rs"}),
            session_tool_contract("read_file"),
        )
        .expect("start recorded");
    let call_id = start.call_id.clone();
    assert!(call_id.starts_with("wc_call_"));
    store.record_tool_call_finished(
        Some(start),
        true,
        &json!({"content": "omitted"}),
        None,
        None,
    );
    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    let tool_events = summary
        .events
        .iter()
        .filter(|event| event.kind.starts_with("tool_call_"))
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0].call_id.as_deref(), Some(call_id.as_str()));
    assert_eq!(tool_events[1].call_id.as_deref(), Some(call_id.as_str()));
}

#[test]
fn reused_logical_invocation_metadata_does_not_suppress_ambiguous_evidence() {
    let store = SessionStore::new_in_memory(10, 20);
    let session = store.start_session(Some("agent:eval:demo".to_string()), None);
    let mut business = ToolCallRecorderMetadata::default();
    business.assign_logical_invocation();
    business.mark_business_execution();

    for path in ["src/one.rs", "src/two.rs"] {
        let start = store.record_tool_call_started_with_metadata(
            Some(&session.session_id),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "agent:eval:demo", "path": path}),
            Some("agent:eval:demo".to_string()),
            business.clone(),
            session_tool_contract("read_file"),
        );
        store.record_tool_call_finished(start, true, &json!({"content": "omitted"}), None, None);
    }

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    assert_eq!(
        summary.counts.tool_calls, 2,
        "a duplicated valid-looking business correlation is ambiguous and must stay conservative"
    );
    let canonical = super::events::canonical_tool_call_finished_events(&summary.events);
    assert_eq!(canonical.len(), 2);
}

#[test]
fn legacy_session_events_without_observed_paths_restore_with_empty_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("legacy exploration".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_file",
        &json!({"project": "demo", "path": "src/legacy.rs"}),
        session_tool_contract("read_file"),
    );
    store.record_tool_call_finished(start, true, &json!({"content": "omitted"}), None, None);
    store.flush_persistence();

    let mut ledger_value: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    for event in ledger_value["sessions"][0]["events"]
        .as_array_mut()
        .unwrap()
    {
        event.as_object_mut().unwrap().remove("observed_paths");
    }
    std::fs::write(&ledger, serde_json::to_vec_pretty(&ledger_value).unwrap()).unwrap();

    let restored = SessionStore::with_persistence(ledger, 10, 10);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    assert!(summary
        .events
        .iter()
        .all(|event| event.observed_paths.is_empty()));
    assert_eq!(restored.status().restored_sessions, 1);
}

#[test]
fn legacy_session_events_without_validation_output_summary_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("legacy validation".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": "agent:eval:demo"}),
        session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(start, true, &json!({"exit_code": 0}), None, None);

    store.flush_persistence();
    let ledger_text = std::fs::read_to_string(&ledger).unwrap();
    assert!(
        !ledger_text.contains("validation_output_summary"),
        "legacy fixture should omit validation_output_summary: {ledger_text}"
    );
    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();

    assert_eq!(summary.counts.tool_calls, 1);
    assert_eq!(finished.tool_name, "cargo_check");
    assert!(finished.validation_output_summary.is_none());
}

#[test]
fn resolved_message_survives_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, None);
    let message = post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Todo,
        "finish persistence tests",
    );
    store
        .resolve_message(
            &session.session_id,
            &message.message_id,
            Some("covered".to_string()),
        )
        .unwrap();

    let restored = flush_and_restore(&store, ledger);
    let messages = restored
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: Some(SessionMessageKind::Todo),
                status: Some(SessionMessageStatus::Resolved),
                message_id: None,
                reply_to: None,
                limit: Some(10),
            },
        )
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].status, SessionMessageStatus::Resolved);
    assert_eq!(messages[0].resolution.as_deref(), Some("covered"));
    assert!(messages[0].resolved_at.is_some());
}

#[test]
fn corrupted_ledger_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    std::fs::write(&ledger, "{not valid json").unwrap();

    let store = persistent_store(ledger);
    let status = store.status();
    assert_eq!(status.persistence, "enabled");
    assert_eq!(status.restored_sessions, 0);
    assert!(status
        .last_persist_error
        .as_deref()
        .unwrap()
        .contains("restore_failed"));
    assert!(store.summary("wc_sess_missing", None).is_none());
}

#[test]
fn list_session_messages_filters_and_clamps_limit() {
    let store = SessionStore::default();
    let session = store.start_session(None, None);
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Guidance,
        "g1",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "p1",
    );
    post_message(&store, &session.session_id, SessionMessageKind::Risk, "r1");

    let guidance = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: Some(SessionMessageKind::Guidance),
                status: None,
                message_id: None,
                reply_to: None,
                limit: None,
            },
        )
        .unwrap();
    assert_eq!(guidance.len(), 1);
    assert_eq!(guidance[0].kind, SessionMessageKind::Guidance);

    let open = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: None,
                status: Some(SessionMessageStatus::Open),
                message_id: None,
                reply_to: None,
                limit: Some(usize::MAX),
            },
        )
        .unwrap();
    assert_eq!(open.len(), 3);
    assert_eq!(open[0].message, "r1");
}

#[test]
fn session_message_unknown_errors_are_explicit() {
    let store = SessionStore::default();
    let session = store.start_session(None, None);
    let unknown_session = store.post_message(PostSessionMessageInput {
        session_id: "wc_sess_missing".to_string(),
        kind: SessionMessageKind::Note,
        message: "hello".to_string(),
        tags: Vec::new(),
        reply_to: None,
        priority: SessionMessagePriority::Normal,
    });
    assert!(matches!(
        unknown_session,
        Err(SessionMessageError::UnknownSession)
    ));

    let unknown_message = store.resolve_message(&session.session_id, "wc_msg_missing", None);
    assert!(matches!(
        unknown_message,
        Err(SessionMessageError::UnknownMessage)
    ));
}

#[test]
fn session_summary_includes_bounded_message_summary() {
    let store = SessionStore::default();
    let session = store.start_session(None, None);
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Guidance,
        "g1",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "p1",
    );
    post_message(&store, &session.session_id, SessionMessageKind::Risk, "r1");
    post_message(&store, &session.session_id, SessionMessageKind::Todo, "t1");

    let summary = store.summary(&session.session_id, Some(50)).unwrap();
    assert_eq!(summary.messages.total, 4);
    assert_eq!(summary.messages.open, 4);
    assert_eq!(summary.messages.pending_guidance, 1);
    assert_eq!(summary.messages.open_risks, 1);
    assert_eq!(summary.messages.open_todos, 1);
    assert_eq!(summary.messages.recent_progress.len(), 1);
    assert!(serde_json::to_value(summary)
        .unwrap()
        .get("messages")
        .is_some());
}

/// Create-entry funnel: convenience wrappers must produce the same shape as
/// the authoritative `start_session_with_options` path.

#[test]
fn start_session_wrappers_funnel_to_single_create_entry() {
    let store = SessionStore::default();

    let via_start = store.start_session(Some("proj-a".to_string()), Some("t1".to_string()));
    let via_guards = store.start_session_with_guards(
        Some("proj-a".to_string()),
        Some("t2".to_string()),
        SessionMode::Normal,
        SessionGuards::default(),
    );
    let via_options = store
        .start_session_with_options(SessionCreateOptions::new(
            Some("proj-a".to_string()),
            Some("t3".to_string()),
            SessionMode::Normal,
            SessionGuards::default(),
        ))
        .unwrap();
    let via_read_only = store.start_session_with_guards(
        Some("proj-a".to_string()),
        Some("ro".to_string()),
        SessionMode::ReadOnly,
        SessionGuards::default(),
    );

    for summary in [&via_start, &via_guards, &via_options, &via_read_only] {
        assert!(summary.session_id.starts_with("wc_sess_"));
        assert!(store.contains_session(&summary.session_id));
        assert_eq!(summary.project.as_deref(), Some("proj-a"));
    }
    assert_eq!(via_start.mode, SessionMode::Normal);
    assert!(!via_start.guards.deny_write_tools);
    assert!(!via_start.guards.deny_shell_tools);
    assert_eq!(via_read_only.mode, SessionMode::ReadOnly);
    assert!(via_read_only.guards.deny_write_tools);
    assert!(via_read_only.guards.deny_shell_tools);
    assert!(store
        .guard_denial(
            &via_read_only.session_id,
            session_tool_contract("write_project_file")
        )
        .is_some());
    assert!(store
        .guard_denial(
            &via_start.session_id,
            session_tool_contract("write_project_file")
        )
        .is_none());
}

/// Unknown sessions never accept events or messages, and are not recreated.

#[test]
fn unknown_session_mutations_do_not_recreate_session() {
    let store = SessionStore::default();
    let missing = "wc_sess_does_not_exist";

    assert!(!store.contains_session(missing));
    assert!(store.summary(missing, None).is_none());
    assert!(store
        .record_tool_call_started(
            Some(missing),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "demo", "path": "a.rs"}),
            session_tool_contract("read_file"),
        )
        .is_none());
    assert!(!store.contains_session(missing));

    let post = store.post_message(PostSessionMessageInput {
        session_id: missing.to_string(),
        kind: SessionMessageKind::Note,
        message: "nope".to_string(),
        tags: Vec::new(),
        reply_to: None,
        priority: SessionMessagePriority::Normal,
    });
    assert!(matches!(post, Err(SessionMessageError::UnknownSession)));
    assert!(!store.contains_session(missing));

    let resolve = store.resolve_message(missing, "wc_msg_x", None);
    assert!(matches!(resolve, Err(SessionMessageError::UnknownSession)));
}

/// Evicted (capacity-bound) sessions stay gone: events must not revive them.

#[test]
fn evicted_session_is_not_reactivated_by_events_or_messages() {
    let store = SessionStore::new(1, 10);
    let first = store.start_session(None, Some("first".to_string()));
    let second = store.start_session(None, Some("second".to_string()));

    assert!(!store.contains_session(&first.session_id));
    assert!(store.contains_session(&second.session_id));

    assert!(store
        .record_tool_call_started(
            Some(&first.session_id),
            SessionTransport::Api,
            "read_file",
            &json!({"project": "demo", "path": "a.rs"}),
            session_tool_contract("read_file"),
        )
        .is_none());
    assert!(!store.contains_session(&first.session_id));
    assert!(store.summary(&first.session_id, None).is_none());

    let post = store.post_message(PostSessionMessageInput {
        session_id: first.session_id.clone(),
        kind: SessionMessageKind::Note,
        message: "revive?".to_string(),
        tags: Vec::new(),
        reply_to: None,
        priority: SessionMessagePriority::Normal,
    });
    assert!(matches!(post, Err(SessionMessageError::UnknownSession)));
    assert!(!store.contains_session(&first.session_id));
}

#[test]
fn session_message_create_list_and_resolve_contract() {
    let store = SessionStore::default();
    let session = store.start_session(None, None);
    let posted = store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "do the thing".to_string(),
            tags: vec!["work".to_string(), "constraint".to_string()],
            reply_to: None,
            priority: SessionMessagePriority::High,
        })
        .unwrap();
    assert!(posted.message_id.starts_with(MESSAGE_ID_PREFIX));
    assert_eq!(posted.session_id, session.session_id);
    assert_eq!(posted.kind, SessionMessageKind::Todo);
    assert_eq!(posted.status, SessionMessageStatus::Open);
    assert_eq!(posted.priority, SessionMessagePriority::High);
    assert_eq!(posted.message, "do the thing");
    assert_eq!(posted.tags, vec!["work", "constraint"]);

    let listed = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: Some(SessionMessageKind::Todo),
                status: Some(SessionMessageStatus::Open),
                message_id: None,
                reply_to: None,
                limit: Some(10),
            },
        )
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].message_id, posted.message_id);

    let resolved = store
        .resolve_message(
            &session.session_id,
            &posted.message_id,
            Some("shipped".to_string()),
        )
        .unwrap();
    assert_eq!(resolved.status, SessionMessageStatus::Resolved);
    assert_eq!(resolved.resolution.as_deref(), Some("shipped"));
    let first_resolved_at = resolved.resolved_at.expect("resolved_at set");

    let idempotent = store
        .resolve_message(&session.session_id, &posted.message_id, None)
        .unwrap();
    assert_eq!(idempotent.status, SessionMessageStatus::Resolved);
    assert_eq!(idempotent.resolved_at, Some(first_resolved_at));
    assert_eq!(idempotent.resolution.as_deref(), Some("shipped"));

    let updated = store
        .resolve_message(
            &session.session_id,
            &posted.message_id,
            Some("still done".to_string()),
        )
        .unwrap();
    assert_eq!(updated.status, SessionMessageStatus::Resolved);
    assert_eq!(updated.resolved_at, Some(first_resolved_at));
    assert_eq!(updated.resolution.as_deref(), Some("still done"));

    let open = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                kind: None,
                status: Some(SessionMessageStatus::Open),
                message_id: None,
                reply_to: None,
                limit: None,
            },
        )
        .unwrap();
    assert!(open.is_empty());
}

#[test]
fn wrapper_resolution_requires_ack_rejects_todo_and_replays_idempotently() {
    let store = SessionStore::default();
    let session = store.start_session(None, None);
    let guidance = store
        .post_message_with_ack(
            PostSessionMessageInput {
                session_id: session.session_id.clone(),
                kind: SessionMessageKind::Guidance,
                message: "apply the reviewed direction".to_string(),
                tags: Vec::new(),
                reply_to: None,
                priority: SessionMessagePriority::High,
            },
            true,
        )
        .unwrap();

    let missing_ack = store.resolve_message_from_wrapper(
        &session.session_id,
        &guidance.message_id,
        "handled".to_string(),
        false,
    );
    assert!(matches!(
        missing_ack,
        Err(SessionMessageError::InvalidInput(_))
    ));
    let still_open = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                message_id: Some(guidance.message_id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(still_open[0].status, SessionMessageStatus::Open);

    let resolved = store
        .resolve_message_from_wrapper(
            &session.session_id,
            &guidance.message_id,
            "handled".to_string(),
            true,
        )
        .unwrap();
    assert_eq!(resolved.status, SessionMessageStatus::Resolved);
    assert_eq!(resolved.resolution.as_deref(), Some("handled"));
    let resolved_at = resolved.resolved_at;

    let replay = store
        .resolve_message_from_wrapper(
            &session.session_id,
            &guidance.message_id,
            "handled".to_string(),
            false,
        )
        .unwrap();
    assert_eq!(replay.resolved_at, resolved_at);
    let conflict = store.resolve_message_from_wrapper(
        &session.session_id,
        &guidance.message_id,
        "different completion".to_string(),
        true,
    );
    assert!(matches!(
        conflict,
        Err(SessionMessageError::IdempotencyConflict)
    ));

    let todo = store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Todo,
            message: "finish the delegated task".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let todo_resolution = store.resolve_message_from_wrapper(
        &session.session_id,
        &todo.message_id,
        "done".to_string(),
        true,
    );
    assert!(matches!(
        todo_resolution,
        Err(SessionMessageError::InvalidInput(_))
    ));
    let retained_todo = store
        .list_messages(
            &session.session_id,
            ListSessionMessagesFilter {
                message_id: Some(todo.message_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(retained_todo[0].status, SessionMessageStatus::Open);
}

#[test]
fn read_only_guards_block_write_and_shell_classifications() {
    let store = SessionStore::default();
    let normal =
        store.start_session_with_guards(None, None, SessionMode::Normal, SessionGuards::default());
    let read_only = store.start_session_with_guards(
        None,
        None,
        SessionMode::ReadOnly,
        SessionGuards::default(),
    );
    assert!(store
        .guard_denial(
            &normal.session_id,
            session_tool_contract("write_project_file")
        )
        .is_none());
    assert!(store
        .guard_denial(&normal.session_id, session_tool_contract("run_shell"))
        .is_none());

    let write_denial = store
        .guard_denial(
            &read_only.session_id,
            session_tool_contract("write_project_file"),
        )
        .expect("write denied");
    assert_eq!(write_denial.guard, "deny_write_tools");
    assert_eq!(write_denial.mode, SessionMode::ReadOnly);

    let shell_denial = store
        .guard_denial(&read_only.session_id, session_tool_contract("run_shell"))
        .expect("shell denied");
    assert_eq!(shell_denial.guard, "deny_shell_tools");

    // Reads remain allowed under read_only.
    assert!(store
        .guard_denial(&read_only.session_id, session_tool_contract("read_file"))
        .is_none());
}

#[test]
fn validation_job_terminal_projects_typed_runner_lifecycle_without_absorbing_active_or_recovery() {
    let store = SessionStore::default();
    let project = "agent:workflow-session:runner-lifecycle".to_string();
    let session = store.start_session(Some(project.clone()), Some("runner lifecycle".to_string()));
    let target = "target:0123456789abcdef01234567";
    let cases = [
        ("completed", Some(0), Some(true), "succeeded", None),
        (
            "completed",
            Some(0),
            Some(false),
            "failed",
            Some("validation_failed"),
        ),
        (
            "failed",
            Some(1),
            Some(false),
            "failed",
            Some("command_exit_nonzero"),
        ),
        ("timeout", None, Some(false), "failed", Some("timeout")),
        ("timed_out", None, Some(false), "failed", Some("timeout")),
        ("stopped", None, Some(false), "failed", Some("cancelled")),
        ("cancelled", None, Some(false), "failed", Some("cancelled")),
        ("lost", None, Some(false), "failed", Some("execution_lost")),
    ];

    for (index, (job_status, exit_code, validation_passed, status, failure_kind)) in
        cases.into_iter().enumerate()
    {
        let job_id = format!("job-lifecycle-{index}");
        assert!(store.record_validation_job_terminal(
            &session.session_id,
            &job_id,
            &[job_id.as_str()],
            "cargo_check",
            session_tool_contract("cargo_check"),
            Some(project.clone()),
            target,
            None,
            job_status,
            exit_code,
            validation_passed,
            Some(90),
            Some(100 + index as i64),
            Some(10_000),
            None,
        ));
        let summary = store.summary(&session.session_id, None).unwrap();
        let event = summary
            .events
            .iter()
            .rev()
            .find(|event| event.job_id.as_deref() == Some(job_id.as_str()))
            .expect("terminal validation event");
        assert_eq!(event.status.as_deref(), Some(status), "{job_status}");
        assert_eq!(event.failure_kind.as_deref(), failure_kind, "{job_status}");
    }

    for (index, status) in [
        "queued",
        "agent_queued",
        "started",
        "running",
        "stop_requested",
        "recovering",
        "unknown",
    ]
    .into_iter()
    .enumerate()
    {
        let job_id = format!("nonterminal-lifecycle-{index}");
        assert!(
            !store.record_validation_job_terminal(
                &session.session_id,
                &job_id,
                &[job_id.as_str()],
                "cargo_check",
                session_tool_contract("cargo_check"),
                Some(project.clone()),
                target,
                None,
                status,
                None,
                None,
                Some(90),
                Some(200 + index as i64),
                Some(10_000),
                None,
            ),
            "{status} must not materialize terminal validation evidence"
        );
    }
}
