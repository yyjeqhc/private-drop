//! Git tests for tool_runtime.

use super::super::git::*;
use super::super::git_review::*;
use super::super::helpers::*;
use super::super::*;
use super::support::*;
use crate::runner_protocol::{RunnerCapabilities, RunnerResultRequest};
use crate::tool_runtime::ToolRuntime;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use webcodex_core::runtime_contract::{
    DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES, MAX_GIT_DIFF_HUNKS_PAGE_BYTES,
    MIN_GIT_DIFF_HUNKS_PAGE_BYTES, MODEL_INSPECTION_MAX_RESULT_BYTES,
};

const ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES: usize = 256 * 1024;

async fn register_structured_git_agent_at_path(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &Path,
) -> String {
    let project_path = root.to_string_lossy().to_string();
    register_agent_with_projects(
        runtime,
        client_id,
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            structured_process_argv: true,
            internal_posix_script: true,
            ..Default::default()
        },
        vec![registered_project(project_id, &project_path)],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

async fn run_runner_git_commit_paths(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    expected_head: String,
    paths: Vec<String>,
    message: &str,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let message = message.to_string();
        async move {
            runtime
                .git_commit_paths(project, expected_head, paths, message)
                .await
        }
    });
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(request.kind, "run_internal_posix_script");
    let script = request
        .script
        .as_ref()
        .expect("git_commit_paths must use a typed internal script");
    assert_eq!(script.language.as_str(), "sh");
    assert!(script.script.contains("git update-ref"));
    assert!(script.script.contains("git commit-tree"));
    assert!(!script.script.contains("git push"));
    let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
    complete_patch_agent_request(
        runtime,
        client_id,
        &request.request_id,
        exit_code,
        &stdout,
        &stderr,
    )
    .await;
    task.await.unwrap()
}

#[tokio::test]
async fn git_commit_paths_commits_only_requested_paths_and_preserves_other_worktree_changes() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("a.txt"), "base-a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), "base-b\n").unwrap();
    git_test_command_ok(tmp.path(), "git add a.txt b.txt");
    git_test_command_ok(tmp.path(), "git commit -m base");
    let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
    let base = stdout.trim().to_string();

    fs::write(tmp.path().join("a.txt"), "committed-a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), "still-dirty-b\n").unwrap();
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "commit-paths", "repo", tmp.path()).await;
    let result = run_runner_git_commit_paths(
        &runtime,
        "commit-paths",
        project,
        base.clone(),
        vec!["a.txt".to_string()],
        "commit exact a",
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["committed"], true);
    assert_eq!(result.output["previous_head"], base);
    assert_eq!(result.output["committed_paths"], json!(["a.txt"]));
    assert_eq!(result.output["hook_policy"], "bypassed_exact_tree");

    let new_head = result.output["new_head"].as_str().unwrap();
    let (_, changed, _, _) = run_command_sync(
        &format!("git diff --name-only {} {}", base, new_head),
        tmp.path(),
        30,
    );
    assert_eq!(changed.trim(), "a.txt");
    let (_, staged, _, _) = run_command_sync("git diff --cached --name-only", tmp.path(), 30);
    assert!(
        staged.trim().is_empty(),
        "real index must remain clean: {staged}"
    );
    let (_, status, _, _) = run_command_sync("git status --short", tmp.path(), 30);
    assert_eq!(status.trim(), "M b.txt");
}

#[tokio::test]
async fn git_commit_paths_rejects_existing_staged_state_without_advancing_head() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("a.txt"), "base-a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), "base-b\n").unwrap();
    git_test_command_ok(tmp.path(), "git add a.txt b.txt");
    git_test_command_ok(tmp.path(), "git commit -m base");
    let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
    let base = stdout.trim().to_string();
    fs::write(tmp.path().join("a.txt"), "staged-a\n").unwrap();
    fs::write(tmp.path().join("b.txt"), "requested-b\n").unwrap();
    git_test_command_ok(tmp.path(), "git add a.txt");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "commit-staged-reject", "repo", tmp.path())
            .await;
    let result = run_runner_git_commit_paths(
        &runtime,
        "commit-staged-reject",
        project,
        base.clone(),
        vec!["b.txt".to_string()],
        "must reject staged",
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], "existing_staged");
    assert_eq!(result.output["state_changed"], false);
    let (_, head, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
    assert_eq!(head.trim(), base);
    let (_, staged, _, _) = run_command_sync("git diff --cached --name-only", tmp.path(), 30);
    assert_eq!(staged.trim(), "a.txt");
}

#[tokio::test]
async fn git_commit_paths_rejects_stale_expected_head_before_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("a.txt"), "base\n").unwrap();
    git_test_command_ok(tmp.path(), "git add a.txt");
    git_test_command_ok(tmp.path(), "git commit -m base");
    let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
    let actual = stdout.trim().to_string();
    fs::write(tmp.path().join("a.txt"), "dirty\n").unwrap();

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "commit-head-fence", "repo", tmp.path())
            .await;
    let stale = "f".repeat(40);
    let result = run_runner_git_commit_paths(
        &runtime,
        "commit-head-fence",
        project,
        stale.clone(),
        vec!["a.txt".to_string()],
        "must not commit",
    )
    .await;
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], "head_mismatch");
    assert_eq!(result.output["expected_head"], stale);
    assert_eq!(result.output["actual_head"], actual);
    assert_eq!(result.output["state_changed"], false);
    let (_, head, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
    assert_eq!(head.trim(), actual);
}

#[test]
fn git_commit_paths_audit_keeps_message_private_and_exact_head_bounded() {
    let private_message = "PRIVATE_COMMIT_MESSAGE_MUST_NOT_PERSIST";
    let expected_head = "a".repeat(40);
    let arguments = json!({
        "project": "agent:oe:webcodex",
        "expected_head": expected_head,
        "paths": ["src/tool_runtime/git.rs"],
        "message": private_message,
    });
    let raw = super::super::tool_audit::session_log_arguments_for_tool_request(
        "git_commit_paths",
        &arguments,
    );
    assert_eq!(raw["expected_head_valid"], true);
    assert_eq!(raw["expected_head"], "a".repeat(40));
    assert_eq!(raw["message_present"], true);
    assert_eq!(raw["paths"], json!(["src/tool_runtime/git.rs"]));
    assert!(!raw.to_string().contains(private_message));

    let call = ToolCall::from_tool_name("git_commit_paths", arguments).unwrap();
    let typed = call.session_log_arguments();
    assert_eq!(typed["expected_head_valid"], true);
    assert_eq!(typed["message_present"], true);
    assert!(!typed.to_string().contains(private_message));
}

#[test]
fn git_commit_marker_parser_keeps_mutation_evidence_strict() {
    let expected = "a".repeat(40);
    let new_head = "b".repeat(40);
    let valid = parse_git_commit_marker(&format!(
        "noise\n{GIT_COMMIT_RESULT_PREFIX} status=success previous={expected} new={new_head}\n"
    ))
    .unwrap();
    assert_eq!(valid.status, "success");
    assert_eq!(valid.previous_head.as_deref(), Some(expected.as_str()));
    assert_eq!(valid.new_head.as_deref(), Some(new_head.as_str()));

    let malformed = parse_git_commit_marker(&format!(
        "{GIT_COMMIT_RESULT_PREFIX} status=success previous={expected} new=not-a-sha\n"
    ))
    .unwrap();
    assert_eq!(malformed.status, "success");
    assert!(malformed.new_head.is_none());
}

#[tokio::test]
async fn git_restore_paths_restores_tracked_filename_containing_target() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(
        tmp.path(),
        "SMOKE_TARGET.txt",
        "original\n",
        "track smoke target",
    );
    fs::write(tmp.path().join("SMOKE_TARGET.txt"), "modified\n").unwrap();

    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "restore-target-substring",
        "repo",
        tmp.path(),
    )
    .await;
    let restore = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .git_restore_paths(project, vec!["SMOKE_TARGET.txt".to_string()])
                .await
        }
    });

    let request = wait_for_patch_agent_request(&runtime, "restore-target-substring").await;
    assert_eq!(request.kind, "run_process");
    assert!(request.command.is_empty());
    let process = request.process.as_ref().expect("typed git restore process");
    assert_eq!(process.executable, "git");
    assert_eq!(
        process.args,
        ["restore", "--", "SMOKE_TARGET.txt"].map(str::to_string)
    );
    complete_agent_request_by_running_locally(&runtime, "restore-target-substring", request).await;

    let result = restore.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        fs::read_to_string(tmp.path().join("SMOKE_TARGET.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "original\n"
    );
    let (exit_code, stdout, stderr, _) = run_command_sync("git status --porcelain", tmp.path(), 30);
    assert_eq!(exit_code, 0, "git status failed: {stderr}");
    assert!(stdout.is_empty(), "worktree should be clean: {stdout}");
}

#[tokio::test]
async fn git_path_mutations_pass_shell_sensitive_paths_as_literal_argv() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let tracked = [
        "space name.txt",
        "quote'name.txt",
        "amp&semi;.txt",
        "dollar$(literal).txt",
    ];
    for path in tracked {
        commit_file(tmp.path(), path, "original\n", &format!("track {path}"));
        fs::write(tmp.path().join(path), "modified\n").unwrap();
    }

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "literal-git-paths", "repo", tmp.path())
            .await;
    let restore_paths = tracked.map(str::to_string).to_vec();
    let restore = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let restore_paths = restore_paths.clone();
        async move { runtime.git_restore_paths(project, restore_paths).await }
    });
    let request = wait_for_patch_agent_request(&runtime, "literal-git-paths").await;
    assert_eq!(request.kind, "run_process");
    let process = request.process.as_ref().expect("typed git restore process");
    assert_eq!(process.executable, "git");
    let mut expected = vec!["restore".to_string(), "--".to_string()];
    expected.extend(restore_paths.iter().cloned());
    assert_eq!(process.args, expected);
    complete_agent_request_by_running_locally(&runtime, "literal-git-paths", request).await;
    assert!(restore.await.unwrap().success);
    for path in tracked {
        assert_eq!(
            fs::read_to_string(tmp.path().join(path))
                .unwrap()
                .replace("\r\n", "\n"),
            "original\n"
        );
    }

    let untracked = [
        "untracked space.txt",
        "untracked'quote.txt",
        "untracked&semi;.txt",
        "untracked$(literal).txt",
    ];
    for path in untracked {
        fs::write(tmp.path().join(path), "remove me\n").unwrap();
    }
    let discard_paths = untracked.map(str::to_string).to_vec();
    let discard = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let discard_paths = discard_paths.clone();
        async move { runtime.discard_untracked(project, discard_paths).await }
    });
    let request = wait_for_patch_agent_request(&runtime, "literal-git-paths").await;
    assert_eq!(request.kind, "run_process");
    let process = request.process.as_ref().expect("typed git clean process");
    assert_eq!(process.executable, "git");
    let mut expected = vec!["clean".to_string(), "-f".to_string(), "--".to_string()];
    expected.extend(discard_paths.iter().cloned());
    assert_eq!(process.args, expected);
    complete_agent_request_by_running_locally(&runtime, "literal-git-paths", request).await;
    assert!(discard.await.unwrap().success);
    for path in untracked {
        assert!(
            !tmp.path().join(path).exists(),
            "{path} should be removed literally"
        );
    }
}

#[tokio::test]
async fn git_restore_stays_sync_on_structured_job_capable_runner() {
    let runtime = runtime_with_agent_project("restore-sync-job-capable");
    register_agent(
        &runtime,
        "restore-sync-job-capable",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            jobs: true,
            async_jobs: true,
            structured_process_argv: true,
            structured_execution_jobs: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("restore-sync-job-capable");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .git_restore_paths(project, vec!["safe.txt".to_string()])
                .await
        }
    });

    let request = wait_for_patch_agent_request(&runtime, "restore-sync-job-capable").await;
    assert_eq!(request.kind, "run_process");
    assert!(request.job_id.is_none());
    assert!(request.command.is_empty());
    let process = request.process.as_ref().expect("typed git restore process");
    assert_eq!(process.executable, "git");
    assert_eq!(
        process.args,
        ["restore", "--", "safe.txt"].map(str::to_string)
    );
    complete_patch_agent_request(
        &runtime,
        "restore-sync-job-capable",
        &request.request_id,
        0,
        "",
        "",
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["restored_paths"], json!(["safe.txt"]));
    assert!(
        probe_patch_agent_request(&runtime, "restore-sync-job-capable")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn git_restore_replacement_after_dispatch_reports_outcome_unknown_without_retry() {
    let runtime = runtime_with_agent_project("restore-uncertain");
    register_agent(
        &runtime,
        "restore-uncertain",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            structured_process_argv: true,
            ..Default::default()
        },
    )
    .await;
    let project = agent_test_project_id("restore-uncertain");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .git_restore_paths(project, vec!["safe.txt".to_string()])
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "restore-uncertain").await;
    assert_eq!(request.kind, "run_process");

    runtime
        .runner_registry
        .set_last_seen_for_test("restore-uncertain", chrono::Utc::now().timestamp() - 120)
        .await;
    register_agent_with_instance(
        &runtime,
        "restore-uncertain",
        "inst-b",
        None,
        RunnerCapabilities {
            shell: true,
            git: true,
            structured_process_argv: true,
            ..Default::default()
        },
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "outcome_unknown");
    assert_eq!(result.output["failure_kind"], "outcome_unknown");
    let retry = probe_agent_request_for_instance(&runtime, "restore-uncertain", "inst-b").await;
    assert!(
        retry.is_none(),
        "uncertain mutation must not be retried: {retry:?}"
    );
}

#[test]
fn git_diff_hunks_tool_is_known_and_schema_is_bounded() {
    assert!(is_known_tool_name("git_diff_hunks"));
    let call = ToolCall::from_tool_name(
        "git_diff_hunks",
        json!({
            "project":"agent:oe:webcodex",
            "paths":["src/runtime_http.rs"],
            "max_hunks":20,
            "max_hunk_lines":120,
            "max_page_bytes":98304,
            "cached":true,
            "continuation":"opaque-continuation"
        }),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::GitDiffHunks {
            project,
            cached: Some(true),
            max_page_bytes: Some(98304),
            continuation: Some(continuation),
            ..
        } if project == "agent:oe:webcodex" && continuation == "opaque-continuation"
    ));

    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "git_diff_hunks");
    let props = spec.input_schema["properties"].as_object().unwrap();
    for field in [
        "project",
        "paths",
        "max_hunks",
        "max_hunk_lines",
        "max_page_bytes",
        "cached",
        "base_commit",
        "head_commit",
        "continuation",
    ] {
        assert!(props.contains_key(field), "missing {}", field);
    }
    assert_eq!(
        props["continuation"]["maxLength"],
        GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES
    );
    assert_eq!(
        props["max_page_bytes"]["minimum"],
        MIN_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert_eq!(
        props["max_page_bytes"]["default"],
        DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert_eq!(
        props["max_page_bytes"]["maximum"],
        MAX_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    for field in ["base_commit", "head_commit"] {
        assert_eq!(props[field]["minLength"], 40);
        assert_eq!(props[field]["maxLength"], 40);
        assert_eq!(props[field]["pattern"], "^[0-9A-Fa-f]{40}$");
    }
    assert!(spec.input_schema["allOf"].is_array());
    let committed_call = ToolCall::from_tool_name(
        "git_diff_hunks",
        json!({
            "project": "agent:oe:webcodex",
            "base_commit": "A".repeat(40),
            "head_commit": "b".repeat(40),
            "paths": ["src/runtime_http.rs"]
        }),
    )
    .unwrap();
    assert!(matches!(
        committed_call,
        ToolCall::GitDiffHunks {
            base_commit: Some(base),
            head_commit: Some(head),
            cached: None,
            ..
        } if base == "A".repeat(40) && head == "b".repeat(40)
    ));
    assert!(ToolCall::from_tool_name(
        "git_diff_hunks",
        json!({
            "project": "agent:oe:webcodex",
            "continuation": "x".repeat(GIT_DIFF_HUNKS_CONTINUATION_MAX_BYTES + 1),
        }),
    )
    .is_err());
    let output_props = spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "project",
        "paths",
        "cached",
        "files",
        "max_page_bytes",
        "hunk_count",
        "truncated",
        "truncation_reasons",
        "has_more",
        "next_continuation",
        "recovery",
        "exit_code",
        "stderr",
    ] {
        assert!(output_props.contains_key(field), "missing {}", field);
    }
    let recovery = &output_props["recovery"];
    assert!(recovery["properties"]["arguments"]["anyOf"].is_array());
    let omitted_lines = &recovery["properties"]["omitted_lines"];
    for field in [
        "present",
        "recoverable",
        "reason_code",
        "path_provenance",
        "paths",
        "next_call",
    ] {
        assert!(
            omitted_lines["properties"].get(field).is_some(),
            "missing omitted_lines recovery field {field}"
        );
    }
}

#[test]
fn git_diff_hunks_session_audit_redacts_continuation() {
    let continuation = "WCDH_UNIQUE_AUDIT_CONTINUATION_7f18b4a2";
    let arguments = json!({
        "project": "agent:oe:webcodex",
        "paths": ["src/runtime_http.rs", "src/tool_runtime/git.rs"],
        "max_hunks": 7,
        "max_hunk_lines": 33,
        "max_page_bytes": 96 * 1024,
        "cached": true,
        "continuation": continuation,
    });

    let raw_summary = super::super::tool_audit::session_log_arguments_for_tool_request(
        "git_diff_hunks",
        &arguments,
    );
    assert_eq!(raw_summary["project"], "agent:oe:webcodex");
    assert_eq!(
        raw_summary["paths"],
        json!(["src/runtime_http.rs", "src/tool_runtime/git.rs"])
    );
    assert_eq!(raw_summary["max_hunks"], 7);
    assert_eq!(raw_summary["max_hunk_lines"], 33);
    assert_eq!(raw_summary["max_page_bytes"], 96 * 1024);
    assert_eq!(raw_summary["cached"], true);
    assert_eq!(raw_summary["continuation_present"], true);
    assert!(raw_summary.get("continuation").is_none());
    assert!(!serde_json::to_string(&raw_summary)
        .unwrap()
        .contains(continuation));

    let call = ToolCall::from_tool_name("git_diff_hunks", arguments.clone()).unwrap();
    let typed_summary = call.session_log_arguments();
    assert_eq!(typed_summary["project"], "agent:oe:webcodex");
    assert_eq!(typed_summary["paths"], raw_summary["paths"]);
    assert_eq!(typed_summary["max_hunks"], 7);
    assert_eq!(typed_summary["max_hunk_lines"], 33);
    assert_eq!(typed_summary["max_page_bytes"], 96 * 1024);
    assert_eq!(typed_summary["cached"], true);
    assert!(typed_summary.get("continuation").is_none());
    assert!(!serde_json::to_string(&typed_summary)
        .unwrap()
        .contains(continuation));

    let defensive =
        super::super::sessions::session_input_summary_for_tool("git_diff_hunks", &arguments);
    assert_eq!(defensive["project"], "agent:oe:webcodex");
    assert_eq!(defensive["paths"], raw_summary["paths"]);
    assert_eq!(defensive["max_hunks"], 7);
    assert_eq!(defensive["max_hunk_lines"], 33);
    assert_eq!(defensive["max_page_bytes"], 96 * 1024);
    assert_eq!(defensive["cached"], true);
    assert!(defensive.get("continuation").is_none());
    assert!(!serde_json::to_string(&defensive)
        .unwrap()
        .contains(continuation));

    let runtime = test_runtime();
    let session = runtime.sessions.start_session(
        Some("agent:oe:webcodex".to_string()),
        Some("git diff audit".to_string()),
    );
    runtime.sessions.record_tool_call_started(
        Some(&session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Api,
        "git_diff_hunks",
        &arguments,
        crate::tool_runtime::sessions::session_tool_contract("git_diff_hunks"),
    );
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(10))
        .unwrap();
    let input_summary = summary.events[0].input_summary.as_ref().unwrap();
    assert_eq!(input_summary["project"], "agent:oe:webcodex");
    assert_eq!(input_summary["paths"], raw_summary["paths"]);
    assert_eq!(input_summary["max_hunks"], 7);
    assert_eq!(input_summary["max_hunk_lines"], 33);
    assert_eq!(input_summary["max_page_bytes"], 96 * 1024);
    assert_eq!(input_summary["cached"], true);
    assert!(input_summary.get("continuation").is_none());
    assert!(!serde_json::to_string(input_summary)
        .unwrap()
        .contains(continuation));

    let base = "A".repeat(40);
    let head = "b".repeat(40);
    let committed_arguments = json!({
        "project": "agent:oe:webcodex",
        "paths": ["src/runtime_http.rs"],
        "base_commit": base,
        "head_commit": head,
        "continuation": continuation,
    });
    let committed_summary = super::super::tool_audit::session_log_arguments_for_tool_request(
        "git_diff_hunks",
        &committed_arguments,
    );
    assert_eq!(committed_summary["base_commit"], "a".repeat(40));
    assert_eq!(committed_summary["head_commit"], "b".repeat(40));
    assert_eq!(committed_summary["base_commit_valid"], true);
    assert_eq!(committed_summary["head_commit_valid"], true);
    assert!(committed_summary.get("continuation").is_none());

    let private_hunk = "PRIVATE_GIT_DIFF_HUNK_BODY_91e2";
    let result_summary = super::super::tool_audit::session_log_result_for_tool(
        "git_diff_hunks",
        &json!({
            "project": "agent:oe:webcodex",
            "scope": {
                "mode": "committed",
                "requested_base": "a".repeat(40),
                "requested_head": "b".repeat(40),
                "merge_base": "a".repeat(40),
                "base_is_ancestor": true,
                "diff_range": format!("{}..{}", "a".repeat(40), "b".repeat(40))
            },
            "cached": false,
            "files": [{"path": "src/runtime_http.rs", "hunks": [{"diff": private_hunk}]}],
            "hunk_count": 1,
            "truncated": true,
            "truncation_reasons": ["page_hunk_limit"],
            "has_more": true,
            "next_continuation": continuation,
            "exit_code": 0,
            "stderr": "PRIVATE_STDERR",
        }),
    );
    let serialized_result = serde_json::to_string(&result_summary).unwrap();
    assert_eq!(result_summary["hunk_count"], 1);
    assert_eq!(result_summary["file_count"], 1);
    assert!(result_summary.get("files").is_none());
    assert!(result_summary.get("next_continuation").is_none());
    assert!(result_summary.get("stderr").is_none());
    assert!(!serialized_result.contains(private_hunk));
    assert!(!serialized_result.contains(continuation));
    assert!(!serialized_result.contains("PRIVATE_STDERR"));
}

#[test]
fn show_changes_tool_is_known_and_parses() {
    assert!(is_known_tool_name("show_changes"));
    let call = ToolCall::from_tool_name(
        "show_changes",
        json!({
            "project": "agent:oe:webcodex",
            "include_diff": true,
            "max_hunks": 4,
            "max_hunk_lines": 12,
            "session_id": "wc_sess_1234",
            "session_event_limit": 8
        }),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::ShowChanges {
            project,
            session_id: Some(session_id),
            include_diff: Some(true),
            max_hunks: Some(4),
            max_hunk_lines: Some(12),
            session_event_limit: Some(8)
        } if project == "agent:oe:webcodex" && session_id == "wc_sess_1234"
    ));

    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "show_changes");
    let output_props = spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(
        output_props.contains_key("verdict"),
        "show_changes output schema should expose verdict"
    );
    assert!(
        output_props.contains_key("diff_stat_status"),
        "show_changes output schema should expose strict diff-stat observation"
    );
}

async fn run_runner_git_diff_hunks_page(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    repo: &Path,
    paths: Option<Vec<String>>,
    max_hunks: usize,
    max_hunk_lines: usize,
    cached: bool,
    continuation: Option<String>,
) -> (ToolResult, usize, String) {
    run_runner_git_diff_hunks_page_with_budget(
        runtime,
        client_id,
        project,
        repo,
        paths,
        max_hunks,
        max_hunk_lines,
        None,
        cached,
        continuation,
    )
    .await
}

async fn run_runner_git_diff_hunks_page_with_budget(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    repo: &Path,
    paths: Option<Vec<String>>,
    max_hunks: usize,
    max_hunk_lines: usize,
    max_page_bytes: Option<usize>,
    cached: bool,
    continuation: Option<String>,
) -> (ToolResult, usize, String) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        async move {
            runtime
                .git_diff_hunks_continued_with_range_and_page_bytes(
                    project,
                    paths,
                    Some(max_hunks),
                    Some(max_hunk_lines),
                    max_page_bytes,
                    Some(cached),
                    None,
                    None,
                    continuation,
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(runtime, client_id).await;
    assert_eq!(request.kind, "run_internal_posix_script");
    assert_eq!(
        request.cwd.as_deref(),
        Some(repo.to_string_lossy().as_ref())
    );
    assert!(request.command.is_empty());
    let script = request
        .script
        .as_ref()
        .expect("git_diff_hunks must carry a typed internal script")
        .script
        .clone();
    let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
    let stdout_bytes = stdout.len();
    complete_patch_agent_request(
        runtime,
        client_id,
        &request.request_id,
        exit_code,
        &stdout,
        &stderr,
    )
    .await;
    (task.await.unwrap(), stdout_bytes, script)
}

async fn run_parser_ready_worktree_git_diff_hunks_call(
    runtime: &ToolRuntime,
    client_id: &str,
    repo: &Path,
    call: &Value,
) -> ToolResult {
    let tool = call["tool"]
        .as_str()
        .expect("recovery call tool must be a string");
    let parsed = ToolCall::from_tool_name(tool, call["arguments"].clone())
        .expect("recovery next_call must parse directly");
    let ToolCall::GitDiffHunks {
        project,
        paths,
        max_hunks,
        max_hunk_lines,
        cached,
        base_commit,
        max_page_bytes,
        head_commit,
        continuation,
        ..
    } = parsed
    else {
        panic!("recovery next_call must target git_diff_hunks");
    };
    assert!(base_commit.is_none() && head_commit.is_none());
    let (result, _, _) = run_runner_git_diff_hunks_page_with_budget(
        runtime,
        client_id,
        &project,
        repo,
        paths,
        max_hunks.expect("recovery call must preserve max_hunks"),
        max_hunk_lines.expect("recovery call must preserve max_hunk_lines"),
        max_page_bytes,
        cached.unwrap_or(false),
        continuation,
    )
    .await;
    result
}

async fn run_runner_git_diff_hunks_committed_page(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    repo: &Path,
    paths: Option<Vec<String>>,
    max_hunks: usize,
    max_hunk_lines: usize,
    base_commit: String,
    head_commit: String,
    continuation: Option<String>,
) -> (ToolResult, usize, Vec<String>) {
    run_runner_git_diff_hunks_committed_page_with_budget(
        runtime,
        client_id,
        project,
        repo,
        paths,
        max_hunks,
        max_hunk_lines,
        None,
        base_commit,
        head_commit,
        continuation,
    )
    .await
}

async fn run_runner_git_diff_hunks_committed_page_with_budget(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    repo: &Path,
    paths: Option<Vec<String>>,
    max_hunks: usize,
    max_hunk_lines: usize,
    max_page_bytes: Option<usize>,
    base_commit: String,
    head_commit: String,
    continuation: Option<String>,
) -> (ToolResult, usize, Vec<String>) {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.to_string();
        async move {
            runtime
                .git_diff_hunks_continued_with_range_and_page_bytes(
                    project,
                    paths,
                    Some(max_hunks),
                    Some(max_hunk_lines),
                    max_page_bytes,
                    None,
                    Some(base_commit),
                    Some(head_commit),
                    continuation,
                )
                .await
        }
    });
    let mut scripts = Vec::new();
    let mut page_stdout_bytes = 0usize;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    for _ in 0..16 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "committed git_diff_hunks did not finish within 10 seconds for client {client_id}"
        );
        if task.is_finished() {
            break;
        }
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            assert_eq!(request.kind, "run_internal_posix_script");
            assert_eq!(
                request.cwd.as_deref(),
                Some(repo.to_string_lossy().as_ref())
            );
            assert!(request.command.is_empty());
            let payload = request
                .script
                .as_ref()
                .expect("committed git_diff_hunks must use typed internal scripts");
            let script = payload.script.clone();
            assert!(script.contains("GIT_NO_REPLACE_OBJECTS=1"));
            assert!(script.contains("GIT_NO_LAZY_FETCH=1"));
            assert!(script.contains("GIT_OPTIONAL_LOCKS=0"));
            assert!(script.contains("GIT_CONFIG_GLOBAL=/dev/null"));
            assert!(script.contains("attributesFile = /dev/null"));
            for forbidden in [
                "git fetch",
                "git apply",
                "git commit",
                "git checkout",
                "git reset",
                "git push",
                "git stash",
                "git rebase",
                "git clean",
                "git add ",
            ] {
                assert!(
                    !script.contains(forbidden),
                    "committed git_diff_hunks must remain read-only; found {forbidden}: {script}"
                );
            }
            if script.contains(" diff ") {
                assert!(script.contains("--no-ext-diff"));
                assert!(script.contains("--no-textconv"));
            }
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
            if script.contains("page_budget=") {
                page_stdout_bytes = stdout.len();
            }
            complete_patch_agent_request(
                runtime,
                client_id,
                &request.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
            scripts.push(script);
        } else {
            tokio::task::yield_now().await;
        }
    }
    assert!(
        task.is_finished(),
        "committed git_diff_hunks exceeded its 16-request protocol bound for client {client_id}"
    );
    (task.await.unwrap(), page_stdout_bytes, scripts)
}

#[tokio::test]
async fn git_diff_hunks_committed_exact_range_isolated_targeted_and_head_attributed() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), "src/a.rs", "pub fn a() -> u8 { 1 }\n");
    write_git_review_fixture_file(tmp.path(), "src/b.rs", "pub fn b() -> u8 { 1 }\n");
    write_git_review_fixture_file(
        tmp.path(),
        "src/space file.rs",
        "pub fn spaced() -> u8 { 1 }\n",
    );
    write_git_review_fixture_file(tmp.path(), "src/你好.rs", "pub fn unicode() -> u8 { 1 }\n");
    write_git_review_fixture_file(tmp.path(), "src/old.rs", "pub fn renamed() -> u8 { 1 }\n");
    write_git_review_fixture_file(tmp.path(), "src/delete.rs", "pub fn deleted() {}\n");
    fs::write(tmp.path().join("asset.bin"), [0u8, 1, 2, 3, 0, 4]).unwrap();
    let base = commit_git_review_fixture(tmp.path(), "base");

    write_git_review_fixture_file(tmp.path(), "src/a.rs", "pub fn a() -> u8 { 2 }\n");
    write_git_review_fixture_file(tmp.path(), "src/b.rs", "pub fn b() -> u8 { 2 }\n");
    write_git_review_fixture_file(
        tmp.path(),
        "src/space file.rs",
        "pub fn spaced() -> u8 { 2 }\n",
    );
    write_git_review_fixture_file(tmp.path(), "src/你好.rs", "pub fn unicode() -> u8 { 2 }\n");
    fs::rename(tmp.path().join("src/old.rs"), tmp.path().join("src/new.rs")).unwrap();
    fs::remove_file(tmp.path().join("src/delete.rs")).unwrap();
    write_git_review_fixture_file(tmp.path(), "src/add.rs", "pub fn added() {}\n");
    fs::write(tmp.path().join("asset.bin"), [0u8, 255, 2, 3, 0, 4]).unwrap();
    write_git_review_fixture_file(tmp.path(), ".gitattributes", "src/b.rs -diff\n");
    let head = commit_git_review_fixture(tmp.path(), "head");

    write_git_review_fixture_file(tmp.path(), "src/a.rs", "DIRTY_WORKTREE_MUST_NOT_APPEAR\n");
    write_git_review_fixture_file(
        tmp.path(),
        ".gitattributes",
        "src/a.rs -diff\nsrc/b.rs diff\n",
    );
    fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
    fs::write(
        tmp.path().join(".git/info/attributes"),
        "src/a.rs -diff\nsrc/b.rs diff\n",
    )
    .unwrap();
    git_test_command_ok(tmp.path(), "git config diff.external false");
    git_test_command_ok(tmp.path(), "git config diff.custom.textconv false");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "committed-targeted", "repo", tmp.path())
            .await;
    let paths = vec![
        "src/a.rs".to_string(),
        "src/b.rs".to_string(),
        "src/space file.rs".to_string(),
        "src/你好.rs".to_string(),
    ];
    let (result, raw_bytes, scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-targeted",
        &project,
        tmp.path(),
        Some(paths.clone()),
        20,
        120,
        base.clone(),
        head.clone(),
        None,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["scope"]["mode"], "committed");
    assert_eq!(result.output["scope"]["requested_base"], base);
    assert_eq!(result.output["scope"]["requested_head"], head);
    assert_eq!(result.output["scope"]["merge_base"], base);
    assert_eq!(result.output["scope"]["base_is_ancestor"], true);
    assert_eq!(result.output["paths"], json!(paths));
    assert!(
        raw_bytes < 48 * 1024,
        "page producer output must remain bounded"
    );
    assert_eq!(scripts.len(), 2, "scope + page observation expected");
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains("DIRTY_WORKTREE_MUST_NOT_APPEAR"));
    assert!(serialized.contains("space file.rs"));
    assert!(serialized.contains("你好.rs"));
    let files = result.output["files"].as_array().unwrap();
    let a = files
        .iter()
        .find(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("a.rs"))
        })
        .expect("src/a.rs committed hunk");
    assert!(!a["hunks"].as_array().unwrap().is_empty());
    let b = files
        .iter()
        .find(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("b.rs"))
        })
        .expect("src/b.rs committed file");
    assert_eq!(
        b["binary"], true,
        "reviewed-head attributes must be authoritative"
    );
    assert!(b["hunks"].as_array().unwrap().is_empty());

    let (all_result, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-targeted",
        &project,
        tmp.path(),
        None,
        40,
        120,
        base,
        head,
        None,
    )
    .await;
    assert!(all_result.success, "{:?}", all_result.error);
    let all_files = all_result.output["files"].as_array().unwrap();
    assert!(all_files.iter().any(|file| file["status"] == "renamed"));
    assert!(all_files.iter().any(|file| file["status"] == "deleted"));
    assert!(all_files.iter().any(|file| file["status"] == "added"));
    assert!(all_files.iter().any(|file| file["binary"] == true));
}

#[cfg(unix)]
#[tokio::test]
async fn git_diff_hunks_ignores_external_diff_helpers_in_worktree_and_cached_modes() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), "safe.txt", "safe-old\n");
    write_git_review_fixture_file(tmp.path(), ".env", "FAKE_PRIVATE_MARKER_117\n");
    commit_git_review_fixture(tmp.path(), "base");
    write_git_review_fixture_file(tmp.path(), "safe.txt", "safe-new\n");
    let helper = tmp.path().join("extdiff.sh");
    fs::write(
        &helper,
        "#!/bin/sh\nprintf '%s\\n' 'diff --git a/safe.txt b/safe.txt' '--- a/safe.txt' '+++ b/safe.txt' '@@ -1 +1 @@' '-safe-old'\nprintf '+%s\\n' \"$(cat .env)\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&helper, permissions).unwrap();
    git_test_command_ok(
        tmp.path(),
        &format!(
            "git config diff.external {}",
            shell_escape_simple(helper.to_string_lossy().as_ref())
        ),
    );

    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "diff-hunks-no-external",
        "repo",
        tmp.path(),
    )
    .await;
    let paths = Some(vec!["safe.txt".to_string()]);
    let (worktree, _, worktree_script) = run_runner_git_diff_hunks_page(
        &runtime,
        "diff-hunks-no-external",
        &project,
        tmp.path(),
        paths.clone(),
        10,
        80,
        false,
        None,
    )
    .await;
    assert!(worktree.success, "{:?}", worktree.error);
    assert!(worktree_script.contains("--no-ext-diff"));
    assert!(worktree_script.contains("--no-textconv"));
    let worktree_output = serde_json::to_string(&worktree.output).unwrap();
    assert!(worktree_output.contains("safe-new"));
    assert!(!worktree_output.contains("FAKE_PRIVATE_MARKER_117"));

    git_test_command_ok(tmp.path(), "git add -- safe.txt");
    let (cached, _, cached_script) = run_runner_git_diff_hunks_page(
        &runtime,
        "diff-hunks-no-external",
        &project,
        tmp.path(),
        paths,
        10,
        80,
        true,
        None,
    )
    .await;
    assert!(cached.success, "{:?}", cached.error);
    assert!(cached_script.contains("--no-ext-diff"));
    assert!(cached_script.contains("--no-textconv"));
    let cached_output = serde_json::to_string(&cached.output).unwrap();
    assert!(cached_output.contains("safe-new"));
    assert!(!cached_output.contains("FAKE_PRIVATE_MARKER_117"));
}

#[tokio::test]
async fn git_diff_hunks_never_returns_secret_path_content_in_any_mode() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), ".env", "API_TOKEN=base-secret\n");
    write_git_review_fixture_file(tmp.path(), "secret key.pem", "API_TOKEN=worktree-base\n");
    let base = commit_git_review_fixture(tmp.path(), "secret base");
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::rename(tmp.path().join(".env"), tmp.path().join("src/config.rs")).unwrap();
    write_git_review_fixture_file(tmp.path(), "src/config.rs", "API_TOKEN=committed-secret\n");
    let head = commit_git_review_fixture(tmp.path(), "secret head");
    write_git_review_fixture_file(tmp.path(), "secret key.pem", "API_TOKEN=dirty-secret\n");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "diff-secret-boundary", "repo", tmp.path())
            .await;

    let (committed, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "diff-secret-boundary",
        &project,
        tmp.path(),
        None,
        20,
        120,
        base,
        head,
        None,
    )
    .await;
    assert!(!committed.success);
    assert_eq!(committed.output["reason_code"], "sensitive_path");
    let committed_serialized = serde_json::to_string(&committed).unwrap();
    for secret in [
        "base-secret",
        "committed-secret",
        "worktree-base",
        "dirty-secret",
    ] {
        assert!(
            !committed_serialized.contains(secret),
            "committed diff leaked protected content: {committed_serialized}"
        );
    }

    let (worktree, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        "diff-secret-boundary",
        &project,
        tmp.path(),
        None,
        20,
        120,
        false,
        None,
    )
    .await;
    assert!(
        !worktree.success,
        "unexpected protected diff success: {worktree:?}"
    );
    assert_eq!(worktree.output["reason_code"], "sensitive_path");
    let worktree_serialized = serde_json::to_string(&worktree).unwrap();
    assert!(!worktree_serialized.contains("dirty-secret"));

    let explicit = runtime
        .git_diff_hunks_continued(
            project,
            Some(vec![".env".to_string()]),
            Some(20),
            Some(120),
            Some(false),
            None,
        )
        .await;
    assert!(!explicit.success);
    assert_eq!(explicit.output["reason_code"], "sensitive_path");
    assert!(
        probe_patch_agent_request(&runtime, "diff-secret-boundary")
            .await
            .is_none(),
        "explicit protected path must fail before Runner dispatch"
    );
}

#[tokio::test]
async fn git_diff_hunks_committed_range_validation_and_merge_base_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), "base.txt", "base\n");
    let base = commit_git_review_fixture(tmp.path(), "base");
    write_git_review_fixture_file(tmp.path(), "base.txt", "head\n");
    let head = commit_git_review_fixture(tmp.path(), "head");
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "committed-validation", "repo", tmp.path())
            .await;

    for (base_arg, head_arg, cached, reason) in [
        (
            Some("HEAD".to_string()),
            Some(head.clone()),
            None,
            "invalid_commit_id",
        ),
        (
            Some(base.clone()),
            None,
            None,
            "committed_range_requires_base_and_head",
        ),
        (
            Some(base.clone()),
            Some(head.clone()),
            Some(false),
            "committed_range_conflicts_with_cached",
        ),
    ] {
        let result = runtime
            .git_diff_hunks_continued_with_range(
                project.clone(),
                None,
                Some(10),
                Some(80),
                cached,
                base_arg,
                head_arg,
                None,
            )
            .await;
        assert!(!result.success);
        assert_eq!(result.output["reason_code"], reason);
        assert!(
            probe_patch_agent_request(&runtime, "committed-validation")
                .await
                .is_none(),
            "invalid committed input must fail before Runner dispatch"
        );
    }

    let (same, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-validation",
        &project,
        tmp.path(),
        None,
        10,
        80,
        base.clone(),
        base.clone(),
        None,
    )
    .await;
    assert!(same.success, "{:?}", same.error);
    assert_eq!(same.output["hunk_count"], 0);
    assert_eq!(same.output["files"], json!([]));

    let missing = "f".repeat(40);
    let (missing_result, _, missing_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-validation",
        &project,
        tmp.path(),
        None,
        10,
        80,
        base.clone(),
        missing.clone(),
        None,
    )
    .await;
    assert!(!missing_result.success);
    assert_eq!(
        missing_result.output["reason_code"],
        "head_commit_missing_or_not_commit"
    );
    assert_eq!(
        missing_scripts.len(),
        1,
        "missing object must stop before page diff"
    );

    let (blob_exit, blob_stdout, blob_stderr, _) =
        run_command_sync("printf blob | git hash-object -w --stdin", tmp.path(), 30);
    assert_eq!(blob_exit, 0, "{blob_stderr}");
    let blob = blob_stdout.trim().to_string();
    let (blob_result, _, blob_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-validation",
        &project,
        tmp.path(),
        None,
        10,
        80,
        base,
        blob,
        None,
    )
    .await;
    assert!(!blob_result.success);
    assert_eq!(
        blob_result.output["reason_code"],
        "head_commit_missing_or_not_commit"
    );
    assert_eq!(blob_scripts.len(), 1);
}

#[tokio::test]
async fn git_diff_hunks_committed_nonancestor_disconnected_and_ambiguous_merge_base() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), "root.txt", "root\n");
    let root = commit_git_review_fixture(tmp.path(), "root");
    let (_, root_branch, _, _) = run_command_sync("git branch --show-current", tmp.path(), 30);
    let root_branch = root_branch.trim().to_string();
    git_test_command_ok(tmp.path(), "git checkout -b feature");
    write_git_review_fixture_file(tmp.path(), "feature.txt", "feature\n");
    let feature = commit_git_review_fixture(tmp.path(), "feature");
    git_test_command_ok(
        tmp.path(),
        &format!("git checkout {}", shell_escape_simple(&root_branch)),
    );
    write_git_review_fixture_file(tmp.path(), "main.txt", "main\n");
    let requested_base = commit_git_review_fixture(tmp.path(), "main");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "committed-merge-base", "repo", tmp.path())
            .await;
    let (nonancestor, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-merge-base",
        &project,
        tmp.path(),
        None,
        10,
        80,
        requested_base.clone(),
        feature.clone(),
        None,
    )
    .await;
    assert!(nonancestor.success, "{:?}", nonancestor.error);
    assert_eq!(
        nonancestor.output["scope"]["requested_base"],
        requested_base
    );
    assert_eq!(nonancestor.output["scope"]["requested_head"], feature);
    assert_eq!(nonancestor.output["scope"]["merge_base"], root);
    assert_eq!(nonancestor.output["scope"]["base_is_ancestor"], false);

    let disconnected = tempfile::tempdir().unwrap();
    init_git_repo(disconnected.path());
    write_git_review_fixture_file(disconnected.path(), "one.txt", "one\n");
    let first = commit_git_review_fixture(disconnected.path(), "one");
    git_test_command_ok(disconnected.path(), "git checkout --orphan other");
    git_test_command_ok(disconnected.path(), "git rm -rf .");
    write_git_review_fixture_file(disconnected.path(), "two.txt", "two\n");
    let second = commit_git_review_fixture(disconnected.path(), "two");
    let runtime2 = test_runtime();
    let project2 = register_structured_git_agent_at_path(
        &runtime2,
        "committed-disconnected",
        "repo",
        disconnected.path(),
    )
    .await;
    let (no_base, _, scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime2,
        "committed-disconnected",
        &project2,
        disconnected.path(),
        None,
        10,
        80,
        first,
        second,
        None,
    )
    .await;
    assert!(!no_base.success);
    assert_eq!(no_base.output["reason_code"], "no_merge_base");
    assert_eq!(scripts.len(), 1);

    let ambiguous = tempfile::tempdir().unwrap();
    init_git_repo(ambiguous.path());
    write_git_review_fixture_file(ambiguous.path(), "root.txt", "root\n");
    commit_git_review_fixture(ambiguous.path(), "root");
    git_test_command_ok(ambiguous.path(), "git checkout -b side-a");
    write_git_review_fixture_file(ambiguous.path(), "a.txt", "a\n");
    let a1 = commit_git_review_fixture(ambiguous.path(), "a1");
    git_test_command_ok(ambiguous.path(), "git checkout -b side-b HEAD~1");
    write_git_review_fixture_file(ambiguous.path(), "b.txt", "b\n");
    let b1 = commit_git_review_fixture(ambiguous.path(), "b1");
    git_test_command_ok(ambiguous.path(), "git checkout side-a");
    git_test_command_ok(
        ambiguous.path(),
        &format!("git merge --no-ff -m merge-b {}", shell_escape_simple(&b1)),
    );
    let (_, a2, _, _) = run_command_sync("git rev-parse HEAD", ambiguous.path(), 30);
    let a2 = a2.trim().to_string();
    git_test_command_ok(ambiguous.path(), "git checkout side-b");
    git_test_command_ok(
        ambiguous.path(),
        &format!("git merge --no-ff -m merge-a {}", shell_escape_simple(&a1)),
    );
    let (_, b2, _, _) = run_command_sync("git rev-parse HEAD", ambiguous.path(), 30);
    let b2 = b2.trim().to_string();
    let runtime3 = test_runtime();
    let project3 = register_structured_git_agent_at_path(
        &runtime3,
        "committed-ambiguous",
        "repo",
        ambiguous.path(),
    )
    .await;
    let (ambiguous_result, _, ambiguous_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime3,
        "committed-ambiguous",
        &project3,
        ambiguous.path(),
        None,
        10,
        80,
        a2,
        b2,
        None,
    )
    .await;
    assert!(!ambiguous_result.success);
    assert_eq!(
        ambiguous_result.output["reason_code"],
        "ambiguous_merge_base"
    );
    assert_eq!(ambiguous_scripts.len(), 1);
}

#[tokio::test]
async fn git_diff_hunks_committed_continuation_binds_range_paths_mode_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let base_body = (0..1200)
        .map(|line| format!("line-{line:04}\n"))
        .collect::<String>();
    write_git_review_fixture_file(tmp.path(), "large.txt", &base_body);
    write_git_review_fixture_file(tmp.path(), "other.txt", "same\n");
    let base = commit_git_review_fixture(tmp.path(), "base");
    let head_body = (0..1200)
        .map(|line| {
            if matches!(line, 10 | 310 | 610 | 910) {
                format!("changed-{line:04}\n")
            } else {
                format!("line-{line:04}\n")
            }
        })
        .collect::<String>();
    write_git_review_fixture_file(tmp.path(), "large.txt", &head_body);
    let head = commit_git_review_fixture(tmp.path(), "head");

    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "committed-continuation",
        "repo",
        tmp.path(),
    )
    .await;
    let paths = Some(vec!["large.txt".to_string()]);
    let (first, first_bytes, first_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        head.clone(),
        None,
    )
    .await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.output["has_more"], true);
    assert!(first_bytes < 48 * 1024);
    assert_eq!(first_scripts.len(), 2);
    let token = first.output["next_continuation"]
        .as_str()
        .expect("first committed page continuation")
        .to_string();
    assert!(token.starts_with("wcdh1."));
    let recovery = &first.output["recovery"];
    assert_eq!(recovery["kind"], "page");
    assert_eq!(recovery["arguments"]["project"], project);
    assert_eq!(recovery["arguments"]["paths"], json!(["large.txt"]));
    assert_eq!(recovery["arguments"]["base_commit"], base);
    assert_eq!(recovery["arguments"]["head_commit"], head);
    assert_eq!(recovery["arguments"]["max_hunks"], 1);
    assert_eq!(recovery["arguments"]["max_hunk_lines"], 120);
    assert_eq!(recovery["arguments"]["continuation"], token);
    assert!(recovery["arguments"].get("cached").is_none());
    assert_git_diff_hunks_recovery_call_parses(recovery);
    assert_eq!(
        recovery["arguments"]["max_page_bytes"],
        DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    let (page_budget_mismatch, _, mismatch_scripts) =
        run_runner_git_diff_hunks_committed_page_with_budget(
            &runtime,
            "committed-continuation",
            &project,
            tmp.path(),
            paths.clone(),
            1,
            120,
            Some(MIN_GIT_DIFF_HUNKS_PAGE_BYTES),
            base.clone(),
            head.clone(),
            Some(token.clone()),
        )
        .await;
    assert!(!page_budget_mismatch.success);
    assert_eq!(
        page_budget_mismatch.output["reason_code"],
        "continuation_mismatch"
    );
    assert_eq!(
        mismatch_scripts.len(),
        1,
        "committed page-budget mismatch may resolve range scope but must stop before the page producer"
    );

    let first_diff = first.output["files"][0]["hunks"][0]["diff"]
        .as_str()
        .unwrap()
        .to_string();

    let (second, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        head.clone(),
        Some(token.clone()),
    )
    .await;
    assert!(second.success, "{:?}", second.error);
    let second_diff = second.output["files"][0]["hunks"][0]["diff"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        first_diff, second_diff,
        "continuation must not replay page one"
    );

    let (second_again, _, _) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        head.clone(),
        Some(token.clone()),
    )
    .await;
    assert!(second_again.success);
    assert_eq!(second_again.output["files"], second.output["files"]);
    assert_eq!(
        second_again.output["next_continuation"],
        second.output["next_continuation"]
    );

    let mut seen = vec![first_diff, second_diff];
    let mut next = second.output["next_continuation"]
        .as_str()
        .map(str::to_string);
    while let Some(current) = next {
        let (page, _, _) = run_runner_git_diff_hunks_committed_page(
            &runtime,
            "committed-continuation",
            &project,
            tmp.path(),
            paths.clone(),
            1,
            120,
            base.clone(),
            head.clone(),
            Some(current),
        )
        .await;
        assert!(page.success, "{:?}", page.error);
        if page.output["hunk_count"].as_u64().unwrap_or(0) > 0 {
            let diff = page.output["files"][0]["hunks"][0]["diff"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(!seen.contains(&diff), "hunk page replayed: {diff}");
            seen.push(diff);
        }
        next = page.output["next_continuation"]
            .as_str()
            .map(str::to_string);
    }
    assert_eq!(
        seen.len(),
        4,
        "all four committed hunks must be returned once"
    );

    let (path_mismatch, _, path_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        Some(vec!["other.txt".to_string()]),
        1,
        120,
        base.clone(),
        head.clone(),
        Some(token.clone()),
    )
    .await;
    assert!(!path_mismatch.success);
    assert_eq!(path_mismatch.output["reason_code"], "continuation_mismatch");
    assert_eq!(
        path_scripts.len(),
        1,
        "mismatch must stop before page producer"
    );

    let (range_mismatch, _, range_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        base.clone(),
        Some(token.clone()),
    )
    .await;
    assert!(!range_mismatch.success);
    assert_eq!(
        range_mismatch.output["reason_code"],
        "continuation_mismatch"
    );
    assert_eq!(range_scripts.len(), 1);

    let mut tampered = token.clone().into_bytes();
    let last = tampered.len() - 1;
    tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();
    let (tampered_result, _, tampered_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        head.clone(),
        Some(tampered),
    )
    .await;
    assert!(!tampered_result.success);
    assert_eq!(
        tampered_result.output["reason_code"],
        "invalid_continuation"
    );
    assert_eq!(tampered_scripts.len(), 1);

    {
        use base64::{engine::general_purpose, Engine as _};
        let encoded = token.strip_prefix("wcdh1.").unwrap();
        let decoded = general_purpose::URL_SAFE_NO_PAD.decode(encoded).unwrap();
        let mut state: Value = serde_json::from_slice(&decoded).unwrap();
        state["next"] = json!(999u64);
        let forged_next = format!(
            "wcdh1.{}",
            general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&state).unwrap())
        );
        let (forged_result, _, forged_scripts) = run_runner_git_diff_hunks_committed_page(
            &runtime,
            "committed-continuation",
            &project,
            tmp.path(),
            paths.clone(),
            1,
            120,
            base.clone(),
            head.clone(),
            Some(forged_next),
        )
        .await;
        assert!(!forged_result.success);
        assert_eq!(forged_result.output["reason_code"], "invalid_continuation");
        assert_eq!(
            forged_scripts.len(),
            1,
            "pagination-state tamper must fail after scope resolution and before page observation"
        );
    }

    let dirty_body = (0..1200)
        .map(|line| {
            if matches!(line, 20 | 500 | 1000) {
                format!("dirty-{line:04}\n")
            } else {
                if line == 10 || line == 310 || line == 610 || line == 910 {
                    format!("changed-{line:04}\n")
                } else {
                    format!("line-{line:04}\n")
                }
            }
        })
        .collect::<String>();
    write_git_review_fixture_file(tmp.path(), "large.txt", &dirty_body);
    let (worktree_page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        false,
        None,
    )
    .await;
    assert!(worktree_page.success);
    let worktree_token = worktree_page.output["next_continuation"]
        .as_str()
        .expect("worktree continuation")
        .to_string();
    let (wrong_mode, _, wrong_mode_scripts) = run_runner_git_diff_hunks_committed_page(
        &runtime,
        "committed-continuation",
        &project,
        tmp.path(),
        paths.clone(),
        1,
        120,
        base.clone(),
        head.clone(),
        Some(worktree_token),
    )
    .await;
    assert!(!wrong_mode.success);
    assert_eq!(wrong_mode.output["reason_code"], "continuation_mismatch");
    assert_eq!(wrong_mode_scripts.len(), 1);

    let reverse_mode = runtime
        .git_diff_hunks_continued(project, paths, Some(1), Some(120), Some(false), Some(token))
        .await;
    assert!(!reverse_mode.success);
    assert_eq!(reverse_mode.output["reason_code"], "continuation_mismatch");
}

#[tokio::test]
async fn git_diff_hunks_committed_drains_bounded_consumer_and_preserves_producer_failure() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), "file.txt", "old\n");
    let base = commit_git_review_fixture(tmp.path(), "base");
    write_git_review_fixture_file(tmp.path(), "file.txt", "new\n");
    let head = commit_git_review_fixture(tmp.path(), "head");
    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "committed-producer-failure",
        "repo",
        tmp.path(),
    )
    .await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let base = base.clone();
        let head = head.clone();
        async move {
            runtime
                .git_diff_hunks_continued_with_range(
                    project,
                    Some(vec!["file.txt".to_string()]),
                    Some(1),
                    Some(40),
                    None,
                    Some(base),
                    Some(head),
                    None,
                )
                .await
        }
    });

    let scope_request = wait_for_patch_agent_request(&runtime, "committed-producer-failure").await;
    complete_agent_request_by_running_locally(
        &runtime,
        "committed-producer-failure",
        scope_request,
    )
    .await;

    let mut page_request =
        wait_for_patch_agent_request(&runtime, "committed-producer-failure").await;
    let page_script = page_request
        .script
        .as_ref()
        .expect("typed page script")
        .script
        .clone();
    let head_q = shell_escape_simple(&head);
    let needle = format!(
        "git --no-pager -c core.quotePath=false -c attr.tree={head_q} diff --no-ext-diff --no-textconv --find-renames --unified=80 {} {head_q} -- 'file.txt'",
        shell_escape_simple(&base),
    );
    assert_eq!(
        page_script.matches(&needle).count(),
        1,
        "middle diff producer must be uniquely injectable"
    );
    let producer_body = concat!(
        "printf 'diff --git a/file.txt b/file.txt\\n--- a/file.txt\\n+++ b/file.txt\\n@@ -1 +1 @@\\n-old\\n'; ",
        "i=0; while [ \"$i\" -lt 120000 ]; do printf '+payload-%06d\\n' \"$i\"; i=$((i+1)); done; ",
        "exit 7"
    );
    let injected = format!("sh -c {}", shell_escape_simple(producer_body));
    page_request
        .script
        .as_mut()
        .expect("typed page script")
        .script = page_script.replacen(&needle, &injected, 1);
    let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&page_request);
    assert_eq!(
        exit_code, 0,
        "page envelope should carry producer failure structurally: {stderr}"
    );
    assert!(
        stdout.contains("diff_exit=7\n"),
        "bounded consumer must drain to the producer's real exit status, not SIGPIPE: {stdout}"
    );
    assert!(!stdout.contains("diff_exit=141\n"));
    assert!(
        stdout.len() < 48 * 1024,
        "consumer must retain only a bounded page despite multi-megabyte producer output"
    );
    complete_patch_agent_request(
        &runtime,
        "committed-producer-failure",
        &page_request.request_id,
        exit_code,
        &stdout,
        &stderr,
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["reason_code"], "git_diff_failed");
}

fn git_test_command_ok(repo: &Path, command: &str) {
    let (exit_code, stdout, stderr, _) = run_command_sync(command, repo, 30);
    assert_eq!(
        exit_code, 0,
        "{command} failed: stdout={stdout} stderr={stderr}"
    );
}

fn assert_git_diff_hunks_recovery_call_parses(recovery: &Value) {
    let tool = recovery["tool"]
        .as_str()
        .expect("recovery tool must be a string");
    ToolCall::from_tool_name(tool, recovery["arguments"].clone())
        .expect("structured git diff recovery call must parse directly");
}

#[tokio::test]
async fn git_diff_hunks_stable_multi_page_traversal_has_no_duplicate_or_missing_records() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..500)
        .map(|line| format!("line-{line:03}\n"))
        .collect::<String>();
    for file in 0..3 {
        commit_file(
            repo.path(),
            &format!("file-{file}.txt"),
            &original,
            &format!("add file {file}"),
        );
    }
    for file in 0..3 {
        let changed = (0..500)
            .map(|line| {
                if matches!(line, 10 | 210 | 410) {
                    format!("changed-{file}-{line:03}\n")
                } else {
                    format!("line-{line:03}\n")
                }
            })
            .collect::<String>();
        fs::write(repo.path().join(format!("file-{file}.txt")), changed).unwrap();
    }

    let runtime = test_runtime();
    let client_id = "diff-hunks-pages";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let mut continuation = None;
    let mut logical_files = Vec::new();
    let mut logical_hunks = Vec::new();
    let mut seen_tokens = HashSet::new();
    let mut finished = false;

    for _ in 0..10 {
        let prior_token = continuation.clone();
        let (result, _stdout_bytes, command) = run_runner_git_diff_hunks_page(
            &runtime,
            client_id,
            &project,
            repo.path(),
            None,
            2,
            400,
            false,
            continuation,
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        if let Some(prior_token) = prior_token.as_deref() {
            assert!(
                !command.contains(prior_token),
                "opaque continuation must not be interpolated into the shell command"
            );
        }
        for file in result.output["files"].as_array().unwrap() {
            let path = file["path"].as_str().unwrap().to_string();
            if file.get("continued").and_then(Value::as_bool) != Some(true) {
                logical_files.push(path.clone());
            }
            for hunk in file["hunks"].as_array().unwrap() {
                logical_hunks.push(format!("{}|{}", path, hunk["header"].as_str().unwrap()));
            }
        }
        if result.output["has_more"] == false {
            assert_eq!(result.output["next_continuation"], Value::Null);
            finished = true;
            break;
        }
        let next = result.output["next_continuation"]
            .as_str()
            .expect("non-final page continuation")
            .to_string();
        let recovery = &result.output["recovery"];
        assert_eq!(recovery["kind"], "page");
        assert_eq!(recovery["safe_continuation_for_omitted_lines"], Value::Null);
        assert_eq!(recovery["continuation"]["available"], true);
        assert_eq!(recovery["continuation"]["recovers_later_hunks"], true);
        assert_eq!(recovery["continuation"]["recovers_omitted_lines"], false);
        assert_eq!(recovery["arguments"]["project"], project);
        assert_eq!(recovery["arguments"]["paths"], json!([]));
        assert_eq!(recovery["arguments"]["cached"], false);
        assert_eq!(recovery["arguments"]["max_hunks"], 2);
        assert_eq!(recovery["arguments"]["max_hunk_lines"], 400);
        assert_eq!(recovery["arguments"]["continuation"], next);
        assert_eq!(
            recovery["continuation"]["next_call"]["arguments"],
            recovery["arguments"]
        );
        assert_git_diff_hunks_recovery_call_parses(recovery);
        assert!(
            seen_tokens.insert(next.clone()),
            "continuation did not advance"
        );
        continuation = Some(next);
    }

    assert!(finished, "multi-page traversal did not terminate");
    assert_eq!(
        logical_files,
        vec!["file-0.txt", "file-1.txt", "file-2.txt"]
    );
    assert_eq!(
        logical_hunks.len(),
        9,
        "unexpected hunk traversal: {logical_hunks:?}"
    );
    assert_eq!(
        logical_hunks.iter().collect::<HashSet<_>>().len(),
        logical_hunks.len(),
        "duplicate logical hunk returned across pages"
    );
}

#[tokio::test]
async fn git_diff_hunks_large_raw_diff_is_bounded_before_runner_result_retention() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..2500)
        .map(|line| format!("original-{line:04}-{}\n", "x".repeat(72)))
        .collect::<String>();
    for file in 0..2 {
        fs::write(repo.path().join(format!("large-{file}.txt")), &original).unwrap();
    }
    git_test_command_ok(repo.path(), "git add -- . && git commit -m large-baseline");
    for file in 0..2 {
        let changed = (0..2500)
            .map(|line| format!("changed-{file}-{line:04}-{}\n", "y".repeat(72)))
            .collect::<String>();
        fs::write(repo.path().join(format!("large-{file}.txt")), changed).unwrap();
    }
    let (raw_exit, raw_diff, raw_stderr) =
        run_command_full_capture("git diff --unified=80", repo.path(), 30);
    assert_eq!(raw_exit, 0, "raw git diff failed: {raw_stderr}");
    assert!(
        raw_diff.len() > ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES,
        "fixture did not exceed ordinary Runner result-retention compatibility floor: {} bytes",
        raw_diff.len()
    );

    let runtime = test_runtime();
    let client_id = "diff-hunks-large";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (result, runner_stdout_bytes, _command) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        1,
        12,
        false,
        None,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["max_page_bytes"],
        DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert!(
        runner_stdout_bytes <= DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES + 4096,
        "producer sent {runner_stdout_bytes} bytes beyond default page-retention headroom"
    );
    assert_eq!(result.output["files"][0]["path"], "large-0.txt");
    assert_eq!(result.output["has_more"], true);
    assert!(result.output["next_continuation"].as_str().is_some());
    let reasons = result.output["truncation_reasons"].as_array().unwrap();
    assert!(reasons.iter().any(|reason| reason == "page_hunk_limit"));
    assert!(reasons.iter().any(|reason| reason == "hunk_line_limit"));
    assert!(!reasons.iter().any(|reason| reason == "page_byte_budget"));
    let recovery = &result.output["recovery"];
    assert_eq!(recovery["kind"], "mixed");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(recovery["omitted_lines"]["present"], true);
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "max_hunk_lines_ceiling_insufficient"
    );
    assert_eq!(recovery["omitted_lines"]["path_provenance"], "exact");
    assert_eq!(recovery["omitted_lines"]["paths"], json!(["large-0.txt"]));
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["continuation"]["available"], true);
    assert_eq!(
        recovery["arguments"],
        recovery["continuation"]["next_call"]["arguments"]
    );
    assert_eq!(recovery["arguments"]["paths"], json!([]));
    assert_eq!(recovery["arguments"]["max_hunk_lines"], 12);
    assert_eq!(
        recovery["arguments"]["max_page_bytes"],
        DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert_eq!(
        recovery["continuation"]["next_call"]["arguments"]["max_hunk_lines"],
        12
    );
    assert_eq!(
        recovery["continuation"]["next_call"]["arguments"]["continuation"],
        result.output["next_continuation"]
    );
    assert_git_diff_hunks_recovery_call_parses(recovery);
    assert!(
        serde_json::to_vec(&result).unwrap().len() <= MODEL_INSPECTION_MAX_RESULT_BYTES,
        "serialized result exceeded explicit model-inspection ceiling"
    );
}

#[tokio::test]
async fn git_diff_hunks_page_budget_is_configurable_bounded_and_scope_bound() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    for file in 0..40 {
        let body = (0..30)
            .map(|line| format!("base-{file:02}-{line:02}-{}\n", "x".repeat(72)))
            .collect::<String>();
        fs::write(repo.path().join(format!("page-{file:02}.txt")), body).unwrap();
    }
    git_test_command_ok(repo.path(), "git add -- . && git commit -m page-baseline");
    for file in 0..40 {
        let body = (0..30)
            .map(|line| format!("changed-{file:02}-{line:02}-{}\n", "y".repeat(72)))
            .collect::<String>();
        fs::write(repo.path().join(format!("page-{file:02}.txt")), body).unwrap();
    }

    let runtime = test_runtime();
    let client_id = "diff-hunks-page-budget";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (small, small_stdout_bytes, _) = run_runner_git_diff_hunks_page_with_budget(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        100,
        100,
        Some(MIN_GIT_DIFF_HUNKS_PAGE_BYTES),
        false,
        None,
    )
    .await;
    assert!(small.success, "{:?}", small.error);
    assert_eq!(
        small.output["max_page_bytes"],
        MIN_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert!(small_stdout_bytes <= MIN_GIT_DIFF_HUNKS_PAGE_BYTES + 4096);
    assert_eq!(small.output["has_more"], true);
    let token = small.output["next_continuation"]
        .as_str()
        .expect("small producer page must continue")
        .to_string();
    assert_eq!(
        small.output["recovery"]["arguments"]["max_page_bytes"],
        MIN_GIT_DIFF_HUNKS_PAGE_BYTES
    );

    let mismatch = runtime
        .git_diff_hunks_continued_with_range_and_page_bytes(
            project.clone(),
            None,
            Some(100),
            Some(100),
            Some(DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES),
            Some(false),
            None,
            None,
            Some(token),
        )
        .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["reason_code"], "continuation_mismatch");
    assert!(
        probe_patch_agent_request(&runtime, client_id)
            .await
            .is_none(),
        "page-budget scope mismatch must stop before producer dispatch"
    );

    let (large, large_stdout_bytes, _) = run_runner_git_diff_hunks_page_with_budget(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        100,
        100,
        Some(MAX_GIT_DIFF_HUNKS_PAGE_BYTES + 64 * 1024),
        false,
        None,
    )
    .await;
    assert!(large.success, "{:?}", large.error);
    assert_eq!(
        large.output["max_page_bytes"],
        MAX_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    assert!(large_stdout_bytes <= MAX_GIT_DIFF_HUNKS_PAGE_BYTES + 4096);
    assert!(
        large.output["hunk_count"].as_u64().unwrap() > small.output["hunk_count"].as_u64().unwrap(),
        "larger producer page should return more complete hunk records"
    );
    assert!(
        serde_json::to_vec(&large).unwrap().len() <= MODEL_INSPECTION_MAX_RESULT_BYTES,
        "producer page and final model-facing ceiling must remain independently bounded"
    );
}

#[tokio::test]
async fn git_diff_hunks_worktree_continuation_fails_stale_after_relevant_change() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "a.txt", "a0\n", "add a");
    commit_file(repo.path(), "b.txt", "b0\n", "add b");
    fs::write(repo.path().join("a.txt"), "a1\n").unwrap();
    fs::write(repo.path().join("b.txt"), "b1\n").unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-worktree-stale";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        1,
        40,
        false,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    let token = page.output["next_continuation"]
        .as_str()
        .expect("first page continuation")
        .to_string();
    fs::write(repo.path().join("b.txt"), "b2\nextra\n").unwrap();

    let (stale, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        1,
        40,
        false,
        Some(token),
    )
    .await;
    assert!(!stale.success);
    assert_eq!(stale.output["reason_code"], "stale_continuation");
    assert_eq!(stale.output["files"], json!([]));
    assert_eq!(stale.output["next_continuation"], Value::Null);
    assert_eq!(stale.output["state_changed"], false);
}

#[tokio::test]
async fn git_diff_hunks_cached_continuation_fails_stale_after_index_change() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "a.txt", "a0\n", "add a");
    commit_file(repo.path(), "b.txt", "b0\n", "add b");
    fs::write(repo.path().join("a.txt"), "a1\n").unwrap();
    fs::write(repo.path().join("b.txt"), "b1\n").unwrap();
    git_test_command_ok(repo.path(), "git add -- a.txt b.txt");

    let runtime = test_runtime();
    let client_id = "diff-hunks-cached-stale";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        1,
        40,
        true,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    let token = page.output["next_continuation"]
        .as_str()
        .expect("cached continuation")
        .to_string();
    fs::write(repo.path().join("b.txt"), "b2\n").unwrap();
    git_test_command_ok(repo.path(), "git add -- b.txt");

    let (stale, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        1,
        40,
        true,
        Some(token),
    )
    .await;
    assert!(!stale.success);
    assert_eq!(stale.output["reason_code"], "stale_continuation");
    assert_eq!(stale.output["files"], json!([]));
    assert_eq!(stale.output["next_continuation"], Value::Null);
}

#[tokio::test]
async fn git_diff_hunks_scoped_fence_ignores_outside_change_and_rejects_scope_mismatch() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "a.txt", "a0\n", "add a");
    commit_file(repo.path(), "b.txt", "b0\n", "add b");
    commit_file(repo.path(), "outside.txt", "outside0\n", "add outside");
    fs::write(repo.path().join("a.txt"), "a1\n").unwrap();
    fs::write(repo.path().join("b.txt"), "b1\n").unwrap();
    let scope = Some(vec!["a.txt".to_string(), "b.txt".to_string()]);

    let runtime = test_runtime();
    let client_id = "diff-hunks-scoped";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        scope.clone(),
        1,
        40,
        false,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    let token = page.output["next_continuation"]
        .as_str()
        .expect("scoped continuation")
        .to_string();
    fs::write(repo.path().join("outside.txt"), "outside1\n").unwrap();

    let (continued, _, command) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        Some(vec!["b.txt".to_string(), "a.txt".to_string()]),
        1,
        40,
        false,
        Some(token.clone()),
    )
    .await;
    assert!(continued.success, "{:?}", continued.error);
    assert!(
        !command.contains(&token),
        "opaque continuation content reached the shell command"
    );

    let other_client_id = "diff-hunks-scoped-other";
    let other_project =
        register_runner_project_at_path(&runtime, other_client_id, "repo", repo.path()).await;
    let project_mismatch = runtime
        .git_diff_hunks_continued(
            other_project,
            scope.clone(),
            Some(1),
            Some(40),
            Some(false),
            Some(token.clone()),
        )
        .await;
    assert!(!project_mismatch.success);
    assert_eq!(
        project_mismatch.output["reason_code"],
        "continuation_mismatch"
    );
    assert!(probe_patch_agent_request(&runtime, other_client_id)
        .await
        .is_none());

    let mismatch = runtime
        .git_diff_hunks_continued(
            project.clone(),
            Some(vec!["a.txt".to_string()]),
            Some(1),
            Some(40),
            Some(false),
            Some(token.clone()),
        )
        .await;
    assert!(!mismatch.success);
    assert_eq!(mismatch.output["reason_code"], "continuation_mismatch");
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());

    let cached_mismatch = runtime
        .git_diff_hunks_continued(project, scope, Some(1), Some(40), Some(true), Some(token))
        .await;
    assert!(!cached_mismatch.success);
    assert_eq!(
        cached_mismatch.output["reason_code"],
        "continuation_mismatch"
    );
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
}

#[tokio::test]
async fn git_diff_hunks_binary_records_advance_across_byte_bounded_pages() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let file_count = 120usize;
    let names = (0..file_count)
        .map(|index| format!("binary-{index:03}-{}.bin", "x".repeat(96)))
        .collect::<Vec<_>>();
    for (index, name) in names.iter().enumerate() {
        let mut bytes = vec![0u8; 512];
        bytes[1] = index as u8;
        fs::write(repo.path().join(name), bytes).unwrap();
    }
    git_test_command_ok(repo.path(), "git add -- . && git commit -m binary-baseline");
    for (index, name) in names.iter().enumerate() {
        let mut bytes = vec![0u8; 512];
        bytes[1] = index as u8;
        bytes[2] = 1;
        fs::write(repo.path().join(name), bytes).unwrap();
    }

    let runtime = test_runtime();
    let client_id = "diff-hunks-binary";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let mut continuation = None;
    let mut returned = Vec::new();
    let mut finished = false;
    for _ in 0..10 {
        let (page, runner_stdout_bytes, _) = run_runner_git_diff_hunks_page(
            &runtime,
            client_id,
            &project,
            repo.path(),
            None,
            1,
            20,
            false,
            continuation,
        )
        .await;
        assert!(page.success, "{:?}", page.error);
        assert_eq!(page.output["hunk_count"], 0);
        assert!(runner_stdout_bytes <= DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES + 4096);
        for file in page.output["files"].as_array().unwrap() {
            assert_ne!(file.get("continued").and_then(Value::as_bool), Some(true));
            returned.push(file["path"].as_str().unwrap().to_string());
        }
        if page.output["has_more"] == false {
            assert_eq!(page.output["next_continuation"], Value::Null);
            finished = true;
            break;
        }
        assert!(page.output["truncation_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "page_byte_budget"));
        let recovery = &page.output["recovery"];
        assert_eq!(recovery["kind"], "page");
        assert_eq!(recovery["safe_continuation_for_omitted_lines"], Value::Null);
        assert_eq!(recovery["arguments"]["cached"], false);
        assert_eq!(recovery["arguments"]["max_hunks"], 1);
        assert_eq!(recovery["arguments"]["max_hunk_lines"], 20);
        assert_eq!(
            recovery["arguments"]["continuation"],
            page.output["next_continuation"]
        );
        assert_git_diff_hunks_recovery_call_parses(recovery);
        continuation = Some(
            page.output["next_continuation"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(finished, "binary pagination did not terminate");
    assert_eq!(returned.len(), file_count);
    assert_eq!(returned.iter().collect::<HashSet<_>>().len(), file_count);
    assert_eq!(returned, names);
}

#[tokio::test]
async fn git_diff_hunks_hunk_line_limit_does_not_create_fake_continuation() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..80)
        .map(|line| format!("old-{line:03}\n"))
        .collect::<String>();
    commit_file(repo.path(), "long.txt", &original, "add long");
    let changed = (0..80)
        .map(|line| format!("new-{line:03}\n"))
        .collect::<String>();
    fs::write(repo.path().join("long.txt"), changed).unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-line-limit";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        5,
        false,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["hunk_count"], 1);
    assert_eq!(page.output["has_more"], false);
    assert_eq!(page.output["next_continuation"], Value::Null);
    assert_eq!(page.output["files"][0]["hunks"][0]["truncated"], true);
    assert!(page.output["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "hunk_line_limit"));
    assert!(!page.output["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "page_hunk_limit"));
    let recovery = &page.output["recovery"];
    assert_eq!(recovery["kind"], "hunk_lines");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(recovery["continuation"]["available"], false);
    assert_eq!(recovery["continuation"]["next_call"], Value::Null);
    assert_eq!(recovery["omitted_lines"]["present"], true);
    assert_eq!(recovery["omitted_lines"]["recoverable"], true);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "larger_max_hunk_lines_available"
    );
    assert_eq!(recovery["omitted_lines"]["path_provenance"], "exact");
    assert_eq!(recovery["omitted_lines"]["paths"], json!(["long.txt"]));
    assert_eq!(recovery["arguments"]["paths"], json!(["long.txt"]));
    assert_eq!(recovery["arguments"]["max_hunk_lines"], 400);
    assert!(recovery["arguments"].get("continuation").is_none());
    assert_git_diff_hunks_recovery_call_parses(recovery);
    let recovered = run_parser_ready_worktree_git_diff_hunks_call(
        &runtime,
        client_id,
        repo.path(),
        &recovery["omitted_lines"]["next_call"],
    )
    .await;
    assert!(recovered.success, "{:?}", recovered.error);
    assert_eq!(recovered.output["truncated"], false);
    assert_eq!(recovered.output["has_more"], false);
    assert_eq!(recovered.output["next_continuation"], Value::Null);
    assert!(recovered.output.get("recovery").is_none());
    assert_eq!(recovered.output["files"][0]["hunks"][0]["truncated"], false);
    assert!(recovered.output["files"][0]["hunks"][0]["diff"]
        .as_str()
        .unwrap()
        .contains("+new-079"));
    let mut projected = ToolResult::ok(page.output.clone());
    sparsify_complete_git_review_success("git_diff_hunks", &mut projected);
    assert_eq!(
        projected.output, page.output,
        "truncated recovery evidence must never be sparsified"
    );
}

#[tokio::test]
async fn git_diff_hunks_complete_model_projection_keeps_scope_and_drops_derived_metadata() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "a.txt", "before\n", "baseline");
    fs::write(repo.path().join("a.txt"), "after\n").unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-complete-projection";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        Some(vec!["a.txt".to_string()]),
        10,
        400,
        false,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["truncated"], false);
    assert_eq!(page.output["has_more"], false);
    assert_eq!(page.output["next_continuation"], Value::Null);
    assert!(page.output.get("recovery").is_none());
    assert!(page.output["files"][0]["hunks"][0]
        .get("line_count")
        .is_some());

    let canonical = page.output.clone();
    let mut projected = ToolResult::ok(canonical.clone());
    sparsify_complete_git_review_success("git_diff_hunks", &mut projected);
    let output = &projected.output;
    assert_eq!(output["project"], project);
    assert_eq!(output["paths"], json!(["a.txt"]));
    assert_eq!(output["cached"], false);
    assert!(output.get("files").is_some());
    for omitted in [
        "hunk_count",
        "truncated",
        "truncation_reasons",
        "has_more",
        "next_continuation",
        "exit_code",
        "stderr",
    ] {
        assert!(
            output.get(omitted).is_none(),
            "derived {omitted} leaked: {output}"
        );
    }
    assert!(output["files"][0].get("old_path").is_none());
    assert!(output["files"][0]["hunks"][0].get("line_count").is_none());
    assert!(output["files"][0]["hunks"][0].get("truncated").is_none());
    assert!(canonical.get("hunk_count").is_some());
    assert!(canonical.get("truncated").is_some());
}

#[tokio::test]
async fn git_diff_hunks_page_recovery_replays_cached_path_filtered_scope() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..500)
        .map(|line| format!("line-{line:03}\n"))
        .collect::<String>();
    for name in ["a.txt", "b.txt"] {
        commit_file(repo.path(), name, &original, "baseline");
        let changed = (0..500)
            .map(|line| {
                if matches!(line, 10 | 210 | 410) {
                    format!("changed-{name}-{line:03}\n")
                } else {
                    format!("line-{line:03}\n")
                }
            })
            .collect::<String>();
        fs::write(repo.path().join(name), changed).unwrap();
    }
    git_test_command_ok(repo.path(), "git add -- a.txt b.txt");

    let runtime = test_runtime();
    let client_id = "diff-hunks-cached-scoped-recovery";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        Some(vec!["a.txt".to_string()]),
        1,
        400,
        true,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["files"][0]["path"], "a.txt");
    assert_eq!(page.output["has_more"], true);
    let recovery = &page.output["recovery"];
    assert_eq!(recovery["kind"], "page");
    assert_eq!(recovery["arguments"]["project"], project);
    assert_eq!(recovery["arguments"]["cached"], true);
    assert_eq!(recovery["arguments"]["paths"], json!(["a.txt"]));
    assert_eq!(recovery["arguments"]["max_hunks"], 1);
    assert_eq!(recovery["arguments"]["max_hunk_lines"], 400);
    assert_eq!(
        recovery["arguments"]["continuation"],
        page.output["next_continuation"]
    );
    assert!(recovery["arguments"].get("base_commit").is_none());
    assert!(recovery["arguments"].get("head_commit").is_none());
    assert_git_diff_hunks_recovery_call_parses(recovery);
}

#[tokio::test]
async fn git_diff_hunks_byte_truncated_final_hunk_is_omitted_evidence_without_fake_continuation() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let old = format!("old-{}\n", "x".repeat(96 * 1024));
    commit_file(repo.path(), "huge.txt", &old, "baseline");
    let new = format!("new-{}\n", "y".repeat(96 * 1024));
    fs::write(repo.path().join("huge.txt"), new).unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-final-record-byte-truncation";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        400,
        false,
        None,
    )
    .await;

    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["has_more"], false);
    assert_eq!(page.output["next_continuation"], Value::Null);
    assert!(page.output["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "page_byte_budget"));
    assert!(!page.output["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "hunk_line_limit"));
    assert_eq!(page.output["files"][0]["hunks"][0]["truncated"], true);
    let recovery = &page.output["recovery"];
    assert_eq!(recovery["kind"], "mixed");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(recovery["continuation"]["available"], false);
    assert_eq!(recovery["continuation"]["next_call"], Value::Null);
    assert_eq!(recovery["omitted_lines"]["present"], true);
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "page_byte_budget_prevents_proven_recovery"
    );
    assert_eq!(recovery["omitted_lines"]["path_provenance"], "exact");
    assert_eq!(recovery["omitted_lines"]["paths"], json!(["huge.txt"]));
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["arguments"], Value::Null);
}

#[tokio::test]
async fn git_diff_hunks_line_recovery_requires_proven_future_byte_fit() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..60)
        .map(|line| format!("old-{line:03}-{}\n", "x".repeat(400)))
        .collect::<String>();
    commit_file(repo.path(), "wide-lines.txt", &original, "baseline");
    let changed = (0..60)
        .map(|line| format!("new-{line:03}-{}\n", "y".repeat(400)))
        .collect::<String>();
    fs::write(repo.path().join("wide-lines.txt"), changed).unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-future-byte-ceiling";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    // This test is specifically about a future recovery being blocked by a
    // constrained producer byte page; do not depend on the product default.
    let constrained_page_bytes = 32 * 1024;
    let (page, _, _) = run_runner_git_diff_hunks_page_with_budget(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        5,
        Some(constrained_page_bytes),
        false,
        None,
    )
    .await;

    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["max_page_bytes"], constrained_page_bytes);
    let reasons = page.output["truncation_reasons"].as_array().unwrap();
    assert!(reasons.iter().any(|reason| reason == "hunk_line_limit"));
    assert!(!reasons.iter().any(|reason| reason == "page_byte_budget"));
    let recovery = &page.output["recovery"];
    assert_eq!(recovery["kind"], "hunk_lines");
    assert_eq!(recovery["omitted_lines"]["present"], true);
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "page_byte_budget_prevents_proven_recovery"
    );
    assert_eq!(recovery["omitted_lines"]["path_provenance"], "exact");
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["arguments"], Value::Null);
}

#[tokio::test]
async fn git_diff_hunks_line_ceiling_does_not_advertise_impossible_recovery() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let original = (0..250)
        .map(|line| format!("old-{line:03}\n"))
        .collect::<String>();
    commit_file(repo.path(), "too-long.txt", &original, "baseline");
    let changed = (0..250)
        .map(|line| format!("new-{line:03}\n"))
        .collect::<String>();
    fs::write(repo.path().join("too-long.txt"), changed).unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-line-ceiling";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (bounded, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        5,
        false,
        None,
    )
    .await;
    assert!(bounded.success, "{:?}", bounded.error);
    assert_eq!(bounded.output["has_more"], false);
    assert!(bounded.output["truncation_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "hunk_line_limit"));
    let recovery = &bounded.output["recovery"];
    assert_eq!(recovery["kind"], "hunk_lines");
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "max_hunk_lines_ceiling_insufficient"
    );
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["arguments"], Value::Null);

    let (at_ceiling, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        400,
        false,
        None,
    )
    .await;
    assert!(at_ceiling.success, "{:?}", at_ceiling.error);
    let recovery = &at_ceiling.output["recovery"];
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "max_hunk_lines_ceiling_reached"
    );
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["arguments"], Value::Null);
}

#[tokio::test]
async fn git_diff_hunks_mixed_byte_omission_keeps_later_hunk_continuation_actionable() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    let huge_old = format!("old-{}\n", "x".repeat(96 * 1024));
    commit_file(repo.path(), "huge.txt", &huge_old, "huge baseline");
    commit_file(repo.path(), "later.txt", "before\n", "later baseline");
    let huge_new = format!("new-{}\n", "y".repeat(96 * 1024));
    fs::write(repo.path().join("huge.txt"), huge_new).unwrap();
    fs::write(repo.path().join("later.txt"), "after\n").unwrap();

    let runtime = test_runtime();
    let client_id = "diff-hunks-mixed-byte-continuation";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let (page, _, _) = run_runner_git_diff_hunks_page(
        &runtime,
        client_id,
        &project,
        repo.path(),
        None,
        10,
        400,
        false,
        None,
    )
    .await;
    assert!(page.success, "{:?}", page.error);
    assert_eq!(page.output["has_more"], true);
    assert!(page.output["next_continuation"].as_str().is_some());
    let recovery = &page.output["recovery"];
    assert_eq!(recovery["kind"], "mixed");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(recovery["omitted_lines"]["recoverable"], false);
    assert_eq!(
        recovery["omitted_lines"]["reason_code"],
        "page_byte_budget_prevents_proven_recovery"
    );
    assert_eq!(recovery["omitted_lines"]["next_call"], Value::Null);
    assert_eq!(recovery["continuation"]["available"], true);
    assert_eq!(recovery["continuation"]["recovers_later_hunks"], true);
    assert_eq!(recovery["continuation"]["recovers_omitted_lines"], false);
    assert_eq!(
        recovery["arguments"],
        recovery["continuation"]["next_call"]["arguments"]
    );
    assert_git_diff_hunks_recovery_call_parses(recovery);

    let continued = run_parser_ready_worktree_git_diff_hunks_call(
        &runtime,
        client_id,
        repo.path(),
        &recovery["continuation"]["next_call"],
    )
    .await;
    assert!(continued.success, "{:?}", continued.error);
    assert!(continued.output["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "later.txt"));
}

#[tokio::test]
async fn git_diff_hunks_malformed_continuation_fails_before_runner_dispatch() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "a.txt", "a0\n", "add a");
    fs::write(repo.path().join("a.txt"), "a1\n").unwrap();
    let runtime = test_runtime();
    let client_id = "diff-hunks-invalid-token";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", repo.path()).await;
    let result = runtime
        .git_diff_hunks_continued(
            project,
            None,
            Some(1),
            Some(40),
            Some(false),
            Some("not-a-valid-continuation".to_string()),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.output["reason_code"], "invalid_continuation");
    assert_eq!(result.output["files"], json!([]));
    assert_eq!(result.output["next_continuation"], Value::Null);
    assert!(probe_patch_agent_request(&runtime, client_id)
        .await
        .is_none());
}

#[test]
fn git_diff_hunks_parser_handles_modified_empty_and_limits() {
    let binary = "\
diff --git a/binary file.bin b/binary file.bin
index 1111111..2222222 100644
Binary files a/binary file.bin and b/binary file.bin differ
";
    let (binary_files, binary_hunks, binary_truncated) = parse_git_diff_hunks(binary, 10, 20);
    assert!(!binary_truncated);
    assert_eq!(binary_hunks, 0);
    assert_eq!(binary_files.len(), 1);
    assert_eq!(binary_files[0]["path"], "binary file.bin");
    assert_eq!(binary_files[0]["old_path"], "binary file.bin");
    assert_eq!(binary_files[0]["binary"], true);

    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@ fn demo()
 line one
-old
+new
+added
";
    let (files, hunk_count, truncated) = parse_git_diff_hunks(diff, 10, 20);
    assert!(!truncated);
    assert_eq!(hunk_count, 1);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "src/lib.rs");
    assert_eq!(files[0]["status"], "modified");
    assert_eq!(files[0]["hunks"][0]["old_start"], 1);
    assert!(files[0]["hunks"][0]["diff"]
        .as_str()
        .unwrap()
        .contains("+new"));

    let (files, hunk_count, truncated) = parse_git_diff_hunks("", 10, 20);
    assert!(files.is_empty());
    assert_eq!(hunk_count, 0);
    assert!(!truncated);

    let (_files, hunk_count, truncated) = parse_git_diff_hunks(diff, 0, 20);
    assert_eq!(hunk_count, 0);
    assert!(truncated);

    let (files, _hunk_count, truncated) = parse_git_diff_hunks(diff, 10, 2);
    assert!(truncated);
    assert_eq!(files[0]["hunks"][0]["truncated"], true);
}

#[test]
fn show_changes_command_is_read_only() {
    let without_diff = show_changes_command(false, 20, 80);
    let with_diff = show_changes_command(true, 20, 80);
    assert!(
        without_diff.len() <= crate::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES,
        "include_diff=false command is {} bytes",
        without_diff.len()
    );
    assert!(
        with_diff.len() <= crate::runner_protocol::RAW_SHELL_COMMAND_MAX_BYTES,
        "include_diff=true command is {} bytes",
        with_diff.len()
    );
    eprintln!(
        "show_changes_command_lengths without_diff={} with_diff={}",
        without_diff.len(),
        with_diff.len()
    );
    for cmd in [&without_diff, &with_diff] {
        assert!(cmd.contains("git status --porcelain=v1 -b"));
        assert!(cmd.contains("git log -1"));
        assert!(cmd.contains("git diff --stat"));
        assert!(cmd.contains("LC_ALL=C; export LC_ALL"));
        assert!(!cmd.contains("head_buf=$("), "HEAD must be streamed: {cmd}");
        assert!(
            !cmd.contains("stat_buf=$("),
            "diff-stat must be streamed: {cmd}"
        );
        assert!(
            !cmd.contains("while IFS= read -r hline"),
            "HEAD subject must not be read into an unbounded shell variable: {cmd}"
        );
        assert!(
            !cmd.contains("head_buf="),
            "HEAD must not accumulate an unbounded multi-line buffer: {cmd}"
        );
        assert!(
            cmd.contains("head_subject=$(git log -1 --format=%s \"$head_commit\" 2>/dev/null | dd bs=1 count=$((head_subject_limit+1)) 2>/dev/null)"),
            "HEAD subject producer must be byte-bounded before command substitution: {cmd}"
        );
        assert!(
            cmd.contains(&format!(
                "head_subject_limit=$(({}-head_prefix_bytes))",
                SHOW_CHANGES_HEAD_BYTES
            )),
            "HEAD subject limit must derive from the remaining frame budget: {cmd}"
        );
        let forbidden = ["python3", "-c"].join(" ");
        assert!(
            !cmd.contains(&forbidden),
            "show_changes command must not invoke a Python helper: {cmd}"
        );
        for forbidden in [
            " clean",
            " restore",
            " add",
            " commit",
            " reset",
            " checkout",
            " push",
            " stash",
            " merge",
            " rebase",
            " rm ",
        ] {
            assert!(
                !cmd.contains(forbidden),
                "show_changes command must not contain '{}': {}",
                forbidden,
                cmd
            );
        }
    }
}

#[test]
fn show_changes_command_emits_bounded_metadata_frames() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    for i in 0..3 {
        std::fs::write(tmp.path().join(format!("new{i}.txt")), "x\n").unwrap();
    }
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let cmd = show_changes_command(true, 2, 5);
    let (exit, stdout, stderr, _) = run_command_sync(&cmd, tmp.path(), 30);
    assert_eq!(
        exit, 0,
        "command failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("## "), "missing branch header: {stdout}");
    assert!(
        stdout.contains("files_total="),
        "missing files_total: {stdout}"
    );
    assert!(
        stdout.contains("files_returned="),
        "missing files_returned: {stdout}"
    );
    assert!(
        stdout.contains("files_limit="),
        "missing files_limit: {stdout}"
    );
    assert!(
        stdout.contains("diff_hunks_returned="),
        "missing diff meta: {stdout}"
    );
    assert!(
        stdout.contains("diff_hunks_truncated="),
        "missing diff truncation: {stdout}"
    );
    for field in [
        "status_bytes=",
        "head_bytes=",
        "diff_stat_bytes=",
        "diff_trunc_hunk_count=",
        "diff_trunc_hunk_lines=",
        "diff_trunc_bytes=",
        "diff_trunc_bytes_in_hunk=",
        "diff_bytes=",
    ] {
        assert!(stdout.contains(field), "missing {field}: {stdout}");
    }
    // No raw placeholder may leak into the generated command.
    assert!(
        !cmd.contains("__SENTINEL__"),
        "sentinel placeholder leaked: {cmd}"
    );
    assert!(
        !cmd.contains("__HUNK_LIMIT__") && !cmd.contains("__LINE_LIMIT__"),
        "limit placeholder leaked: {cmd}"
    );
}

#[test]
fn show_changes_bounds_status_files_and_keeps_totals_exact() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    // Exceed the production status-file cap by many untracked files.
    let cap = 200usize;
    let total = cap + 50;
    for i in 0..total {
        std::fs::write(tmp.path().join(format!("u{i}.txt")), "x\n").unwrap();
    }
    let output = bounded_show_changes_output(tmp.path(), true, 4, 80);
    assert_eq!(
        output["files_total"].as_u64(),
        Some(total as u64),
        "files_total must count every entry: {output}"
    );
    assert_eq!(
        output["files_returned"].as_u64(),
        Some(cap as u64),
        "files_returned must hit the cap: {output}"
    );
    assert_eq!(
        output["files_truncated"], true,
        "must report truncation: {output}"
    );
    assert_eq!(
        output["files_limit"].as_u64(),
        Some(cap as u64),
        "files_limit must equal the cap: {output}"
    );
    let files = output["files"].as_array().unwrap();
    assert_eq!(
        files.len(),
        cap,
        "files array length must equal returned: {output}"
    );
    // Per-category counts must reflect ALL entries, not the truncated subset.
    assert_eq!(
        output["counts"]["untracked"].as_u64(),
        Some(total as u64),
        "untracked count must be exact over all entries: {output}"
    );
    assert_eq!(output["clean"], false, "truncated dirty repo is not clean");
    // The production-side count cap fired, so output_truncated is true with a
    // stable reason; the bound is still provably transport-safe.
    assert_eq!(output["transport_safe"], true);
    assert_eq!(output["output_truncated"], true);
    let reasons = output["truncation_reasons"]
        .as_array()
        .expect("truncation reasons array");
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str() == Some("status_file_count_limit")),
        "expected status_file_count_limit reason: {reasons:?}"
    );
    assert_show_changes_envelope_value_matches_schema(&output, "bounded status");
}

/// Build a long (but <= NAME_MAX) leaf filename so a *real* >256 KiB `git
/// status` can be produced from many tracked files in a single flat directory,
/// without relying on a single path exceeding filesystem limits. Each name is
/// ~237 printable chars plus a per-file suffix and `.x`, staying under the
/// 255-byte NAME_MAX.
fn long_leaf_name(i: usize) -> String {
    format!("{}{i}.x", "g".repeat(234))
}

#[test]
fn show_changes_status_over_retention_floor_keeps_branch_header_observable() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    // A *real* >256 KiB status: ~1200 tracked files with long (~237-byte)
    // names in a single flat directory, committed and then modified. The full
    // status legitimately exceeds the ordinary Runner result-retention
    // compatibility floor, not just the count limit.
    let dir = tmp.path().join("d");
    std::fs::create_dir_all(&dir).unwrap();
    let total = 1200usize;
    for i in 0..total {
        let path = dir.join(long_leaf_name(i));
        std::fs::write(&path, "x\n").unwrap();
    }
    let (add_exit, _, add_stderr, _) = run_command_sync("git add -A", tmp.path(), 30);
    assert_eq!(add_exit, 0, "git add failed: {add_stderr}");
    // Use `-q` so the commit summary (which can be very large for many files)
    // does not flood stdout under the synchronous command runner.
    let (commit_exit, _, commit_stderr, _) = run_command_sync("git commit -qm add", tmp.path(), 30);
    assert_eq!(commit_exit, 0, "git commit failed: {commit_stderr}");
    for i in 0..total {
        let path = dir.join(long_leaf_name(i));
        std::fs::write(&path, "y\nx\n").unwrap();
    }
    // First assert the *unbounded* raw status actually exceeds 256 KiB. The raw
    // status legitimately exceeds the OS pipe buffer, so use the full-capture
    // helper that drains the pipes concurrently (the polling helper deadlocks).
    let (raw_exit, raw_stdout, raw_stderr) =
        run_command_full_capture("git status --porcelain=v1 -b", tmp.path(), 30);
    assert_eq!(
        raw_exit, 0,
        "raw status failed\nstdout:\n{raw_stdout}\nstderr:\n{raw_stderr}"
    );
    let raw_status_bytes = raw_stdout.len();
    assert!(
        raw_status_bytes > ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES,
        "raw status must exceed the ordinary Runner result-retention compatibility floor; got {raw_status_bytes} bytes"
    );
    // The bounded show_changes command must stay within the production budget.
    // Its output is also larger than the pipe buffer, so use full capture.
    let cmd = show_changes_command(false, 20, 80);
    let (exit, stdout, stderr) = run_command_full_capture(&cmd, tmp.path(), 30);
    assert_eq!(
        exit, 0,
        "command failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let bounded_stdout_bytes = stdout.len();
    assert!(
        bounded_stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "bounded stdout must stay within the production budget; got {bounded_stdout_bytes} bytes (budget {})",
        SHOW_CHANGES_OUTPUT_BUDGET_BYTES
    );
    // Simulate the ordinary Runner per-stream result-retention compatibility
    // floor. This is not a polling/WebSocket/QUIC wire-body or frame ceiling.
    let retained = simulate_runner_result_retention_tail(
        &stdout,
        ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES,
    );
    assert_eq!(
        retained, stdout,
        "bounded output must be unchanged by ordinary Runner result retention"
    );
    let frames = split_show_changes_stdout(&retained, false);
    assert!(
        frames
            .status
            .lines()
            .any(|line| parse_status_header(line).is_some()),
        "branch header must survive ordinary Runner result retention: {retained}"
    );
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, "");
    assert!(observation.status_observed());
    // Totals stay exact even though records were bounded.
    assert_eq!(frames.files_total, Some(total));
    assert_eq!(frames.files_truncated, Some(true));
    assert!(frames.files_returned.unwrap_or(0) <= SHOW_CHANGES_MAX_STATUS_FILES);
    // Parse the structured output to assert transport/truncation fields.
    let output = parse_show_changes_output_with_observation(
        "demo",
        &frames.status,
        &frames.head,
        &frames.stat,
        None,
        20,
        80,
        Some(exit),
        &stderr,
        observation,
        &frames,
    );
    assert_eq!(output["transport_safe"], true);
    assert_eq!(output["output_truncated"], true);
    let reasons = output["truncation_reasons"]
        .as_array()
        .expect("truncation reasons array");
    assert!(
        reasons
            .iter()
            .any(|r| r.as_str() == Some("status_file_count_limit")
                || r.as_str() == Some("status_byte_budget")),
        "expected a status count or byte budget reason: {reasons:?}"
    );
    assert_show_changes_envelope_value_matches_schema(&output, "256k status");
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[test]
fn show_changes_head_fields_match_git_in_sh_and_bash() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let subject = "fix: preserve labelled HEAD fields exactly";
    commit_file(tmp.path(), "README.md", "hello\n", subject);

    let expected = |git_command: &str| {
        let (exit, stdout, stderr) = run_command_full_capture(git_command, tmp.path(), 30);
        assert_eq!(exit, 0, "{git_command} failed: {stderr}");
        stdout.trim_end_matches('\n').to_string()
    };
    let expected_commit = expected("git rev-parse HEAD");
    let expected_short = expected("git rev-parse --short HEAD");
    let expected_summary = expected("git log -1 --format=%s");
    let command = show_changes_command(false, 20, 80);

    for shell in ["sh", "bash"] {
        let wrapped = format!("{shell} -c {}", shell_single_quote(&command));
        let (exit, stdout, stderr) = run_command_full_capture(&wrapped, tmp.path(), 30);
        assert_eq!(exit, 0, "{shell} failed: {stderr}\n{stdout}");
        let frames = split_show_changes_stdout(&stdout, false);
        assert_eq!(frames.head_bytes, Some(frames.head.len()));
        let output =
            bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
        assert_eq!(
            output["head"]["commit"].as_str(),
            Some(expected_commit.as_str()),
            "{shell} commit"
        );
        assert_eq!(
            output["head"]["short"].as_str(),
            Some(expected_short.as_str()),
            "{shell} short"
        );
        assert_eq!(
            output["head"]["summary"].as_str(),
            Some(expected_summary.as_str()),
            "{shell} summary"
        );
        assert_eq!(output["transport_safe"], true, "{shell}: {output}");
        eprintln!(
            "head_shell={shell} commit={expected_commit} short={expected_short} summary={expected_summary}"
        );
    }
}

#[test]
fn show_changes_unborn_repository_emits_empty_complete_head_frame() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let command = show_changes_command(false, 20, 80);
    let wrapped = format!("sh -c {}", shell_single_quote(&command));
    let (exit, stdout, stderr) = run_command_full_capture(&wrapped, tmp.path(), 30);
    assert_eq!(exit, 0, "unborn show_changes failed: {stderr}\n{stdout}");
    assert!(stdout.len() <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES);
    let frames = split_show_changes_stdout(&stdout, false);
    assert!(frames.head.is_empty());
    assert!(frames.head_exit.is_some_and(|value| value != 0));
    assert_eq!(frames.head_truncated, Some(false));
    assert_eq!(frames.head_bytes, Some(0));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    assert_eq!(output["head"]["commit"], Value::Null);
    assert_eq!(output["head"]["short"], Value::Null);
    assert_eq!(output["head"]["summary"], Value::Null);
    assert_eq!(output["transport_safe"], true);
}

#[test]
fn show_changes_crlf_diff_bytes_match_parsed_frame_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "crlf.txt", "before\r\n", "track CRLF");
    std::fs::write(tmp.path().join("crlf.txt"), b"before\r\nafter\r\n").unwrap();

    let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    let frames = split_show_changes_stdout(&stdout, true);
    assert!(
        frames.diff.ends_with('\r'),
        "final CR from CRLF diff line was lost: {:?}",
        frames.diff.as_bytes().last()
    );
    assert_eq!(frames.diff_bytes, Some(frames.diff.len()));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(output["transport_safe"], true, "{output}");
    eprintln!(
        "crlf_diff frame_bytes={} metadata_bytes={}",
        frames.diff.len(),
        frames.diff_bytes.unwrap()
    );
}

fn multibyte_status_path(i: usize) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::new();
    for level in 0..3 {
        path.push(format!("{}-{level}", "界".repeat(60)));
    }
    path.push(format!("文件-{i:03}.txt"));
    path
}

#[test]
fn show_changes_bash_utf8_locale_counts_multibyte_status_in_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let total = 500usize;
    for i in 0..total {
        let relative = multibyte_status_path(i);
        let path = tmp.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "before\n").unwrap();
    }
    let (setup_exit, _, setup_stderr) = run_command_full_capture(
        "git config core.quotePath false && git add -A && git commit -qm multibyte",
        tmp.path(),
        60,
    );
    assert_eq!(setup_exit, 0, "setup failed: {setup_stderr}");
    for i in 0..total {
        std::fs::write(tmp.path().join(multibyte_status_path(i)), "after\n").unwrap();
    }
    let (raw_exit, raw_status, raw_stderr) = run_command_full_capture(
        "LC_ALL=zh_CN.utf8 git status --porcelain=v1 -b",
        tmp.path(),
        60,
    );
    assert_eq!(raw_exit, 0, "raw status failed: {raw_stderr}");
    assert!(
        raw_status.len() > ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES,
        "raw status bytes={}",
        raw_status.len()
    );

    let cmd = show_changes_command(false, 20, 80);
    let bash_cmd = format!("LC_ALL=zh_CN.utf8 bash -c {}", shell_single_quote(&cmd));
    let (exit, stdout, stderr) = run_command_full_capture(&bash_cmd, tmp.path(), 60);
    assert_eq!(exit, 0, "bash command failed: {stderr}");
    assert!(
        stdout.len() <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "bounded bytes={}",
        stdout.len()
    );
    assert_eq!(
        simulate_runner_result_retention_tail(
            &stdout,
            ORDINARY_RUNNER_RESULT_RETENTION_COMPAT_BYTES,
        )
        .as_bytes(),
        stdout.as_bytes()
    );

    let frames = split_show_changes_stdout(&stdout, false);
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, &stderr);
    assert!(observation.status_observed());
    assert!(frames
        .status
        .lines()
        .any(|line| parse_status_header(line).is_some()));
    assert_eq!(frames.files_total, Some(total));
    assert_eq!(frames.counts_modified, Some(total));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    assert_eq!(output["transport_safe"], true);
    eprintln!(
        "multibyte_status raw_bytes={} bounded_bytes={}",
        raw_status.len(),
        stdout.len()
    );
}

#[test]
fn show_changes_transport_safe_requires_every_modern_metadata_frame() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "before\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "after\n").unwrap();
    let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    let frames = split_show_changes_stdout(&stdout, true);
    let complete =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(complete["transport_safe"], true);

    let mut variants = Vec::new();
    let mut missing_status = frames.clone();
    missing_status.status_bytes = None;
    variants.push(("status", missing_status));
    let mut missing_head = frames.clone();
    missing_head.head_bytes = None;
    variants.push(("head", missing_head));
    let mut missing_stat = frames.clone();
    missing_stat.diff_stat_bytes = None;
    variants.push(("stat", missing_stat));
    let mut missing_diff = frames.clone();
    missing_diff.diff_trunc_bytes = None;
    variants.push(("diff", missing_diff));
    let mut missing_diff_hunk_byte_provenance = frames.clone();
    missing_diff_hunk_byte_provenance.diff_trunc_bytes_in_hunk = None;
    variants.push((
        "diff_hunk_byte_provenance",
        missing_diff_hunk_byte_provenance,
    ));

    for (label, incomplete) in variants {
        let output =
            bounded_show_changes_output_from_frames(&incomplete, tmp.path(), true, 20, 80, &stderr);
        assert_eq!(output["transport_safe"], false, "missing {label} metadata");
    }

    let mut wrong_diff_bytes = frames.clone();
    wrong_diff_bytes.diff_bytes = Some(frames.diff.len() + 1);
    let wrong_diff_output = bounded_show_changes_output_from_frames(
        &wrong_diff_bytes,
        tmp.path(),
        true,
        20,
        80,
        &stderr,
    );
    assert_eq!(wrong_diff_output["transport_safe"], false);

    let mut oversized_stat = frames.clone();
    oversized_stat.stat = "x".repeat(SHOW_CHANGES_DIFF_STAT_BYTES + 1);
    oversized_stat.diff_stat_bytes = Some(oversized_stat.stat.len());
    let oversized_output =
        bounded_show_changes_output_from_frames(&oversized_stat, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(oversized_output["transport_safe"], false);
    eprintln!(
        "invalid_metadata wrong_diff_bytes_transport_safe={} oversized_stat_transport_safe={}",
        wrong_diff_output["transport_safe"], oversized_output["transport_safe"]
    );
}

#[test]
fn show_changes_status_config_error_is_not_reported_clean() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    let (config_exit, _, config_stderr, _) = run_command_sync(
        "git config status.showUntrackedFiles invalid",
        tmp.path(),
        30,
    );
    assert_eq!(
        config_exit, 0,
        "failed to set regression config: {config_stderr}"
    );
    let cmd = show_changes_command(false, 20, 80);
    let (exit, stdout, _stderr, _) = run_command_sync(&cmd, tmp.path(), 30);
    assert_ne!(exit, 0, "status config error must not exit 0");
    let frames = split_show_changes_stdout(&stdout, false);
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, &_stderr);
    assert_eq!(observation.as_json()["status"], "command_failed");
    assert_eq!(
        observation.as_json()["reason_code"],
        "git_status_config_error"
    );
    // A failed status must never claim cleanliness.
    let output = parse_show_changes_output(
        "demo",
        &frames.status,
        &frames.head,
        &frames.stat,
        None,
        20,
        80,
        Some(exit),
        &_stderr,
    );
    assert_eq!(output["clean"], Value::Null);
    assert_eq!(output["counts"]["conflicted"], Value::Null);
}

#[test]
fn show_changes_include_diff_false_omits_diff_and_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let output = bounded_show_changes_output(tmp.path(), false, 20, 80);
    assert!(
        output.get("hunks").is_none(),
        "no hunks without include_diff"
    );
    assert!(output.get("hunk_count").is_none());
    assert!(output.get("hunks_truncated").is_none());
    assert!(output.get("diff_review_handoff").is_none());
}

#[test]
fn show_changes_complete_diff_does_not_handoff_to_git_diff_hunks() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();

    let output = bounded_show_changes_output(tmp.path(), true, 20, 80);
    assert_eq!(output["hunks_truncated"], false);
    assert!(output.get("diff_review_handoff").is_none());
    assert!(!output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action
            .as_str()
            .is_some_and(|action| action.contains("git_diff_hunks"))));
    assert_show_changes_envelope_value_matches_schema(&output, "complete diff handoff");
}

#[test]
fn show_changes_small_mid_file_edit_stays_within_default_hunk_lines() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let original = (0..240)
        .map(|index| format!("line-{index:03}\n"))
        .collect::<String>();
    commit_file(tmp.path(), "middle.txt", &original, "initial");
    let changed = original.replacen("line-120\n", "line-120 changed\n", 1);
    std::fs::write(tmp.path().join("middle.txt"), changed).unwrap();

    let output = bounded_show_changes_output(tmp.path(), true, 20, 80);
    assert_eq!(output["hunks_truncated"], false, "{output}");
    assert!(output.get("diff_review_handoff").is_none(), "{output}");
    let diff = output["hunks"][0]["hunks"][0]["diff"]
        .as_str()
        .expect("returned hunk diff");
    assert!(
        diff.lines().count() <= 80,
        "ordinary small mid-file change should fit the default hunk line budget: {diff}"
    );
}

#[test]
fn show_changes_complete_model_projection_removes_only_derived_review_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();

    let canonical = bounded_show_changes_output(tmp.path(), true, 20, 80);
    assert_eq!(canonical["transport_safe"], true);
    assert_eq!(canonical["output_truncated"], false);
    assert_eq!(canonical["hunks_truncated"], false);
    assert!(canonical.get("hunk_count").is_some());
    assert!(canonical["hunks"][0]["hunks"][0]
        .get("line_count")
        .is_some());

    let mut projected = ToolResult::ok(canonical.clone());
    sparsify_complete_git_review_success("show_changes", &mut projected);
    let output = &projected.output;
    for retained in [
        "project",
        "branch",
        "head",
        "counts",
        "files",
        "diff_stat",
        "hunks",
    ] {
        assert!(
            output.get(retained).is_some(),
            "missing retained {retained}: {output}"
        );
    }
    for omitted in [
        "git_available",
        "non_git_project",
        "status_observation",
        "files_total",
        "files_returned",
        "files_truncated",
        "files_limit",
        "transport_safe",
        "output_budget_bytes",
        "output_truncated",
        "truncation_reasons",
        "diff_exit",
        "diff_status",
        "diff_stat_exit",
        "diff_stat_status",
        "head_exit",
        "hunk_count",
        "hunks_truncated",
        "warnings",
        "exit_code",
        "stderr",
    ] {
        assert!(
            output.get(omitted).is_none(),
            "derived {omitted} leaked: {output}"
        );
    }
    assert!(output["hunks"][0].get("old_path").is_none());
    assert!(output["hunks"][0]["hunks"][0].get("line_count").is_none());
    assert!(output["hunks"][0]["hunks"][0].get("truncated").is_none());
    assert!(canonical.get("transport_safe").is_some());
    assert!(canonical.get("hunk_count").is_some());
}

#[test]
fn show_changes_diff_respects_max_hunks() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "a.txt", "a\n", "initial");
    // Three modified files -> three diff hunks; cap at 1.
    for name in ["a.txt", "b.txt", "c.txt"] {
        commit_file(tmp.path(), name, "line\n", "add");
        std::fs::write(tmp.path().join(name), "line\nmore\n").unwrap();
    }
    let output = bounded_show_changes_output(tmp.path(), true, 1, 80);
    assert_eq!(
        output["hunk_count"].as_u64(),
        Some(1),
        "must cap at max_hunks"
    );
    assert_eq!(
        output["hunks_truncated"], true,
        "must report hunk truncation"
    );
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(reasons.iter().any(|r| r == "diff_hunk_count_limit"));
    assert!(!reasons.iter().any(|r| r == "diff_hunk_line_limit"));
    assert!(!reasons.iter().any(|r| r == "diff_byte_budget"));
    assert_eq!(output["diff_review_handoff"]["tool"], "git_diff_hunks");
    assert_eq!(output["diff_review_handoff"]["scope"], "worktree");
    assert_eq!(
        output["diff_review_handoff"]["reason"],
        "show_changes_diff_truncated"
    );
    assert_eq!(
        output["diff_review_handoff"]["truncation_reasons"],
        json!(["diff_hunk_count_limit"])
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["project"],
        "demo"
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["cached"],
        false
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["paths"],
        json!([])
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["max_hunks"],
        30
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["max_page_bytes"],
        DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
    );
    let recovery = &output["diff_review_handoff"]["recovery"];
    assert_eq!(recovery["kind"], "page");
    assert_eq!(recovery["tool"], "git_diff_hunks");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], Value::Null);
    assert_eq!(
        recovery["arguments"],
        output["diff_review_handoff"]["suggested_call"]
    );
    assert_git_diff_hunks_recovery_call_parses(recovery);
    let suggested_call = output["diff_review_handoff"]["suggested_call"].clone();
    ToolCall::from_tool_name("git_diff_hunks", suggested_call)
        .expect("show_changes suggested_call must be directly reusable as git_diff_hunks input");
    assert!(
        crate::tool_runtime::tool_definition::is_adaptive_runtime_direct_tool(
            output["diff_review_handoff"]["tool"].as_str().unwrap()
        )
    );
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(!actions
        .iter()
        .any(|action| action == "review workspace changes with show_changes"));
    assert!(actions.iter().any(|action| action
        == "continue the diff review with git_diff_hunks; use paths to narrow scope when useful"));
    assert!(actions
        .iter()
        .any(|action| action == "follow git_diff_hunks.next_continuation while has_more=true"));
    assert!(!actions.iter().any(|action| action
        .as_str()
        .is_some_and(|action| action.contains("continuation alone does not recover"))));
    assert_show_changes_envelope_value_matches_schema(&output, "hunk count handoff");
}

#[test]
fn show_changes_diff_respects_max_hunk_lines() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    // One file with many changed lines -> one big hunk; cap lines at 3.
    let content = (0..20).map(|i| format!("line{i}\n")).collect::<String>();
    commit_file(tmp.path(), "big.txt", &content, "initial");
    std::fs::write(
        tmp.path().join("big.txt"),
        format!("{content}extra\nmore\n"),
    )
    .unwrap();
    let output = bounded_show_changes_output(tmp.path(), true, 20, 3);
    assert_eq!(output["hunk_count"].as_u64(), Some(1));
    let files = output["hunks"].as_array().unwrap();
    let hunks = files[0]["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty(), "expected at least one hunk: {files:?}");
    let lines = hunks[0]["diff"].as_str().unwrap().lines().count();
    // header line + up to 3 content lines = at most 4 lines.
    assert!(lines <= 4, "hunk must be line-bounded: {hunks:?}");
    assert_eq!(output["hunks_truncated"], true);
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(reasons.iter().any(|r| r == "diff_hunk_line_limit"));
    assert!(!reasons.iter().any(|r| r == "diff_hunk_count_limit"));
    assert!(!reasons.iter().any(|r| r == "diff_byte_budget"));
    assert_eq!(output["diff_review_handoff"]["tool"], "git_diff_hunks");
    assert_eq!(
        output["diff_review_handoff"]["truncation_reasons"],
        json!(["diff_hunk_line_limit"])
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["paths"],
        json!([]),
        "line truncation must not guess a narrower path without per-hunk provenance"
    );
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["max_hunk_lines"],
        400
    );
    let recovery = &output["diff_review_handoff"]["recovery"];
    assert_eq!(recovery["kind"], "hunk_lines");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(
        recovery["arguments"],
        output["diff_review_handoff"]["suggested_call"]
    );
    assert_git_diff_hunks_recovery_call_parses(recovery);
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(actions.iter().any(|action| action
        == "increase git_diff_hunks.max_hunk_lines and/or narrow paths; continuation alone does not recover omitted lines from the same hunk"));
    assert!(!actions
        .iter()
        .any(|action| action == "follow git_diff_hunks.next_continuation while has_more=true"));
    assert_show_changes_envelope_value_matches_schema(&output, "hunk line handoff");
}

#[test]
fn show_changes_combined_hunk_count_and_line_truncation_keeps_both_guidance_paths() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let original = (0..20).map(|i| format!("line-{i}\n")).collect::<String>();
    for name in ["a.txt", "b.txt"] {
        commit_file(tmp.path(), name, &original, "initial");
        let changed = (0..20)
            .map(|i| format!("changed-{name}-{i}\n"))
            .collect::<String>();
        std::fs::write(tmp.path().join(name), changed).unwrap();
    }

    let output = bounded_show_changes_output(tmp.path(), true, 1, 3);
    assert_eq!(output["hunks_truncated"], true);
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(reasons
        .iter()
        .any(|reason| reason == "diff_hunk_count_limit"));
    assert!(reasons
        .iter()
        .any(|reason| reason == "diff_hunk_line_limit"));
    let handoff_reasons = output["diff_review_handoff"]["truncation_reasons"]
        .as_array()
        .unwrap();
    assert!(handoff_reasons
        .iter()
        .any(|reason| reason == "diff_hunk_count_limit"));
    assert!(handoff_reasons
        .iter()
        .any(|reason| reason == "diff_hunk_line_limit"));
    assert_eq!(
        output["diff_review_handoff"]["suggested_call"]["paths"],
        json!([]),
        "combined page truncation must not narrow away later files"
    );
    let recovery = &output["diff_review_handoff"]["recovery"];
    assert_eq!(recovery["kind"], "mixed");
    assert_eq!(recovery["safe_continuation_for_omitted_lines"], false);
    assert_eq!(
        recovery["arguments"],
        output["diff_review_handoff"]["suggested_call"]
    );
    assert_git_diff_hunks_recovery_call_parses(recovery);
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|action| action == "follow git_diff_hunks.next_continuation while has_more=true"));
    assert!(actions.iter().any(|action| action
        .as_str()
        .is_some_and(|action| action.contains("continuation alone does not recover"))));
    assert_show_changes_envelope_value_matches_schema(&output, "combined diff handoff");
}

#[tokio::test]
async fn show_changes_untracked_preview_truncation_does_not_create_diff_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    for i in 0..6 {
        std::fs::write(
            tmp.path().join(format!("untracked-{i}.txt")),
            format!("u{i}\n"),
        )
        .unwrap();
    }

    let runtime = test_runtime();
    let client_id = "show-untracked-handoff";
    let project = register_runner_project_at_path(&runtime, client_id, "repo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, client_id, project, None, true).await;
    assert!(result.success, "{:?}", result.error);
    assert_show_changes_envelope_matches_schema("untracked-only truncation", &result);
    let output = &result.output;
    assert_eq!(output["hunks_truncated"], false);
    assert_eq!(output["untracked_previews_truncated"], true);
    assert!(output.get("diff_review_handoff").is_none());
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(!actions
        .iter()
        .any(|action| action == "review workspace changes with show_changes"));
    assert!(actions
        .iter()
        .any(|action| action == "inspect the relevant untracked files separately"));
    assert!(!actions.iter().any(|action| action
        .as_str()
        .is_some_and(|action| action.contains("git_diff_hunks"))));
}

#[test]
fn show_changes_large_diff_does_not_depend_on_runner_retained_tail() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let big = (0..50).map(|i| format!("line{i}\n")).collect::<String>();
    commit_file(tmp.path(), "big.txt", &big, "initial");
    // Make a diff with many hunks; the command bounds to max_hunks=1, line=10.
    let modified = (0..50)
        .map(|i| {
            if i % 2 == 0 {
                format!("mod{i}\n")
            } else {
                format!("line{i}\n")
            }
        })
        .collect::<String>();
    std::fs::write(tmp.path().join("big.txt"), modified).unwrap();
    let output = bounded_show_changes_output(tmp.path(), true, 1, 10);
    // The first selected hunk must be present and the diff must be bounded.
    assert_eq!(output["hunk_count"].as_u64(), Some(1));
    // The structured fields (not a tail marker) report truncation.
    assert_eq!(output["hunks_truncated"], true);
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(
        !serialized.contains("[output truncated"),
        "must not rely on transport tail marker: {serialized}"
    );
}

#[test]
fn show_changes_schema_covers_truncation_and_transport_fields() {
    let schema = crate::tool_runtime::registry::output_schema_for_tool("show_changes");
    let properties = schema["properties"]["output"]["properties"]
        .as_object()
        .expect("show_changes output properties");
    for field in [
        "files_total",
        "files_returned",
        "files_truncated",
        "files_limit",
        "transport_safe",
        "output_budget_bytes",
        "output_truncated",
        "truncation_reasons",
        "diff_review_handoff",
    ] {
        assert!(
            properties.contains_key(field),
            "missing truncation/transport field {field}"
        );
    }
    let handoff = &properties["diff_review_handoff"];
    assert_eq!(handoff["type"], "object");
    assert_eq!(handoff["additionalProperties"], false);
    assert_eq!(handoff["properties"]["tool"]["const"], "git_diff_hunks");
    assert_eq!(handoff["properties"]["scope"]["const"], "worktree");
    assert_eq!(
        handoff["properties"]["reason"]["const"],
        "show_changes_diff_truncated"
    );
    assert_eq!(
        handoff["properties"]["truncation_reasons"]["items"]["enum"],
        json!([
            "diff_hunk_count_limit",
            "diff_hunk_line_limit",
            "diff_byte_budget",
            "diff_hunk_byte_budget"
        ])
    );
    assert_eq!(
        handoff["required"],
        json!([
            "tool",
            "scope",
            "reason",
            "truncation_reasons",
            "recovery",
            "suggested_call"
        ])
    );
    let recovery = &handoff["properties"]["recovery"];
    assert_eq!(recovery["additionalProperties"], false);
    assert_eq!(recovery["properties"]["tool"]["const"], "git_diff_hunks");
    assert_eq!(
        recovery["properties"]["kind"]["enum"],
        json!(["page", "hunk_lines", "mixed"])
    );
    let suggested = &handoff["properties"]["suggested_call"];
    assert_eq!(suggested["additionalProperties"], false);
    assert_eq!(suggested["properties"]["cached"]["const"], false);
    assert_eq!(
        suggested["required"],
        json!([
            "project",
            "cached",
            "paths",
            "max_hunks",
            "max_hunk_lines",
            "max_page_bytes"
        ])
    );
}

#[tokio::test]
async fn show_changes_include_diff_agent_command_does_not_enqueue_python_helper() {
    let runtime = runtime_with_agent_project("show-native");
    let caps = RunnerCapabilities {
        shell: true,
        internal_posix_script: true,
        ..Default::default()
    };
    register_agent(&runtime, "show-native", None, caps).await;
    let project = agent_test_project_id("show-native");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ShowChanges {
                        project,
                        session_id: None,
                        include_diff: Some(true),
                        max_hunks: None,
                        max_hunk_lines: None,
                        session_event_limit: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "show-native").await;
    assert_eq!(req.kind, "run_internal_posix_script");
    assert!(req.command.is_empty());
    let payload = req
        .script
        .as_ref()
        .expect("show_changes must carry a typed internal script");
    assert_eq!(
        payload.language,
        crate::runner_protocol::ShellScriptLanguage::Sh
    );
    assert!(payload.args.is_empty());
    let forbidden = ["python3", "-c"].join(" ");
    assert!(
        !payload.script.contains(&forbidden),
        "show_changes include_diff must not enqueue a Python helper: {}",
        payload.script
    );
    assert!(payload.script.contains("git diff --unified="));
    let stdout = framed_clean_show_changes_test_stdout("head", true);
    complete_patch_agent_request(&runtime, "show-native", &req.request_id, 0, &stdout, "").await;
    let result = task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["untracked_previews"], json!([]));
}

#[test]
fn show_changes_clean_worktree() {
    let output = parse_show_changes_output(
            "agent:oe:webcodex",
            "## main...origin/main",
            "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix: route anchor edit file ops through agent dispatch",
            "",
            None,
            20,
            80,
            Some(0),
            "",
        );
    assert_eq!(output["clean"], true);
    assert_eq!(output["branch"], "main");
    assert_eq!(output["head"]["short"], "b47e4fb");
    assert_eq!(output["counts"]["modified"], 0);
    assert!(output["files"].as_array().unwrap().is_empty());
    assert!(output.get("hunks").is_none());
    assert!(output["session"].is_null());
    assert_eq!(output["suggested_next_actions"][0], "no changes detected");
    assert_review_verdict_shape(&output["verdict"]);
    assert_ne!(output["verdict"]["status"], "fail");
    assert_eq!(output["verdict"]["blocking"], false);
}

#[test]
fn show_changes_without_session_id_treats_dirty_workspace_as_advisory() {
    let mut output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n M src/lib.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        " src/lib.rs | 2 +-",
        None,
        20,
        80,
        Some(0),
        "",
    );
    apply_show_changes_session(&mut output, None, None, None);
    assert_eq!(output["clean"], false);
    assert_eq!(output["counts"]["modified"], 1);
    assert_review_verdict_shape(&output["verdict"]);
    assert_eq!(output["verdict"]["status"], "warn");
    assert_eq!(output["verdict"]["blocking"], false);
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "workspace_dirty");
    assert!(output["session"].is_null());
    assert!(output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "review diff"));
}

#[test]
fn show_changes_with_session_id_defaults_to_compact_session_summary() {
    let runtime = test_runtime();
    let session = runtime.sessions.start_session(
        Some("agent:oe:webcodex".to_string()),
        Some("finish task".to_string()),
    );
    let write_args = json!({"project": "agent:oe:webcodex", "path": "src/foo.rs"});
    let write = runtime.sessions.record_tool_call_started(
        Some(&session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Api,
        "write_project_file",
        &write_args,
        crate::tool_runtime::sessions::session_tool_contract("write_project_file"),
    );
    runtime
        .sessions
        .record_tool_call_finished(write, true, &json!({}), None, None);
    let shell_args = json!({"project": "agent:oe:webcodex", "command": "cargo test"});
    let shell = runtime.sessions.record_tool_call_started(
        Some(&session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Api,
        "run_shell",
        &shell_args,
        crate::tool_runtime::sessions::session_tool_contract("run_shell"),
    );
    runtime
        .sessions
        .record_tool_call_finished(shell, true, &json!({}), None, None);

    let mut output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n M src/foo.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        " src/foo.rs | 2 +-",
        None,
        20,
        80,
        Some(0),
        "",
    );
    let summary = runtime.sessions.summary(&session.session_id, Some(30));
    apply_show_changes_session(&mut output, Some(&session.session_id), summary, None);

    assert_eq!(output["session"]["found"], true);
    assert_eq!(output["session"]["session_id"], session.session_id);
    assert_eq!(output["session"]["title"], "finish task");
    assert_eq!(output["session"]["counts"]["tool_calls"], 2);
    assert_eq!(output["session"]["counts"]["write_like"], 1);
    assert_eq!(output["session"]["counts"]["shell_like"], 1);
    assert_eq!(output["session"]["changed_paths"], json!(["src/foo.rs"]));
    assert_eq!(output["session"]["signals"]["failed"], false);
    assert_eq!(output["session"]["signals"]["write_like"], true);
    assert_eq!(output["session"]["signals"]["shell_like"], true);
    assert!(output["session"].get("recent_events").is_none());
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|v| v == "review changed paths from this session"));
    assert!(actions
        .iter()
        .any(|v| v == "check command/test results before commit"));
}

#[test]
fn show_changes_with_missing_session_id_returns_warning_not_panic() {
    let mut output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        "",
        None,
        20,
        80,
        Some(0),
        "",
    );
    apply_show_changes_session(&mut output, Some("wc_sess_missing"), None, None);
    assert_eq!(output["session"]["found"], false);
    assert_eq!(output["session"]["session_id"], "wc_sess_missing");
    assert!(output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["kind"] == "session_not_found"));
    assert_eq!(output["suggested_next_actions"][0], "no changes detected");
}

#[test]
fn show_changes_session_changed_paths_are_deduped() {
    let runtime = test_runtime();
    let session = runtime.sessions.start_session(None, None);
    for path in ["src/foo.rs", "src/foo.rs", "src/bar.rs"] {
        let args = json!({"project": "agent:oe:webcodex", "path": path});
        let start = runtime.sessions.record_tool_call_started(
            Some(&session.session_id),
            crate::tool_runtime::sessions::SessionTransport::Api,
            "write_project_file",
            &args,
            crate::tool_runtime::sessions::session_tool_contract("write_project_file"),
        );
        runtime
            .sessions
            .record_tool_call_finished(start, true, &json!({}), None, None);
    }
    let mut output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n M src/foo.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        " src/foo.rs | 2 +-",
        None,
        20,
        80,
        Some(0),
        "",
    );
    let summary = runtime.sessions.summary(&session.session_id, Some(30));
    apply_show_changes_session(&mut output, Some(&session.session_id), summary, None);
    assert_eq!(
        output["session"]["changed_paths"],
        json!(["src/foo.rs", "src/bar.rs"])
    );
}

#[tokio::test]
async fn show_changes_explicit_session_event_limit_is_bounded() {
    let runtime = runtime_with_agent_project("show");
    let caps = RunnerCapabilities {
        shell: true,
        internal_posix_script: true,
        ..Default::default()
    };
    register_agent(&runtime, "show", None, caps).await;
    let session = runtime.sessions.start_session(None, None);
    for idx in 0..250 {
        let args =
            json!({"project": agent_test_project_id("show"), "path": format!("src/{idx}.rs")});
        let start = runtime.sessions.record_tool_call_started(
            Some(&session.session_id),
            crate::tool_runtime::sessions::SessionTransport::Api,
            "write_project_file",
            &args,
            crate::tool_runtime::sessions::session_tool_contract("write_project_file"),
        );
        runtime
            .sessions
            .record_tool_call_finished(start, true, &json!({}), None, None);
    }
    let runtime_for_task = runtime.clone();
    let project = agent_test_project_id("show");
    let session_id = session.session_id.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .show_changes(project, Some(session_id), None, None, None, Some(999))
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "show").await;
    let stdout = framed_clean_show_changes_test_stdout("head", false);
    complete_patch_agent_request(&runtime, "show", &req.request_id, 0, &stdout, "").await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let len = result.output["session"]["recent_events"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(len, 200);
}

#[test]
fn show_changes_reports_modified_file() {
    let output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n M src/users_http.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        " src/users_http.rs | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)",
        None,
        20,
        80,
        Some(0),
        "",
    );
    assert_eq!(output["clean"], false);
    assert_eq!(output["counts"]["modified"], 1);
    assert_eq!(output["counts"]["unstaged"], 1);
    assert_eq!(output["verdict"]["status"], "warn");
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "workspace_dirty");
    assert_eq!(output["files"][0]["path"], "src/users_http.rs");
    assert_eq!(output["files"][0]["status"], "modified");
    assert_eq!(output["files"][0]["kind"], "tracked");
    assert!(output["diff_stat"]
        .as_str()
        .unwrap()
        .contains("1 file changed"));
}

#[test]
fn show_changes_reports_untracked_file() {
    let output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n?? webcodex-anchor-edit-smoke-c99f7de.txt",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        "",
        None,
        20,
        80,
        Some(0),
        "",
    );
    assert_eq!(output["clean"], false);
    assert_eq!(output["counts"]["untracked"], 1);
    assert_eq!(output["counts"]["conflicted"], 0);
    assert_eq!(output["files"][0]["status"], "untracked");
    assert_eq!(output["files"][0]["staged"], false);
    assert_eq!(output["warnings"][0]["kind"], "untracked_smoke_file");
    assert_eq!(output["verdict"]["status"], "warn");
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "workspace_dirty");
    assert!(output["suggested_next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str().unwrap().contains("untracked")));
}

#[test]
fn show_changes_reports_conflicted_file() {
    let output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\nUU conflicted.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        "",
        None,
        20,
        80,
        Some(0),
        "",
    );
    assert_eq!(output["clean"], false);
    assert_eq!(output["counts"]["conflicted"], 1);
    assert_eq!(output["counts"]["modified"], 0);
    assert_eq!(output["files"][0]["path"], "conflicted.rs");
    assert_eq!(output["files"][0]["status"], "conflicted");
    assert_eq!(output["files"][0]["kind"], "conflicted");
    assert!(
        output["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "workspace_conflicts"),
        "expected workspace_conflicts warning: {}",
        output["warnings"]
    );
    // A real merge conflict is a hard blocker; ordinary dirty state is advisory.
    assert_eq!(output["verdict"]["status"], "fail");
    assert_reason_list_contains(
        &output["verdict"],
        "blocking_reasons",
        "workspace_conflicts",
    );
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "workspace_dirty");
}

#[test]
fn show_changes_include_diff_true_returns_bounded_hunks() {
    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 line one
-old
+new
 line three
@@ -10,3 +10,3 @@
 alpha
-beta
+gamma
 omega
";
    let output = parse_show_changes_output(
        "agent:oe:webcodex",
        "## main\n M src/lib.rs",
        "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=fix",
        " src/lib.rs | 4 ++--",
        Some(diff),
        1,
        4,
        Some(0),
        "",
    );
    assert_eq!(output["hunk_count"], 1);
    assert_eq!(output["hunks_truncated"], true);
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "truncated_by_limit");
    let hunks = output["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0]["path"], "src/lib.rs");
    assert_eq!(hunks[0]["hunks"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn show_changes_clean_repo_include_diff_false_has_no_untracked_previews() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());

    let output = show_changes_output_from_command(tmp.path(), false);

    assert_eq!(output["clean"], true);
    assert_eq!(output["counts"]["untracked"], 0);
    assert!(output.get("untracked_previews").is_none());
}

#[tokio::test]
async fn show_changes_untracked_text_include_diff_false_omits_preview() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let content = "webcodex untracked preview body";
    fs::write(tmp.path().join("notes.txt"), content).unwrap();

    let output = show_changes_output_from_command(tmp.path(), false);

    assert_eq!(output["counts"]["untracked"], 1);
    assert!(output_has_file(&output, "notes.txt"));
    assert!(output.get("untracked_previews").is_none());
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(
        !serialized.contains(content),
        "include_diff=false leaked untracked file content: {serialized}"
    );
}

#[tokio::test]
async fn show_changes_untracked_text_include_diff_true_returns_bounded_preview() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("notes.txt"), "alpha\nbeta\n").unwrap();

    let output = show_changes_output_from_command(tmp.path(), true);

    assert_eq!(output["counts"]["untracked"], 1);
    assert!(output_has_file(&output, "notes.txt"));
    let preview = preview_for_path(&output, "notes.txt");
    assert_eq!(preview["kind"], "text");
    assert_eq!(preview["line_count"], 2);
    assert_eq!(preview["truncated"], false);
    assert_eq!(preview["lines"][0]["line"], 1);
    assert_eq!(preview["lines"][0]["text"], "alpha");
    assert_eq!(preview["lines"][1]["line"], 2);
    assert_eq!(preview["lines"][1]["text"], "beta");
    assert_eq!(output["hunk_count"], 0);
}

#[tokio::test]
async fn show_changes_untracked_large_file_preview_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("large.txt"), vec![b'x'; 8193]).unwrap();

    let output = show_changes_output_from_command(tmp.path(), true);

    assert_eq!(output["counts"]["untracked"], 1);
    let preview = preview_for_path(&output, "large.txt");
    assert_eq!(preview["kind"], "skipped");
    assert_eq!(preview["reason"], "too_large");
    assert_eq!(preview["byte_count"], 8193);
}

#[tokio::test]
async fn show_changes_untracked_binary_preview_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("binary.bin"), [0, 159, 146, 150]).unwrap();

    let output = show_changes_output_from_command(tmp.path(), true);

    assert_eq!(output["counts"]["untracked"], 1);
    let preview = preview_for_path(&output, "binary.bin");
    assert_eq!(preview["kind"], "skipped");
    assert_eq!(preview["reason"], "binary_or_non_utf8");
}

#[tokio::test]
async fn show_changes_untracked_sensitive_path_preview_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    fs::write(tmp.path().join("runner.toml"), "RUNNER_TOKEN=secret\n").unwrap();
    fs::write(tmp.path().join("agent.toml"), "API_TOKEN=secret\n").unwrap();

    let output = show_changes_output_from_command(tmp.path(), true);

    assert_eq!(output["counts"]["untracked"], 2);
    for path in ["runner.toml", "agent.toml"] {
        let preview = preview_for_path(&output, path);
        assert_eq!(preview["kind"], "skipped", "{path}");
        assert_eq!(preview["reason"], "sensitive_or_excluded_path", "{path}");
    }
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(!serialized.contains("RUNNER_TOKEN=secret"));
    assert!(
        !serialized.contains("API_TOKEN=secret"),
        "sensitive file content leaked: {serialized}"
    );
    assert_verdict_omits_raw_output_and_sensitive_values(
        &output["verdict"],
        &["RUNNER_TOKEN=secret", "API_TOKEN=secret"],
        "show_changes sensitive preview verdict",
    );
}

#[test]
fn git_diff_hunks_command_is_read_only_and_scoped_to_paths() {
    let command = git_diff_hunks_command(&["src/lib.rs".to_string()], false).unwrap();
    assert!(command.contains("git diff"));
    assert!(command.contains("--no-ext-diff"));
    assert!(command.contains("--no-textconv"));
    assert!(command.contains("--unified=80 -- 'src/lib.rs'"));
}

#[tokio::test]
async fn git_diff_hunks_rejects_unsafe_paths_before_project_dispatch() {
    let runtime = test_runtime();
    let result = runtime
        .git_diff_hunks(
            "agent:oe:webcodex".to_string(),
            Some(vec!["../outside".to_string()]),
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.error.unwrap().contains("parent traversal"));
}

#[tokio::test]
async fn show_changes_with_session_id_returns_session_block_and_records_call() {
    let runtime = runtime_with_agent_project("telemetry-show");
    let caps = RunnerCapabilities {
        file_read: true,
        shell: true,
        internal_posix_script: true,
        ..Default::default()
    };
    register_agent(&runtime, "telemetry-show", None, caps).await;
    let project = agent_test_project_id("telemetry-show");
    let session = runtime.sessions.start_session(Some(project.clone()), None);

    let read_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "README.md".to_string(),
                            start_line: None,
                            limit: Some(1),
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_instance(&runtime, "telemetry-show", "inst").await;
    complete_patch_agent_request(
        &runtime,
        "telemetry-show",
        &req.request_id,
        0,
        &canonical_agent_file_read_range("hello\n", 1, 1),
        "",
    )
    .await;
    let read = read_task.await.unwrap();
    assert!(read.success, "{:?}", read.error);

    let show_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ShowChanges {
                        project,
                        session_id: Some(session_id),
                        include_diff: Some(false),
                        max_hunks: None,
                        max_hunk_lines: None,
                        session_event_limit: Some(20),
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "telemetry-show").await;
    let stdout = format!(
        "{}{}{}",
        framed_block(
            'S',
            "## main\n M README.md\n",
            "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0\nfiles_total=1\nfiles_returned=1\nfiles_truncated=0\nfiles_limit=200\nmodified=1\nadded=0\ndeleted=0\nrenamed=0\ncopied=0\nuntracked=0\nconflicted=0\nstaged=0\nunstaged=1\nstatus_trunc_count=0\nstatus_trunc_bytes=0\nstatus_trunc_path=0\nstatus_bytes=20\n"
        ),
        framed_block(
            'H',
            "commit=abc123\nshort=abc123\nsummary=head\n",
            "head_exit=0\nhead_truncated=0\nhead_bytes=39\n"
        ),
        framed_block(
            'T',
            "README.md | 1 +\n",
            "diff_stat_exit=0\ndiff_stat_truncated=0\ndiff_stat_bytes=15\n"
        )
    );
    complete_patch_agent_request(&runtime, "telemetry-show", &req.request_id, 0, &stdout, "").await;
    let result = show_task.await.unwrap();

    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
    assert_eq!(result.output["session"]["found"], true);
    assert_eq!(result.output["session"]["counts"]["tool_calls"], 1);
    assert!(result.output["session"]["recent_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["tool_name"] == "read_files"));
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.tool_calls, 2);
    assert_eq!(summary.counts.change_summary_like, 1);
    let event = finished_event(&summary, "show_changes");
    assert!(event.git_like);
    assert!(event.change_summary_like);
}

#[tokio::test]
async fn show_changes_accepts_unique_short_id() {
    let runtime = runtime_with_resolver_projects().await;
    let bootstrap = auth_context(None, true);
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ShowChanges {
                        project: "other-repo".to_string(),
                        session_id: None,
                        include_diff: Some(false),
                        max_hunks: None,
                        max_hunk_lines: None,
                        session_event_limit: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let req = wait_for_runner_request_for_client(&runtime, "workstation").await;
    assert_eq!(req.cwd.as_deref(), Some("/root/git/workstation-other-repo"));
    let stdout = framed_clean_show_changes_test_stdout("head", false);
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "workstation".to_string(),
            runner_instance_id: "inst-workstation".to_string(),
            request_id: req.request_id,
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(String::new()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], "other-repo");
}

#[test]
fn git_read_commands_are_non_mutating_and_log_is_bounded() {
    assert_eq!(normalize_git_log_limit(None), 20);
    assert_eq!(normalize_git_log_limit(Some(0)), 20);
    assert_eq!(normalize_git_log_limit(Some(999)), 100);
    assert_eq!(normalize_git_log_skip(Some(20_000)), 10_000);

    let log = git_log_command(21, 7);
    assert!(log.contains("git log"));
    assert!(log.contains("-n 22"));
    assert!(log.contains("--skip 7"));

    for forbidden in [
        "apply", "commit", "checkout", "reset", "push", "stash", "merge", "rebase", "rm ",
    ] {
        assert!(
            !log.contains(forbidden),
            "git_log command must not contain {forbidden:?}: {log}"
        );
    }
}

#[test]
fn git_log_parser_splits_commits_refs_and_truncation() {
    let stdout = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\u{1f}aaaaaaa\u{1f}HEAD -> main, tag: v1\u{1f}Ada\u{1f}ada@example.com\u{1f}2026-06-30T00:00:00+00:00\u{1f}newest\u{1e}bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\u{1f}bbbbbbb\u{1f}\u{1f}Ben\u{1f}ben@example.com\u{1f}2026-06-29T00:00:00+00:00\u{1f}older\u{1e}";
    let (commits, truncated) = parse_git_log_commits(stdout, 1).unwrap();
    assert!(truncated);
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["short_hash"], "aaaaaaa");
    assert_eq!(commits[0]["subject"], "newest");
    assert_eq!(commits[0]["refs"], json!(["HEAD", "main", "v1"]));

    for marker in [
        "[output truncated]\n",
        "[...]\n",
        "[output truncated to last 262144 bytes]\n",
    ] {
        assert!(parse_git_log_commits(&format!("{marker}{stdout}"), 1).is_err());
    }
    assert!(parse_git_log_commits(stdout.trim_end_matches('\u{1e}'), 1).is_err());
    assert!(parse_git_log_commits("partial record\u{1e}", 1).is_err());
}

fn write_git_review_fixture_file(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full, content).unwrap();
}

fn commit_git_review_fixture(root: &Path, subject: &str) -> String {
    for cmd in [
        "git add -A".to_string(),
        format!("git commit -m {}", shell_escape_simple(subject)),
    ] {
        let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, root, 30);
        assert_eq!(
            exit_code, 0,
            "git review fixture command failed: {cmd}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    let (exit_code, stdout, stderr, _) = run_command_sync("git rev-parse HEAD", root, 30);
    assert_eq!(exit_code, 0, "rev-parse failed: {stderr}");
    let sha = stdout.trim().to_string();
    assert_eq!(sha.len(), 40);
    sha
}

async fn run_git_review_summary_via_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    base_commit: String,
    head_commit: String,
) -> ToolResult {
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .git_review_summary(project, base_commit, head_commit)
            .await
    });
    let timeout_secs = if cfg!(windows) { 60 } else { 10 };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while !task.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "git_review_summary did not finish within {timeout_secs} seconds for client {client_id}"
        );
        if let Some(request) = probe_patch_agent_request(runtime, client_id).await {
            assert_eq!(request.kind, "run_internal_posix_script");
            assert!(request.command.is_empty());
            let payload = request
                .script
                .as_ref()
                .expect("git_review_summary must use a typed internal script");
            assert_eq!(
                payload.language,
                crate::runner_protocol::ShellScriptLanguage::Sh
            );
            assert!(payload.args.is_empty());
            assert!(payload.script.contains("GIT_NO_REPLACE_OBJECTS=1"));
            assert!(payload.script.contains("GIT_NO_LAZY_FETCH=1"));
            assert!(payload.script.contains("GIT_ATTR_NOSYSTEM=1"));
            assert!(payload.script.contains("attributesFile = /dev/null"));
            assert!(payload.script.contains("GIT_CONFIG_GLOBAL=/dev/null"));
            for forbidden in [
                "git fetch",
                "git apply",
                "git commit",
                "git checkout",
                "git reset",
                "git push",
                "git stash",
                "git merge ",
                "git rebase",
                "git clean",
                "git add ",
            ] {
                assert!(
                    !payload.script.contains(forbidden),
                    "git_review_summary internal script must remain read-only; found {forbidden}: {}",
                    payload.script
                );
            }
            if payload.script.contains(" diff ") {
                assert!(payload.script.contains("--no-ext-diff"));
                assert!(payload.script.contains("--no-textconv"));
            }
            let (exit_code, stdout, stderr) = run_runner_shell_request_locally(&request);
            assert_eq!(
                exit_code, 0,
                "git_review_summary internal script failed\nscript:\n{}\nstdout:\n{}\nstderr:\n{}",
                payload.script, stdout, stderr
            );
            complete_patch_agent_request(
                runtime,
                client_id,
                &request.request_id,
                exit_code,
                &stdout,
                &stderr,
            )
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    task.await.unwrap()
}

#[tokio::test]
async fn git_review_summary_maps_exact_committed_range_without_raw_diff() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(tmp.path(), ".gitattributes", "*.rs diff=rust\n");
    write_git_review_fixture_file(
        tmp.path(),
        "src/auth/scopes.rs",
        "pub fn auth_scope() -> bool {\n    false\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/protocol.rs",
        "pub fn wire_version() -> u8 {\n    1\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/runtime/job.rs",
        "pub fn job_state() -> &'static str {\n    \"RAW_SECRET_BODY_MARKER_OLD\"\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/tokenizer.rs",
        "pub fn tokenizer_mode() -> u8 {\n    1\n}\n",
    );
    write_git_review_fixture_file(tmp.path(), "tests/auth.rs", "assert_eq!(1, 1);\n");
    write_git_review_fixture_file(tmp.path(), "docs/AUTH.md", "old auth docs\n");
    let base = commit_git_review_fixture(tmp.path(), "base");

    write_git_review_fixture_file(
        tmp.path(),
        "src/auth/scopes.rs",
        "pub fn auth_scope() -> bool {\n    true\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/protocol.rs",
        "pub fn wire_version() -> u8 {\n    2\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/runtime/job.rs",
        "pub fn job_state() -> &'static str {\n    \"RAW_SECRET_BODY_MARKER_NEW\"\n}\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        "src/tokenizer.rs",
        "pub fn tokenizer_mode() -> u8 {\n    2\n}\n",
    );
    write_git_review_fixture_file(tmp.path(), "tests/auth.rs", "assert_eq!(2, 2);\n");
    write_git_review_fixture_file(tmp.path(), "docs/AUTH.md", "new auth docs\n");
    let head = commit_git_review_fixture(tmp.path(), "head");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-summary", "repo", tmp.path()).await;
    let result = run_git_review_summary_via_agent(
        &runtime,
        "review-summary",
        project,
        base.clone(),
        head.clone(),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let output = &result.output;
    assert_eq!(output["scope"]["requested_base"], base);
    assert_eq!(output["scope"]["requested_head"], head);
    assert_eq!(
        output["scope"]["merge_base"],
        output["scope"]["requested_base"]
    );
    assert_eq!(output["scope"]["base_is_ancestor"], true);
    assert_eq!(output["scope"]["commit_count"], 1);
    assert_eq!(output["stats"]["files_changed"], 6);
    assert_eq!(output["stats"]["insertions"], 6);
    assert_eq!(output["stats"]["deletions"], 6);
    assert_eq!(output["stats"]["binary_files"], 0);
    assert_eq!(output["coverage"]["production_changed"], true);
    assert_eq!(output["coverage"]["tests_changed"], true);
    assert_eq!(output["coverage"]["docs_changed"], true);
    assert_eq!(output["deterministic"], true);
    assert_eq!(output["llm_summary"], false);
    assert_eq!(output["truncated"], false, "{output}");

    let signals = output["signals"].as_array().unwrap();
    let signal_names = signals
        .iter()
        .filter_map(|signal| signal["name"].as_str())
        .collect::<HashSet<_>>();
    for expected in [
        "auth_or_scope_surface_touched",
        "protocol_or_wire_schema_surface_touched",
        "execution_lifecycle_surface_touched",
    ] {
        assert!(
            signal_names.contains(expected),
            "missing {expected}: {signals:?}"
        );
    }
    assert!(!signal_names.contains("production_without_test_changes"));
    assert!(!signal_names.contains("contract_surface_without_doc_changes"));

    let files = output["files"].as_array().unwrap();
    assert_eq!(files.len(), 6);
    let tokenizer = files
        .iter()
        .find(|file| file["path"] == "src/tokenizer.rs")
        .unwrap();
    assert!(tokenizer["classes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|class| class == "production"));
    assert!(!tokenizer["classes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|class| class == "auth_security"));
    let auth = files
        .iter()
        .find(|file| file["path"] == "src/auth/scopes.rs")
        .unwrap();
    assert!(auth["classes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|class| class == "auth_security"));
    assert!(auth["symbols"].as_array().unwrap().len() <= GIT_REVIEW_MAX_SYMBOLS_PER_FILE);
    assert!(
        output["truncation"]["symbols_returned"].as_u64().unwrap()
            <= GIT_REVIEW_MAX_TOTAL_SYMBOLS as u64
    );

    let serialized = serde_json::to_string(output).unwrap();
    assert!(!serialized.contains("RAW_SECRET_BODY_MARKER_OLD"));
    assert!(!serialized.contains("RAW_SECRET_BODY_MARKER_NEW"));
    assert!(!serialized.contains("@@ -"));

    let audit = crate::tool_runtime::audit_safe_result_for_tool("git_review_summary", output);
    assert!(
        audit.get("files").is_none(),
        "audit must not persist paths/symbols: {audit}"
    );
    assert!(
        audit.get("signals").is_none(),
        "audit must not persist signal paths: {audit}"
    );
    assert_eq!(
        audit["scope"]["requested_base"],
        output["scope"]["requested_base"]
    );
    assert_eq!(audit["stats"], output["stats"]);
}

#[tokio::test]
async fn git_review_summary_exact_range_ignores_mutable_git_attributes_and_config() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "data.txt", "a\nb\n", "base");
    let base = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    commit_file(tmp.path(), "data.txt", "a\nc\n", "head");
    let head = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-attributes", "repo", tmp.path())
            .await;
    let before = run_git_review_summary_via_agent(
        &runtime,
        "review-attributes",
        project.clone(),
        base.clone(),
        head.clone(),
    )
    .await;
    assert!(before.success, "{:?}", before.error);
    assert_eq!(before.output["stats"]["files_changed"], 1);
    assert_eq!(before.output["stats"]["insertions"], 1);
    assert_eq!(before.output["stats"]["deletions"], 1);
    assert_eq!(before.output["stats"]["binary_files"], 0);

    fs::write(tmp.path().join(".gitattributes"), "data.txt -diff\n").unwrap();
    fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
    fs::write(tmp.path().join(".git/info/attributes"), "data.txt -diff\n").unwrap();
    let mutable_attributes = tmp.path().join("mutable.attributes");
    fs::write(&mutable_attributes, "data.txt -diff\n").unwrap();
    let config_command = format!(
        "git config core.attributesFile {}",
        shell_escape_simple(mutable_attributes.to_string_lossy().as_ref())
    );
    let (exit_code, _, stderr, _) = run_command_sync(&config_command, tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");

    let after =
        run_git_review_summary_via_agent(&runtime, "review-attributes", project, base, head).await;
    assert!(after.success, "{:?}", after.error);
    assert_eq!(
        after.output, before.output,
        "mutable worktree/info/config attributes must not change an exact committed review"
    );
}

#[tokio::test]
async fn git_review_summary_uses_reviewed_head_committed_attributes() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "data.txt", "a\nb\n", "base");
    let base = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    write_git_review_fixture_file(tmp.path(), ".gitattributes", "data.txt -diff\n");
    write_git_review_fixture_file(tmp.path(), "data.txt", "a\nc\n");
    let head = commit_git_review_fixture(tmp.path(), "head attributes");

    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "review-head-attributes",
        "repo",
        tmp.path(),
    )
    .await;
    let result =
        run_git_review_summary_via_agent(&runtime, "review-head-attributes", project, base, head)
            .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["stats"]["files_changed"], 2);
    assert_eq!(result.output["stats"]["binary_files"], 1);
    let data = result.output["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "data.txt")
        .unwrap();
    assert_eq!(data["binary"], true);
    assert_eq!(data["symbol_inspection"], "skipped_binary");
}

#[tokio::test]
async fn git_review_summary_rename_preserves_old_path_classification_and_privacy() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(
        tmp.path(),
        "src/auth/scopes.rs",
        "pub fn auth_scope() -> bool { true }\n",
    );
    write_git_review_fixture_file(
        tmp.path(),
        ".env",
        concat!(
            "pub fn credential_fixture() -> u8 {\n",
            "    let a = 1;\n",
            "    let b = 2;\n",
            "    let c = 3;\n",
            "    let d = 4;\n",
            "    let e = 5;\n",
            "    a + b + c + d + e + 1\n",
            "}\n",
        ),
    );
    let base = commit_git_review_fixture(tmp.path(), "rename base");

    fs::rename(
        tmp.path().join("src/auth/scopes.rs"),
        tmp.path().join("src/tokenizer.rs"),
    )
    .unwrap();
    fs::rename(tmp.path().join(".env"), tmp.path().join("src/config.rs")).unwrap();
    write_git_review_fixture_file(
        tmp.path(),
        "src/config.rs",
        concat!(
            "pub fn credential_fixture() -> u8 {\n",
            "    let a = 1;\n",
            "    let b = 2;\n",
            "    let c = 3;\n",
            "    let d = 4;\n",
            "    let e = 5;\n",
            "    a + b + c + d + e + 2\n",
            "}\n",
        ),
    );
    let head = commit_git_review_fixture(tmp.path(), "rename head");

    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "review-rename-boundaries",
        "repo",
        tmp.path(),
    )
    .await;
    let result =
        run_git_review_summary_via_agent(&runtime, "review-rename-boundaries", project, base, head)
            .await;
    assert!(result.success, "{:?}", result.error);
    let files = result.output["files"].as_array().unwrap();

    let tokenizer = files
        .iter()
        .find(|file| file["path"] == "src/tokenizer.rs")
        .unwrap();
    assert_eq!(tokenizer["status"], "renamed");
    assert_eq!(tokenizer["previous_path"], "src/auth/scopes.rs");
    assert!(tokenizer["classes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|class| class == "auth_security"));
    assert!(result.output["signals"]
        .as_array()
        .unwrap()
        .iter()
        .any(|signal| signal["name"] == "auth_or_scope_surface_touched"));

    let credential = files
        .iter()
        .find(|file| file["path"] == "src/config.rs")
        .unwrap();
    assert_eq!(credential["status"], "renamed");
    assert_eq!(credential["previous_path"], ".env");
    assert_eq!(
        credential["symbol_inspection"],
        "skipped_sensitive_or_excluded"
    );
    assert_eq!(credential["symbols"], json!([]));
}

#[tokio::test]
async fn git_review_summary_uses_merge_base_when_requested_base_is_not_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "base.txt", "root\n", "root");
    let root = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    let root_branch = {
        let (_, stdout, _, _) = run_command_sync("git branch --show-current", tmp.path(), 30);
        stdout.trim().to_string()
    };
    assert!(!root_branch.is_empty());
    let (exit_code, _, stderr, _) = run_command_sync("git checkout -b feature", tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "feature.txt", "feature\n", "feature");
    let head = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    let checkout_root = format!("git checkout {}", shell_escape_simple(&root_branch));
    let (exit_code, _, stderr, _) = run_command_sync(&checkout_root, tmp.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(tmp.path(), "main.txt", "main\n", "main");
    let requested_base = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-merge-base", "repo", tmp.path())
            .await;
    let result = run_git_review_summary_via_agent(
        &runtime,
        "review-merge-base",
        project,
        requested_base.clone(),
        head.clone(),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["scope"]["requested_base"], requested_base);
    assert_eq!(result.output["scope"]["requested_head"], head);
    assert_eq!(result.output["scope"]["merge_base"], root);
    assert_eq!(result.output["scope"]["base_is_ancestor"], false);
    assert_eq!(result.output["scope"]["commit_count"], 1);
    assert_eq!(result.output["stats"]["files_changed"], 1);
    assert_eq!(result.output["files"][0]["path"], "feature.txt");
}

#[tokio::test]
async fn git_review_summary_no_change_and_missing_object_are_structured() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "same\n", "root");
    let exact = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-empty", "repo", tmp.path()).await;
    let empty = run_git_review_summary_via_agent(
        &runtime,
        "review-empty",
        project.clone(),
        exact.clone(),
        exact.clone(),
    )
    .await;
    assert!(empty.success, "{:?}", empty.error);
    assert_eq!(empty.output["stats"]["files_changed"], 0);
    assert_eq!(empty.output["files"], json!([]));
    assert_eq!(empty.output["coverage"]["production_changed"], false);
    assert_eq!(empty.output["truncated"], false);

    let missing = "f".repeat(40);
    let failed =
        run_git_review_summary_via_agent(&runtime, "review-empty", project, exact, missing).await;
    assert!(!failed.success);
    assert_eq!(
        failed.output["reason_code"],
        "head_commit_missing_or_not_commit"
    );
}

#[tokio::test]
async fn git_review_summary_real_git_edges_cover_rename_delete_add_binary_and_utf8() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    write_git_review_fixture_file(
        tmp.path(),
        "old name.rs",
        "pub fn renamed() -> u8 {\n    1\n}\n",
    );
    write_git_review_fixture_file(tmp.path(), "deleted.rs", "pub fn deleted() {}\n");
    write_git_review_fixture_file(tmp.path(), "路径.rs", "pub fn utf8() -> u8 {\n    1\n}\n");
    fs::write(tmp.path().join("binary.bin"), [0u8, 1, 0, 2, 3]).unwrap();
    let base = commit_git_review_fixture(tmp.path(), "edge base");

    fs::rename(
        tmp.path().join("old name.rs"),
        tmp.path().join("new name.rs"),
    )
    .unwrap();
    fs::remove_file(tmp.path().join("deleted.rs")).unwrap();
    write_git_review_fixture_file(tmp.path(), "added.rs", "pub fn added() {}\n");
    write_git_review_fixture_file(tmp.path(), "路径.rs", "pub fn utf8() -> u8 {\n    2\n}\n");
    fs::write(tmp.path().join("binary.bin"), [0u8, 9, 0, 8, 7]).unwrap();
    let head = commit_git_review_fixture(tmp.path(), "edge head");

    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-edges", "repo", tmp.path()).await;
    let result =
        run_git_review_summary_via_agent(&runtime, "review-edges", project, base, head).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["stats"]["files_changed"], 5);
    assert_eq!(result.output["stats"]["binary_files"], 1);
    let files = result.output["files"].as_array().unwrap();
    let renamed = files
        .iter()
        .find(|file| file["path"] == "new name.rs")
        .unwrap();
    assert_eq!(renamed["status"], "renamed");
    assert_eq!(renamed["previous_path"], "old name.rs");
    let deleted = files
        .iter()
        .find(|file| file["path"] == "deleted.rs")
        .unwrap();
    assert_eq!(deleted["status"], "deleted");
    let added = files
        .iter()
        .find(|file| file["path"] == "added.rs")
        .unwrap();
    assert_eq!(added["status"], "added");
    assert!(files.iter().any(|file| file["path"] == "路径.rs"));
    let binary = files
        .iter()
        .find(|file| file["path"] == "binary.bin")
        .unwrap();
    assert_eq!(binary["binary"], true);
    assert_eq!(binary["symbol_inspection"], "skipped_binary");
    assert_eq!(binary["symbols"], json!([]));
}

#[tokio::test]
async fn git_review_summary_large_file_count_reports_partial_coverage_truthfully() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "base\n", "base");
    let base = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", tmp.path(), 30);
        stdout.trim().to_string()
    };
    for index in 0..(GIT_REVIEW_MAX_FILES + 2) {
        write_git_review_fixture_file(
            tmp.path(),
            &format!("src/generated/file_{index:03}.rs"),
            &format!("pub fn generated_{index:03}() -> usize {{ {index} }}\n"),
        );
    }
    let head = commit_git_review_fixture(tmp.path(), "many files");
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-many", "repo", tmp.path()).await;
    let result =
        run_git_review_summary_via_agent(&runtime, "review-many", project, base, head).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.output["stats"]["files_changed"],
        (GIT_REVIEW_MAX_FILES + 2) as u64
    );
    assert_eq!(
        result.output["truncation"]["files_returned"],
        GIT_REVIEW_MAX_FILES as u64
    );
    assert_eq!(result.output["truncation"]["files_truncated"], true);
    assert_eq!(result.output["coverage"]["production_changed"], true);
    assert!(result.output["coverage"]["tests_changed"].is_null());
    assert!(result.output["coverage"]["docs_changed"].is_null());
    assert_eq!(result.output["coverage"]["partial"], true);
    assert_eq!(result.output["truncated"], true);
}

#[tokio::test]
async fn git_review_summary_large_diff_saturates_symbol_probe_without_source_output() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let old_body = format!(
        "pub fn huge() -> &'static str {{\n    \"{}\"\n}}\n",
        "A".repeat(GIT_REVIEW_MAX_DIFF_BYTES + 4096)
    );
    write_git_review_fixture_file(tmp.path(), "src/runtime/huge.rs", &old_body);
    let base = commit_git_review_fixture(tmp.path(), "huge base");
    let new_body = format!(
        "pub fn huge() -> &'static str {{\n    \"{}\"\n}}\n",
        "B".repeat(GIT_REVIEW_MAX_DIFF_BYTES + 4096)
    );
    write_git_review_fixture_file(tmp.path(), "src/runtime/huge.rs", &new_body);
    let head = commit_git_review_fixture(tmp.path(), "huge head");
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-huge", "repo", tmp.path()).await;
    let result =
        run_git_review_summary_via_agent(&runtime, "review-huge", project, base, head).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["truncation"]["symbols_partial"], true);
    assert_eq!(result.output["truncated"], true);
    assert!(
        result.output["truncation"]["diff_bytes_inspected"]
            .as_u64()
            .unwrap()
            <= GIT_REVIEW_MAX_DIFF_BYTES as u64
    );
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(!serialized.contains(&"A".repeat(256)));
    assert!(!serialized.contains(&"B".repeat(256)));
}

#[tokio::test]
async fn git_review_summary_structures_non_git_no_merge_base_and_unborn_head() {
    let non_git = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-non-git", "repo", non_git.path())
            .await;
    let failed = run_git_review_summary_via_agent(
        &runtime,
        "review-non-git",
        project,
        "a".repeat(40),
        "b".repeat(40),
    )
    .await;
    assert!(!failed.success);
    assert_eq!(failed.output["reason_code"], "not_a_git_repository");

    let disconnected = tempfile::tempdir().unwrap();
    init_git_repo(disconnected.path());
    commit_file(disconnected.path(), "one.txt", "one\n", "one");
    let first = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", disconnected.path(), 30);
        stdout.trim().to_string()
    };
    let (exit_code, _, stderr, _) = run_command_sync(
        "git checkout --orphan disconnected",
        disconnected.path(),
        30,
    );
    assert_eq!(exit_code, 0, "{stderr}");
    let (exit_code, _, stderr, _) = run_command_sync("git rm -rf .", disconnected.path(), 30);
    assert_eq!(exit_code, 0, "{stderr}");
    commit_file(disconnected.path(), "two.txt", "two\n", "two");
    let second = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", disconnected.path(), 30);
        stdout.trim().to_string()
    };
    let runtime = test_runtime();
    let project = register_structured_git_agent_at_path(
        &runtime,
        "review-disconnected",
        "repo",
        disconnected.path(),
    )
    .await;
    let failed =
        run_git_review_summary_via_agent(&runtime, "review-disconnected", project, first, second)
            .await;
    assert!(!failed.success);
    assert_eq!(failed.output["reason_code"], "no_merge_base");

    let unborn = tempfile::tempdir().unwrap();
    init_git_repo(unborn.path());
    commit_file(unborn.path(), "kept.txt", "kept\n", "kept");
    let exact = {
        let (_, stdout, _, _) = run_command_sync("git rev-parse HEAD", unborn.path(), 30);
        stdout.trim().to_string()
    };
    let (exit_code, _, stderr, _) = run_command_sync(
        "git symbolic-ref HEAD refs/heads/unborn-review",
        unborn.path(),
        30,
    );
    assert_eq!(exit_code, 0, "{stderr}");
    let runtime = test_runtime();
    let project =
        register_structured_git_agent_at_path(&runtime, "review-unborn", "repo", unborn.path())
            .await;
    let result = run_git_review_summary_via_agent(
        &runtime,
        "review-unborn",
        project,
        exact.clone(),
        exact.clone(),
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["scope"]["requested_head"], exact);
    assert_eq!(result.output["stats"]["files_changed"], 0);
}

#[test]
fn git_review_summary_tool_schema_metadata_and_oauth_are_read_only() {
    use crate::auth::scopes::{oauth_scope_policy_for_runtime_tool, OAuthToolScopePolicy};
    use crate::auth::SCOPE_PROJECT_READ;
    use crate::tool_runtime::metadata::{lookup_tool_metadata, ToolRisk};
    let base = "a".repeat(40);
    let head = "b".repeat(40);
    let call = ToolCall::from_tool_name(
        "git_review_summary",
        json!({
            "project": SAMPLE_PROJECT,
            "base_commit": base,
            "head_commit": head,
            "session_id": "wc_sess_review"
        }),
    )
    .expect("git_review_summary should parse through generic ToolCall");
    match call {
        ToolCall::GitReviewSummary {
            project,
            base_commit,
            head_commit,
            session_id,
        } => {
            assert_eq!(project, SAMPLE_PROJECT);
            assert_eq!(base_commit, "a".repeat(40));
            assert_eq!(head_commit, "b".repeat(40));
            assert_eq!(session_id.as_deref(), Some("wc_sess_review"));
        }
        other => panic!("expected git_review_summary, got {other:?}"),
    }
    let request_audit = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "git_review_summary",
        &json!({
            "project": SAMPLE_PROJECT,
            "base_commit": "a".repeat(40),
            "head_commit": "b".repeat(40),
            "source_body": "must-not-persist"
        }),
    );
    assert_eq!(request_audit["project"], SAMPLE_PROJECT);
    assert_eq!(request_audit["base_commit"], "a".repeat(40));
    assert_eq!(request_audit["head_commit"], "b".repeat(40));
    assert!(request_audit.get("source_body").is_none());
    assert_eq!(request_audit["base_commit_valid"], true);
    assert_eq!(request_audit["head_commit_valid"], true);
    let malformed = "source-like-invalid-value".repeat(1024);
    let malformed_audit = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "git_review_summary",
        &json!({
            "project": SAMPLE_PROJECT,
            "base_commit": malformed,
            "head_commit": "b".repeat(40)
        }),
    );
    assert_eq!(malformed_audit["base_commit_valid"], false);
    assert!(malformed_audit.get("base_commit").is_none());
    assert_eq!(malformed_audit["head_commit_valid"], true);
    let audit_text = serde_json::to_string(&malformed_audit).unwrap();
    assert!(!audit_text.contains("source-like-invalid-value"));

    let typed_malformed = ToolCall::GitReviewSummary {
        project: SAMPLE_PROJECT.to_string(),
        base_commit: "source-like-invalid-value".repeat(1024),
        head_commit: "b".repeat(40),
        session_id: None,
    }
    .session_log_arguments();
    assert_eq!(typed_malformed["base_commit_valid"], false);
    assert!(typed_malformed["base_commit"].is_null());
    assert_eq!(typed_malformed["head_commit_valid"], true);
    assert!(!serde_json::to_string(&typed_malformed)
        .unwrap()
        .contains("source-like-invalid-value"));
    let definition =
        crate::tool_runtime::tool_definition::lookup_tool_definition("git_review_summary")
            .expect("git_review_summary definition");
    assert_eq!(
        definition.metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require(SCOPE_PROJECT_READ)
    );
    assert!(definition.visibility.is_model_visible());
    let metadata = lookup_tool_metadata("git_review_summary").expect("git_review_summary metadata");
    assert_eq!(metadata.risk, ToolRisk::Read);
    assert_eq!(
        metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require(SCOPE_PROJECT_READ)
    );
    assert!(metadata.requires_project);
    assert_eq!(
        metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert!(!metadata.destructive);
    assert_eq!(
        oauth_scope_policy_for_runtime_tool("git_review_summary"),
        OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ)
    );
    let specs = crate::tool_runtime::registered_tool_specs();
    let spec = specs
        .iter()
        .find(|spec| spec.name == "git_review_summary")
        .expect("git_review_summary public spec");
    assert!(spec
        .description
        .contains("Deterministic bounded committed-range review map"));
    for field in ["base_commit", "head_commit"] {
        assert_eq!(spec.input_schema["properties"][field]["minLength"], 40);
        assert_eq!(spec.input_schema["properties"][field]["maxLength"], 40);
        assert_eq!(
            spec.input_schema["properties"][field]["pattern"],
            "^[0-9A-Fa-f]{40}$"
        );
    }
    let output = crate::tool_runtime::registry::output_schema_for_tool("git_review_summary");
    let payload = &output["properties"]["output"];
    for field in [
        "scope",
        "stats",
        "file_classes",
        "subsystems",
        "signals",
        "files",
        "coverage",
        "bounds",
        "truncation",
        "deterministic",
        "llm_summary",
        "truncated",
        "warnings",
    ] {
        assert!(
            payload["properties"].get(field).is_some(),
            "missing output field {field}"
        );
    }
}

#[tokio::test]
async fn git_or_shell_tools_rejected_without_git_or_shell_capability() {
    let runtime = runtime_with_agent_project("oe");
    register_agent(
        &runtime,
        "oe",
        None,
        RunnerCapabilities {
            shell: false,
            ..Default::default()
        },
    )
    .await;
    let bootstrap = auth_context(None, true);

    let calls = [
        ToolCall::GitReviewSummary {
            project: agent_test_project_id("oe"),
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            session_id: None,
        },
        ToolCall::ShowChanges {
            project: agent_test_project_id("oe"),
            session_id: None,
            include_diff: None,
            max_hunks: None,
            max_hunk_lines: None,
            session_event_limit: None,
        },
    ];
    for call in calls {
        let name = format!("{:?}", call);
        let result = runtime.dispatch_with_auth(call, Some(&bootstrap)).await;
        assert!(!result.success, "{name} should be rejected");
        let err = result.error.unwrap();
        assert!(
            err.contains("shell") || err.contains("git"),
            "{name} should require shell or git capability: {err}",
        );
    }
}

#[test]
fn show_changes_status_observation_distinguishes_failure_classes() {
    let observed = parse_show_changes_status_observation(
        "## main",
        "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0",
        "",
    );
    assert_eq!(observed.as_json()["status"], "observed");

    let non_git = parse_show_changes_status_observation(
        "",
        "status_exit=128\nrepository_probe=outside_worktree\nrepository_probe_exit=128",
        "fatal: not a git repository",
    );
    assert_eq!(non_git.as_json()["status"], "non_git");
    assert_eq!(non_git.as_json()["reason_code"], "not_a_git_repository");

    let config_failure = parse_show_changes_status_observation(
        "",
        "status_exit=128\nrepository_probe=inside_worktree\nrepository_probe_exit=0",
        "fatal: bad config variable 'status.showuntrackedfiles'",
    );
    assert_eq!(config_failure.as_json()["status"], "command_failed");
    assert_eq!(
        config_failure.as_json()["reason_code"],
        "git_status_config_error"
    );
    assert_eq!(
        config_failure.as_json()["repository_probe"],
        "inside_worktree"
    );

    let permission_failure = parse_show_changes_status_observation(
        "",
        "status_exit=128\nrepository_probe=unavailable\nrepository_probe_exit=128",
        "fatal: Permission denied",
    );
    assert_eq!(permission_failure.as_json()["status"], "command_failed");
    assert_eq!(
        permission_failure.as_json()["reason_code"],
        "git_status_permission_denied"
    );

    let unavailable = parse_show_changes_status_observation(
        "",
        "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0",
        "",
    );
    assert_eq!(unavailable.as_json()["status"], "output_unavailable");
    assert_eq!(
        unavailable.as_json()["reason_code"],
        "git_status_header_unavailable"
    );
}

async fn run_show_changes_via_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    session_id: Option<String>,
    include_diff: bool,
) -> ToolResult {
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .show_changes(project, session_id, Some(include_diff), None, None, None)
            .await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "show_changes did not finish within 10 seconds for client {client_id}"
        );
        if let Some(req) = probe_patch_agent_request(runtime, client_id).await {
            assert_eq!(req.kind, "run_internal_posix_script");
            assert!(req.command.is_empty());
            let payload = req
                .script
                .as_ref()
                .expect("show_changes must carry a typed internal script");
            assert_eq!(
                payload.language,
                crate::runner_protocol::ShellScriptLanguage::Sh
            );
            assert!(payload.args.is_empty());
            complete_agent_request_by_running_locally(runtime, client_id, req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    task.await.unwrap()
}

fn framed_block(kind: char, body: &str, metadata: &str) -> String {
    crate::tool_runtime::git::framed_show_changes_test_block(kind, body, metadata)
}

#[test]
fn show_changes_modern_framing_requires_exact_blocks_and_tail() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "before\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "after\n").unwrap();

    for include_diff in [false, true] {
        let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), include_diff, 20, 80);
        assert_eq!(
            stdout.matches("WCSF1:").count(),
            if include_diff { 4 } else { 3 }
        );
        let frames = split_show_changes_stdout(&stdout, include_diff);
        assert!(frames.framing_valid);
        let output = bounded_show_changes_output_from_frames(
            &frames,
            tmp.path(),
            include_diff,
            20,
            80,
            &stderr,
        );
        assert_eq!(output["transport_safe"], true, "{output}");
    }

    let (_, valid, stderr) = run_bounded_show_changes_full(tmp.path(), false, 20, 80);
    let trailer = valid.rfind("WCSF1:T:").unwrap();
    let mut variants = Vec::new();
    variants.push(("extra_tail", format!("{valid}x")));
    variants.push(("missing_tail", valid[..valid.len() - 1].to_string()));
    let mut invalid_header = valid.clone().into_bytes();
    invalid_header[trailer] = b'X';
    variants.push(("invalid_header", String::from_utf8(invalid_header).unwrap()));
    let mut length_mismatch = valid.clone().into_bytes();
    length_mismatch[trailer + 8] = if length_mismatch[trailer + 8] == b'9' {
        b'8'
    } else {
        b'9'
    };
    variants.push((
        "length_mismatch",
        String::from_utf8(length_mismatch).unwrap(),
    ));
    let mut body_early_end = valid.into_bytes();
    body_early_end[trailer + 8..trailer + 18].copy_from_slice(b"9999999999");
    variants.push(("body_early_end", String::from_utf8(body_early_end).unwrap()));

    for (label, malformed) in variants {
        let frames = split_show_changes_stdout(&malformed, false);
        assert!(!frames.framing_valid, "{label}");
        let output =
            bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
        assert_eq!(output["transport_safe"], false, "{label}: {output}");
    }

    let synthetic = format!(
        "{}{}{}",
        framed_block('S', "## main\n", "status_exit=0\n"),
        framed_block('H', "", "head_exit=1\n"),
        framed_block('T', "", "diff_stat_exit=0\n")
    );
    assert!(split_show_changes_stdout(&synthetic, false).framing_valid);

    let legacy = "## main\n@@WEBCODEX_SHOW_CHANGES_SEP@@\nabc123\0abc123\0head\n@@WEBCODEX_SHOW_CHANGES_SEP@@\n";
    let legacy_frames = split_show_changes_stdout(legacy, false);
    assert!(!legacy_frames.framing_valid);
    assert!(legacy_frames.status.is_empty());
    assert!(legacy_frames.head.is_empty());
    let legacy_head = parse_show_changes_output(
        "demo",
        "## main",
        "abc123\0abc123\0head",
        "",
        None,
        20,
        80,
        Some(0),
        "",
    );
    assert!(legacy_head["head"]["commit"].is_null());
    assert!(legacy_head["head"]["short"].is_null());
    assert!(legacy_head["head"]["summary"].is_null());
}

#[tokio::test]
async fn show_changes_preserves_sentinel_text_in_normal_diff_and_tool_result() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "collision.txt", "before\n", "initial");
    std::fs::write(
        tmp.path().join("collision.txt"),
        format!("before\n{SHOW_CHANGES_SENTINEL}\nprefix{SHOW_CHANGES_SENTINEL}suffix\n"),
    )
    .unwrap();

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "show-collision", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "show-collision", project, None, true).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["diff_exit"], 0);
    assert_eq!(result.output["transport_safe"], true);
    let serialized = result.output.to_string();
    assert!(serialized.contains(&format!("+{SHOW_CHANGES_SENTINEL}")));
    assert!(serialized.contains(&format!("+prefix{SHOW_CHANGES_SENTINEL}suffix")));
    assert_show_changes_envelope_matches_schema("sentinel normal diff", &result);
}

#[tokio::test]
async fn show_changes_agent_untracked_preview_uses_internal_posix_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "tracked\n", "initial");
    std::fs::write(tmp.path().join("notes.txt"), "alpha\nbeta\n").unwrap();

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "show-untracked", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "show-untracked", project, None, true).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["counts"]["untracked"], 1);
    let preview = preview_for_path(&result.output, "notes.txt");
    assert_eq!(preview["kind"], "text");
    assert_eq!(preview["lines"][0]["text"], "alpha");
    assert_eq!(preview["lines"][1]["text"], "beta");
}

#[cfg(unix)]
#[test]
fn show_changes_preserves_sentinel_text_from_external_diff() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "external.txt", "before\n", "initial");
    std::fs::write(tmp.path().join("external.txt"), "after\n").unwrap();
    let script = tmp.path().join("external-diff.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' '{SHOW_CHANGES_SENTINEL}'\nexit 0\n"),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    let config = format!(
        "git config diff.external {}",
        shell_single_quote(script.to_str().unwrap())
    );
    let (exit, _, stderr) = run_command_full_capture(&config, tmp.path(), 30);
    assert_eq!(exit, 0, "{stderr}");

    let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    let frames = split_show_changes_stdout(&stdout, true);
    assert!(frames.framing_valid);
    assert_eq!(frames.diff_exit, Some(0));
    assert_eq!(frames.diff, SHOW_CHANGES_SENTINEL);
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(output["transport_safe"], true, "{output}");
}

#[test]
fn show_changes_preserves_sentinel_text_in_head_subject() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let subject = format!("subject {SHOW_CHANGES_SENTINEL} suffix");
    commit_file(tmp.path(), "README.md", "hello\n", &subject);
    let (commit_exit, expected_commit, commit_stderr) =
        run_command_full_capture("git rev-parse HEAD", tmp.path(), 30);
    assert_eq!(commit_exit, 0, "{commit_stderr}");
    let (short_exit, expected_short, short_stderr) =
        run_command_full_capture("git rev-parse --short HEAD", tmp.path(), 30);
    assert_eq!(short_exit, 0, "{short_stderr}");
    let expected_commit = expected_commit.trim();
    let expected_short = expected_short.trim();

    let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), false, 20, 80);
    let frames = split_show_changes_stdout(&stdout, false);
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    assert_eq!(output["head"]["commit"], expected_commit);
    assert_eq!(output["head"]["short"], expected_short);
    assert_eq!(output["head"]["summary"], subject);
    assert_eq!(output["transport_safe"], true, "{output}");
}

#[test]
fn show_changes_preserves_sentinel_text_in_tracked_filename() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let filename = format!("tracked-{SHOW_CHANGES_SENTINEL}-file.txt");
    commit_file(tmp.path(), &filename, "before\n", "initial");
    std::fs::write(tmp.path().join(&filename), "after\n").unwrap();

    let (_, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    let frames = split_show_changes_stdout(&stdout, true);
    assert!(frames.status.contains(&filename));
    assert!(frames.stat.contains(&filename));
    assert!(frames.diff.contains(&filename));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(output["transport_safe"], true, "{output}");
    assert!(output["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == filename));
    assert_show_changes_envelope_value_matches_schema(&output, "sentinel filename");
}

fn assert_show_changes_envelope_matches_schema(label: &str, result: &ToolResult) {
    let schema = crate::tool_runtime::registry::output_schema_for_tool("show_changes");
    let envelope = json!({
        "success": result.success,
        "output": result.output,
        "error": result.error,
    });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&envelope, &schema)
        .unwrap_or_else(|error| {
            panic!("{label} show_changes schema mismatch: {error}\n{envelope}")
        });
}

/// Validate a bare `show_changes` output `Value` against the output schema by
/// wrapping it in the success envelope.
fn assert_show_changes_envelope_value_matches_schema(output: &Value, label: &str) {
    let schema = crate::tool_runtime::registry::output_schema_for_tool("show_changes");
    let envelope = json!({
        "success": true,
        "output": output,
        "error": Value::Null,
    });
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&envelope, &schema)
        .unwrap_or_else(|error| {
            panic!("{label} show_changes schema mismatch: {error}\n{envelope}")
        });
}

/// Run the production-bounded `show_changes` command in a local repo and
/// parse it into the structured `show_changes` output value, the same way the
/// runtime does (without the separate untracked-preview collection).
fn bounded_show_changes_output(
    root: &Path,
    include_diff: bool,
    max_hunks: usize,
    max_hunk_lines: usize,
) -> Value {
    let cmd = show_changes_command(include_diff, max_hunks, max_hunk_lines);
    let (exit_code, stdout, stderr, _) = run_command_sync(&cmd, root, 30);
    let frames = split_show_changes_stdout(&stdout, include_diff);
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, &stderr);
    let effective_exit = if observation.exit_code != Some(0) {
        observation.exit_code
    } else {
        Some(exit_code)
    };
    parse_show_changes_output_with_observation(
        "demo",
        &frames.status,
        &frames.head,
        &frames.stat,
        include_diff.then_some(frames.diff.as_str()),
        max_hunks,
        max_hunk_lines,
        effective_exit,
        &stderr,
        observation,
        &frames,
    )
}

/// Mirror the ordinary Runner per-stream result-retention tail behavior used by
/// the current 256 KiB compatibility floor. This is intentionally not a model
/// result ceiling or a polling/WebSocket/QUIC wire/body/frame bound.
fn simulate_runner_result_retention_tail(stdout: &str, max_bytes: usize) -> String {
    if stdout.len() <= max_bytes {
        return stdout.to_string();
    }
    let mut start = stdout.len() - max_bytes;
    while start < stdout.len() && !stdout.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[output truncated to last {} bytes]\n{}",
        max_bytes,
        &stdout[start..]
    )
}

/// Run a shell command and fully capture its stdout/stderr without the pipe
/// deadlock that affects `run_command_sync` for outputs above the OS pipe
/// buffer (~64 KiB). The stdout/stderr pipes are drained on dedicated threads
/// so a large-output git command can write freely while the main thread polls
/// the child status. Used by large-output regression tests where the command
/// legitimately exceeds the pipe buffer.
fn run_command_full_capture(cmd: &str, cwd: &Path, timeout_secs: u64) -> (i32, String, String) {
    use std::io::Read;
    #[cfg(windows)]
    use std::io::Write;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let mut command = Command::new(crate::tool_runtime::helpers::test_shell());
    #[cfg(windows)]
    command.arg("-s").stdin(std::process::Stdio::piped());
    #[cfg(not(windows))]
    command.arg("-c").arg(cmd);
    command
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(error) => return (-1, String::new(), format!("failed to spawn: {error}")),
    };
    #[cfg(windows)]
    {
        let write_result = child
            .stdin
            .take()
            .expect("test shell stdin")
            .write_all(cmd.as_bytes());
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return (
                -1,
                String::new(),
                format!("failed to write shell command: {error}"),
            );
        }
    }
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    // Drain each pipe on its own thread so the child never blocks on a full
    // pipe while we wait for it to exit.
    let (tx_out, rx_out) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    let (tx_err, rx_err) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.read_to_end(&mut buf).map(|_| buf);
        let _ = tx_out.send(res);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stderr.read_to_end(&mut buf).map(|_| buf);
        let _ = tx_err.send(res);
    });
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    -1,
                    String::new(),
                    format!("Command timed out after {timeout_secs} seconds"),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return (-1, String::new(), "failed to wait".to_string()),
        }
    };
    let stdout = rx_out
        .recv()
        .map(|r| r.unwrap_or_default())
        .unwrap_or_default();
    let stderr = rx_err
        .recv()
        .map(|r| r.unwrap_or_default())
        .unwrap_or_default();
    (
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

#[tokio::test]
async fn show_changes_degrades_gracefully_for_non_git_project() {
    let tmp = tempfile::tempdir().unwrap();
    // Intentionally do NOT init a git repo.
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "ng", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "ng", project, None, false).await;
    assert!(
        result.success,
        "non-git project must not be a runtime failure: {:?}",
        result.error
    );
    assert_eq!(result.output["non_git_project"], true);
    assert_eq!(result.output["git_available"], false);
    let git_error = result.output["git_error"].as_str().unwrap_or_default();
    assert!(
        git_error.contains("not a git repository"),
        "unexpected git_error: {git_error}"
    );
    // No full git usage/fatal stderr must leak into the user-facing payload.
    assert_eq!(result.output["stderr"], "");
    let serialized = serde_json::to_string(&result.output).unwrap();
    assert!(
        !serialized.contains("--no-index"),
        "leaked git diff usage: {serialized}"
    );
    assert!(
        !serialized.contains("usage") && !serialized.contains("用法"),
        "leaked git usage: {serialized}"
    );
    assert!(result.output["files"].as_array().unwrap().is_empty());
    assert!(result.output["session"].is_null());
    assert_review_verdict_shape(&result.output["verdict"]);
    assert_eq!(result.output["verdict"]["status"], "warn");
    assert_eq!(result.output["verdict"]["blocking"], false);
    assert_reason_list_contains(
        &result.output["verdict"],
        "warning_reasons",
        "git_unavailable",
    );
    let actions = result.output["suggested_next_actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|a| a.as_str().unwrap().contains("unavailable")));
    assert_eq!(result.output["status_observation"]["status"], "non_git");
    assert_eq!(
        result.output["head"],
        json!({
            "commit": null,
            "short": null,
            "summary": null,
        })
    );
    assert_show_changes_envelope_matches_schema("non-git", &result);
}

#[tokio::test]
async fn show_changes_non_git_project_still_returns_session_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "ngs", "demo", tmp.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("task".to_string()));
    let args = json!({"project": project, "path": "src/foo.rs"});
    let start = runtime.sessions.record_tool_call_started(
        Some(&session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Api,
        "write_project_file",
        &args,
        crate::tool_runtime::sessions::session_tool_contract("write_project_file"),
    );
    runtime
        .sessions
        .record_tool_call_finished(start, true, &json!({}), None, None);

    let result = run_show_changes_via_agent(
        &runtime,
        "ngs",
        project,
        Some(session.session_id.clone()),
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["non_git_project"], true);
    assert_eq!(result.output["git_available"], false);
    assert_eq!(result.output["session"]["found"], true);
    assert_eq!(result.output["session"]["session_id"], session.session_id);
    assert!(result.output["session"].get("recent_events").is_none());
    assert_eq!(result.output["session"]["signals"]["write_like"], true);
    assert_eq!(
        result.output["session"]["changed_paths"],
        json!(["src/foo.rs"])
    );
    // Session-signal suggestions are layered on top of the git-unavailable hint.
    let actions = result.output["suggested_next_actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|a| a.as_str().unwrap().contains("unavailable")));
    assert!(actions
        .iter()
        .any(|a| a.as_str().unwrap().contains("review changed paths")));
}

#[tokio::test]
async fn show_changes_real_git_repo_marks_git_available_and_reports_status() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "gr", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "gr", project, None, false).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["non_git_project"], false);
    assert_eq!(result.output["git_available"], true);
    assert_eq!(result.output["git_error"], serde_json::Value::Null);
    assert_eq!(result.output["clean"], true);
    assert_review_verdict_shape(&result.output["verdict"]);
    assert_ne!(result.output["verdict"]["status"], "fail");
    assert_eq!(result.output["verdict"]["blocking"], false);
    assert!(result.output["branch"].as_str().is_some());
    assert!(result.output["head"]["short"].as_str().is_some());
    assert_eq!(result.output["counts"]["modified"], 0);
    assert!(result.output["files"].as_array().unwrap().is_empty());
    // No git-unavailable suggestion for a real repo.
    let actions = result.output["suggested_next_actions"].as_array().unwrap();
    assert!(!actions
        .iter()
        .any(|a| a.as_str().unwrap().contains("unavailable")));
    assert_eq!(result.output["status_observation"]["status"], "observed");
    assert_show_changes_envelope_matches_schema("git include_diff=false", &result);
}

#[tokio::test]
async fn show_changes_real_git_repo_include_diff_true_matches_schema() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "grd", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "grd", project, None, true).await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["status_observation"]["status"], "observed");
    assert_eq!(result.output["clean"], false);
    assert!(result.output["hunk_count"].as_u64().unwrap_or(0) > 0);
    assert_show_changes_envelope_matches_schema("git include_diff=true", &result);
}

#[tokio::test]
async fn show_changes_status_failure_is_not_masked_by_successful_diff() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let (config_exit, _, config_stderr, _) = run_command_sync(
        "git config status.showUntrackedFiles invalid",
        tmp.path(),
        30,
    );
    assert_eq!(
        config_exit, 0,
        "failed to set regression config: {config_stderr}"
    );

    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "gsf", "demo", tmp.path()).await;
    let result = run_show_changes_via_agent(&runtime, "gsf", project, None, false).await;
    assert!(!result.success, "status failure must fail show_changes");
    assert_eq!(
        result.output["status_observation"]["status"],
        "command_failed"
    );
    assert_eq!(
        result.output["status_observation"]["reason_code"],
        "git_status_config_error"
    );
    assert_eq!(
        result.output["status_observation"]["repository_probe"],
        "inside_worktree"
    );
    assert_eq!(result.output["git_available"], true);
    assert_eq!(result.output["non_git_project"], false);
    assert_eq!(result.output["clean"], Value::Null);
    assert_eq!(result.output["counts"]["conflicted"], Value::Null);
    assert!(
        result.output["diff_stat"]
            .as_str()
            .unwrap_or_default()
            .contains("README.md"),
        "successful diff output should remain available: {}",
        result.output
    );
    assert!(result.output["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["reason_code"] == "git_status_config_error"));
    assert_show_changes_envelope_matches_schema("status command failure", &result);
}

#[test]
fn show_changes_parses_upstream_observation_states() {
    let parse = |status: &str| {
        parse_show_changes_output(
            "agent:oe:webcodex",
            status,
            "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=head",
            "",
            None,
            20,
            80,
            Some(0),
            "",
        )
    };

    let synced = parse("## main...origin/main");
    assert_eq!(synced["upstream_status"], "available");
    assert_eq!(synced["upstream_reason_code"], Value::Null);
    assert_eq!(synced["upstream"], "origin/main");
    assert_eq!(synced["ahead"], 0);
    assert_eq!(synced["behind"], 0);

    let diverged = parse("## main...origin/main [ahead 3, behind 2]");
    assert_eq!(diverged["upstream_status"], "available");
    assert_eq!(diverged["ahead"], 3);
    assert_eq!(diverged["behind"], 2);

    let gone = parse("## main...origin/main [gone]");
    assert_eq!(gone["upstream_status"], "gone");
    assert_eq!(gone["upstream_reason_code"], "upstream_gone");
    assert_eq!(gone["upstream"], "origin/main");
    assert_eq!(gone["ahead"], Value::Null);
    assert_eq!(gone["behind"], Value::Null);

    let absent = parse("## main");
    assert_eq!(absent["upstream_status"], "absent");
    assert_eq!(absent["upstream_reason_code"], Value::Null);
    assert_eq!(absent["upstream"], Value::Null);
    assert_eq!(absent["ahead"], Value::Null);
    assert_eq!(absent["behind"], Value::Null);
}

#[test]
fn show_changes_parses_unborn_and_detached_branch_headers() {
    for status in ["## No commits yet on main", "## Initial commit on main"] {
        let output = parse_show_changes_output(
            "agent:oe:webcodex",
            status,
            "",
            "",
            None,
            20,
            80,
            Some(0),
            "",
        );
        assert_eq!(output["branch"], "main", "status: {status}");
        assert_eq!(output["head"]["commit"], Value::Null);
        assert_eq!(output["upstream_status"], "absent");
    }

    for status in ["## HEAD (no branch)", "## HEAD (detached at b47e4fb)"] {
        let output = parse_show_changes_output(
            "agent:oe:webcodex",
            status,
            "commit=b47e4fb000000000000000000000000000000000\nshort=b47e4fb\nsummary=head",
            "",
            None,
            20,
            80,
            Some(0),
            "",
        );
        assert_eq!(output["branch"], Value::Null, "status: {status}");
        assert_eq!(output["upstream_status"], "absent");
    }
}

#[test]
fn non_git_show_changes_preserves_unobserved_state() {
    let output = non_git_show_changes_payload("agent:oe:plain", Some(128), false);
    assert_eq!(output["git_available"], false);
    assert_eq!(output["clean"], Value::Null);
    assert_eq!(output["counts"]["conflicted"], Value::Null);
    assert_eq!(output["upstream_status"], "unobserved");
    assert_eq!(output["upstream_reason_code"], "git_unavailable");
    assert_eq!(output["upstream"], Value::Null);
    assert_eq!(output["ahead"], Value::Null);
    assert_eq!(output["behind"], Value::Null);
    assert_reason_list_contains(&output["verdict"], "warning_reasons", "git_unavailable");
}

#[test]
fn show_changes_schema_explicitly_models_upstream_and_nullable_observation() {
    let schema = crate::tool_runtime::registry::output_schema_for_tool("show_changes");
    let properties = schema["properties"]["output"]["properties"]
        .as_object()
        .expect("show_changes output properties");
    for field in [
        "upstream_status",
        "upstream_reason_code",
        "upstream",
        "ahead",
        "behind",
    ] {
        assert!(
            properties.contains_key(field),
            "missing explicit field {field}"
        );
    }
    assert_eq!(
        properties["upstream_status"]["enum"],
        json!(["available", "absent", "gone", "unobserved"])
    );
    assert_eq!(properties["head"]["additionalProperties"], false);
    assert_eq!(properties["counts"]["additionalProperties"], false);
    assert!(properties["clean"]["anyOf"].is_array());
    assert!(properties["counts"]["properties"]["conflicted"]["anyOf"].is_array());
}

fn assert_review_verdict_shape(verdict: &serde_json::Value) {
    let status = verdict["status"].as_str().expect("status string");
    assert!(
        matches!(status, "pass" | "warn" | "fail"),
        "unexpected verdict status {status}: {verdict}"
    );
    assert!(verdict["blocking"].is_boolean(), "blocking bool: {verdict}");
    for key in [
        "blocking_reasons",
        "warning_reasons",
        "suggested_next_actions",
    ] {
        assert!(verdict[key].is_array(), "{key} array: {verdict}");
    }
}

fn assert_reason_list_contains(verdict: &serde_json::Value, key: &str, reason: &str) {
    let reasons = verdict[key].as_array().expect("reason list");
    assert!(
        reasons.iter().any(|value| value.as_str() == Some(reason)),
        "{key} should contain {reason}: {verdict}"
    );
}

fn assert_verdict_omits_raw_output_and_sensitive_values(
    verdict: &serde_json::Value,
    forbidden_values: &[&str],
    context: &str,
) {
    let serialized = serde_json::to_string(verdict).unwrap();
    for forbidden in ["stdout", "stderr", "tail", "excerpt", "command"] {
        assert!(
            !serialized.contains(forbidden),
            "{context} leaked raw output marker {forbidden}: {serialized}"
        );
    }
    for forbidden in forbidden_values {
        assert!(
            !serialized.contains(forbidden),
            "{context} leaked sensitive value {forbidden}: {serialized}"
        );
    }
}

/// Helper: run the bounded show_changes command with full stdout/stderr capture
/// (large outputs exceed the pipe buffer) and return (stdout_bytes, stdout).
fn run_bounded_show_changes_full(
    root: &Path,
    include_diff: bool,
    max_hunks: usize,
    max_hunk_lines: usize,
) -> (usize, String, String) {
    let cmd = show_changes_command(include_diff, max_hunks, max_hunk_lines);
    let (exit, stdout, stderr) = run_command_full_capture(&cmd, root, 30);
    assert_eq!(
        exit, 0,
        "show_changes failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    (stdout.len(), stdout, stderr)
}

fn near_path_max_diff_path(i: usize) -> std::path::PathBuf {
    let mut path = std::path::PathBuf::new();
    for level in 0..3 {
        path.push(format!("{}-{level:02}", "p".repeat(200)));
    }
    path.push(format!("{}-{i:02}.txt", "f".repeat(170)));
    path
}

#[test]
fn show_changes_long_path_diff_budgets_complete_preambles_and_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let total = 70usize;
    let paths: Vec<_> = (0..total).map(near_path_max_diff_path).collect();
    for path in &paths {
        let full = tmp.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, "before\n").unwrap();
    }
    let (setup_exit, _, setup_stderr) =
        run_command_full_capture("git add -A && git commit -qm long-paths", tmp.path(), 60);
    assert_eq!(setup_exit, 0, "setup failed: {setup_stderr}");
    for (i, path) in paths.iter().enumerate() {
        std::fs::write(tmp.path().join(path), format!("before\nchanged-{i}\n")).unwrap();
    }
    let (raw_exit, raw_diff, raw_stderr) =
        run_command_full_capture("git diff --unified=80", tmp.path(), 60);
    assert_eq!(raw_exit, 0, "raw diff failed: {raw_stderr}");
    assert!(
        raw_diff.len() > 192 * 1024,
        "raw diff bytes={}",
        raw_diff.len()
    );

    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 80, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "bounded bytes={stdout_bytes}"
    );
    let frames = split_show_changes_stdout(&stdout, true);
    assert_eq!(frames.diff_exit, Some(0));
    assert_eq!(frames.diff_trunc_bytes, Some(true));
    assert_eq!(frames.diff_trunc_hunk_count, Some(false));
    assert_eq!(frames.diff_trunc_hunk_lines, Some(false));
    assert_eq!(frames.diff_trunc_bytes_in_hunk, Some(false));
    assert_eq!(frames.diff_bytes, Some(frames.diff.len()));

    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 80, 80, &stderr);
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r == "diff_byte_budget"),
        "reasons={reasons:?}"
    );
    assert!(!reasons.iter().any(|r| r == "diff_hunk_count_limit"));
    assert!(!reasons.iter().any(|r| r == "diff_hunk_line_limit"));

    assert_eq!(output["hunks_truncated"], true);
    assert_eq!(output["diff_review_handoff"]["tool"], "git_diff_hunks");
    assert_eq!(
        output["diff_review_handoff"]["truncation_reasons"],
        json!(["diff_byte_budget"])
    );
    assert_eq!(output["diff_review_handoff"]["recovery"]["kind"], "page");
    assert_eq!(
        output["diff_review_handoff"]["recovery"]["safe_continuation_for_omitted_lines"],
        Value::Null
    );
    assert_git_diff_hunks_recovery_call_parses(&output["diff_review_handoff"]["recovery"]);
    let actions = output["suggested_next_actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|action| action == "follow git_diff_hunks.next_continuation while has_more=true"));
    assert_show_changes_envelope_value_matches_schema(&output, "diff byte handoff");

    let mut rejected_seen = false;
    for (i, path) in paths.iter().enumerate() {
        let display = path.to_string_lossy().replace('\\', "/");
        let preamble = format!("diff --git a/{display} b/{display}");
        let body = format!("+changed-{i}");
        let accepted = frames.diff.contains(&preamble);
        assert_eq!(
            frames.diff.contains(&body),
            accepted,
            "partial/leaked record for {display}"
        );
        if !accepted {
            rejected_seen = true;
        }
    }
    assert!(
        rejected_seen,
        "fixture must reject at least one file by byte budget"
    );
    eprintln!(
        "long_path_diff raw_bytes={} bounded_bytes={} diff_frame_bytes={}",
        raw_diff.len(),
        stdout_bytes,
        frames.diff.len()
    );
}

#[test]
fn show_changes_hunk_limit_does_not_leak_subsequent_file_bodies_or_preambles() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    // 20 modified tracked files, each with one hunk. Cap at max_hunks=1 so only
    // the first file's first hunk should ever appear in the production output.
    for i in 0..20 {
        let name = format!("file{i}.txt");
        commit_file(tmp.path(), &name, "line\n", "init");
        std::fs::write(tmp.path().join(&name), "line\nmore\n").unwrap();
    }
    let (stdout_bytes, stdout, _stderr) = run_bounded_show_changes_full(tmp.path(), true, 1, 80);
    // The original production-side output must stay within the budget.
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "bounded stdout must stay within budget; got {stdout_bytes}"
    );
    // Exactly one selected hunk header may appear in the raw production output.
    let hunk_headers: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("@@ "))
        .collect();
    assert_eq!(
        hunk_headers.len(),
        1,
        "exactly one hunk must be selected; got {} headers: {stdout}",
        hunk_headers.len()
    );
    // Exactly one `diff --git` file preamble may appear (only the selected
    // file's preamble is flushed; unselected files keep no preamble).
    let file_headers: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("diff --git "))
        .collect();
    assert_eq!(
        file_headers.len(),
        1,
        "exactly one file preamble must appear; got {} headers: {stdout}",
        file_headers.len()
    );
    // The bodies of the other 19 files must not leak. Each file's content is
    // `line\nmore\n`; the diff would show `+more`. Only one `+more` may appear.
    let plus_more_count = stdout.lines().filter(|line| *line == "+more").count();
    assert_eq!(
        plus_more_count, 1,
        "only the selected file's body may appear; got {plus_more_count} '+more' lines: {stdout}"
    );
    // The structured output must report a single returned hunk with truncation.
    let frames = split_show_changes_stdout(&stdout, true);
    assert_eq!(frames.diff_hunks_returned, Some(1));
    assert_eq!(frames.diff_hunks_truncated, Some(true));
    assert_eq!(frames.diff_exit, Some(0));
    assert_eq!(frames.diff_trunc_hunk_count, Some(true));
    assert_eq!(frames.diff_trunc_hunk_lines, Some(false));
    assert_eq!(frames.diff_trunc_bytes, Some(false));
    assert_show_changes_envelope_value_matches_schema(
        &bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 1, 80, &_stderr),
        "hunk limit leak",
    );
}

/// Parse already-captured frames into the structured show_changes output,
/// reusing the captured stderr (avoids re-running the command).
fn bounded_show_changes_output_from_frames(
    frames: &ShowChangesStdout,
    root: &Path,
    include_diff: bool,
    max_hunks: usize,
    max_hunk_lines: usize,
    stderr: &str,
) -> Value {
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, stderr);
    let effective_exit = if observation.exit_code != Some(0) {
        observation.exit_code
    } else {
        Some(0)
    };
    let mut output = parse_show_changes_output_with_observation(
        "demo",
        &frames.status,
        &frames.head,
        &frames.stat,
        include_diff.then_some(frames.diff.as_str()),
        max_hunks,
        max_hunk_lines,
        effective_exit,
        stderr,
        observation,
        frames,
    );
    if include_diff {
        let untracked_paths = show_changes_untracked_paths(&output);
        let (previews, truncated) =
            collect_show_changes_untracked_previews_for_root(root, &untracked_paths);
        output["untracked_previews"] = json!(previews);
        output["untracked_previews_truncated"] = json!(truncated);
    }
    output
}

#[cfg(unix)]
#[test]
fn show_changes_oversized_no_hunk_preamble_is_bounded_and_drained() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "before\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "after\n").unwrap();

    let payload_path = tmp.path().join("external-diff-payload.txt");
    let line = format!("{}\n", "P".repeat(256));
    let payload = line.repeat((SHOW_CHANGES_DIFF_BYTES / line.len()) + 40);
    std::fs::write(&payload_path, &payload).unwrap();
    let script_path = tmp.path().join("external-diff.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\ncat \"$WEBCODEX_TEST_DIFF_PAYLOAD\"\nexit 7\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script_path, permissions).unwrap();

    let env = format!(
        "export WEBCODEX_TEST_DIFF_PAYLOAD={}; export GIT_EXTERNAL_DIFF={};",
        shell_single_quote(payload_path.to_str().unwrap()),
        shell_single_quote(script_path.to_str().unwrap())
    );
    let (raw_exit, raw_diff, raw_stderr) =
        run_command_full_capture(&format!("{env} git diff --unified=80"), tmp.path(), 30);
    assert_ne!(raw_exit, 0, "external diff must fail: {raw_stderr}");
    assert!(
        raw_diff.len() > SHOW_CHANGES_DIFF_BYTES,
        "raw no-hunk bytes={} budget={SHOW_CHANGES_DIFF_BYTES}",
        raw_diff.len()
    );
    assert!(!raw_diff.lines().any(|line| line.starts_with("@@ ")));

    let command = show_changes_command(true, 20, 80);
    let (exit, stdout, stderr) =
        run_command_full_capture(&format!("{env} {command}"), tmp.path(), 30);
    assert_eq!(exit, 0, "bounded command failed: {stderr}\n{stdout}");
    assert!(
        stdout.len() <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "bounded stdout bytes={}",
        stdout.len()
    );
    let frames = split_show_changes_stdout(&stdout, true);
    assert!(
        frames.diff.is_empty(),
        "partial preamble leaked: {}",
        frames.diff
    );
    assert_eq!(frames.diff_bytes, Some(0));
    assert_eq!(frames.diff_trunc_bytes, Some(true));
    assert_eq!(frames.diff_trunc_hunk_count, Some(false));
    assert_eq!(frames.diff_trunc_hunk_lines, Some(false));
    assert_eq!(frames.diff_exit, Some(raw_exit));

    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    assert_eq!(output["transport_safe"], true, "{output}");
    assert_eq!(
        output["truncation_reasons"],
        json!(["diff_byte_budget"]),
        "only the byte budget fired: {output}"
    );
    eprintln!(
        "oversized_no_hunk raw_bytes={} bounded_stdout_bytes={} diff_exit={} reasons={}",
        raw_diff.len(),
        stdout.len(),
        raw_exit,
        output["truncation_reasons"]
    );
}

#[test]
fn show_changes_propagates_real_full_diff_exit_status() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    // An external diff program that exits non-zero only for the full diff
    // (it is invoked by `git diff`, not by `git diff --stat`). It writes a
    // marker so we can confirm it was actually run.
    let marker = tmp.path().join("extdiff_ran.txt");
    let marker_str = marker.to_str().unwrap();
    let ext_diff = format!(
        "sh -c 'printf x > {marker_str}; exit 7' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\" \"$7\""
    );
    // `git diff --stat` does not invoke the external diff (it computes stats
    // from the diffcore), so it should still return 0.
    let (stat_exit, _stat_out, stat_err) = run_command_full_capture(
        &format!("GIT_EXTERNAL_DIFF={ext_diff:?} git diff --stat"),
        tmp.path(),
        30,
    );
    assert_eq!(
        stat_exit, 0,
        "git diff --stat should return 0 even with a failing external diff: {stat_err}"
    );
    // The full `git diff` must return non-zero because the external diff fails.
    let (diff_exit, _diff_out, diff_err) = run_command_full_capture(
        &format!("GIT_EXTERNAL_DIFF={ext_diff:?} git diff --unified=80"),
        tmp.path(),
        30,
    );
    assert_ne!(diff_exit, 0, "full git diff must fail: {diff_err}");
    // show_changes(include_diff=true) must report failure: the diff filter
    // success must not mask the upstream git diff failure. Run the bounded
    // command with the same failing external diff in the environment so the
    // full `git diff` inside the command also fails.
    let cmd = show_changes_command(true, 20, 80);
    // Export the failing external diff into the environment so the full
    // `git diff` inside the bounded production script inherits it. The prefix
    // must be `export ...;` rather than an inline `VAR=... <cmd>` assignment:
    // the production script starts with a brace group `{ ... }`, and a
    // variable-assignment prefix is only valid before a *simple* command, not
    // a compound command (POSIX sh reports `Syntax error: "}" unexpected`).
    let (_exit, stdout, stderr) = run_command_full_capture(
        &format!("export GIT_EXTERNAL_DIFF={ext_diff:?}; {cmd}"),
        tmp.path(),
        30,
    );
    let stdout_bytes = stdout.len();
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "stdout {stdout_bytes}\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, true);
    // The structured metadata must carry the real full diff exit code (git
    // reports 128 when an external diff dies) and a command_failed diff_status.
    let diff_exit = frames.diff_exit.expect("diff_exit must be captured");
    assert_ne!(diff_exit, 0, "full git diff exit code must be non-zero");
    let output = bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, "");
    assert_eq!(output["diff_exit"], diff_exit);
    assert_eq!(output["diff_status"]["status"], "command_failed");
    assert_eq!(output["diff_status"]["exit_code"], diff_exit);
    // diff --stat succeeded (it does not invoke the external diff); its exit
    // must be reported as 0.
    assert_eq!(frames.diff_stat_exit, Some(0));
    assert_eq!(output["diff_stat_exit"], 0);
    assert_show_changes_envelope_value_matches_schema(&output, "diff exit propagation");
    // The structured diff_status already reports the failure; the runtime path
    // (separate test) asserts the tool result itself is not successful.
}

#[test]
fn show_changes_long_commit_subject_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    // A commit subject far longer than any single frame; the HEAD metadata
    // segment is bounded by its own byte budget by the production script.
    let subject = "Z".repeat(40_000);
    commit_file(tmp.path(), "README.md", "hello\n", &subject);
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let command = show_changes_command(false, 20, 80);
    assert!(
        command.contains("dd bs=1 count=$((head_subject_limit+1))"),
        "HEAD producer must read at most budget+1 bytes: {command}"
    );
    assert!(!command.contains("while IFS= read -r hline"));
    assert!(!command.contains("head_buf="));
    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), false, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "long-subject stdout must stay within budget; got {stdout_bytes}\n{stdout}\n{stderr}"
    );
    assert!(
        !stdout.contains(&subject),
        "the complete oversized subject must never reach the accumulated stdout"
    );
    let frames = split_show_changes_stdout(&stdout, false);
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, &stderr);
    assert!(observation.status_observed());
    // The pathological subject overflows the HEAD byte budget, so the HEAD
    // record is dropped whole and reported via the structured truncation flag
    // and reason code rather than leaking into the output.
    assert_eq!(frames.head_truncated, Some(true));
    assert!(frames.head.is_empty(), "HEAD record must be dropped whole");
    assert_eq!(frames.head_bytes, Some(0));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r == "head_metadata_byte_budget"),
        "expected head_metadata_byte_budget reason: {reasons:?}"
    );
    assert_eq!(output["head"]["commit"], Value::Null);
    assert_eq!(output["transport_safe"], true);
    assert_show_changes_envelope_value_matches_schema(&output, "long subject");
}

#[cfg(unix)]
#[tokio::test]
async fn show_changes_runtime_rejects_stat_only_failure_for_both_diff_modes() {
    use std::os::unix::fs::PermissionsExt;

    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "README.md", "before\n", "initial");
    std::fs::write(repo.path().join("README.md"), "after\n").unwrap();

    let (git_lookup_exit, real_git, git_lookup_stderr) =
        run_command_full_capture("command -v git", repo.path(), 30);
    assert_eq!(git_lookup_exit, 0, "cannot locate git: {git_lookup_stderr}");
    let real_git = real_git.trim();
    assert!(!real_git.is_empty());

    let wrapper_dir = tempfile::tempdir().unwrap();
    let wrapper_path = wrapper_dir.path().join("git");
    std::fs::write(
        &wrapper_path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = diff ] && [ \"$2\" = --stat ]; then\n  exit 77\nfi\nexec {} \"$@\"\n",
            shell_single_quote(real_git)
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&wrapper_path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, permissions).unwrap();

    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "stat-only", "demo", repo.path()).await;
    let path_prefix = format!(
        "PATH={}:\"$PATH\"; export PATH;",
        shell_single_quote(wrapper_dir.path().to_str().unwrap())
    );

    for include_diff in [false, true] {
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let project = project.clone();
            async move {
                runtime
                    .show_changes(project, None, Some(include_diff), None, None, None)
                    .await
            }
        });
        let req = wait_for_patch_agent_request(&runtime, "stat-only").await;
        assert_eq!(req.kind, "run_internal_posix_script");
        assert!(req.command.is_empty());
        let payload = req
            .script
            .as_ref()
            .expect("show_changes must carry a typed internal script");
        assert_eq!(
            payload.language,
            crate::runner_protocol::ShellScriptLanguage::Sh
        );
        assert!(payload.args.is_empty());
        assert!(
            payload.script.len() <= crate::runner_protocol::RAW_SHELL_WIRE_MAX_BYTES,
            "script bytes={}",
            payload.script.len()
        );
        let full_command = format!("{path_prefix} {}", payload.script);
        let (command_exit, stdout, stderr) =
            run_command_full_capture(&full_command, repo.path(), 30);
        assert_eq!(
            command_exit, 0,
            "bounded envelope failed: {stderr}\n{stdout}"
        );
        assert!(
            stdout.len() <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
            "bounded stdout bytes={}",
            stdout.len()
        );

        let frames = split_show_changes_stdout(&stdout, include_diff);
        let status_exit = frames
            .status_result
            .lines()
            .find_map(|line| line.strip_prefix("status_exit="))
            .and_then(|value| value.parse::<i32>().ok());
        assert_eq!(status_exit, Some(0));
        assert_eq!(frames.diff_stat_exit, Some(77));
        if include_diff {
            assert_eq!(frames.diff_exit, Some(0));
        } else {
            assert_eq!(frames.diff_exit, None);
        }

        complete_patch_agent_request(
            &runtime,
            "stat-only",
            &req.request_id,
            command_exit,
            &stdout,
            &stderr,
        )
        .await;
        let result = task.await.unwrap();
        assert!(
            !result.success,
            "stat failure must fail ToolResult: {result:?}"
        );
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("diff-stat inspection")),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(result.output["diff_stat_exit"], 77);
        assert_eq!(
            result.output["diff_stat_status"]["status"],
            "command_failed"
        );
        assert_eq!(result.output["diff_stat_status"]["exit_code"], 77);
        assert_eq!(
            result.output["diff_stat_status"]["reason_code"],
            "git_diff_stat_command_failed"
        );
        assert_eq!(result.output["transport_safe"], true);
        assert_eq!(result.output["status_observation"]["status"], "observed");
        if include_diff {
            assert_eq!(result.output["diff_exit"], 0);
            assert_eq!(result.output["diff_status"]["status"], "observed");
        }
        assert_show_changes_envelope_matches_schema("runtime stat-only failure", &result);
        eprintln!(
            "stat_only include_diff={include_diff} command_exit={command_exit} status_exit={} diff_stat_exit={} diff_exit={:?} tool_success={} stdout_bytes={}",
            status_exit.unwrap(),
            frames.diff_stat_exit.unwrap(),
            frames.diff_exit,
            result.success,
            stdout.len()
        );
    }
}

#[tokio::test]
async fn show_changes_runtime_rejects_unavailable_diff_stat_observation() {
    let repo = tempfile::tempdir().unwrap();
    init_git_repo(repo.path());
    commit_file(repo.path(), "README.md", "before\n", "initial");
    std::fs::write(repo.path().join("README.md"), "after\n").unwrap();

    let runtime = test_runtime();
    let project =
        register_runner_project_at_path(&runtime, "stat-missing", "demo", repo.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .show_changes(project, None, Some(false), None, None, None)
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "stat-missing").await;
    assert_eq!(req.kind, "run_internal_posix_script");
    assert!(req.command.is_empty());
    let payload = req
        .script
        .as_ref()
        .expect("show_changes must carry a typed internal script");
    let (command_exit, stdout, stderr) = run_command_full_capture(&payload.script, repo.path(), 30);
    assert_eq!(command_exit, 0, "show_changes command failed: {stderr}");
    let missing_stat_exit = stdout.replacen("diff_stat_exit=0", "xiff_stat_exit=0", 1);
    complete_patch_agent_request(
        &runtime,
        "stat-missing",
        &req.request_id,
        command_exit,
        &missing_stat_exit,
        &stderr,
    )
    .await;

    let result = task.await.unwrap();
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("diff-stat inspection unavailable")),
        "unexpected error: {:?}",
        result.error
    );
    assert_eq!(result.output["diff_stat_exit"], Value::Null);
    assert_eq!(
        result.output["diff_stat_status"],
        json!({
            "status": "output_unavailable",
            "exit_code": null,
            "reason_code": "git_diff_stat_result_unavailable",
        })
    );
    assert_show_changes_envelope_matches_schema("runtime unavailable stat", &result);
}

#[test]
fn show_changes_many_long_path_diff_stat_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    // Many tracked files with long names produce a large `git diff --stat`
    // segment; the production script bounds it.
    let dir = tmp.path().join("d");
    std::fs::create_dir_all(&dir).unwrap();
    let total = 220usize;
    for i in 0..total {
        std::fs::write(dir.join(long_leaf_name(i)), "x\n").unwrap();
    }
    let (add_exit, _, add_stderr, _) = run_command_sync("git add -A", tmp.path(), 30);
    assert_eq!(add_exit, 0, "add failed: {add_stderr}");
    let (commit_exit, _, commit_stderr, _) = run_command_sync("git commit -qm add", tmp.path(), 30);
    assert_eq!(commit_exit, 0, "commit failed: {commit_stderr}");
    for i in 0..total {
        std::fs::write(dir.join(long_leaf_name(i)), "y\nx\n").unwrap();
    }
    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), false, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "many-long-path diff --stat stdout must stay within budget; got {stdout_bytes}\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, false);
    // The diff --stat exit code must be reported (git diff --stat succeeds).
    assert_eq!(frames.diff_stat_exit, Some(0));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    assert_show_changes_envelope_value_matches_schema(&output, "many long path diff stat");
}

#[test]
fn show_changes_single_overlong_diff_line_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    let original = "a\n".to_string();
    commit_file(tmp.path(), "big.txt", &original, "initial");
    // One diff line far larger than the diff byte budget; a single overlong
    // line must not overflow the global budget.
    let giant = "X".repeat(60_000);
    std::fs::write(tmp.path().join("big.txt"), format!("a\n{giant}\n")).unwrap();
    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "overlong-diff-line stdout must stay within budget; got {stdout_bytes}\n{stdout}\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, true);
    assert_eq!(frames.diff_exit, Some(0));
    assert_eq!(frames.diff_hunks_truncated, Some(true));
    assert_eq!(frames.diff_trunc_bytes, Some(true));
    assert_eq!(frames.diff_trunc_bytes_in_hunk, Some(true));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    let reasons = output["truncation_reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| matches!(
            r.as_str(),
            Some("diff_hunk_line_limit") | Some("diff_byte_budget")
        )),
        "expected a diff line/byte budget reason: {reasons:?}"
    );
    assert!(reasons.iter().any(|r| r == "diff_hunk_byte_budget"));
    assert_eq!(output["diff_review_handoff"]["recovery"]["kind"], "mixed");
    assert_eq!(
        output["diff_review_handoff"]["recovery"]["safe_continuation_for_omitted_lines"],
        false
    );
    assert_show_changes_envelope_value_matches_schema(&output, "overlong diff line");
    // The giant line must not appear in full in the structured output.
    let serialized = serde_json::to_string(&output).unwrap();
    assert!(
        !serialized.contains(&giant),
        "a single overlong diff line must not leak into the structured output"
    );
}

#[test]
fn show_changes_binary_diff_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "bin.dat", "text\n", "initial");
    std::fs::write(tmp.path().join("bin.dat"), vec![0, 1, 2, 3, 0, 4]).unwrap();
    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "binary diff stdout must stay within budget; got {stdout_bytes}\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, true);
    assert_eq!(frames.diff_exit, Some(0));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    // A binary diff produces no hunks but must not crash or leak binary bytes.
    assert_eq!(output["hunk_count"].as_u64().unwrap_or(0), 0);
    assert_show_changes_envelope_value_matches_schema(&output, "binary diff");
}

#[test]
fn show_changes_multi_file_multi_hunk_diff_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    // 5 files, 3 hunks each, all selected under max_hunks=20.
    for i in 0..5 {
        let content = (0..30).map(|n| format!("line{n}\n")).collect::<String>();
        commit_file(tmp.path(), &format!("f{i}.txt"), &content, "init");
    }
    for i in 0..5 {
        let path = tmp.path().join(format!("f{i}.txt"));
        let content = (0..30)
            .map(|n| {
                if n % 10 == 0 {
                    format!("mod{n}\n")
                } else {
                    format!("line{n}\n")
                }
            })
            .collect::<String>();
        std::fs::write(path, content).unwrap();
    }
    let (stdout_bytes, stdout, stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "multi-file multi-hunk stdout must stay within budget; got {stdout_bytes}\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, true);
    assert_eq!(frames.diff_exit, Some(0));
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), true, 20, 80, &stderr);
    // 5 files, ~15 hunks selected (some may be line-bounded).
    let hunk_count = output["hunk_count"].as_u64().unwrap_or(0);
    assert!(hunk_count >= 5, "expected several hunks, got {hunk_count}");
    assert_show_changes_envelope_value_matches_schema(&output, "multi-file multi-hunk");
}

#[test]
fn show_changes_include_diff_false_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    for i in 0..10 {
        std::fs::write(tmp.path().join(format!("u{i}.txt")), "x\n").unwrap();
    }
    let (stdout_bytes, _stdout, _stderr) = run_bounded_show_changes_full(tmp.path(), false, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "include_diff=false stdout must stay within budget; got {stdout_bytes}"
    );
}

#[test]
fn show_changes_include_diff_true_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    let (stdout_bytes, _stdout, _stderr) = run_bounded_show_changes_full(tmp.path(), true, 20, 80);
    assert!(
        stdout_bytes <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "include_diff=true stdout must stay within budget; got {stdout_bytes}"
    );
}

#[test]
fn show_changes_status_command_failure_stays_within_budget_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    let (cfg_exit, _, cfg_err) = run_command_full_capture(
        "git config status.showUntrackedFiles invalid",
        tmp.path(),
        30,
    );
    assert_eq!(cfg_exit, 0, "config failed: {cfg_err}");
    let cmd = show_changes_command(false, 20, 80);
    let (exit, stdout, stderr) = run_command_full_capture(&cmd, tmp.path(), 30);
    assert_ne!(exit, 0, "status config error must not exit 0: {stderr}");
    assert!(
        stdout.len() <= SHOW_CHANGES_OUTPUT_BUDGET_BYTES,
        "status-failure stdout must stay within budget; got {}",
        stdout.len()
    );
    let frames = split_show_changes_stdout(&stdout, false);
    let observation =
        parse_show_changes_status_observation(&frames.status, &frames.status_result, &stderr);
    assert_eq!(observation.as_json()["status"], "command_failed");
    let output =
        bounded_show_changes_output_from_frames(&frames, tmp.path(), false, 20, 80, &stderr);
    assert_eq!(output["clean"], Value::Null);
    assert_eq!(output["counts"]["conflicted"], Value::Null);
    assert_eq!(output["verdict"]["status"], "fail");
    assert_show_changes_envelope_value_matches_schema(&output, "status command failure");
}

#[tokio::test]
async fn show_changes_runtime_propagates_full_diff_failure_as_tool_failure() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial");
    std::fs::write(tmp.path().join("README.md"), "hello\nchanged\n").unwrap();
    // Install a failing external diff that affects the full `git diff` only.
    let marker = tmp.path().join("extdiff_runtime_ran.txt");
    let marker_str = marker.to_str().unwrap();
    let ext_diff = format!(
        "sh -c 'printf x > {marker_str}; exit 5' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\" \"$7\""
    );
    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, "extd", "demo", tmp.path()).await;
    // The agent completes the request by running the bounded command locally,
    // but with the failing external diff exported into the environment. The
    // production script starts with a brace group, so the external diff must
    // be `export`ed (with a `;` separator): an inline `VAR=... <cmd>` prefix
    // is only valid before a simple command, not the compound `{ ... }`.
    let ext_env = format!("export GIT_EXTERNAL_DIFF={ext_diff:?};");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            runtime
                .show_changes(project, None, Some(true), None, None, None)
                .await
        }
    });
    let req = wait_for_patch_agent_request(&runtime, "extd").await;
    // Run the generated internal script locally with the failing external diff
    // in the environment so the full `git diff` fails.
    assert_eq!(req.kind, "run_internal_posix_script");
    assert!(req.command.is_empty());
    let payload = req
        .script
        .as_ref()
        .expect("show_changes must carry a typed internal script");
    let full = format!("{ext_env} {}", payload.script);
    let (exit, stdout, stderr) = run_command_full_capture(&full, tmp.path(), 30);
    complete_patch_agent_request(&runtime, "extd", &req.request_id, exit, &stdout, &stderr).await;
    let result = task.await.unwrap();
    // The full git diff failed, so show_changes must report a tool failure even
    // though the production-side diff filter succeeded.
    assert!(
        !result.success,
        "show_changes must fail when the full git diff fails: {:?}",
        result.error
    );
    assert_eq!(result.output["diff_status"]["status"], "command_failed");
    let diff_exit = result.output["diff_status"]["exit_code"]
        .as_i64()
        .expect("diff exit code");
    assert_ne!(diff_exit, 0, "full git diff exit code must be non-zero");
    assert_eq!(result.output["diff_exit"], diff_exit);
    // diff --stat itself succeeded (it does not invoke the external diff); its
    // exit must be reported as 0.
    assert_eq!(result.output["diff_stat_exit"], 0);
    assert_eq!(result.output["status_observation"]["status"], "observed");
    assert_show_changes_envelope_matches_schema("runtime diff failure", &result);
}
