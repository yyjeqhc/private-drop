use super::*;
use crate::tool_runtime::tool_audit::{
    session_log_arguments_for_tool_request, session_log_result_for_tool,
};

#[test]
fn canonical_audit_policies_keep_private_payloads_out_of_durable_session_ledger() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = directory.path().join("audit-policies.json");
    let definitions: Vec<_> = webcodex_tool_contracts::tool_definitions().collect();
    // Keep every tested event so an early leak cannot be hidden by retention.
    let event_capacity = definitions.len() * 2 + 20;
    let store = SessionStore::with_persistence(&ledger, 10, event_capacity);
    let project = "agent:test:audit";
    let session = store.start_session(
        Some(project.to_string()),
        Some("Audit policy regression".into()),
    );
    let input = json!({
        "project":project,
        "body":"PRIVATE_BODY", "query":"PRIVATE_QUERY", "value":"PRIVATE_VALUE",
        "content":"PRIVATE_CONTENT", "text":"PRIVATE_TEXT", "instruction":"PRIVATE_PROMPT",
        "script":"PRIVATE_SCRIPT", "command":"echo PRIVATE_COMMAND", "stdin":"PRIVATE_STDIN",
        "args":["PRIVATE_NATIVE_DESTINATION"], "executable":"PRIVATE_EXECUTABLE",
        "package_base64":"PRIVATE_PACKAGE", "package":"webcodex",
        "idempotency_key":"PRIVATE_IDEMPOTENCY", "consume_token":"PRIVATE_CONSUME",
        "after_observation_token":"PRIVATE_OBSERVATION", "binding":"PRIVATE_BINDING",
        "arguments":{"opaque":"PRIVATE_PLUGIN_ARGUMENTS"},
        "target":{"host":"PRIVATE_HOST", "password":"PRIVATE_CREDENTIAL"},
        "native_path":"PRIVATE_NATIVE_PATH", "opaque":{"payload":"PRIVATE_OPAQUE"}
    });
    let output = json!({
        "project":project, "success":true, "state":"completed", "exit_code":0,
        "body":"PRIVATE_BODY", "text":"PRIVATE_TEXT", "value":"PRIVATE_VALUE",
        "content":"PRIVATE_CONTENT", "content_base64":"PRIVATE_IMAGE",
        "stdout":"PRIVATE_FULL_STDOUT", "stderr":"PRIVATE_FULL_STDERR",
        "raw_payload":{"body":"PRIVATE_TRACE_PAYLOAD"},
        "binding":"PRIVATE_BINDING", "native_path":"PRIVATE_NATIVE_PATH",
        "credentials":"PRIVATE_CREDENTIAL", "clipboard_text":"PRIVATE_CLIPBOARD",
        "accessibility":{"description":"PRIVATE_UI_CONTENT"}, "image":"PRIVATE_IMAGE",
        "events":[{"kind":"text", "text":"PRIVATE_CODING_EVENT"}],
        "messages":[{"body":"PRIVATE_MESSAGE_BODY"}],
        "entries":[{"value":"PRIVATE_MEMORY_VALUE"}],
        "skills":[{"body":"PRIVATE_SKILL_BODY", "path":"PRIVATE_SKILL_PATH"}]
    });
    for name in definitions
        .iter()
        .map(|definition| definition.name)
        .chain(["unknown_audit_tool"])
    {
        let mut tool_input = input.clone();
        let mut tool_output = output.clone();
        // Pre-existing bounded contracts: task-context instruction preview and
        // git-status porcelain stdout evidence.
        if name == "work_on_project" {
            tool_input["instruction"] = json!("Audit fixture task");
        }
        if name == "git_status" {
            tool_output["stdout"] = json!(" M src/lib.rs\n");
        }
        let arguments = session_log_arguments_for_tool_request(name, &tool_input);
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            name,
            &arguments,
            crate::tool_runtime::sessions::session_tool_contract(name),
        );
        assert!(start.is_some(), "{name}");
        let projected = session_log_result_for_tool(name, &tool_output);
        store.record_tool_call_finished(start, true, &projected, None, None);
    }
    store.flush_persistence();
    let persisted = std::fs::read_to_string(&ledger).unwrap();
    assert!(
        !persisted.contains("PRIVATE_"),
        "private sentinel in persisted audit ledger near {:?}",
        persisted
            .find("PRIVATE_")
            .map(|index| &persisted[index.saturating_sub(120)..(index + 80).min(persisted.len())])
    );
    assert!(persisted.contains("unknown_audit_tool"));
    let restored = SessionStore::with_persistence(&ledger, 10, event_capacity);
    let summary = serde_json::to_string(
        &restored
            .summary(&session.session_id, Some(event_capacity))
            .unwrap(),
    )
    .unwrap();
    assert!(!summary.contains("PRIVATE_"));
}

#[test]
fn canonical_audit_evidence_preserves_only_existing_bounded_execution_excerpts() {
    for name in ["read_files", "run_process"] {
        let directory = tempfile::tempdir().unwrap();
        let ledger = directory.path().join("execution-audit.json");
        let store = persistent_store(ledger.clone());
        let project = "agent:test:audit";
        let session = store.start_session(Some(project.to_string()), None);
        let raw_input = if name == "read_files" {
            json!({"project": project, "items": [{"path": "src/lib.rs"}]})
        } else {
            json!({"project":project, "executable":"printf", "purpose":"diagnostic"})
        };
        let input = session_log_arguments_for_tool_request(name, &raw_input);
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            name,
            &input,
            crate::tool_runtime::sessions::session_tool_contract(name),
        );
        let output = json!({
            "project":project, "exit_code":0, "execution_state":"completed",
            "stdout":"PRIVATE_FULL_STDOUT", "stderr":"PRIVATE_FULL_STDERR",
            // The existing validation excerpt retains the tail, not the head.
            "stdout_tail":format!("PRIVATE_OUTSIDE_BOUND\n{}\nallowed stdout witness\n", "x".repeat(20000)),
            "stderr_tail":"allowed stderr witness\n", "purpose":"diagnostic",
            "command_started":true, "command_completed":true
        });
        let projected = session_log_result_for_tool(name, &output);
        assert_eq!(
            projected, output,
            "declared canonical evidence must not become a sparse model projection"
        );
        store.record_tool_call_finished(start, true, &projected, None, None);
        store.flush_persistence();
        let persisted = std::fs::read_to_string(&ledger).unwrap();
        assert!(!persisted.contains("PRIVATE_"), "{name}");
        assert_eq!(
            persisted.contains("allowed stdout witness"),
            name == "run_process"
        );
        assert_eq!(
            persisted.contains("allowed stderr witness"),
            name == "run_process"
        );
    }
}
