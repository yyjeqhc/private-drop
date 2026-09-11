use super::super::project_instructions::{LoadedInstructionCandidate, ProjectInstructionsSnapshot};
use super::super::tool_inputs::SessionMode;
use super::events::{
    changed_paths_for_tool, normalize_observed_project_path, observed_paths_for_successful_result,
    session_input_summary_for_tool, SessionToolClassification,
};
use super::model::{PersistedSessionLedger, MAX_OBSERVED_PATHS_PER_EVENT, SESSION_LEDGER_VERSION};
use super::persistence::write_ledger_atomic;
use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

mod audit_policy;

fn record_model_facing_result(
    store: &SessionStore,
    session_id: &str,
    tool_name: &str,
    ack: SessionContextRevisionAck,
    success: bool,
    output: Value,
) -> RecordedModelFacingToolCall {
    let arguments = match tool_name {
        "read_files" => read_files_input("proj", "src/lib.rs"),
        "search_project_texts" => search_project_texts_input("proj", "needle", Some("src")),
        _ => json!({"project": "proj"}),
    };
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
            crate::tool_runtime::sessions::session_tool_contract(tool_name),
        )
        .expect("recorded call start");
    let session_output = super::super::tool_audit::session_log_result_for_tool(tool_name, &output);
    store
        .record_model_facing_tool_call_finished(
            Some(start),
            success,
            &session_output,
            (!success).then_some("business failure"),
            (!success).then_some("business_failure"),
        )
        .expect("recorded model-facing result")
}

fn persistent_store(path: PathBuf) -> SessionStore {
    SessionStore::with_persistence(path, 10, 10)
}

fn flush_and_restore(store: &SessionStore, path: PathBuf) -> SessionStore {
    store.flush_persistence();
    SessionStore::with_persistence(path, 10, 10)
}

fn read_files_input(project: &str, path: &str) -> Value {
    json!({"project": project, "items": [{"path": path}]})
}

fn search_project_texts_input(project: &str, pattern: &str, path: Option<&str>) -> Value {
    let mut query = json!({"pattern": pattern});
    if let Some(path) = path {
        query["path"] = json!(path);
    }
    json!({"project": project, "queries": [query]})
}

fn single_search_batch_output(output: Value) -> Value {
    json!({
        "items": [{
            "index": 0,
            "success": true,
            "output": output,
            "error": null
        }]
    })
}

fn record_console_tool(
    store: &SessionStore,
    session_id: &str,
    project: &str,
    tool: &str,
    input: Value,
    success: bool,
    output: Value,
) {
    let start = store.record_tool_call_started(
        Some(session_id),
        SessionTransport::Api,
        tool,
        &input,
        crate::tool_runtime::sessions::session_tool_contract(tool),
    );
    store.record_tool_call_finished(
        start,
        success,
        &output,
        (!success).then_some("tool failed"),
        (!success).then_some("validation_failed"),
    );
    debug_assert_eq!(input.get("project").and_then(Value::as_str), Some(project));
}

fn record_console_validation(
    store: &SessionStore,
    session_id: &str,
    project: &str,
    success: bool,
    tests_run_count: u64,
) {
    record_console_tool(
        store,
        session_id,
        project,
        "cargo_test",
        json!({"project": project}),
        success,
        json!({
            "exit_code": if success { 0 } else { 101 },
            "execution_state": "completed",
            "command_started": true,
            "command_completed": true,
            "tests_detected": true,
            "tests_run_count": tests_run_count,
            "zero_tests_run": tests_run_count == 0,
            "stdout_tail": if success { "test result: ok" } else { "test result: FAILED" },
            "stderr_tail": "",
            "stdout_lines": 1,
            "stderr_lines": 0,
            "failure_kind": if success { Value::Null } else { json!("test_failure") }
        }),
    );
}

#[test]
fn session_tool_classification_uses_definition_policy() {
    for (tool, risk_class) in [
        ("show_changes", "read_only"),
        ("start_session", "workflow_manage"),
        ("close_session", "session_collaborate"),
        ("workspace_checkpoint_create", "checkpoint_manage"),
        ("coding_agent_cancel", "run_control"),
        ("write_project_file", "project_write"),
        ("apply_unified_diff", "project_write"),
        ("run_process", "job_run"),
        ("run_script", "job_run"),
        ("run_shell", "job_run"),
        ("cargo_test", "job_run"),
        ("definitely_not_a_tool", "unknown"),
    ] {
        assert_eq!(
            SessionToolClassification::for_tool(tool).risk_class,
            risk_class,
            "{tool}"
        );
    }
}

#[test]
fn changed_paths_single_path_and_path_list_from_metadata() {
    assert_eq!(
        changed_paths_for_tool(
            "write_project_file",
            &json!({"project": "demo", "path": " src/lib.rs "}),
        ),
        vec!["src/lib.rs".to_string()]
    );
    assert_eq!(
        changed_paths_for_tool(
            "delete_project_files",
            &json!({"project": "demo", "paths": ["src/lib.rs", "", "src/lib.rs", "README.md"]}),
        ),
        vec!["src/lib.rs".to_string(), "README.md".to_string()]
    );
    assert_eq!(
        changed_paths_for_tool(
            "apply_text_edits",
            &json!({
                "project": "demo",
                "changes": [
                    {"kind": "edit", "path": "src/lib.rs"},
                    {"kind": "rename", "path": "old.rs", "to_path": "new.rs"}
                ]
            }),
        ),
        vec![
            "src/lib.rs".to_string(),
            "old.rs".to_string(),
            "new.rs".to_string()
        ]
    );
    assert_eq!(
        changed_paths_for_tool(
            "save_project_artifact",
            &json!({"project": "demo", "path": "out/image.png"}),
        ),
        vec!["out/image.png".to_string()]
    );
    assert!(
        changed_paths_for_tool("read_files", &read_files_input("demo", "src/lib.rs"),).is_empty()
    );
    assert!(changed_paths_for_tool(
        "apply_unified_diff",
        &json!({"project": "demo", "diff": "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n"}),
    )
    .is_empty());
}

#[test]
fn apply_unified_diff_finished_event_records_trusted_result_changed_paths() {
    let store = SessionStore::default();
    let session = store.start_session(Some("demo".to_string()), Some("patch evidence".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "apply_unified_diff",
        &json!({"project": "demo", "diff": "@@ -1 +1 @@\n-old\n+new\n"}),
        crate::tool_runtime::sessions::session_tool_contract("apply_unified_diff"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "state_changed": true,
            "affected_files": ["src/lib.rs", "src/lib.rs", "../escape.rs"]
        }),
        None,
        None,
    );

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "apply_unified_diff")
        .unwrap();
    assert_eq!(finished.changed_paths, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        finished
            .effect_evidence
            .as_ref()
            .and_then(|evidence| evidence.state_changed),
        Some(true)
    );
}

#[test]
fn checkpoint_restore_finished_event_records_trusted_result_changed_paths() {
    let store = SessionStore::default();
    let session = store.start_session(
        Some("demo".to_string()),
        Some("restore evidence".to_string()),
    );
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "workspace_checkpoint_restore",
        &json!({"project": "demo", "checkpoint_id": "wc_ckpt_demo", "confirm": true}),
        crate::tool_runtime::sessions::session_tool_contract("workspace_checkpoint_restore"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "state_changed": true,
            "changed_paths": ["src/lib.rs", "src/lib.rs", "../escape.rs"]
        }),
        None,
        None,
    );

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| {
            event.kind == "tool_call_finished" && event.tool_name == "workspace_checkpoint_restore"
        })
        .unwrap();
    assert_eq!(finished.changed_paths, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        finished
            .effect_evidence
            .as_ref()
            .and_then(|evidence| evidence.state_changed),
        Some(true)
    );
}

#[test]
fn exploration_paths_are_normalized_and_reject_escape_absolute_and_uri_forms() {
    for (raw, expected) in [
        (
            " src\\tool_runtime\\mod.rs ",
            Some("src/tool_runtime/mod.rs"),
        ),
        ("src/./lib.rs", Some("src/lib.rs")),
        ("", None),
        (".", None),
        ("/etc/passwd", None),
        ("../secret", None),
        ("src/../../secret", None),
        ("C:\\repo\\file.rs", None),
        ("\\\\server\\share\\file.rs", None),
        ("\\repo\\file.rs", None),
        ("file:///root/git/repo/src/lib.rs", None),
        ("https://example.test/source.rs", None),
        ("src/\0secret.rs", None),
        ("src/\nsecret.rs", None),
    ] {
        assert_eq!(
            normalize_observed_project_path(raw).as_deref(),
            expected,
            "{raw:?}"
        );
    }
}

#[test]
fn successful_reads_record_input_paths_but_failed_reads_do_not_finish_with_evidence() {
    let store = SessionStore::new(10, 20);
    let session = store.start_session(None, Some("read evidence".to_string()));

    let successful = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_files",
        &read_files_input("demo", "src\\lib.rs"),
        crate::tool_runtime::sessions::session_tool_contract("read_files"),
    );
    store.record_tool_call_finished(
        successful,
        true,
        &json!({"content": "not persisted"}),
        None,
        None,
    );

    let failed = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_files",
        &read_files_input("demo", "src/failed.rs"),
        crate::tool_runtime::sessions::session_tool_contract("read_files"),
    );
    store.record_tool_call_finished(
        failed,
        false,
        &json!({"content": "failed body must not become evidence"}),
        Some("read failed"),
        None,
    );

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    let finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished")
        .collect::<Vec<_>>();
    assert_eq!(finished[0].observed_paths, ["src/lib.rs"]);
    assert!(finished[1].observed_paths.is_empty());
    assert!(!serde_json::to_string(&summary)
        .unwrap()
        .contains("not persisted"));
    assert!(!serde_json::to_string(&summary)
        .unwrap()
        .contains("failed body must not become evidence"));
}

#[test]
fn search_result_modes_extract_only_known_structured_file_paths() {
    let cases = [
        (
            "matches",
            json!({
                "matches": [
                    {
                        "path": "src/matches.rs",
                        "line": 7,
                        "preview": "SEARCH_PREVIEW_MUST_NOT_PERSIST",
                        "context_before": ["SEARCH_CONTEXT_BEFORE_MUST_NOT_PERSIST"],
                        "context_after": ["SEARCH_CONTEXT_AFTER_MUST_NOT_PERSIST"]
                    },
                    {"path": "src/shared.rs", "line": 9, "preview": "duplicate"}
                ],
                "unrelated": {"path": "src/not-evidence.rs"}
            }),
            vec!["src/matches.rs", "src/shared.rs"],
        ),
        (
            "files_with_matches",
            json!({
                "files": [
                    {"path": "src/files.rs", "match_count": 4},
                    {"path": "src/shared.rs", "match_count": 1}
                ],
                "matches": []
            }),
            vec!["src/files.rs", "src/shared.rs"],
        ),
        (
            "count",
            json!({
                "files": [
                    {"path": "src/count.rs", "match_count": 8},
                    {"path": "../escape.rs", "match_count": 1}
                ],
                "total_match_count": 9
            }),
            vec!["src/count.rs"],
        ),
    ];

    for (mode, output, expected) in cases {
        let paths = observed_paths_for_successful_result(
            "search_project_texts",
            Vec::new(),
            &single_search_batch_output(output),
        );
        assert_eq!(paths, expected, "{mode}");
    }

    let bounded = observed_paths_for_successful_result(
        "search_project_texts",
        Vec::new(),
        &single_search_batch_output(json!({
            "files": (0..MAX_OBSERVED_PATHS_PER_EVENT + 5)
                .map(|index| json!({"path": format!("src/file-{index:03}.rs"), "match_count": 1}))
                .collect::<Vec<_>>()
        })),
    );
    assert_eq!(bounded.len(), MAX_OBSERVED_PATHS_PER_EVENT);
}

#[test]
fn lsp_observations_use_path_metadata_and_known_typed_result_locations_only() {
    let store = SessionStore::new(10, 20);
    let session = store.start_session(None, Some("lsp evidence".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "goto_definition",
        &json!({
            "project": "demo",
            "path": "src/caller.rs",
            "line": 4,
            "column": 9
        }),
        crate::tool_runtime::sessions::session_tool_contract("goto_definition"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "project": "demo",
            "path": "src/caller.rs",
            "query_position": {"line": 4, "column": 9},
            "locations": [
                {
                    "path": "src/definition.rs",
                    "range": {
                        "start": {"line": 1, "column": 1},
                        "end": {"line": 1, "column": 7}
                    }
                },
                {
                    "path": "src/shared.rs",
                    "range": {
                        "start": {"line": 2, "column": 1},
                        "end": {"line": 2, "column": 7}
                    }
                }
            ],
            "total_results": 2,
            "returned_count": 2,
            "truncated": false,
            "external_results_omitted": 0,
            "invalid_results_omitted": 0,
            "arbitrary": {"path": "src/not-evidence.rs"}
        }),
        None,
        None,
    );

    let summary = store.summary(&session.session_id, Some(20)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    assert_eq!(
        finished.observed_paths,
        ["src/caller.rs", "src/definition.rs", "src/shared.rs"]
    );
    assert!(!finished
        .observed_paths
        .iter()
        .any(|path| path == "src/not-evidence.rs"));
}

#[test]
fn exploration_input_audit_omits_queries_and_shell_commands() {
    let search = session_input_summary_for_tool(
        "search_project_texts",
        &search_project_texts_input(
            "demo",
            "RAW_SEARCH_PATTERN wc_pat_PRIVATE_TOKEN",
            Some("src"),
        ),
    );
    assert_eq!(search["queries"][0]["path"], "src");
    assert!(search["queries"][0].get("pattern").is_none());

    let symbols = session_input_summary_for_tool(
        "workspace_symbols",
        &json!({"project": "demo", "query": "RAW_SYMBOL_QUERY", "limit": 20}),
    );
    assert!(symbols.get("query").is_none());

    let shell = session_input_summary_for_tool(
        "run_shell",
        &json!({
            "project": "demo",
            "command": "printf RAW_SHELL_COMMAND",
            "command_summary": "printf RAW_SHELL_COMMAND",
            "command_present": true
        }),
    );
    assert_eq!(shell["command_present"], true);
    assert!(shell.get("command").is_none());
    assert!(shell.get("command_summary").is_none());

    let process = session_input_summary_for_tool(
        "run_process",
        &json!({
            "project": "demo",
            "executable": "RAW_EXECUTABLE",
            "args": ["RAW_ARG", "secret"],
            "stdin": "RAW_STDIN",
            "process_summary": "RAW_EXECUTABLE RAW_ARG",
            "executable_present": true,
            "arg_count": 2,
            "stdin_present": true,
            "purpose": "diagnostic"
        }),
    );
    assert_eq!(process["executable_present"], true);
    assert_eq!(process["arg_count"], 2);
    assert_eq!(process["stdin_present"], true);
    assert_eq!(process["purpose"], "diagnostic");
    assert!(process.get("executable").is_none());
    assert!(process.get("args").is_none());
    assert!(process.get("stdin").is_none());
    assert!(process.get("process_summary").is_none());

    let script = session_input_summary_for_tool(
        "run_script",
        &json!({
            "project": "demo",
            "language": "bash",
            "script": "RAW_SCRIPT_BODY",
            "args": ["RAW_ARG", "secret"],
            "stdin": "RAW_STDIN",
            "script_summary": "bash script (15 bytes, 2 args)",
            "script_bytes": 15,
            "arg_count": 2,
            "stdin_present": true
        }),
    );
    assert_eq!(script["language"], "bash");
    assert_eq!(script["script_bytes"], 15);
    assert_eq!(script["arg_count"], 2);
    assert_eq!(script["stdin_present"], true);
    assert!(script.get("script").is_none());
    assert!(script.get("args").is_none());
    assert!(script.get("stdin").is_none());
    assert!(script.get("script_summary").is_none());

    let persistent = session_input_summary_for_tool(
        "session_shell_exec",
        &json!({
            "project": "demo",
            "session_id": "wc_sess_demo",
            "shell_id": "wc_shell_demo",
            "command": "export PRIVATE_TOKEN=secret",
            "command_summary": "export PRIVATE_TOKEN=secret",
            "command_present": true
        }),
    );
    assert_eq!(persistent["command_present"], true);
    assert!(persistent.get("command").is_none());
    assert!(persistent.get("command_summary").is_none());
}

#[test]
fn coding_agent_lifecycle_evidence_is_project_scoped_body_free_and_durable() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let project = "agent:special:acp-test";
    let session = store.start_session(Some(project.to_string()), Some("ACP recorder".to_string()));
    let private_prompt = "PRIVATE_PROMPT_MUST_NOT_PERSIST";
    let private_reasoning = "PRIVATE_REASONING_MUST_NOT_PERSIST";

    assert!(store.record_coding_agent_lifecycle_evidence(
        &session.session_id,
        project,
        "wc_agent_run_recorder0001",
        "codex",
        "coding_agent_started",
        "running",
        "started",
        None,
        None,
    ));
    assert!(store.record_coding_agent_lifecycle_evidence(
        &session.session_id,
        project,
        "wc_agent_run_recorder0001",
        "codex",
        "coding_agent_waiting_permission",
        "waiting_permission",
        "started",
        None,
        None,
    ));
    assert!(store.record_coding_agent_lifecycle_evidence(
        &session.session_id,
        project,
        "wc_agent_run_recorder0001",
        "codex",
        "coding_agent_terminal",
        "completed",
        "completed",
        Some("end_turn"),
        None,
    ));
    assert!(!store.record_coding_agent_lifecycle_evidence(
        &session.session_id,
        "agent:special:other",
        "wc_agent_run_wrongproject",
        "codex",
        "coding_agent_started",
        "running",
        "started",
        None,
        None,
    ));

    // Sentinels model bodies that are intentionally absent from the lifecycle API.
    assert!(
        !serde_json::to_string(&store.summary(&session.session_id, Some(20)).unwrap())
            .unwrap()
            .contains(private_prompt)
    );
    assert!(
        !serde_json::to_string(&store.summary(&session.session_id, Some(20)).unwrap())
            .unwrap()
            .contains(private_reasoning)
    );

    let restored = flush_and_restore(&store, ledger.clone());
    let summary = restored.summary(&session.session_id, Some(20)).unwrap();
    let kinds = summary
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"coding_agent_started"));
    assert!(kinds.contains(&"coding_agent_waiting_permission"));
    assert!(kinds.contains(&"coding_agent_terminal"));
    let serialized = std::fs::read_to_string(&ledger).unwrap();
    assert!(!serialized.contains(private_prompt));
    assert!(!serialized.contains(private_reasoning));
    assert!(!serialized.contains("idempotency_key"));
    assert!(!serialized.contains("acp_session"));
    assert!(!serialized.contains("wc_agent_run_wrongproject"));
}

#[test]
fn skill_read_body_and_catalog_descriptions_never_enter_durable_session_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let project = "agent:special:skills-private";
    let session = store.start_session(
        Some(project.to_string()),
        Some("skills privacy".to_string()),
    );
    let private_body = "PRIVATE_SKILL_BODY_MUST_NOT_PERSIST";
    let private_description = "PRIVATE_SKILL_DESCRIPTION_MUST_NOT_PERSIST";

    let list_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "skill_list",
        &json!({"project": project, "query_present": false, "limit": 20}),
        crate::tool_runtime::sessions::session_tool_contract("skill_list"),
    );
    store.record_model_facing_tool_call_finished(
        list_start,
        true,
        &json!({
            "project": project,
            "catalog_revision": "wc_skillcat_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "total_count": 1,
            "returned_count": 1,
            "truncated": false,
            "invalid_count": 0,
            "discovery_truncated": false,
            "skills": [{"name": "private", "description": private_description}]
        }),
        None,
        None,
    );

    let read_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "skill_read_file",
        &json!({
            "project": project,
            "skill_id": "wc_skill_0123456789abcdef0123456789abcdef",
            "path": "SKILL.md",
            "start_line": 1,
            "limit": 20
        }),
        crate::tool_runtime::sessions::session_tool_contract("skill_read_file"),
    );
    store.record_model_facing_tool_call_finished(
        read_start,
        true,
        &json!({
            "project": project,
            "skill_id": "wc_skill_0123456789abcdef0123456789abcdef",
            "definition_revision": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "package_revision": "wc_skillpkg_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "path": "SKILL.md",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "text": private_body,
            "start_line": 1,
            "end_line": 2,
            "returned_lines": 2,
            "has_more": false,
            "next_start_line": null
        }),
        None,
        None,
    );

    let restored = flush_and_restore(&store, ledger.clone());
    let raw = std::fs::read_to_string(&ledger).unwrap();
    assert!(!raw.contains(private_body));
    assert!(!raw.contains(private_description));
    assert!(!raw.contains("\"skills\""));
    assert!(raw.contains("wc_skill_0123456789abcdef0123456789abcdef"));
    assert!(raw
        .contains("wc_skillpkg_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"));
    assert!(raw.contains("returned_lines"));
    assert!(raw.contains("catalog_revision"));

    let recovery =
        serde_json::to_string(&restored.summary(&session.session_id, Some(20)).unwrap()).unwrap();
    assert!(!recovery.contains(private_body));
    assert!(!recovery.contains(private_description));
}

#[test]
fn memory_body_summary_query_and_tags_never_enter_durable_session_ledger_or_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let project = "agent:special:memory-private";
    let session = store.start_session(
        Some(project.to_string()),
        Some("memory privacy".to_string()),
    );
    let private_query = "PRIVATE_MEMORY_QUERY_MUST_NOT_PERSIST";
    let private_summary = "PRIVATE_MEMORY_SUMMARY_MUST_NOT_PERSIST";
    let private_body = "PRIVATE_MEMORY_BODY_MUST_NOT_PERSIST";
    let private_tag = "PRIVATE_MEMORY_TAG_MUST_NOT_PERSIST";
    let memory_id = "wc_mem_0123456789abcdef0123456789abcdef";
    let revision = format!("wc_memrev_{}", "a".repeat(64));
    let catalog_revision = format!("wc_memcat_{}", "b".repeat(64));
    let private_principal_digest = format!("wc_memprincipal_{}", "c".repeat(64));
    let scope_id = format!("wc_memscope_{}", "d".repeat(64));
    let private_root_fingerprint = format!("wc_memroot_{}", "e".repeat(64));
    let private_native_root = "/PRIVATE/NATIVE/MEMORY/ROOT";

    let set_args = super::super::ToolCall::MemorySet {
        project: project.to_string(),
        memory_key: "deployment-policy".to_string(),
        summary: private_summary.to_string(),
        body: Some(private_body.to_string()),
        priority: Some("high".to_string()),
        bootstrap: Some(true),
        tags: Some(vec![private_tag.to_string()]),
        expected_revision: None,
        session_id: Some(session.session_id.clone()),
    }
    .session_log_arguments();
    let set_args_serialized = set_args.to_string();
    assert!(!set_args_serialized.contains(private_summary));
    assert!(!set_args_serialized.contains(private_body));
    assert!(!set_args_serialized.contains(private_tag));
    let set_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "memory_set",
        &set_args,
        crate::tool_runtime::sessions::session_tool_contract("memory_set"),
    );
    store.record_model_facing_tool_call_finished(
        set_start,
        true,
        &json!({
            "project": project,
            "memory_id": memory_id,
            "memory_key": "deployment-policy",
            "revision": revision,
            "created": true,
            "state_changed": true,
            "summary": private_summary,
            "body": private_body,
            "tags": [private_tag]
        }),
        None,
        None,
    );

    let search_args = super::super::tool_audit::session_log_arguments_for_tool_request(
        "memory_search",
        &json!({
            "project": project,
            "query": private_query,
            "tags": [private_tag],
            "limit": 20
        }),
    );
    assert!(!search_args.to_string().contains(private_query));
    assert!(!search_args.to_string().contains(private_tag));
    let search_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "memory_search",
        &search_args,
        crate::tool_runtime::sessions::session_tool_contract("memory_search"),
    );
    store.record_model_facing_tool_call_finished(
        search_start,
        true,
        &json!({
            "project": project,
            "catalog_revision": catalog_revision,
            "total_count": 1,
            "returned_count": 1,
            "offset": 0,
            "next_offset": null,
            "truncated": false,
            "memories": [{
                "memory_id": memory_id,
                "memory_key": "deployment-policy",
                "summary": private_summary,
                "priority": "high",
                "bootstrap": true,
                "tags": [private_tag],
                "revision": revision
            }]
        }),
        None,
        None,
    );

    let read_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "memory_read",
        &json!({
            "project": project,
            "memory_key": "deployment-policy",
            "expected_revision": revision
        }),
        crate::tool_runtime::sessions::session_tool_contract("memory_read"),
    );
    store.record_model_facing_tool_call_finished(
        read_start,
        true,
        &json!({
            "project": project,
            "memory_id": memory_id,
            "memory_key": "deployment-policy",
            "summary": private_summary,
            "body": private_body,
            "priority": "high",
            "bootstrap": true,
            "tags": [private_tag],
            "revision": revision,
            "created_at_unix_ms": 1,
            "updated_at_unix_ms": 2,
            "provenance": {
                "created_by_kind": "user",
                "updated_by_kind": "oauth2",
                "created_by_principal_digest": private_principal_digest,
                "updated_by_principal_digest": private_principal_digest
            }
        }),
        None,
        None,
    );

    let scope_list_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "memory_scope_list",
        &json!({"offset": 0, "limit": 50}),
        crate::tool_runtime::sessions::session_tool_contract("memory_scope_list"),
    );
    store.record_model_facing_tool_call_finished(
        scope_list_start,
        true,
        &json!({
            "total_count": 1,
            "returned_count": 1,
            "offset": 0,
            "next_offset": null,
            "truncated": false,
            "scopes": [{
                "memory_scope_id": scope_id,
                "identity_state": "attributed",
                "current_status": "not_current",
                "project_runtime_id": project,
                "runner_client_id": "private-runner",
                "root_fingerprint": private_root_fingerprint,
                "catalog_revision": catalog_revision,
                "summary": private_summary,
                "body": private_body,
                "native_root": private_native_root,
                "principal_digest": private_principal_digest
            }]
        }),
        None,
        None,
    );

    let purge_args = super::super::ToolCall::MemoryScopePurge {
        memory_scope_id: scope_id.clone(),
        expected_catalog_revision: catalog_revision.clone(),
        confirm: true,
    }
    .session_log_arguments();
    assert!(purge_args.get("confirm").is_none());
    let purge_start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Mcp,
        "memory_scope_purge",
        &purge_args,
        crate::tool_runtime::sessions::session_tool_contract("memory_scope_purge"),
    );
    store.record_model_facing_tool_call_finished(
        purge_start,
        true,
        &json!({
            "memory_scope_id": scope_id,
            "catalog_revision": catalog_revision,
            "purged_count": 1,
            "purged": true,
            "state_changed": true,
            "summary": private_summary,
            "body": private_body,
            "tags": [private_tag],
            "native_root": private_native_root,
            "root_fingerprint": private_root_fingerprint,
            "principal_digest": private_principal_digest
        }),
        None,
        None,
    );

    let restored = flush_and_restore(&store, ledger.clone());
    let raw = std::fs::read_to_string(&ledger).unwrap();
    for private in [
        private_query,
        private_summary,
        private_body,
        private_tag,
        private_principal_digest.as_str(),
        private_root_fingerprint.as_str(),
        private_native_root,
    ] {
        assert!(!raw.contains(private), "ledger leaked {private}");
    }
    assert!(raw.contains(memory_id));
    assert!(raw.contains(&revision));
    assert!(raw.contains(&catalog_revision));
    assert!(raw.contains(&scope_id));
    assert!(raw.contains("returned_body_bytes"));
    assert!(raw.contains("purged_count"));
    assert!(!raw.contains("\"memories\""));
    assert!(!raw.contains("\"scopes\""));
    assert!(!raw.contains("\"confirm\""));
    assert!(!raw.contains("\"purged\""));

    let recovery =
        serde_json::to_string(&restored.summary(&session.session_id, Some(30)).unwrap()).unwrap();
    for private in [
        private_query,
        private_summary,
        private_body,
        private_tag,
        private_principal_digest.as_str(),
        private_root_fingerprint.as_str(),
        private_native_root,
    ] {
        assert!(!recovery.contains(private), "recovery leaked {private}");
    }
}

#[test]
fn write_ledger_atomic_cleans_up_temp_file_when_rename_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger_path = tmp.path().join("sessions.json");
    std::fs::create_dir(&ledger_path).unwrap();
    let ledger = PersistedSessionLedger {
        version: SESSION_LEDGER_VERSION,
        sessions: Vec::new(),
    };

    let err = write_ledger_atomic(&ledger_path, &ledger).unwrap_err();
    assert_ne!(err.kind(), std::io::ErrorKind::NotFound);

    let temp_files: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".sessions.json.tmp-")
        })
        .collect();
    assert!(
        temp_files.is_empty(),
        "failed write left a temporary ledger"
    );
}

#[test]
fn legacy_session_events_without_call_id_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(
        Some("agent:eval:demo".to_string()),
        Some("legacy correlation".to_string()),
    );
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_files",
        &json!({"project": "agent:eval:demo", "path": "src/legacy.rs"}),
        crate::tool_runtime::sessions::session_tool_contract("read_files"),
    );
    store.record_tool_call_finished(start, true, &json!({"content": "omitted"}), None, None);
    store.flush_persistence();

    let mut ledger_value: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    for event in ledger_value["sessions"][0]["events"]
        .as_array_mut()
        .unwrap()
    {
        event.as_object_mut().unwrap().remove("call_id");
    }
    std::fs::write(&ledger, serde_json::to_vec_pretty(&ledger_value).unwrap()).unwrap();
    drop(store);

    let restored = SessionStore::with_persistence(ledger, 10, 20);
    let summary = restored.summary(&session.session_id, Some(20)).unwrap();
    assert_eq!(summary.counts.tool_calls, 1);
    assert!(summary.events.iter().all(|event| event.call_id.is_none()));
    let detail = restored
        .console_detail_for_project(
            "agent:eval:demo",
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.activity.len(), 1);
    assert_eq!(detail.activity[0].state, "succeeded");
    assert!(!detail.running_call);
}

#[test]
fn console_overview_counts_runtime_work_attention_and_sanitizes_reported_progress() {
    let store = SessionStore::new_in_memory(10, 80);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("overview".to_string()));

    for (tool, input, output) in [
        (
            "read_files",
            read_files_input(project, "src/lib.rs"),
            json!({"content": "RAW_FILE_CONTENT"}),
        ),
        (
            "search_project_texts",
            search_project_texts_input(project, "SECRET_PATTERN", Some("src")),
            single_search_batch_output(json!({"matches": [{"path": "src/lib.rs"}]})),
        ),
        (
            "goto_definition",
            json!({"project": project, "path": "src/lib.rs", "line": 1, "column": 1}),
            json!({"locations": [{"path": "src/lib.rs"}]}),
        ),
        (
            "apply_text_edits",
            json!({"project": project, "changes": [{"kind": "edit", "path": "src/lib.rs", "text": "SECRET_DIFF"}]}),
            json!({"changed_paths": ["src/lib.rs"]}),
        ),
        (
            "git_status",
            json!({"project": project}),
            json!({"status": "clean"}),
        ),
    ] {
        record_console_tool(
            &store,
            &session.session_id,
            project,
            tool,
            input,
            true,
            output,
        );
    }
    record_console_validation(&store, &session.session_id, project, true, 3);
    record_console_tool(
        &store,
        &session.session_id,
        project,
        "run_shell",
        json!({"project": project, "command": "printf SECRET_COMMAND"}),
        true,
        json!({
            "exit_code": 0,
            "execution_state": "completed",
            "command_started": true,
            "command_completed": true,
            "stdout_tail": "SECRET_STDOUT",
            "stderr_tail": "",
            "stdout_lines": 1,
            "stderr_lines": 0
        }),
    );

    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Guidance,
        "PRIVATE_GUIDANCE_BODY",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Question,
        "PRIVATE_QUESTION_BODY",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Risk,
        "PRIVATE_RISK_BODY",
    );
    let resolved_todo = post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Todo,
        "PRIVATE_RESOLVED_TODO_BODY",
    );
    store
        .resolve_message(&session.session_id, &resolved_todo.message_id, None)
        .unwrap();
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Todo,
        "PRIVATE_OPEN_TODO_BODY",
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "working in /root/private/work.rs",
    );

    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert_eq!(row.overview.work.exploration, 3);
    assert_eq!(row.overview.work.edits, 1);
    assert_eq!(row.overview.work.reviews, 1);
    assert_eq!(row.overview.work.validations, 1);
    assert_eq!(row.overview.work.runs, 1);
    assert!(row.overview.work.history_complete);
    assert!(!row.overview.work.history_truncated);
    assert_eq!(row.overview.validation.state, "passed");
    assert_eq!(row.overview.validation.latest_kind.as_deref(), Some("test"));
    assert_eq!(row.overview.validation.tests_run_count, Some(3));
    assert_eq!(row.overview.validation.unresolved_failure_count, 0);
    assert_eq!(row.overview.attention.open_guidance, 1);
    assert_eq!(row.overview.attention.open_questions, 1);
    assert_eq!(row.overview.attention.open_risks, 1);
    assert_eq!(row.overview.attention.open_todos, 1);
    assert!(row.overview.reported_progress.is_none());
    let list_serialized = serde_json::to_string(&list).unwrap();
    for hidden in [
        "PRIVATE_GUIDANCE_BODY",
        "PRIVATE_QUESTION_BODY",
        "PRIVATE_RISK_BODY",
        "PRIVATE_RESOLVED_TODO_BODY",
        "PRIVATE_OPEN_TODO_BODY",
        "working in /root/private/work.rs",
    ] {
        assert!(
            !list_serialized.contains(hidden),
            "list leaked message body {hidden}: {list_serialized}"
        );
    }

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(100),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let progress = detail.overview.reported_progress.as_ref().unwrap();
    assert!(
        progress.text.contains("[private path]"),
        "{}",
        progress.text
    );
    assert_eq!(detail.overview.validation.state, "passed");

    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "token=SUPER_SECRET_VALUE",
    );
    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(100),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(
        detail
            .overview
            .reported_progress
            .as_ref()
            .map(|progress| progress.text.as_str()),
        Some("[redacted]")
    );
    assert_eq!(detail.overview.validation.state, "passed");

    let serialized = serde_json::to_string(&detail).unwrap();
    for hidden in [
        "RAW_FILE_CONTENT",
        "SECRET_PATTERN",
        "SECRET_DIFF",
        "SECRET_COMMAND",
        "SECRET_STDOUT",
        "SUPER_SECRET_VALUE",
        "stdout_tail",
        "stderr_tail",
        "command_summary",
        "validation_output_summary",
    ] {
        assert!(
            !serialized.contains(hidden),
            "{hidden} leaked: {serialized}"
        );
    }
}

#[test]
fn console_overview_marks_retained_event_history_truncated_without_claiming_totals() {
    let store = SessionStore::new_in_memory(10, 4);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("recent work".to_string()));
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        record_console_tool(
            &store,
            &session.session_id,
            project,
            "read_files",
            read_files_input(project, path),
            true,
            json!({"content": "omitted"}),
        );
    }

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(100),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert!(detail.overview.work.history_truncated);
    assert!(!detail.overview.work.history_complete);
    assert_eq!(detail.overview.work.exploration, 2);
    assert_eq!(detail.overview.validation.state, "unavailable");
    assert!(detail.overview.validation.history_truncated);
    assert!(!detail.overview.validation.history_complete);
}

#[test]
fn console_overview_validation_reuses_terminal_and_failure_resolution_semantics() {
    let store = SessionStore::new_in_memory(10, 80);
    let project = "agent:eval:demo";

    let not_run = store.start_session(Some(project.to_string()), Some("not run".to_string()));
    let detail = store
        .console_detail_for_project(
            project,
            &not_run.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.overview.validation.state, "not_run");
    assert_eq!(detail.overview.validation.unresolved_failure_count, 0);

    let passed = store.start_session(Some(project.to_string()), Some("passed".to_string()));
    record_console_validation(&store, &passed.session_id, project, true, 4);
    let detail = store
        .console_detail_for_project(
            project,
            &passed.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.overview.validation.state, "passed");
    assert_eq!(
        detail.overview.validation.latest_kind.as_deref(),
        Some("test")
    );
    assert_eq!(detail.overview.validation.tests_run_count, Some(4));
    assert!(detail.overview.validation.latest_at.is_some());

    let failed = store.start_session(Some(project.to_string()), Some("failed".to_string()));
    record_console_validation(&store, &failed.session_id, project, false, 1);
    let detail = store
        .console_detail_for_project(
            project,
            &failed.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.overview.validation.state, "failed");
    assert_eq!(detail.overview.validation.unresolved_failure_count, 1);

    let resolved = store.start_session(Some(project.to_string()), Some("resolved".to_string()));
    record_console_validation(&store, &resolved.session_id, project, false, 1);
    record_console_validation(&store, &resolved.session_id, project, true, 5);
    let detail = store
        .console_detail_for_project(
            project,
            &resolved.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.overview.validation.state, "passed");
    assert_eq!(detail.overview.validation.unresolved_failure_count, 0);
    assert_eq!(detail.overview.validation.tests_run_count, Some(5));
}

#[test]
fn console_overview_running_validation_job_handoff_is_work_not_validation_outcome() {
    let store = SessionStore::new_in_memory(10, 40);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("handoff".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_test",
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract("cargo_test"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "execution_state": "running",
            "job_id": "11111111-2222-3333-4444-555555555555",
            "job_status": "running",
            "promoted_to_job": true,
            "command_started": true,
            "command_completed": false,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0
        }),
        None,
        None,
    );
    post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Progress,
        "validation passed according to the model",
    );

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(detail.overview.work.validations, 1);
    assert_eq!(detail.overview.validation.state, "unavailable");
    assert_eq!(detail.overview.validation.unresolved_failure_count, 0);
    assert!(detail.overview.reported_progress.is_some());
}

#[test]
fn console_overview_unfinished_validation_call_is_current_without_terminal_evidence() {
    let store = SessionStore::new_in_memory(10, 40);
    let project = "agent:eval:demo";
    let session = store.start_session(
        Some(project.to_string()),
        Some("unfinished validation".to_string()),
    );
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_test",
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract("cargo_test"),
    );
    assert!(start.is_some());

    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert!(row.running_call);
    let current = row.current_activity.as_ref().unwrap();
    assert_eq!(current.kind, "Tested");
    assert_eq!(current.state, "running");
    assert_eq!(row.overview.work.validations, 0);
    assert_eq!(row.overview.validation.state, "unavailable");
    assert_eq!(row.overview.validation.unresolved_failure_count, 0);
}

#[test]
fn console_projection_is_bounded_semantic_and_progress_is_informational() {
    let store = SessionStore::new_in_memory(20, 80);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("observe work".to_string()));

    let completed = [
        (
            "read_files",
            read_files_input(project, "src/lib.rs"),
            json!({"content": "RAW_FILE_CONTENT"}),
        ),
        (
            "search_project_texts",
            search_project_texts_input(project, "SECRET_PATTERN", Some("src")),
            single_search_batch_output(json!({"matches": [{"path": "src/lib.rs"}]})),
        ),
        (
            "apply_text_edits",
            json!({"project": project, "changes": [{"kind": "edit", "path": "src/lib.rs"}]}),
            json!({"changed_paths": ["src/lib.rs"]}),
        ),
        (
            "cargo_test",
            json!({"project": project}),
            json!({"exit_code": 0, "stdout_tail": "test result: ok", "stderr_tail": "", "stdout_lines": 1, "stderr_lines": 0}),
        ),
    ];
    for (tool, input, output) in completed {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool,
            &input,
            crate::tool_runtime::sessions::session_tool_contract(tool),
        );
        store.record_tool_call_finished(start, true, &output, None, None);
    }
    let running = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "goto_definition",
        &json!({"project": project, "path": "src/lib.rs", "line": 1, "column": 1}),
        crate::tool_runtime::sessions::session_tool_contract("goto_definition"),
    );
    assert!(running.is_some());
    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: "validation is next".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(200),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let kinds = detail
        .activity
        .iter()
        .map(|item| item.kind.as_str())
        .collect::<Vec<_>>();
    for expected in ["Edited", "Tested", "Navigated", "Progress"] {
        assert!(kinds.contains(&expected), "missing {expected}: {kinds:?}");
    }
    if let Some(explored) = detail.activity.iter().find(|item| item.kind == "Explored") {
        assert_eq!(explored.group_count, Some(2));
        assert_eq!(explored.group_kinds, vec!["Read", "Searched"]);
        assert!(explored.group_tools.contains(&"read_files".to_string()));
        assert!(explored
            .group_tools
            .contains(&"search_project_texts".to_string()));
    } else {
        // If Progress shares this coarse timestamp, conservative grouping keeps
        // the exploration facts separate rather than crossing an unordered
        // Progress boundary.
        assert!(kinds.contains(&"Read"));
        assert!(kinds.contains(&"Searched"));
    }
    assert!(detail.running_call);
    let progress = detail
        .activity
        .iter()
        .find(|item| item.kind == "Progress")
        .unwrap();
    assert_eq!(progress.state, "info");
    let serialized = serde_json::to_string(&detail).unwrap();
    for secret in [
        "RAW_FILE_CONTENT",
        "SECRET_PATTERN",
        "stdout_tail",
        "stderr_tail",
    ] {
        assert!(
            !serialized.contains(secret),
            "{secret} leaked: {serialized}"
        );
    }

    let bounded = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(2),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(bounded.activity_returned, 2);
    assert!(bounded.activity_total > bounded.activity_returned);
    assert!(bounded.activity_truncated);

    let other = store.start_session(
        Some("agent:elsewhere:hidden".to_string()),
        Some("hidden".to_string()),
    );
    assert!(store
        .console_detail_for_project(
            project,
            &other.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks()
        )
        .is_none());
    let _second_visible = store.start_session(
        Some(project.to_string()),
        Some("second visible".to_string()),
    );
    let list = store.console_list_for_project(
        project,
        Some(1),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    assert_eq!(list.returned, 1);
    assert_eq!(list.total, 2);
    assert!(list.truncated);
    assert!(list
        .sessions
        .iter()
        .all(|row| row.session_id != other.session_id));
}

#[test]
fn console_list_orders_recent_activity_first_with_deterministic_session_id_ties() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let project = "agent:eval:ordering";
    let older = "wc_sess_order_old";
    let tie_a = "wc_sess_order_tie_a";
    let tie_z = "wc_sess_order_tie_z";

    let seed = persistent_store(ledger.clone());
    let seed_older = seed.start_session(Some(project.to_string()), Some("older".to_string()));
    let seed_tie_a = seed.start_session(Some(project.to_string()), Some("tie a".to_string()));
    let seed_tie_z = seed.start_session(Some(project.to_string()), Some("tie z".to_string()));
    seed.flush_persistence();
    drop(seed);

    let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    for (seed_id, session_id, created_at, updated_at) in [
        (seed_older.session_id.as_str(), older, 1, 10),
        (seed_tie_a.session_id.as_str(), tie_a, 2, 20),
        (seed_tie_z.session_id.as_str(), tie_z, 3, 20),
    ] {
        let record = raw["sessions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|record| record["session_id"] == seed_id)
            .unwrap();
        record["session_id"] = Value::String(session_id.to_string());
        record["title"] = Value::String(session_id.to_string());
        record["created_at"] = Value::from(created_at);
        record["updated_at"] = Value::from(updated_at);
    }
    std::fs::write(&ledger, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let store = persistent_store(ledger);

    let initial = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    assert_eq!(
        initial
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![tie_z, tie_a, older],
        "equal updated_at must keep the existing descending session-id tie-break"
    );

    let activity = store
        .record_tool_call_started(
            Some(older),
            SessionTransport::Api,
            "read_files",
            &json!({"project": project, "path": "src/lib.rs"}),
            crate::tool_runtime::sessions::session_tool_contract("read_files"),
        )
        .expect("older Session activity should be recorded");
    assert_eq!(activity.session_id, older);

    let refreshed = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    assert_eq!(refreshed.sessions[0].session_id, older);
    assert!(refreshed.sessions[0].updated_at > refreshed.sessions[1].updated_at);
    assert_eq!(refreshed.sessions[1].session_id, tie_z);
    assert_eq!(refreshed.sessions[2].session_id, tie_a);
}

#[test]
fn console_list_uses_only_unfinished_call_as_now_and_keeps_job_handoff_as_last() {
    let store = SessionStore::new_in_memory(10, 40);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("live list".to_string()));

    let read = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_files",
        &json!({"project": project, "path": "src/lib.rs"}),
        crate::tool_runtime::sessions::session_tool_contract("read_files"),
    );
    store.record_tool_call_finished(read, true, &json!({"content": "omitted"}), None, None);

    let job_id = "11111111-2222-3333-4444-555555555555";
    let cargo = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_test",
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract("cargo_test"),
    );
    store.record_tool_call_finished(
        cargo,
        true,
        &json!({
            "execution_state": "running",
            "job_id": job_id,
            "job_status": "running",
            "promoted_to_job": true,
            "command_started": true,
            "command_completed": false,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0
        }),
        None,
        None,
    );
    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: "this message must not replace runtime activity".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();

    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert!(!row.running_call);
    assert!(row.current_activity.is_none());
    let last = row.last_activity.as_ref().unwrap();
    assert_eq!(last.tool.as_deref(), Some("cargo_test"));
    assert_eq!(last.state, "running");
    assert_eq!(last.execution_state.as_deref(), Some("running"));
    assert!(last.job_handoff);
    assert_eq!(last.job_id.as_deref(), Some(job_id));
    assert!(last
        .summary
        .as_deref()
        .unwrap_or_default()
        .contains("handed off to Job"));

    let running = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "goto_definition",
        &json!({"project": project, "path": "src/lib.rs", "line": 1, "column": 1}),
        crate::tool_runtime::sessions::session_tool_contract("goto_definition"),
    );
    assert!(running.is_some());
    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert!(row.running_call);
    assert_eq!(
        row.current_activity.as_ref().unwrap().tool.as_deref(),
        Some("goto_definition")
    );
    assert_eq!(row.current_activity.as_ref().unwrap().state, "running");
    assert_eq!(
        row.last_activity.as_ref().unwrap().tool.as_deref(),
        Some("cargo_test")
    );
}

#[test]
fn console_list_keeps_started_run_job_handoff_historical_and_later_activity_becomes_last() {
    let store = SessionStore::new_in_memory(10, 30);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("async job".to_string()));
    let job_id = "12345678-1234-5678-9abc-123456789abc";
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "run_job",
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract("run_job"),
    );
    store.record_tool_call_finished(
        start,
        true,
        &json!({
            "execution_state": "started",
            "job_id": job_id,
            "status": "running",
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0
        }),
        None,
        None,
    );

    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert!(row.current_activity.is_none());
    let last = row.last_activity.as_ref().unwrap();
    assert_eq!(last.tool.as_deref(), Some("run_job"));
    assert!(last.job_handoff);
    assert_eq!(last.state, "running");
    assert_eq!(last.execution_state.as_deref(), Some("started"));
    assert_eq!(last.job_id.as_deref(), Some(job_id));

    // A later authoritative Job observation may say terminal completed while
    // the original handoff event remains the historical `started` snapshot.
    let observed = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "job_status",
        &json!({"project": project, "job_id": job_id}),
        crate::tool_runtime::sessions::session_tool_contract("job_status"),
    );
    store.record_tool_call_finished(
        observed,
        true,
        &json!({"job_id": job_id, "status": "completed", "execution_state": "completed"}),
        None,
        None,
    );

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let handoff = detail
        .activity
        .iter()
        .find(|activity| activity.tool.as_deref() == Some("run_job"))
        .unwrap();
    assert_eq!(handoff.execution_state.as_deref(), Some("started"));

    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert!(row.current_activity.is_none());
    assert_eq!(
        row.last_activity.as_ref().unwrap().tool.as_deref(),
        Some("job_status")
    );

    store.close_session(&session.session_id).unwrap();
    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert_eq!(row.lifecycle, "closed");
    assert!(row.current_activity.is_none());
    assert_eq!(
        row.last_activity.as_ref().unwrap().tool.as_deref(),
        Some("job_status")
    );
}

#[test]
fn console_list_without_running_work_shows_last_meaningful_activity() {
    let store = SessionStore::new_in_memory(10, 20);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("last activity".to_string()));
    for (tool, input, output) in [
        (
            "read_files",
            json!({"project": project, "path": "src/lib.rs"}),
            json!({"content": "omitted"}),
        ),
        (
            "git_status",
            json!({"project": project}),
            json!({"status": "clean"}),
        ),
    ] {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool,
            &input,
            crate::tool_runtime::sessions::session_tool_contract(tool),
        );
        store.record_tool_call_finished(start, true, &output, None, None);
    }
    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: "done inspecting".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    store.close_session(&session.session_id).unwrap();
    let list = store.console_list_for_project(
        project,
        Some(10),
        crate::tool_runtime::sessions::console_validation_hooks(),
    );
    let row = list
        .sessions
        .iter()
        .find(|row| row.session_id == session.session_id)
        .unwrap();
    assert_eq!(row.lifecycle, "closed");
    assert!(row.current_activity.is_none());
    assert_eq!(
        row.last_activity.as_ref().unwrap().tool.as_deref(),
        Some("git_status")
    );
    assert_eq!(row.last_activity.as_ref().unwrap().kind, "Reviewed");
}

#[test]
fn console_exploration_grouping_is_ordered_bounded_and_stops_at_fact_barriers() {
    let store = SessionStore::new_in_memory(10, 80);
    let project = "agent:eval:demo";
    let session = store.start_session(
        Some(project.to_string()),
        Some("group activity".to_string()),
    );

    for (tool, input, output) in [
        (
            "read_files",
            read_files_input(project, "src/a.rs"),
            json!({"content": "PRIVATE_CONTENT_A"}),
        ),
        (
            "search_project_texts",
            search_project_texts_input(project, "PRIVATE_PATTERN", Some("src")),
            single_search_batch_output(json!({"matches": [{"path": "src/b.rs"}]})),
        ),
        (
            "goto_definition",
            json!({"project": project, "path": "src/c.rs", "line": 1, "column": 1}),
            json!({"locations": [{"path": "src/c.rs"}]}),
        ),
    ] {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool,
            &input,
            crate::tool_runtime::sessions::session_tool_contract(tool),
        );
        store.record_tool_call_finished(start, true, &output, None, None);
    }

    let edit = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "apply_text_edits",
        &json!({"project": project, "changes": [{"kind": "edit", "path": "src/a.rs"}]}),
        crate::tool_runtime::sessions::session_tool_contract("apply_text_edits"),
    );
    store.record_tool_call_finished(
        edit,
        true,
        &json!({"changed_paths": ["src/a.rs"]}),
        None,
        None,
    );

    let read = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "read_files",
        &read_files_input(project, "src/d.rs"),
        crate::tool_runtime::sessions::session_tool_contract("read_files"),
    );
    store.record_tool_call_finished(
        read,
        true,
        &json!({"content": "PRIVATE_CONTENT_D"}),
        None,
        None,
    );
    let failed = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "search_project_texts",
        &search_project_texts_input(project, "PRIVATE_FAILED_PATTERN", Some("src")),
        crate::tool_runtime::sessions::session_tool_contract("search_project_texts"),
    );
    store.record_tool_call_finished(
        failed,
        false,
        &json!({"failure_kind": "search_failed"}),
        Some("search failed"),
        Some("search_failed"),
    );

    let job_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let job = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "cargo_check",
        &json!({"project": project}),
        crate::tool_runtime::sessions::session_tool_contract("cargo_check"),
    );
    store.record_tool_call_finished(
        job,
        true,
        &json!({
            "execution_state": "running",
            "job_id": job_id,
            "promoted_to_job": true,
            "command_started": true,
            "command_completed": false,
            "stdout_tail": "",
            "stderr_tail": "",
            "stdout_lines": 0,
            "stderr_lines": 0
        }),
        None,
        None,
    );
    // Without an unordered Progress message, adjacent successful exploration
    // groups normally and concrete fact barriers remain separate.
    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let kinds = detail
        .activity
        .iter()
        .map(|activity| activity.kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["Explored", "Edited", "Read", "Searched", "Tested"]
    );
    let group = &detail.activity[0];
    assert_eq!(group.group_count, Some(3));
    assert_eq!(group.group_kinds, vec!["Read", "Searched", "Navigated"]);
    assert_eq!(
        group.group_tools,
        vec!["read_files", "search_project_texts", "goto_definition"]
    );
    assert_eq!(group.paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert_eq!(detail.activity[3].state, "failed");
    assert!(detail.activity[4].job_handoff);
    assert_eq!(
        detail.activity[4].execution_state.as_deref(),
        Some("running")
    );
    assert_eq!(detail.activity[4].job_id.as_deref(), Some(job_id));

    let bounded = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(3),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    assert_eq!(bounded.activity.len(), 3);
    assert!(bounded.activity_truncated);

    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: "waiting for validation".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let with_progress = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let progress = with_progress
        .activity
        .iter()
        .find(|activity| activity.kind == "Progress")
        .unwrap();
    assert_eq!(progress.state, "info");
    let serialized = serde_json::to_string(&with_progress).unwrap();
    for hidden in [
        "PRIVATE_CONTENT_A",
        "PRIVATE_CONTENT_D",
        "PRIVATE_PATTERN",
        "PRIVATE_FAILED_PATTERN",
    ] {
        assert!(
            !serialized.contains(hidden),
            "{hidden} leaked: {serialized}"
        );
    }
}

#[test]
fn console_projection_preserves_job_backed_execution_truth_and_safe_job_ids() {
    let store = SessionStore::new_in_memory(10, 40);
    let project = "agent:eval:demo";
    let session = store.start_session(Some(project.to_string()), Some("job facts".to_string()));
    let real_job_id = "11111111-2222-3333-4444-555555555555";

    for (tool, output) in [
        (
            "cargo_test",
            json!({
                "execution_state": "running",
                "job_id": real_job_id,
                "job_status": "running",
                "promoted_to_job": true,
                "command_started": true,
                "command_completed": false,
                "stdout_tail": "",
                "stderr_tail": "",
                "stdout_lines": 0,
                "stderr_lines": 0
            }),
        ),
        (
            "run_process",
            json!({
                "execution_state": "completed",
                "command_started": true,
                "command_completed": true,
                "exit_code": 0,
                "stdout_tail": "",
                "stderr_tail": "",
                "stdout_lines": 0,
                "stderr_lines": 0
            }),
        ),
        (
            "run_process",
            json!({
                "execution_state": "outcome_unknown",
                "command_started": true,
                "command_completed": false,
                "stdout_tail": "",
                "stderr_tail": "",
                "stdout_lines": 0,
                "stderr_lines": 0
            }),
        ),
        (
            "run_process",
            json!({
                "execution_state": "timed_out",
                "command_started": true,
                "command_completed": false,
                "stdout_tail": "",
                "stderr_tail": "",
                "stdout_lines": 0,
                "stderr_lines": 0
            }),
        ),
        (
            "run_job",
            json!({
                "execution_state": "running",
                "job_id": "../unsafe-job",
                "command_started": true,
                "command_completed": false,
                "stdout_tail": "",
                "stderr_tail": "",
                "stdout_lines": 0,
                "stderr_lines": 0
            }),
        ),
    ] {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool,
            &json!({"project": project}),
            crate::tool_runtime::sessions::session_tool_contract(tool),
        );
        store.record_tool_call_finished(start, true, &output, None, None);
    }

    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(40),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let states = detail
        .activity
        .iter()
        .map(|activity| activity.state.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            "running",
            "succeeded",
            "outcome_unknown",
            "timed_out",
            "running"
        ]
    );
    assert_eq!(detail.activity[0].job_id.as_deref(), Some(real_job_id));
    assert!(detail.activity[4].job_id.is_none());
    let serialized = serde_json::to_string(&detail).unwrap();
    assert!(!serialized.contains("../unsafe-job"));
}

#[test]
fn console_title_and_progress_redact_absolute_private_paths() {
    let store = SessionStore::new_in_memory(10, 20);
    let project = "agent:eval:demo";
    let session = store.start_session(
        Some(project.to_string()),
        Some("inspect /etc/passwd /opt/tool C:\\Users\\alice\\secret \\\\server\\share\\x file:///tmp/item".to_string()),
    );
    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: "checked path=/etc/hosts /opt/config D:/private/data \\\\host\\share\\file file:///var/tmp/x".to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let serialized = serde_json::to_string(&detail).unwrap();
    for private in [
        "/etc/passwd",
        "/opt/tool",
        "C:\\\\Users",
        "\\\\\\\\server",
        "file:///tmp/item",
        "/etc/hosts",
        "/opt/config",
        "D:/private/data",
        "\\\\\\\\host",
        "file:///var/tmp/x",
    ] {
        assert!(
            !serialized.contains(private),
            "private path leaked: {serialized}"
        );
    }
    assert!(serialized.contains("[private path]"));
}

#[test]
fn console_title_and_progress_redact_json_wrapped_private_paths() {
    let store = SessionStore::new_in_memory(10, 20);
    let project = "agent:eval:demo";
    let session = store.start_session(
        Some(project.to_string()),
        Some(r#"inspect {"path":"/etc/passwd"}"#.to_string()),
    );
    store
        .post_message(PostSessionMessageInput {
            session_id: session.session_id.clone(),
            kind: SessionMessageKind::Progress,
            message: r#"checked {"cwd":"C:\\Users\\alice\\secret"}"#.to_string(),
            tags: Vec::new(),
            reply_to: None,
            priority: SessionMessagePriority::Normal,
        })
        .unwrap();
    let detail = store
        .console_detail_for_project(
            project,
            &session.session_id,
            Some(20),
            crate::tool_runtime::sessions::console_validation_hooks(),
        )
        .unwrap();
    let serialized = serde_json::to_string(&detail).unwrap();
    for private in ["/etc/passwd", "C:\\\\Users"] {
        assert!(
            !serialized.contains(private),
            "JSON-wrapped private path leaked: {serialized}"
        );
    }
    assert!(serialized.contains("[private path]"));
}

#[test]
fn failure_history_legacy_event_without_effect_evidence_restores_conservatively() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("legacy effect evidence".to_string()));
    let start = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "apply_text_edits",
        &json!({"project": "agent:eval:demo", "changes": []}),
        crate::tool_runtime::sessions::session_tool_contract("apply_text_edits"),
    );
    store.record_tool_call_finished(
        start,
        false,
        &json!({
            "failure_kind": "stale_precondition",
            "state_changed": false
        }),
        Some("guard rejected"),
        None,
    );
    store.flush_persistence();

    let mut ledger_value: Value =
        serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    let events = ledger_value["sessions"][0]["events"]
        .as_array_mut()
        .unwrap();
    let finished = events
        .iter_mut()
        .find(|event| event["kind"] == "tool_call_finished")
        .unwrap();
    assert!(finished.get("effect_evidence").is_some());
    finished.as_object_mut().unwrap().remove("effect_evidence");
    std::fs::write(&ledger, serde_json::to_vec_pretty(&ledger_value).unwrap()).unwrap();
    drop(store);

    let restored = SessionStore::with_persistence(ledger, 10, 10);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    let finished = summary
        .events
        .iter()
        .find(|event| event.kind == "tool_call_finished")
        .unwrap();
    assert!(finished.effect_evidence.is_none());
    let projected = crate::tool_runtime::handoff::reconcile_closeout_evidence(
        &json!({"unexpected_count": 1}),
        &summary,
        &json!({}),
    )
    .tool_failures;
    assert_eq!(projected["non_actionable_unexpected_count"], 0);
    assert_eq!(projected["actionable_unexpected_count"], 1);
}

#[test]
fn exploration_ledger_persists_only_bounded_relative_paths_and_safe_metadata() {
    const PRIVATE_ROOT: &str = "/root/git/private-drop";
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = SessionStore::with_persistence(&ledger, 10, 30);
    let session = store.start_session(None, Some("private exploration".to_string()));

    let search = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "search_project_texts",
        &search_project_texts_input(
            "demo",
            "RAW_SEARCH_PATTERN wc_pat_PRIVATE_TOKEN",
            Some("src"),
        ),
        crate::tool_runtime::sessions::session_tool_contract("search_project_texts"),
    );
    store.record_tool_call_finished(
        search,
        true,
        &single_search_batch_output(json!({
            "matches": [{
                "path": "src/search.rs",
                "line": 3,
                "preview": "RAW_SEARCH_PREVIEW Authorization: Bearer PRIVATE_SECRET",
                "context_before": ["RAW_CONTEXT_BEFORE"],
                "context_after": ["RAW_CONTEXT_AFTER"]
            }, {
                "path": format!("{PRIVATE_ROOT}/src/absolute.rs"),
                "line": 4,
                "preview": "ABSOLUTE_PATH_PREVIEW"
            }]
        })),
        None,
        None,
    );

    let hover = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "hover",
        &json!({
            "project": "demo",
            "path": "src/hover.rs",
            "line": 1,
            "column": 1
        }),
        crate::tool_runtime::sessions::session_tool_contract("hover"),
    );
    store.record_tool_call_finished(
        hover,
        true,
        &json!({
            "project": "demo",
            "path": "src/hover.rs",
            "position": {"line": 1, "column": 1},
            "hover": {"kind": "markdown", "value": "RAW_HOVER_BODY"},
            "truncated": false,
            "range_omitted": false
        }),
        None,
        None,
    );

    let symbols = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "workspace_symbols",
        &json!({
            "project": "demo",
            "query": "RAW_SYMBOL_QUERY",
            "query_present": true,
            "limit": 10
        }),
        crate::tool_runtime::sessions::session_tool_contract("workspace_symbols"),
    );
    store.record_tool_call_finished(
        symbols,
        true,
        &json!({
            "project": "demo",
            "query": "RAW_SYMBOL_QUERY",
            "symbols": [{
                "name": "RAW_SYMBOL_NAME",
                "kind": "struct",
                "kind_code": 23,
                "container_name": null,
                "path": "src/symbol.rs",
                "range": null
            }],
            "total_results": 1,
            "returned_count": 1,
            "truncated": false,
            "external_results_omitted": 0,
            "invalid_results_omitted": 0
        }),
        None,
        None,
    );

    let diagnostics = store.record_tool_call_started(
        Some(&session.session_id),
        SessionTransport::Api,
        "document_diagnostics",
        &json!({"project": "demo", "path": "src/diagnostics.rs", "limit": 10}),
        crate::tool_runtime::sessions::session_tool_contract("document_diagnostics"),
    );
    store.record_tool_call_finished(
        diagnostics,
        true,
        &json!({
            "project": "demo",
            "path": "src/diagnostics.rs",
            "language": "rust",
            "diagnostics": [{
                "range": {
                    "start": {"line": 1, "column": 1},
                    "end": {"line": 1, "column": 2}
                },
                "severity": "warning",
                "severity_code": 2,
                "code": "unused",
                "source": "rust-analyzer",
                "message": "RAW_DIAGNOSTIC_BODY",
                "tags": []
            }],
            "total_count": 1,
            "returned_count": 1,
            "truncated": false,
            "status": "complete",
            "clean": false,
            "published_version": 1,
            "invalid_results_omitted": 0,
            "related_information_omitted": 0
        }),
        None,
        None,
    );

    for (tool, arguments, output) in [
        (
            "list_project_files",
            json!({"project": "demo", "path": "src"}),
            json!({"entries": [{"path": "src/listed.rs", "kind": "file"}]}),
        ),
        (
            "run_shell",
            json!({
                "project": "demo",
                "command": "printf RAW_SHELL_COMMAND",
                "command_summary": "printf RAW_SHELL_COMMAND",
                "command_present": true
            }),
            json!({
                "stdout": "RAW_SHELL_STDOUT",
                "stderr": "RAW_SHELL_STDERR",
                "path": "src/from-shell.rs"
            }),
        ),
    ] {
        let start = store.record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            tool,
            &arguments,
            crate::tool_runtime::sessions::session_tool_contract(tool),
        );
        store.record_tool_call_finished(start, true, &output, None, None);
    }

    store.flush_persistence();
    let serialized = std::fs::read_to_string(&ledger).unwrap();
    for forbidden in [
        "RAW_SEARCH_PATTERN",
        "wc_pat_PRIVATE_TOKEN",
        "RAW_SEARCH_PREVIEW",
        "Authorization: Bearer PRIVATE_SECRET",
        "RAW_CONTEXT_BEFORE",
        "RAW_CONTEXT_AFTER",
        "RAW_HOVER_BODY",
        "RAW_SYMBOL_QUERY",
        "RAW_SYMBOL_NAME",
        "RAW_DIAGNOSTIC_BODY",
        "RAW_SHELL_COMMAND",
        "RAW_SHELL_STDOUT",
        "RAW_SHELL_STDERR",
        "src/listed.rs",
        "src/from-shell.rs",
        PRIVATE_ROOT,
    ] {
        assert!(
            !serialized.contains(forbidden),
            "ledger leaked {forbidden}: {serialized}"
        );
    }
    assert!(serialized.contains("src/search.rs"));
    assert!(serialized.contains("src/hover.rs"));
    assert!(serialized.contains("src/symbol.rs"));
    assert!(serialized.contains("src/diagnostics.rs"));

    let restored = SessionStore::with_persistence(ledger, 10, 30);
    let summary = restored.summary(&session.session_id, Some(30)).unwrap();
    let successful_observations = summary
        .events
        .iter()
        .filter(|event| {
            event.kind == "tool_call_finished"
                && event.status.as_deref() == Some("succeeded")
                && !event.observed_paths.is_empty()
        })
        .flat_map(|event| event.observed_paths.iter().map(String::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        successful_observations,
        [
            "src/search.rs",
            "src/hover.rs",
            "src/symbol.rs",
            "src/diagnostics.rs"
        ]
    );
}

#[test]
fn search_project_texts_nested_patterns_are_removed_from_session_input_summary() {
    let summary = super::events::session_input_summary_for_tool(
        "search_project_texts",
        &json!({
            "project": "agent:oe:demo",
            "queries": [
                {
                    "pattern": "RAW_BATCH_PATTERN_ALPHA wc_pat_PRIVATE_TOKEN",
                    "path": "src",
                    "result_mode": "matches"
                },
                {
                    "pattern": "RAW_BATCH_PATTERN_BETA Authorization: Bearer PRIVATE",
                    "path": "tests",
                    "result_mode": "files_with_matches"
                }
            ]
        }),
    );
    assert_eq!(summary["queries"][0]["path"], "src");
    assert_eq!(summary["queries"][1]["path"], "tests");
    assert!(summary["queries"][0].get("pattern").is_none());
    assert!(summary["queries"][1].get("pattern").is_none());
    let serialized = serde_json::to_string(&summary).unwrap();
    assert!(!serialized.contains("RAW_BATCH_PATTERN_ALPHA"));
    assert!(!serialized.contains("RAW_BATCH_PATTERN_BETA"));
    assert!(!serialized.contains("PRIVATE_TOKEN"));
}

#[test]
fn persistence_snapshot_shares_payload_and_stays_stable_across_message_cow() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("shared snapshot".to_string()));
    assert!(store
        .record_tool_call_started(
            Some(&session.session_id),
            SessionTransport::Api,
            "read_files",
            &json!({
                "project": "demo",
                "path": "src/lib.rs",
                "query": "snapshot payload"
            }),
            crate::tool_runtime::sessions::session_tool_contract("read_files"),
        )
        .is_some());
    let message = post_message(
        &store,
        &session.session_id,
        SessionMessageKind::Todo,
        "snapshot message payload",
    );
    store.flush_persistence();

    let (snapshot_ready_tx, snapshot_ready_rx) = std::sync::mpsc::channel();
    let (allow_old_write_tx, allow_old_write_rx) = std::sync::mpsc::channel();
    let snapshot_session_id = session.session_id.clone();
    let delayed_store = store.clone();
    let delayed_write = std::thread::spawn(move || {
        delayed_store.persist_after_mutation_with_for_test(|path, ledger| {
            let snapshot = ledger
                .sessions
                .iter()
                .filter_map(|snapshot| snapshot.hot())
                .find(|record| record.session_id == snapshot_session_id)
                .unwrap();
            let event = snapshot.events.last().unwrap();
            let message = snapshot.messages.last().unwrap();
            assert_eq!(std::sync::Arc::strong_count(event), 2);
            assert_eq!(std::sync::Arc::strong_count(message), 2);
            assert_eq!(message.status, SessionMessageStatus::Open);

            snapshot_ready_tx.send(()).unwrap();
            allow_old_write_rx.recv().unwrap();

            assert_eq!(std::sync::Arc::strong_count(message), 1);
            assert_eq!(message.status, SessionMessageStatus::Open);
            assert_eq!(message.resolution, None);
            write_ledger_atomic(path, ledger)
        });
    });
    snapshot_ready_rx.recv().unwrap();

    let resolved = store
        .resolve_message(
            &session.session_id,
            &message.message_id,
            Some("resolved after snapshot".to_string()),
        )
        .unwrap();
    assert_eq!(resolved.status, SessionMessageStatus::Resolved);
    assert_eq!(
        resolved.resolution.as_deref(),
        Some("resolved after snapshot")
    );

    allow_old_write_tx.send(()).unwrap();
    delayed_write.join().unwrap();

    let restored = flush_and_restore(&store, ledger);
    let messages = restored
        .list_messages(&session.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].status, SessionMessageStatus::Resolved);
    assert_eq!(
        messages[0].resolution.as_deref(),
        Some("resolved after snapshot")
    );
}

#[test]
fn concurrent_persistence_reloads_current_snapshot_before_write() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let store = persistent_store(ledger.clone());
    let session = store.start_session(None, Some("ordered writes".to_string()));
    let (old_snapshot_ready_tx, old_snapshot_ready_rx) = std::sync::mpsc::channel();
    let (allow_old_write_tx, allow_old_write_rx) = std::sync::mpsc::channel();

    let delayed_store = store.clone();
    let delayed_write = std::thread::spawn(move || {
        delayed_store.persist_after_mutation_with_for_test(|path, ledger| {
            old_snapshot_ready_tx.send(()).unwrap();
            allow_old_write_rx.recv().unwrap();
            write_ledger_atomic(path, ledger)
        });
    });
    old_snapshot_ready_rx.recv().unwrap();

    let newer_store = store.clone();
    let newer_session_id = session.session_id.clone();
    let newer_mutation = std::thread::spawn(move || {
        post_message(
            &newer_store,
            &newer_session_id,
            SessionMessageKind::Progress,
            "newer mutation",
        );
    });

    let visibility_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let messages = store
            .list_messages(&session.session_id, ListSessionMessagesFilter::default())
            .unwrap();
        if messages
            .iter()
            .any(|message| message.message == "newer mutation")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < visibility_deadline,
            "newer persistence mutation did not become visible within 5 seconds"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    allow_old_write_tx.send(()).unwrap();
    delayed_write.join().unwrap();
    newer_mutation.join().unwrap();

    let restored = flush_and_restore(&store, ledger);
    let messages = restored
        .list_messages(&session.session_id, ListSessionMessagesFilter::default())
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message, "newer mutation");
}

#[test]
fn project_instructions_content_not_persisted_or_leaked_after_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = tmp.path().join("sessions.json");
    let secret_body = "secret project rule that must not persist";
    let store = persistent_store(ledger.clone());
    let session = store
        .start_session_with_options(
            SessionCreateOptions::new(
                Some("agent:oe:private-drop".to_string()),
                Some("instructions".to_string()),
                SessionMode::Normal,
                SessionGuards::default(),
            )
            .with_project_instructions(Some(
                ProjectInstructionsSnapshot::from_candidates(
                    vec![LoadedInstructionCandidate {
                        path: "AGENTS.md".to_string(),
                        content: secret_body.to_string(),
                        total_lines: 1,
                        full_sha256: None,
                    }],
                    true,
                ),
            )),
        )
        .unwrap();

    store.flush_persistence();
    let serialized = std::fs::read_to_string(&ledger).unwrap();
    assert!(!serialized.contains(secret_body));
    assert!(!serialized.contains("project_instructions"));
    let restored = flush_and_restore(&store, ledger);
    let summary = restored.summary(&session.session_id, Some(10)).unwrap();
    assert!(summary.project_instructions.is_none());
    let summary_json = serde_json::to_string(&summary).unwrap();
    assert!(!summary_json.contains(secret_body));
}

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

#[test]
fn raw_model_facing_events_do_not_consume_context_revisions() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("checkpoint loop".to_string()),
    );

    for tool in ["read_files", "search_project_texts", "show_changes"] {
        let observed = record_model_facing_result(
            &store,
            &session.session_id,
            tool,
            SessionContextRevisionAck::Revision(0),
            true,
            json!({"observed": true}),
        );
        assert_eq!(observed.context_revision, 0, "{tool}");
        assert_eq!(observed.pre_response_context_revision, 0, "{tool}");
        assert!(!observed.checkpoint_advanced, "{tool}");
        assert_eq!(
            observed.ack_session_context_revision,
            SessionContextRevisionAck::Revision(0),
            "{tool}"
        );
        let mut projected = super::super::ToolResult::ok(json!({"observed": true}));
        assert!(
            !super::super::session_context::add_session_context_continuity(
                &mut projected,
                &observed,
            ),
            "exact no-op {tool} should not request recovery"
        );
        for field in [
            "session_context_revision",
            "session_continuity",
            "session_recovery",
        ] {
            assert!(
                projected.output.get(field).is_none(),
                "exact no-op {tool} leaked {field}: {}",
                projected.output
            );
        }
    }

    let edit = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(edit.context_revision, 1);
    assert!(edit.checkpoint_advanced);
    let mut edit_projection = super::super::ToolResult::ok(json!({"state_changed": true}));
    assert!(
        !super::super::session_context::add_session_context_continuity(&mut edit_projection, &edit,)
    );
    assert_eq!(edit_projection.output["session_context_revision"], 1);
    assert!(edit_projection.output.get("session_continuity").is_none());
    assert!(edit_projection.output.get("session_recovery").is_none());

    let read_batch = record_model_facing_result(
        &store,
        &session.session_id,
        "read_files",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"observed": true}),
    );
    assert_eq!(read_batch.context_revision, 1);
    assert!(!read_batch.checkpoint_advanced);

    let process = record_model_facing_result(
        &store,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(process.context_revision, 2);
    assert_eq!(store.context_revision(&session.session_id), Some(2));

    let summary = store.summary(&session.session_id, Some(100)).unwrap();
    let finished = summary
        .events
        .iter()
        .filter(|event| event.kind == "tool_call_finished")
        .collect::<Vec<_>>();
    for event in &finished {
        let expected = match event.tool_name.as_str() {
            "apply_text_edits" => Some(1),
            "run_process" => Some(2),
            "read_files" | "search_project_texts" | "show_changes" => None,
            other => panic!("unexpected finished tool {other}"),
        };
        assert_eq!(event.context_revision, expected, "{}", event.tool_name);
    }
}

#[test]
fn history_lost_counts_checkpoint_revisions_not_raw_events() {
    let store = SessionStore::new(10, 40);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("mixed retention".to_string()),
    );

    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    for _ in 0..8 {
        let read = record_model_facing_result(
            &store,
            &session.session_id,
            "read_files",
            SessionContextRevisionAck::Revision(1),
            true,
            json!({"content": "re-observable"}),
        );
        assert_eq!(read.context_revision, 1);
    }
    let second = record_model_facing_result(
        &store,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(second.context_revision, 2);
    for _ in 0..4 {
        let status = record_model_facing_result(
            &store,
            &session.session_id,
            "git_status",
            SessionContextRevisionAck::Revision(2),
            true,
            json!({"clean": true}),
        );
        assert_eq!(status.context_revision, 2);
    }

    let recovered = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(recovered.context_revision, 2);
    assert!(!recovered.history_lost);
    assert_eq!(
        recovered
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(
        super::super::session_context::add_session_context_continuity(&mut response, &recovered,)
    );
    assert_eq!(response.output["session_continuity"]["events_after_ack"], 2);
}

#[test]
fn history_lost_detects_evicted_checkpoint_after_raw_event_churn() {
    let store = SessionStore::new(10, 8);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("raw event retention".to_string()),
    );

    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);

    for _ in 0..3 {
        let read = record_model_facing_result(
            &store,
            &session.session_id,
            "read_files",
            SessionContextRevisionAck::Revision(1),
            true,
            json!({"content": "raw churn"}),
        );
        assert_eq!(read.context_revision, 1);
        assert!(!read.checkpoint_advanced);
    }

    let second = record_model_facing_result(
        &store,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(second.context_revision, 2);

    let recovered = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(recovered.context_revision, 2);
    assert!(recovered.history_lost);
    assert_eq!(
        recovered
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![2],
        "raw event churn may evict checkpoint 1, but must never be counted as extra revisions"
    );

    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(
        super::super::session_context::add_session_context_continuity(&mut response, &recovered,)
    );
    assert_eq!(response.output["session_continuity"]["events_after_ack"], 2);
    assert_eq!(response.output["session_continuity"]["history_lost"], true);
    assert_eq!(
        response.output["session_recovery"]["model_facing_events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn simultaneous_model_facing_results_allocate_unique_ordered_revisions() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(Some("proj".to_string()), Some("concurrent".to_string()));
    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);

    let make_start = || {
        store
            .record_tool_call_started_with_metadata(
                Some(&session.session_id),
                SessionTransport::Mcp,
                "apply_text_edits",
                &json!({"project": "proj"}),
                Some("proj".to_string()),
                ToolCallRecorderMetadata {
                    ack_session_context_revision: SessionContextRevisionAck::Revision(1),
                    ..Default::default()
                },
                crate::tool_runtime::sessions::session_tool_contract("apply_text_edits"),
            )
            .unwrap()
    };
    let start_a = make_start();
    let start_b = make_start();
    assert_eq!(start_a.pre_call_context_revision, 1);
    assert_eq!(start_b.pre_call_context_revision, 1);

    let a_store = store.clone();
    let b_store = store.clone();
    let a = std::thread::spawn(move || {
        a_store
            .record_model_facing_tool_call_finished(
                Some(start_a),
                true,
                &json!({"content": "a"}),
                None,
                None,
            )
            .unwrap()
    });
    let b = std::thread::spawn(move || {
        b_store
            .record_model_facing_tool_call_finished(
                Some(start_b),
                true,
                &json!({"content": "b"}),
                None,
                None,
            )
            .unwrap()
    });
    let mut outcomes = vec![a.join().unwrap(), b.join().unwrap()];
    outcomes.sort_by_key(|recorded| recorded.context_revision);
    assert_eq!(
        outcomes
            .iter()
            .map(|recorded| recorded.context_revision)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(store.context_revision(&session.session_id), Some(3));

    // The later completion started from the same model ACK as the earlier one,
    // so its response must carry the intervening result before revision 3 can be
    // safely acknowledged. Losing the revision-2 HTTP/MCP response must not make
    // revision 2 disappear from the model's recoverable context prefix.
    let later = &outcomes[1];
    assert_eq!(later.pre_call_context_revision, 1);
    assert_eq!(
        later
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let mut later_response = super::super::ToolResult::ok(json!({"content": "later"}));
    assert!(
        super::super::session_context::add_session_context_continuity(&mut later_response, later,)
    );
    assert_eq!(
        later_response.output["session_continuity"]["status"],
        "behind"
    );
    assert_eq!(
        later_response.output["session_continuity"]["events_after_ack"],
        1
    );
    assert_eq!(
        later_response.output["session_recovery"]["model_facing_events"][0]["context_revision"],
        2
    );

    let next = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(3),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(next.pre_call_context_revision, 3);
    assert_eq!(next.context_revision, 3);
    assert!(!next.checkpoint_advanced);
    assert!(next.recovery_events.is_empty());

    let future_start = store
        .record_tool_call_started_with_metadata(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "run_process",
            &json!({"project": "proj"}),
            Some("proj".to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: SessionContextRevisionAck::Revision(4),
                ..Default::default()
            },
            crate::tool_runtime::sessions::session_tool_contract("run_process"),
        )
        .unwrap();
    let intervening_start = store
        .record_tool_call_started_with_metadata(
            Some(&session.session_id),
            SessionTransport::Mcp,
            "run_process",
            &json!({"project": "proj"}),
            Some("proj".to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: SessionContextRevisionAck::Revision(3),
                ..Default::default()
            },
            crate::tool_runtime::sessions::session_tool_contract("run_process"),
        )
        .unwrap();
    assert_eq!(future_start.pre_call_context_revision, 3);
    assert_eq!(intervening_start.pre_call_context_revision, 3);
    let intervening = store
        .record_model_facing_tool_call_finished(
            Some(intervening_start),
            true,
            &json!({"command_started": true, "command_completed": true, "exit_code": 0}),
            None,
            None,
        )
        .unwrap();
    assert_eq!(intervening.context_revision, 4);
    let future = store
        .record_model_facing_tool_call_finished(
            Some(future_start),
            true,
            &json!({"command_started": true, "command_completed": true, "exit_code": 0}),
            None,
            None,
        )
        .unwrap();
    assert_eq!(future.pre_call_context_revision, 3);
    assert_eq!(future.pre_response_context_revision, 4);
    assert_eq!(future.context_revision, 5);
    let mut future_response = super::super::ToolResult::ok(json!({"exit_code": 0}));
    assert!(
        super::super::session_context::add_session_context_continuity(
            &mut future_response,
            &future,
        )
    );
    assert_eq!(
        future_response.output["session_continuity"]["status"],
        "invalid"
    );
}

#[test]
fn batch_budget_preserves_recorder_overlay_for_no_ack_read_batch() {
    fn canonical_read_batch(text: String) -> Value {
        let default_limit =
            webcodex_workspace::file_read_range::EffectiveRange::new(None, None).limit;
        json!({
            "project": "proj",
            "requested_count": 1,
            "returned_count": 1,
            "succeeded_count": 1,
            "failed_count": 0,
            "items": [{
                "index": 0,
                "path": "src/lib.rs",
                "success": true,
                "output": {
                    "text": text,
                    "format": "plain",
                    "path": "src/lib.rs",
                    "sha256": "a".repeat(64),
                    "start_line": 1,
                    "limit": default_limit,
                    "total_lines": 1,
                    "returned_lines": 1,
                    "end_line": 1,
                    "has_more": false,
                    "next_start_line": null
                },
                "error": null
            }],
            "output_truncated": false,
            "next_index": null
        })
    }

    let store = SessionStore::new(10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("batch budget overlays".to_string()),
    );
    let first = record_model_facing_result(
        &store,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);

    for text in ["x".repeat(56 * 1024), "y".repeat(8 * 1024)] {
        let batch = canonical_read_batch(text);
        let recorded = record_model_facing_result(
            &store,
            &session.session_id,
            "read_files",
            SessionContextRevisionAck::Revision(1),
            true,
            batch.clone(),
        );
        assert_eq!(recorded.context_revision, 1);
        assert_eq!(recorded.pre_response_context_revision, 1);
        assert!(!recorded.checkpoint_advanced);
        assert_eq!(
            recorded.ack_session_context_revision,
            SessionContextRevisionAck::Revision(1)
        );
        assert!(recorded.recovery_events.is_empty());

        let mut response = super::super::ToolResult::ok(batch);
        super::super::session_context::add_session_hint(&mut response, &store, &session.session_id);
        assert!(
            !super::super::session_context::add_session_context_continuity(
                &mut response,
                &recorded,
            )
        );
        super::super::read_files::apply_model_facing_output_budget(
            &mut response,
            None,
            &super::super::read_files::ReadModelProjection::None,
        );
        super::super::dispatch::sparsify_complete_read_success("read_files", &mut response);
        assert!(response.output.get("session_recorded").is_none());
        assert!(response.output.get("session_event_id").is_none());
        assert!(response.output.get("session_id").is_none());
        assert!(response.output.get("session_context_revision").is_none());
        assert!(response.output.get("session_continuity").is_none());
        assert!(response.output.get("session_recovery").is_none());
        assert!(
            serde_json::to_vec(&response).unwrap().len()
                <= super::super::read_files::DEFAULT_READ_FILES_RESULT_BYTES
        );
    }
    assert_eq!(store.context_revision(&session.session_id), Some(1));
}

#[tokio::test]
async fn unknown_session_context_recovery_uses_current_handoff_not_history_replay() {
    let runtime = super::super::ToolRuntime::new_for_tests();
    let session = runtime
        .sessions
        .start_session(None, Some("unknown continuity recovery".to_string()));
    let first = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    let second = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "run_process",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"command_started": true, "command_completed": true, "exit_code": 0}),
    );
    assert_eq!(second.context_revision, 2);

    let unknown = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(unknown.pre_call_context_revision, 2);
    assert_eq!(unknown.pre_response_context_revision, 2);
    assert_eq!(unknown.context_revision, 2);
    assert!(!unknown.checkpoint_advanced);
    assert!(unknown.recovery_events.is_empty());
    assert!(!unknown.history_lost);

    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(
        super::super::session_context::add_session_context_continuity(&mut response, &unknown,)
    );
    assert_eq!(
        response.output["session_continuity"]["status"],
        "unacknowledged"
    );
    assert!(response.output["session_continuity"]
        .get("events_after_ack")
        .is_none());
    assert!(response.output["session_recovery"]["model_facing_events"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(response.output["session_recovery"]
        .get("current_handoff")
        .is_none());

    runtime
        .add_session_history_recovery(&mut response, &unknown, None)
        .await;
    let current = &response.output["session_recovery"]["current_handoff"];
    assert!(current.is_object());
    assert!(current["work_performed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|work| work["tool_name"] == "work_on_project"));
    assert!(response.output["session_recovery"]["model_facing_events"]
        .as_array()
        .unwrap()
        .is_empty(),
        "the current ToolResult may inform current state but must not be replayed as missed history");
}

#[tokio::test]
async fn required_session_context_recovery_handoff_failure_does_not_expose_new_revision() {
    let runtime = super::super::ToolRuntime::new_for_tests();
    let session = runtime
        .sessions
        .start_session(None, Some("handoff failure continuity".to_string()));
    let first = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    let mut unknown = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(unknown.context_revision, 1);
    assert!(unknown.recovery_events.is_empty());

    // The production path re-authorizes the exact recorded Session before
    // reading current handoff state. Corrupt only this local projection id to
    // force that read to fail and verify the revision fail-safe.
    unknown.session_id = "wc_sess_missing_for_handoff_test".to_string();
    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(
        super::super::session_context::add_session_context_continuity(&mut response, &unknown,)
    );
    assert_eq!(response.output["session_context_revision"], 1);
    assert_eq!(
        response.output["session_continuity"]["status"],
        "unacknowledged"
    );

    runtime
        .add_session_history_recovery(&mut response, &unknown, None)
        .await;
    assert!(response.output.get("session_context_revision").is_none());
    assert!(response.output["session_recovery"]
        .get("current_handoff")
        .is_none());
}

#[tokio::test]
async fn bounded_session_context_recovery_adds_current_handoff_before_latest_ack() {
    let runtime = super::super::ToolRuntime::new_for_tests();
    let session = runtime
        .sessions
        .start_session(None, Some("bounded continuity recovery".to_string()));
    let mut latest = 0;
    for _ in 0..25 {
        let recorded = record_model_facing_result(
            &runtime.sessions,
            &session.session_id,
            "apply_text_edits",
            SessionContextRevisionAck::Revision(latest),
            true,
            json!({"state_changed": true}),
        );
        latest = recorded.context_revision;
    }
    assert_eq!(latest, 25);

    let stale = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(stale.pre_call_context_revision, 25);
    assert_eq!(stale.pre_response_context_revision, 25);
    assert_eq!(stale.context_revision, 25);
    assert!(!stale.checkpoint_advanced);
    assert_eq!(stale.recovery_events.len(), 25);
    assert!(!stale.history_lost);

    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(super::super::session_context::add_session_context_continuity(&mut response, &stale,));
    assert_eq!(response.output["session_recovery"]["truncated"], true);
    assert_eq!(response.output["session_recovery"]["omitted_count"], 5);
    assert!(response.output["session_recovery"]
        .get("current_handoff")
        .is_none());

    runtime
        .add_session_history_recovery(&mut response, &stale, None)
        .await;
    assert!(
        response.output["session_recovery"]["current_handoff"].is_object(),
        "bounded event omission must add a compact current-state recovery before revision 25 is ACK-able"
    );
}

#[tokio::test]
async fn history_lost_session_context_recovery_adds_current_handoff() {
    let runtime = super::super::ToolRuntime::new_for_tests();
    let session = runtime
        .sessions
        .start_session(None, Some("history lost continuity recovery".to_string()));
    let first = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "apply_text_edits",
        SessionContextRevisionAck::Unacknowledged,
        true,
        json!({"state_changed": true}),
    );
    assert_eq!(first.context_revision, 1);
    let mut behind = record_model_facing_result(
        &runtime.sessions,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(0),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(
        behind
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![1]
    );
    // Store retention behavior is covered independently above. Force the same
    // recorded projection flag here so this test isolates the response fallback.
    behind.history_lost = true;

    let mut response = super::super::ToolResult::ok(json!({"session_id": session.session_id}));
    assert!(super::super::session_context::add_session_context_continuity(&mut response, &behind,));
    assert_eq!(response.output["session_continuity"]["status"], "behind");
    assert_eq!(response.output["session_continuity"]["history_lost"], true);
    assert!(response.output["session_recovery"]
        .get("current_handoff")
        .is_none());

    runtime
        .add_session_history_recovery(&mut response, &behind, None)
        .await;
    assert!(response.output["session_recovery"]["current_handoff"].is_object());
}

#[test]
fn stale_session_recovery_preserves_consequential_git_workspace_evidence() {
    let store = SessionStore::new(10, 100);
    let session = store.start_session(
        Some("proj".to_string()),
        Some("acp reproduction".to_string()),
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

    let checkpoint = record_model_facing_result(
        &store,
        &session.session_id,
        "workspace_checkpoint_create",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({
            "checkpoint_id": "wc_ckpt_demo",
            "head": "pre-commit",
            "branch": "feature",
            "complete": true,
            "tracked_diff_bytes": 4096,
            "staged_diff_bytes": 0,
            "untracked_file_count": 0,
            "status_summary": {"modified": 21},
            "state_changed": true
        }),
    );
    let commit = record_model_facing_result(
        &store,
        &session.session_id,
        "run_shell",
        SessionContextRevisionAck::Revision(checkpoint.context_revision),
        true,
        json!({
            "command_completed": true,
            "command_started": true,
            "exit_code": 0,
            "stdout_tail": "[feature 64ac72ae] checkpointed work\n21 files changed",
            "stderr_tail": ""
        }),
    );
    let push = record_model_facing_result(
        &store,
        &session.session_id,
        "run_shell",
        SessionContextRevisionAck::Revision(commit.context_revision),
        true,
        json!({
            "command_completed": true,
            "command_started": true,
            "exit_code": 0,
            "stdout_tail": "feature -> feature 64ac72ae",
            "stderr_tail": ""
        }),
    );
    let status = record_model_facing_result(
        &store,
        &session.session_id,
        "git_status",
        SessionContextRevisionAck::Revision(push.context_revision),
        true,
        json!({"clean": true, "head": "64ac72ae", "remote_head": "64ac72ae"}),
    );
    assert_eq!(status.context_revision, 4);
    assert_eq!(status.pre_response_context_revision, 4);
    assert!(!status.checkpoint_advanced);

    let resumed = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(1),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(resumed.pre_call_context_revision, 4);
    assert_eq!(resumed.pre_response_context_revision, 4);
    assert_eq!(resumed.context_revision, 4);
    assert!(!resumed.checkpoint_advanced);
    assert_eq!(
        resumed
            .recovery_events
            .iter()
            .filter_map(|event| event.context_revision)
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert!(resumed.recovery_events.iter().any(|event| {
        event.tool_name == "workspace_checkpoint_create"
            && event
                .context_result_summary
                .as_ref()
                .is_some_and(|summary| {
                    summary["status_summary"]["modified"] == 21
                        && summary["checkpoint_id"] == "wc_ckpt_demo"
                })
    }));
    assert!(resumed
        .recovery_events
        .iter()
        .any(|event| event.tool_name == "run_shell"
            && event
                .validation_output_summary
                .as_ref()
                .is_some_and(|summary| summary.to_string().contains("64ac72ae"))));
    assert!(
        resumed
            .recovery_events
            .iter()
            .all(|event| event.tool_name != "git_status"),
        "recovery replays checkpoint knowledge, not re-observable git_status results"
    );

    let exact = record_model_facing_result(
        &store,
        &session.session_id,
        "work_on_project",
        SessionContextRevisionAck::Revision(4),
        true,
        json!({"session_id": session.session_id}),
    );
    assert_eq!(exact.context_revision, 4);
    assert!(!exact.checkpoint_advanced);
    assert!(exact.recovery_events.is_empty());
}
