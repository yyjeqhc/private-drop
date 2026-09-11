use super::reconnect::dispatch_coding_call_in_window;
use super::support::*;
use crate::runner_http::ShellJobStartMetadata;
use crate::runner_protocol::{RunnerCapabilities, RunnerJobUpdateRequest, ShellJobOpRequest};
use crate::tool_runtime::startup_brief::{
    startup_brief_size, validate_schema_instance_for_test, STANDARD_STARTUP_HARD_MAX_BYTES,
};
use crate::tool_runtime::{
    registry, SessionMode, StartupDetail, ToolCall, ToolResult, ToolRuntime,
};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

async fn start(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    _window: &str,
    detail: StartupDetail,
    title: &str,
    resume_session_id: Option<&str>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        let title = title.to_string();
        let resume_session_id = resume_session_id.map(str::to_string);
        let auth = auth_context(None, true);
        async move {
            runtime
                .start_coding_workflow_for_test(
                    project,
                    None,
                    None,
                    Some(title),
                    SessionMode::Normal,
                    false,
                    false,
                    detail,
                    resume_session_id,
                    None,
                    Some(&auth),
                    None,
                    None,
                    crate::tool_runtime::sessions::SessionTransport::Mcp,
                )
                .await
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "coding workflow did not finish within the 10-second test deadline"
        );
        if let Some(req) = runtime
            .runner_registry
            .poll(crate::runner_protocol::RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap()
        {
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&req);
            complete_patch_agent_request(
                runtime,
                client_id,
                &req.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
    task.await.unwrap()
}

fn seed_rules(root: &Path) {
    init_git_repo(root);
    commit_file(
        root,
        "AGENTS.md",
        "# Repository rules\n\n- Preserve unrelated changes.\n- Run focused tests.\n",
        "add agent rules",
    );
    commit_file(
        root,
        "CLAUDE.md",
        "# Additional rules\n\nDo not expose credentials.\n",
        "add additional rules",
    );
}

fn instruction_source<'a>(output: &'a Value, path: &str) -> &'a Value {
    output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == path)
        .unwrap_or_else(|| panic!("missing instruction source {path}: {output}"))
}

fn assert_builtin_workflow(output: &Value) {
    let workflow = &output["workflow"];
    assert_eq!(workflow["contract"], "webcodex.coding_workflow");
    assert_eq!(workflow["version"], 9);
    assert_eq!(workflow["authority"], "model_guidance_only");
    assert!(workflow["role_selection"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    let ack_guidance = workflow["model_protocol"]["session_context_ack"]
        .as_str()
        .expect("Session context ACK guidance");
    assert!(ack_guidance.contains("ack_session_context_revision"));
    assert!(ack_guidance.contains("session_context_revision exactly"));
    assert!(ack_guidance.contains("never derive it"));
    assert!(ack_guidance.contains("No revision"));
    assert!(ack_guidance.contains("if unknown, omit"));
    assert!(ack_guidance.contains("Missing/invalid"));
    assert!(ack_guidance.contains("compact handoff"));
    assert!(ack_guidance.contains("stale may recover"));
    assert!(ack_guidance.contains("Nonblocking"));
    let recording_guidance = workflow["model_protocol"]["session_recording"]
        .as_str()
        .expect("Session recording guidance");
    assert!(recording_guidance.contains("work_on_project creates or resumes"));
    assert!(recording_guidance.contains("recording_session_id"));
    assert!(recording_guidance.contains("recorder provenance only"));
    assert!(recording_guidance.contains("business session_id may target another Session"));
    assert!(recording_guidance.contains("grants no authority"));
    let message_ack_guidance = workflow["model_protocol"]["session_message_ack"]
        .as_str()
        .expect("Session message ACK guidance");
    assert!(message_ack_guidance.contains("session_attention"));
    assert!(message_ack_guidance.contains("requires_ack"));
    assert!(message_ack_guidance.contains("ack_session_message_ids"));
    assert!(message_ack_guidance.contains("request-scoped model-context proof"));
    assert!(message_ack_guidance.contains("resolves messages"));
    assert!(message_ack_guidance.contains("grants authority"));
    assert!(message_ack_guidance.contains("gates execution"));
    let message_resolution_guidance = workflow["model_protocol"]["session_message_resolution"]
        .as_str()
        .expect("Session message resolution guidance");
    assert!(message_resolution_guidance.contains("session_message_resolution"));
    assert!(message_resolution_guidance.contains("next ordinary call"));
    assert!(message_resolution_guidance.contains("recording_session_id"));
    assert!(message_resolution_guidance.contains("ack_session_message_ids"));
    assert!(message_resolution_guidance.contains("cannot predict the main call"));
    assert!(message_resolution_guidance.contains("complete_session_message"));
    let sidecar_guidance = workflow["model_protocol"]["context_sidecar"]
        .as_str()
        .expect("context sidecar guidance");
    assert!(sidecar_guidance.contains("context_request"));
    assert!(sidecar_guidance.contains("after the main tool"));
    assert!(sidecar_guidance.contains("never authorizes"));
    assert!(sidecar_guidance.contains("observation call before dependent mutation"));
    let runner_targeting_guidance = workflow["model_protocol"]["runner_targeting"]
        .as_str()
        .expect("exact Runner targeting guidance");
    assert!(runner_targeting_guidance.contains("exact Runner client_id"));
    assert!(runner_targeting_guidance.contains("runtime_status(client_id=...)"));
    assert!(runner_targeting_guidance.contains("list_projects(client_id=...)"));
    assert!(runner_targeting_guidance.contains("before treating it as absent"));
    let defaults = workflow["guidance"]
        .as_array()
        .expect("default workflow guidance")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    for phrase in [
        "independent read-only inspection",
        "short sync_wait_secs",
        "same-execution Job handoff",
        "do not fan out heavy validations",
        "stale/cache-warmup",
        "final source needs fresh validation",
    ] {
        assert!(defaults.contains(phrase), "workflow guidance: {phrase}");
    }
    let persistent_shell_guidance = workflow["model_protocol"]["persistent_shell"]
        .as_str()
        .expect("persistent shell guidance");
    assert!(persistent_shell_guidance.contains("primarily for repeated remote commands"));
    assert!(persistent_shell_guidance.contains("one named SSH resource"));
    assert!(persistent_shell_guidance.contains("remote cwd/env/exports/functions/umask"));
    assert!(persistent_shell_guidance
        .contains("structured tools -> run_process/run_script -> run_shell"));
    assert!(persistent_shell_guidance
        .contains("local persistent shell only when same-process state is required"));
    assert!(!persistent_shell_guidance.contains("For repeated commands in one Workflow Session"));
    let closeout_guidance = workflow["model_protocol"]["normal_closeout"]
        .as_str()
        .expect("normal closeout guidance");
    assert!(closeout_guidance.contains("finish_coding_task(summary_only=true)"));
    assert!(closeout_guidance.contains("full closeout only"));
    for role in ["implementation_owner", "independent_review"] {
        let role = &workflow["roles"][role];
        assert!(role["purpose"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));
        let guidance = role["guidance"]
            .as_array()
            .expect("workflow guidance array");
        assert!(!guidance.is_empty());
        assert!(
            guidance.len()
                <= crate::tool_runtime::startup_brief::BUILTIN_CODING_WORKFLOW_MAX_GUIDANCE_ITEMS
        );
    }
    let implementation_guidance = workflow["roles"]["implementation_owner"]["guidance"]
        .as_array()
        .unwrap();
    assert!(implementation_guidance.iter().any(|item| {
        item.as_str().is_some_and(|value| {
            value.contains("intentionally rerunning the same logical validation")
                && value.contains("reuse the same assertion_name")
                && value
                    .contains("do not rerun solely to clear stale historical validation evidence")
        })
    }));
    assert!(!implementation_guidance.iter().any(|item| {
        item.as_str()
            .is_some_and(|value| value.contains("resolve that validation identity"))
    }));
    let serialized = workflow.to_string();
    for forbidden in ["ChatGPT", "browser", "another window", "online", "offline"] {
        assert!(
            !serialized.contains(forbidden),
            "workflow leaked host-specific term {forbidden}"
        );
    }
}

#[tokio::test]
async fn fresh_coding_task_session_loads_all_bounded_repository_rules() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-fresh", "demo", root.path()).await;

    let result = start(
        &runtime,
        "rules-fresh",
        &project,
        "rules-fresh-window",
        StartupDetail::Standard,
        "start work",
        None,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["project"]["canonical_repository_root_matches"],
        true
    );
    assert_eq!(result.output["instructions"]["status"], "loaded");
    assert_eq!(result.output["instructions"]["content_included"], true);
    assert_builtin_workflow(&result.output);
    assert_eq!(
        result.output["instructions"]["sources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(instruction_source(&result.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains("Preserve unrelated changes"));
    assert!(instruction_source(&result.output, "CLAUDE.md")["content"]
        .as_str()
        .unwrap()
        .contains("Do not expose credentials"));
    for source in result.output["instructions"]["sources"].as_array().unwrap() {
        assert_eq!(source["fingerprint"].as_str().unwrap().len(), 64);
        assert!(source["headings"].is_array());
    }
    let bytes = startup_brief_size(&result.output);
    eprintln!("common_clean_standard_startup_bytes={bytes}");
    assert!(bytes < 16 * 1024, "{bytes}");
    assert!(bytes <= STANDARD_STARTUP_HARD_MAX_BYTES);
    let serialized = result.output.to_string();
    assert_ne!(
        result.output["workflow"], result.output["instructions"],
        "built-in workflow must remain distinct from project instructions"
    );
    assert!(
        !serialized.contains(&root.path().to_string_lossy().to_string()),
        "standard output leaked the absolute repository root"
    );
}

#[tokio::test]
async fn repository_without_project_instructions_still_receives_builtin_workflow() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    commit_file(root.path(), "README.md", "hello\n", "initial");
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "workflow-no-rules", "demo", root.path()).await;

    let result = start(
        &runtime,
        "workflow-no-rules",
        &project,
        "workflow-no-rules-window",
        StartupDetail::Standard,
        "implementation task",
        None,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["instructions"]["status"], "not_found");
    assert!(result.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_builtin_workflow(&result.output);
}

#[tokio::test]
async fn explicit_coding_task_resume_reuses_rules_without_repeating_content() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-reuse", "demo", root.path()).await;

    let first = start(
        &runtime,
        "rules-reuse",
        &project,
        "rules-reuse-window",
        StartupDetail::Standard,
        "first instruction",
        None,
    )
    .await;
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let second = start(
        &runtime,
        "rules-reuse",
        &project,
        "rules-reuse-window",
        StartupDetail::Standard,
        "continue",
        Some(&session_id),
    )
    .await;

    assert!(first.success && second.success);
    assert_eq!(
        second.output["session"]["session_id"],
        first.output["session"]["session_id"]
    );
    assert_eq!(
        second.output["session"]["continuation"],
        "resumed_explicitly"
    );
    assert_eq!(first.output["workflow"], second.output["workflow"]);
    assert_builtin_workflow(&second.output);
    assert!(second.output["session"].get("workflow").is_none());
    assert_eq!(
        second.output["project"]["canonical_repository_root_matches"],
        Value::Null
    );
    assert_eq!(second.output["instructions"]["status"], "reused");
    assert_eq!(second.output["instructions"]["content_included"], false);
    assert_eq!(second.output["instructions"]["changed_sources"], json!([]));
    for source in second.output["instructions"]["sources"].as_array().unwrap() {
        assert_eq!(source["content"], Value::Null);
        assert!(source["fingerprint"].is_string());
        assert!(source["headings"].is_array());
    }
    let session_id = second.output["session"]["session_id"].as_str().unwrap();
    let summary = runtime.sessions.summary(session_id, Some(50)).unwrap();
    assert_eq!(summary.mode, SessionMode::Normal);
    assert!(!summary.guards.deny_write_tools);
    assert!(!summary.guards.deny_shell_tools);
    assert!(
        !serde_json::to_string(&summary)
            .unwrap()
            .contains("webcodex.coding_workflow"),
        "built-in workflow guidance must not become durable Session authority"
    );
}

#[tokio::test]
async fn changed_and_deleted_repository_rule_sources_are_reported_incrementally() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-change", "demo", root.path()).await;

    let first = start(
        &runtime,
        "rules-change",
        &project,
        "rules-change-window",
        StartupDetail::Standard,
        "first",
        None,
    )
    .await;
    let first_agents_fingerprint =
        instruction_source(&first.output, "AGENTS.md")["fingerprint"].clone();
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    fs::create_dir_all(root.path().join(".codex")).unwrap();
    fs::write(
        root.path().join(".codex/AGENTS.md"),
        "# Added rules\n\nUse the added source.\n",
    )
    .unwrap();
    let added = start(
        &runtime,
        "rules-change",
        &project,
        "rules-change-window",
        StartupDetail::Standard,
        "after rule addition",
        Some(&session_id),
    )
    .await;
    assert_eq!(added.output["instructions"]["status"], "changed");
    assert_eq!(added.output["instructions"]["content_included"], true);
    assert_eq!(
        added.output["instructions"]["changed_sources"],
        json!([".codex/AGENTS.md"])
    );
    assert!(
        instruction_source(&added.output, ".codex/AGENTS.md")["content"]
            .as_str()
            .unwrap()
            .contains("added source")
    );

    fs::write(
        root.path().join("AGENTS.md"),
        "# Repository rules\n\n- Run the new focused target first.\n",
    )
    .unwrap();
    let changed = start(
        &runtime,
        "rules-change",
        &project,
        "rules-change-window",
        StartupDetail::Standard,
        "after rule edit",
        Some(&session_id),
    )
    .await;
    assert_eq!(changed.output["instructions"]["status"], "changed");
    assert_eq!(changed.output["instructions"]["content_included"], true);
    assert_eq!(
        changed.output["instructions"]["changed_sources"],
        json!(["AGENTS.md"])
    );
    assert_ne!(
        instruction_source(&changed.output, "AGENTS.md")["fingerprint"],
        first_agents_fingerprint
    );
    assert!(instruction_source(&changed.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains("new focused target"));

    fs::remove_file(root.path().join("CLAUDE.md")).unwrap();
    let deleted = start(
        &runtime,
        "rules-change",
        &project,
        "rules-change-window",
        StartupDetail::Standard,
        "after rule deletion",
        Some(&session_id),
    )
    .await;
    assert_eq!(deleted.output["instructions"]["status"], "changed");
    assert_eq!(deleted.output["instructions"]["content_included"], true);
    assert_eq!(
        deleted.output["instructions"]["changed_sources"],
        json!(["CLAUDE.md"])
    );
    assert!(deleted.output["instructions"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|source| source["path"] != "CLAUDE.md"));
}

#[tokio::test]
async fn repository_rule_truncation_state_change_invalidates_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    let initial = (0..400)
        .map(|index| format!("rule line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(root.path().join("AGENTS.md"), &initial).unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-truncate", "demo", root.path()).await;
    let first = start(
        &runtime,
        "rules-truncate",
        &project,
        "rules-truncate-window",
        StartupDetail::Standard,
        "first",
        None,
    )
    .await;
    assert_eq!(
        instruction_source(&first.output, "AGENTS.md")["truncated"],
        false
    );
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    fs::write(
        root.path().join("AGENTS.md"),
        format!("{initial}\nrule line 400\n"),
    )
    .unwrap();
    let changed = start(
        &runtime,
        "rules-truncate",
        &project,
        "rules-truncate-window",
        StartupDetail::Standard,
        "after truncation boundary",
        Some(&session_id),
    )
    .await;
    assert_eq!(changed.output["instructions"]["status"], "changed");
    assert_eq!(
        changed.output["instructions"]["changed_sources"],
        json!(["AGENTS.md"])
    );
    assert_eq!(
        instruction_source(&changed.output, "AGENTS.md")["truncated"],
        true
    );
    assert!(instruction_source(&changed.output, "AGENTS.md")["read_more"].is_object());
}

#[tokio::test]
async fn explicit_resume_reuses_unchanged_rules_without_repeating_body() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-explicit", "demo", root.path()).await;
    let first = start(
        &runtime,
        "rules-explicit",
        &project,
        "rules-explicit-window",
        StartupDetail::Standard,
        "first",
        None,
    )
    .await;
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resumed = start(
        &runtime,
        "rules-explicit",
        &project,
        "rules-explicit-window",
        StartupDetail::Standard,
        "resume",
        Some(&session_id),
    )
    .await;

    assert!(resumed.success, "{:?}", resumed.error);
    assert_eq!(resumed.output["session"]["session_id"], session_id);
    assert_eq!(
        resumed.output["session"]["continuation"],
        "resumed_explicitly"
    );
    assert_eq!(
        resumed.output["project"]["canonical_repository_root_matches"],
        Value::Null
    );
    // Exact resume with unchanged rules reports reuse and does not repeat the
    // rule body, while retaining source identity metadata.
    assert_eq!(resumed.output["instructions"]["status"], "reused");
    assert_eq!(resumed.output["instructions"]["content_included"], false);
    assert_eq!(resumed.output["instructions"]["changed_sources"], json!([]));
    for source in resumed.output["instructions"]["sources"]
        .as_array()
        .unwrap()
    {
        assert_eq!(source["content"], Value::Null);
        assert!(source["fingerprint"].is_string());
        assert!(source["headings"].is_array());
        assert!(source["read_more"].is_null() || source["read_more"].is_object());
    }
}

#[tokio::test]
async fn explicit_resume_reports_changed_rules_with_new_bounded_body() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-explicit-change", "demo", root.path())
            .await;
    let first = start(
        &runtime,
        "rules-explicit-change",
        &project,
        "rules-explicit-change-window",
        StartupDetail::Standard,
        "first",
        None,
    )
    .await;
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let first_agents_fingerprint =
        instruction_source(&first.output, "AGENTS.md")["fingerprint"].clone();

    fs::write(
        root.path().join("AGENTS.md"),
        "# Repository rules\n\n- Use the new focused target first.\n",
    )
    .unwrap();
    let resumed = start(
        &runtime,
        "rules-explicit-change",
        &project,
        "rules-explicit-change-window",
        StartupDetail::Standard,
        "resume after rule change",
        Some(&session_id),
    )
    .await;

    assert!(resumed.success, "{:?}", resumed.error);
    assert_eq!(resumed.output["instructions"]["status"], "changed");
    assert_eq!(resumed.output["instructions"]["content_included"], true);
    assert_eq!(
        resumed.output["instructions"]["changed_sources"],
        json!(["AGENTS.md"])
    );
    assert_ne!(
        instruction_source(&resumed.output, "AGENTS.md")["fingerprint"],
        first_agents_fingerprint
    );
    assert!(instruction_source(&resumed.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains("new focused target"));
}

#[tokio::test]
async fn coding_task_project_switching_keeps_rule_snapshots_isolated() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    init_git_repo(root_a.path());
    init_git_repo(root_b.path());
    commit_file(
        root_a.path(),
        "AGENTS.md",
        "# Project A\n\nOnly project A rule.\n",
        "add A rules",
    );
    commit_file(
        root_b.path(),
        "AGENTS.md",
        "# Project B\n\nOnly project B rule.\n",
        "add B rules",
    );
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "rules-switch",
        None,
        RunnerCapabilities {
            shell: false,
            git: true,
            file_read: true,
            file_write: true,
            ..Default::default()
        },
        vec![
            registered_project("a", &root_a.path().to_string_lossy()),
            registered_project("b", &root_b.path().to_string_lossy()),
        ],
    )
    .await;
    let project_a = crate::tool_runtime::runner_project_runtime_id("rules-switch", "a");
    let project_b = crate::tool_runtime::runner_project_runtime_id("rules-switch", "b");

    let first_a = start(
        &runtime,
        "rules-switch",
        &project_a,
        "rules-switch-window",
        StartupDetail::Standard,
        "A",
        None,
    )
    .await;
    let session_a = first_a.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let first_b = start(
        &runtime,
        "rules-switch",
        &project_b,
        "rules-switch-window",
        StartupDetail::Standard,
        "B",
        None,
    )
    .await;
    let again_a = start(
        &runtime,
        "rules-switch",
        &project_a,
        "rules-switch-window",
        StartupDetail::Standard,
        "A again",
        Some(&session_a),
    )
    .await;

    assert_eq!(first_a.output["instructions"]["status"], "loaded");
    assert_eq!(first_b.output["instructions"]["status"], "loaded");
    assert!(instruction_source(&first_a.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains("project A"));
    assert!(instruction_source(&first_b.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains("project B"));
    assert_eq!(
        again_a.output["session"]["session_id"],
        first_a.output["session"]["session_id"]
    );
    assert_ne!(
        again_a.output["session"]["session_id"],
        first_b.output["session"]["session_id"]
    );
    assert_eq!(again_a.output["instructions"]["status"], "reused");
    assert_eq!(again_a.output["instructions"]["content_included"], false);
}

#[tokio::test]
async fn restart_restored_coding_task_session_reloads_rules_without_persisting_body() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let source_marker = "SOURCE_BODY_MUST_NEVER_ENTER_THE_DURABLE_LEDGER";
    fs::create_dir_all(root.path().join("src")).unwrap();
    commit_file(
        root.path(),
        "src/restart.rs",
        &format!("pub const MARKER: &str = \"{source_marker}\";\n"),
        "add restart source",
    );
    let ledger_dir = tempfile::tempdir().unwrap();
    let ledger = ledger_dir.path().join("sessions.json");
    let marker = "RULE_BODY_MUST_NEVER_ENTER_THE_DURABLE_LEDGER";
    fs::write(
        root.path().join("AGENTS.md"),
        format!("# Repository rules\n\n{marker}\n"),
    )
    .unwrap();
    let auth = auth_context(None, true);

    let runtime1 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    let project =
        register_runner_project_at_path(&runtime1, "rules-restart", "demo", root.path()).await;
    let first = start(
        &runtime1,
        "rules-restart",
        &project,
        "rules-restart-window",
        StartupDetail::Standard,
        "before restart",
        None,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    let session_id = first.output["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let read = dispatch_coding_call_in_window(
        &runtime1,
        "rules-restart",
        ToolCall::ReadFiles {
            project: project.clone(),
            items: vec![crate::tool_runtime::ReadFilesItem {
                path: "src/restart.rs".to_string(),
                start_line: None,
                limit: None,
            }],
            session_id: Some(session_id.clone()),
            with_line_numbers: None,
            max_result_bytes: None,
        },
        Some(&auth),
        "rules-restart-window",
    )
    .await;
    assert!(read.success, "{:?}", read.error);
    runtime1.sessions.flush_persistence();
    let persisted = fs::read_to_string(&ledger).unwrap();
    assert!(!persisted.contains(marker));
    assert!(!persisted.contains(source_marker));
    assert!(!persisted.contains("Repository rules"));
    drop(runtime1);

    let runtime2 = ToolRuntime::new_for_tests().with_session_ledger(&ledger);
    register_runner_project_at_path(&runtime2, "rules-restart", "demo", root.path()).await;
    let restored = start(
        &runtime2,
        "rules-restart",
        &project,
        "rules-restart-window",
        StartupDetail::Standard,
        "after restart",
        Some(&session_id),
    )
    .await;

    assert!(restored.success, "{:?}", restored.error);
    assert_eq!(restored.output["session"]["session_id"], session_id);
    assert_eq!(
        restored.output["session"]["continuation"],
        "resumed_explicitly"
    );
    assert_eq!(restored.output["instructions"]["status"], "loaded");
    assert_eq!(restored.output["instructions"]["content_included"], true);
    assert_eq!(
        restored.output["continuation"]["exploration"]["paths"]["items"],
        json!(["src/restart.rs"])
    );
    assert_eq!(
        restored.output["continuation"]["exploration"]["read_count"],
        1
    );
    assert_eq!(
        restored.output["continuation"]["exploration"]["complete"],
        true
    );
    assert!(instruction_source(&restored.output, "AGENTS.md")["content"]
        .as_str()
        .unwrap()
        .contains(marker));
    let summary = runtime2.sessions.summary(&session_id, Some(30)).unwrap();
    assert_eq!(
        summary
            .events
            .iter()
            .filter(|event| {
                event.kind == "tool_call_finished"
                    && event.tool_name == "read_files"
                    && event.status.as_deref() == Some("succeeded")
            })
            .count(),
        1,
        "restart continuation must not reread explored source files"
    );
}

#[tokio::test]
async fn unavailable_repository_rules_fail_conservatively_without_leaking_errors() {
    let root = tempfile::tempdir().unwrap();
    init_git_repo(root.path());
    fs::write(root.path().join("AGENTS.md"), [0xff, 0xfe, 0xfd]).unwrap();
    let runtime = ToolRuntime::new_for_tests();
    register_agent_with_projects(
        &runtime,
        "rules-unavailable",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            file_read: false,
            file_write: true,
            ..Default::default()
        },
        vec![registered_project("demo", &root.path().to_string_lossy())],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id("rules-unavailable", "demo");

    let result = start(
        &runtime,
        "rules-unavailable",
        &project,
        "rules-unavailable-window",
        StartupDetail::Standard,
        "start",
        None,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["instructions"]["status"], "unavailable");
    assert_eq!(result.output["instructions"]["content_included"], false);
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "rules_unavailable"));
    let serialized = result.output.to_string();
    assert!(!serialized.contains("file_read capability unavailable"));
    assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
}

#[tokio::test]
async fn startup_uses_project_scoped_lifecycle_aware_job_summary() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    seed_rules(root_a.path());
    seed_rules(root_b.path());
    let runtime = ToolRuntime::new_for_tests();
    let caps = RunnerCapabilities {
        shell: true,
        git: true,
        file_read: true,
        file_write: true,
        async_shell_jobs: true,
        ..Default::default()
    };
    register_agent_with_projects(
        &runtime,
        "startup-jobs",
        None,
        caps,
        vec![
            registered_project("a", &root_a.path().to_string_lossy()),
            registered_project("b", &root_b.path().to_string_lossy()),
        ],
    )
    .await;
    let project_a = crate::tool_runtime::runner_project_runtime_id("startup-jobs", "a");
    let project_b = crate::tool_runtime::runner_project_runtime_id("startup-jobs", "b");
    let job = runtime
        .runner_registry
        .start_job_with_metadata(
            ShellJobOpRequest {
                op: "start".to_string(),
                client_id: Some("startup-jobs".to_string()),
                cwd: Some(root_a.path().to_string_lossy().to_string()),
                command: Some("sleep 60".to_string()),
                timeout_secs: Some(120),
                job_id: None,
                since_stdout_line: None,
                since_stderr_line: None,
                tail_lines: None,
                limit: None,
                codex: None,
            },
            "startup-test".to_string(),
            ShellJobStartMetadata {
                project_id: Some(project_a.clone()),
                project_cwd: Some(root_a.path().to_string_lossy().to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let start_request =
        wait_for_runner_request_for_instance(&runtime, "startup-jobs", "inst").await;
    assert_eq!(start_request.kind, "start_job");
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "startup-jobs".to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job.job_id.clone(),
            request_id: Some(start_request.request_id.clone()),
            update_seq: None,
            status: "running".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: None,
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: false,
        })
        .await
        .unwrap();

    let running = start(
        &runtime,
        "startup-jobs",
        &project_a,
        "startup-jobs-running",
        StartupDetail::Full,
        "inspect running job",
        None,
    )
    .await;
    assert!(running.success, "{:?}", running.error);
    let running_brief = &running.output["startup_brief"];
    assert_eq!(running_brief["continuation"]["jobs"]["active_count"], 1);
    assert_eq!(
        running_brief["continuation"]["jobs"]["blocking_active_count"],
        1
    );
    assert_eq!(
        running_brief["continuation"]["jobs"]["nonblocking_active_count"],
        0
    );
    assert_eq!(
        running_brief["continuation"]["jobs"]["terminal_pending_count"],
        0
    );
    assert_eq!(
        running_brief["continuation"]["jobs"]["latest_status"],
        "running"
    );
    assert!(running_brief["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "active_jobs_blocking"));
    assert_eq!(running_brief["startup_verdict"]["status"], "fail");
    assert_eq!(running_brief["startup_verdict"]["blocking"], true);
    assert_eq!(running.output["startup_verdict"]["status"], "fail");
    assert_eq!(running.output["startup_verdict"]["blocking"], true);
    assert!(running.output["startup_verdict"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| {
            check["name"] == "jobs"
                && check["status"] == "fail"
                && check["reason"] == "blocking_active_jobs"
        }));

    let other_project = start(
        &runtime,
        "startup-jobs",
        &project_b,
        "startup-jobs-other-project",
        StartupDetail::Standard,
        "inspect other project",
        None,
    )
    .await;
    assert!(other_project.success, "{:?}", other_project.error);
    assert_eq!(
        other_project.output["continuation"]["jobs"]["active_count"],
        0
    );
    assert!(!other_project.output["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "active_jobs_blocking"));
    assert_ne!(other_project.output["startup_verdict"]["status"], "fail");
    assert_eq!(other_project.output["startup_verdict"]["blocking"], false);

    let stopped = runtime
        .runner_registry
        .stop_job(&job.job_id, "startup-test".to_string())
        .await
        .unwrap();
    assert_eq!(stopped.status, "stop_requested");
    let stop_request = wait_for_runner_request_for_instance(&runtime, "startup-jobs", "inst").await;
    assert_eq!(stop_request.kind, "stop_job");
    assert_eq!(stop_request.job_id.as_deref(), Some(job.job_id.as_str()));

    let terminal_pending = start(
        &runtime,
        "startup-jobs",
        &project_a,
        "startup-jobs-stop-requested",
        StartupDetail::Standard,
        "inspect terminal pending job",
        None,
    )
    .await;
    assert!(terminal_pending.success, "{:?}", terminal_pending.error);
    let jobs = &terminal_pending.output["continuation"]["jobs"];
    assert_eq!(jobs["active_count"], 1);
    assert_eq!(jobs["blocking_active_count"], 0);
    assert_eq!(jobs["nonblocking_active_count"], 1);
    assert_eq!(jobs["terminal_pending_count"], 1);
    assert_eq!(jobs["latest_status"], "stop_requested");
    assert!(!terminal_pending.output["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "active_jobs_blocking"));
    assert!(terminal_pending.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning == "active_jobs_present"));
    assert_ne!(terminal_pending.output["startup_verdict"]["status"], "fail");
    assert_eq!(
        terminal_pending.output["startup_verdict"]["blocking"],
        false
    );

    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: "startup-jobs".to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job.job_id.clone(),
            request_id: Some(start_request.request_id),
            update_seq: None,
            status: "stopped".to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: None,
            duration_ms: Some(1),
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
    assert_eq!(
        runtime
            .runner_registry
            .get_job(&job.job_id)
            .await
            .unwrap()
            .status,
        "stopped"
    );
}

#[tokio::test]
async fn startup_runner_health_uses_the_exact_project_client() {
    let target_root = tempfile::tempdir().unwrap();
    let peer_root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let target_project = crate::tool_runtime::runner_project_runtime_id("startup-target", "demo");
    register_agent_with_projects(
        &runtime,
        "startup-target",
        None,
        RunnerCapabilities::default(),
        vec![registered_project(
            "demo",
            &target_root.path().to_string_lossy(),
        )],
    )
    .await;
    register_agent_with_projects(
        &runtime,
        "startup-peer",
        None,
        RunnerCapabilities::default(),
        vec![registered_project(
            "peer",
            &peer_root.path().to_string_lossy(),
        )],
    )
    .await;
    runtime
        .runner_registry
        .reconcile_disconnect("startup-target", "inst")
        .await;

    let status = runtime.runtime_status(None).await;
    assert!(status.success);
    let clients = status.output["agents"]["summary"]["clients"]
        .as_array()
        .unwrap();
    assert!(clients
        .iter()
        .any(|client| client["client_id"] == "startup-peer" && client["status"] == "online"));
    assert!(clients
        .iter()
        .any(|client| { client["client_id"] == "startup-target" && client["status"] != "online" }));

    let auth = auth_context(None, true);
    let unavailable = runtime
        .start_coding_workflow_for_test(
            target_project.clone(),
            None,
            None,
            Some("inspect unavailable target runner".to_string()),
            SessionMode::Normal,
            false,
            false,
            StartupDetail::Full,
            None,
            None,
            Some(&auth),
            None,
            None,
            crate::tool_runtime::sessions::SessionTransport::Api,
        )
        .await;
    assert!(unavailable.success, "{:?}", unavailable.error);
    let unavailable_brief = &unavailable.output["startup_brief"];
    assert!(unavailable_brief["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "runner_unavailable"));
    assert_eq!(unavailable_brief["startup_verdict"]["status"], "fail");
    assert_eq!(unavailable_brief["startup_verdict"]["blocking"], true);
    assert_eq!(unavailable.output["startup_verdict"]["status"], "fail");
    assert_eq!(unavailable.output["startup_verdict"]["blocking"], true);
    assert!(unavailable.output["startup_verdict"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| {
            check["name"] == "agent"
                && check["status"] == "fail"
                && check["reason"] == "agent_offline"
        }));

    register_agent_with_projects(
        &runtime,
        "startup-target",
        None,
        RunnerCapabilities::default(),
        vec![registered_project(
            "demo",
            &target_root.path().to_string_lossy(),
        )],
    )
    .await;
    runtime
        .runner_registry
        .reconcile_disconnect("startup-peer", "inst")
        .await;
    let available = start(
        &runtime,
        "startup-target",
        &target_project,
        "startup-target-online",
        StartupDetail::Full,
        "inspect available target runner",
        None,
    )
    .await;
    assert!(available.success, "{:?}", available.error);
    let available_brief = &available.output["startup_brief"];
    assert!(!available_brief["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|blocker| blocker == "runner_unavailable"));
    assert_ne!(available_brief["startup_verdict"]["status"], "fail");
    assert_eq!(available_brief["startup_verdict"]["blocking"], false);
    assert_ne!(available.output["startup_verdict"]["status"], "fail");
    assert_eq!(available.output["startup_verdict"]["blocking"], false);
    assert!(available.output["startup_verdict"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "agent" && check["status"] == "pass"));
}

#[tokio::test]
async fn minimal_standard_and_full_coding_workflow_diagnostics_validate_against_strict_schema() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-schema", "demo", root.path()).await;
    let schema = registry::coding_workflow_diagnostic_output_schema_for_test();
    let recorder = runtime.sessions.start_session_with_guards(
        Some(project.clone()),
        Some("startup schema recorder".to_string()),
        SessionMode::Normal,
        crate::tool_runtime::sessions::SessionGuards::default(),
    );

    for (detail, label) in [
        (StartupDetail::Minimal, "minimal"),
        (StartupDetail::Standard, "standard"),
        (StartupDetail::Full, "full"),
    ] {
        let result = start(
            &runtime,
            "rules-schema",
            &project,
            &format!("rules-schema-{label}"),
            detail,
            label,
            None,
        )
        .await;
        assert!(result.success, "{label}: {:?}", result.error);
        let output_bytes = serde_json::to_vec(&result.output).unwrap().len();
        let core = if label == "full" {
            &result.output["startup_brief"]
        } else {
            &result.output
        };
        let core_bytes = startup_brief_size(core);
        assert_builtin_workflow(core);
        let action_wrapped_bytes = serde_json::to_vec(&ToolResult::ok(json!({
            "compact": true,
            "startup_brief": core,
        })))
        .unwrap()
        .len();
        eprintln!(
            "actual_{label}_output_bytes={output_bytes} actual_{label}_core_bytes={core_bytes} actual_{label}_action_wrapped_bytes={action_wrapped_bytes}"
        );
        let value = json!({
            "success": result.success,
            "output": result.output,
            "error": result.error,
        });
        validate_schema_instance_for_test(&value, &schema)
            .unwrap_or_else(|error| panic!("{label} startup schema mismatch: {error}\n{value}"));

        let mut recorded = ToolResult::ok(value["output"].clone());
        crate::tool_runtime::add_session_hint(
            &mut recorded,
            &runtime.sessions,
            &recorder.session_id,
        );
        recorded.output["session_hint"] = json!({
            "has_open_messages": true,
            "open_counts": {"guidance": 1, "question": 0, "todo": 0, "risk": 0},
            "highest_priority": "normal",
            "suggested_next_tool": "session_discussion_summary"
        });
        recorded.output["permission"] = json!({"status": "auto_approved"});
        let recorded_value = json!({
            "success": recorded.success,
            "output": recorded.output,
            "error": recorded.error,
        });
        validate_schema_instance_for_test(&recorded_value, &schema).unwrap_or_else(|error| {
            panic!("{label} recorded startup schema mismatch: {error}\n{recorded_value}")
        });
    }

    let standard = start(
        &runtime,
        "rules-schema",
        &project,
        "rules-schema-unknown",
        StartupDetail::Standard,
        "unknown field",
        None,
    )
    .await;
    let mut unknown = json!({
        "success": true,
        "output": standard.output,
        "error": Value::Null,
    });
    unknown["output"]["unknown_field"] = json!(true);
    assert!(validate_schema_instance_for_test(&unknown, &schema).is_err());

    unknown["output"]
        .as_object_mut()
        .unwrap()
        .remove("unknown_field");
    unknown["output"]["continuation"]["validation"]["latest_status"] =
        json!("implementation_schema_drift");
    assert!(validate_schema_instance_for_test(&unknown, &schema).is_err());

    unknown["output"]["continuation"]["validation"]["latest_status"] = json!("not_run");
    unknown["output"]["continuation"]["exploration"]["unknown_field"] = json!(true);
    assert!(
        validate_schema_instance_for_test(&unknown, &schema).is_err(),
        "strict exploration schema must reject unknown fields"
    );
}

#[tokio::test]
async fn worst_case_startup_with_huge_repository_stays_below_hard_limit() {
    let root = tempfile::tempdir().unwrap();
    seed_rules(root.path());
    let runtime = ToolRuntime::new_for_tests();
    let project =
        register_runner_project_at_path(&runtime, "rules-worst", "demo", root.path()).await;

    // A worst-case repository: many tracked manifests, key files, top-level
    // entries, and per-class roots, all with long names, alongside a large
    // rule body. The shared brief must remain below the hard limit and keep a
    // valid schema.
    for index in 0..120 {
        let dir = root
            .path()
            .join(format!("crates/package-{index:03}-{}", "p".repeat(80)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("Cargo.toml-{index:03}-{}", "m".repeat(90))),
            b"manifest metadata",
        )
        .unwrap();
    }
    for index in 0..60 {
        std::fs::write(
            root.path()
                .join(format!("generated-doc-{index:03}-{}.md", "d".repeat(120))),
            b"doc",
        )
        .unwrap();
    }
    for cmd in ["git add -A", "git commit -m 'seed worst-case repo'"] {
        let (exit_code, stdout, stderr, _) =
            crate::tool_runtime::helpers::run_command_sync(cmd, root.path(), 30);
        assert_eq!(exit_code, 0, "{stdout}{stderr}");
    }

    let result = start(
        &runtime,
        "rules-worst",
        &project,
        "rules-worst-window",
        StartupDetail::Standard,
        "worst case",
        None,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["repository"]["status"], "available");

    let schema = registry::coding_workflow_diagnostic_output_schema_for_test();
    let value = json!({
        "success": result.success,
        "output": result.output,
        "error": result.error,
    });
    validate_schema_instance_for_test(&value, &schema)
        .unwrap_or_else(|error| panic!("worst-case startup schema mismatch: {error}"));

    let bytes = startup_brief_size(&result.output);
    eprintln!("worst_case_huge_repository_startup_bytes={bytes}");
    assert!(
        bytes <= STANDARD_STARTUP_HARD_MAX_BYTES,
        "shared startup brief exceeded hard limit: {bytes}"
    );

    // Bounded repository lists record truncation metadata honestly.
    let top_level = &result.output["repository"]["top_level"];
    assert!(top_level["returned"].as_u64().unwrap() <= 24);
    assert!(
        top_level["truncated"] == Value::Bool(true) || top_level["truncated"] == Value::Bool(false)
    );
    let manifests = &result.output["repository"]["manifests"];
    assert!(manifests["returned"].as_u64().unwrap() <= 12);

    // Rule prose outranks repository entries: rule sources still carry content.
    for source in result.output["instructions"]["sources"].as_array().unwrap() {
        assert!(
            source["content"]
                .as_str()
                .is_some_and(|content| !content.is_empty()),
            "each loaded rule source must retain a usable bounded excerpt"
        );
    }
}
