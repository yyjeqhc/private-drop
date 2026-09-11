//! Tests for the `workspace_hygiene_check` read-only runtime tool.

use super::super::*;
use super::support::*;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

// =========================================================================
// Helpers
// =========================================================================

/// Dispatch `workspace_hygiene_check` against an agent-registered project and
/// complete its internal read-only agent shell requests locally.
async fn dispatch_hygiene_with_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    project: String,
    max_findings: Option<usize>,
    include_tracked: Option<bool>,
    session_id: Option<String>,
) -> ToolResult {
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        let bootstrap = auth_context(None, true);
        runtime_for_task
            .dispatch_with_auth(
                ToolCall::WorkspaceHygieneCheck {
                    project,
                    max_findings,
                    include_tracked,
                    session_id,
                },
                Some(&bootstrap),
            )
            .await
    });

    let forbidden = ["python3", "-c"].join(" ");
    // A hygiene check can issue both the fixed diagnostic script and a bounded
    // size-probe script. On Windows each local POSIX-shell startup is noticeably
    // more expensive, especially while the full Rust test suite is running in
    // parallel, so the old 10-second harness deadline was below the production
    // per-script timeout and could fail despite healthy bounded execution.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !task.is_finished() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "hygiene check did not finish within 30 seconds for client {client_id}"
        );
        if let Some(req) = probe_patch_agent_request(runtime, client_id).await {
            assert_eq!(req.kind, "run_internal_posix_script");
            assert!(req.command.is_empty());
            let payload = req
                .script
                .as_ref()
                .expect("hygiene diagnostics must carry a typed internal script");
            assert_eq!(
                payload.language,
                crate::runner_protocol::ShellScriptLanguage::Sh
            );
            assert!(payload.args.is_empty());
            assert!(
                !payload.script.contains(&forbidden),
                "hygiene diagnostics must not enqueue a Python helper: {}",
                payload.script
            );
            complete_agent_request_by_running_locally(runtime, client_id, req).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    task.await.unwrap()
}

/// Set up a clean git repo at a temp dir and return (tempdir, project_id).
async fn setup_clean_git_repo(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
) -> (TempDir, String) {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    commit_file(tmp.path(), "README.md", "hello\n", "initial commit");
    let project = register_runner_project_at_path(runtime, client_id, project_id, tmp.path()).await;
    (tmp, project)
}

// =========================================================================
// 1. Known tool / specs / metadata / MCP / OpenAPI consistency
// =========================================================================

#[test]
fn workspace_hygiene_check_is_known_and_in_specs() {
    assert!(is_known_tool_name("workspace_hygiene_check"));

    let specs = registered_tool_specs();
    let spec = spec_named(&specs, "workspace_hygiene_check");

    // Input schema: project is required, others optional.
    let required = required_fields(spec);
    assert_eq!(required, vec!["project".to_string()]);

    // Metadata: read-only, project:read.
    let metadata =
        crate::tool_runtime::metadata::lookup_tool_metadata("workspace_hygiene_check").unwrap();
    assert_eq!(
        metadata.effect,
        crate::tool_runtime::metadata::ToolEffect::Observe
    );
    assert!(!metadata.destructive);
    assert!(!metadata.shell_like);
    assert!(metadata.requires_project);
    assert_eq!(
        metadata.authority,
        crate::tool_runtime::metadata::ToolAuthorityPolicy::Require(
            crate::auth::SCOPE_PROJECT_READ
        )
    );

    // MCP annotations: readOnlyHint=true.
    assert_eq!(spec.annotations["readOnlyHint"], true);
    assert_eq!(spec.annotations["destructiveHint"], false);
    let output_props = spec.output_schema["properties"]["output"]["properties"]
        .as_object()
        .unwrap();
    assert!(
        output_props.contains_key("verdict"),
        "workspace_hygiene_check output schema should expose verdict"
    );
    assert!(
        !output_props.contains_key("suggested_next_actions"),
        "action guidance should have one canonical home under verdict"
    );
    assert!(output_props["counts"]["description"]
        .as_str()
        .is_some_and(|description| description.contains("Sparse non-zero")));

    // OpenAPI ToolCallRequest.tool description includes the name.
    let openapi_spec = crate::openapi::build_openapi_spec();
    let tool_desc = &openapi_spec["components"]["schemas"]["ToolCallRequest"]["properties"]["tool"]
        ["description"]
        .as_str()
        .unwrap();
    assert!(
        tool_desc.contains("workspace_hygiene_check"),
        "ToolCallRequest.tool description should list workspace_hygiene_check"
    );

    // tool_manifest category: cleanup.
    assert_eq!(tool_manifest_category("workspace_hygiene_check"), "cleanup");
}

#[test]
fn workspace_hygiene_check_openapi_operation_count_unchanged() {
    let spec = crate::openapi::build_openapi_spec();
    let count: usize = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|m| m.as_object().unwrap().len())
        .sum();
    assert_eq!(count, 20, "operation count must stay 20");
}

// =========================================================================
// 2. Clean git repo
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_clean_git_repo() {
    let runtime = test_runtime();
    let (_tmp, project) = setup_clean_git_repo(&runtime, "hyc-clean", "demo").await;

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-clean", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["git_available"], true);
    assert_eq!(result.output["clean"], true);
    for omitted in [
        "counts",
        "findings",
        "truncated",
        "warnings",
        "suggested_next_actions",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "clean hygiene output must omit default field {omitted}: {}",
            result.output
        );
    }
    assert_review_verdict_shape(&result.output["verdict"]);
    assert_eq!(result.output["verdict"]["status"], "pass");
    assert_eq!(result.output["verdict"]["blocking"], false);
    assert!(result.output["verdict"]["blocking_reasons"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(result.output["verdict"]["warning_reasons"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(result.output["verdict"]["suggested_next_actions"]
        .as_array()
        .unwrap()
        .is_empty());
}

// =========================================================================
// 3. Detects untracked smoke/temp file
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_detects_untracked_smoke_temp_file() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-smoke", "demo").await;

    fs::write(tmp.path().join(".webcodex-smoke-acceptance.txt"), "ok\n").unwrap();

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-smoke", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    let smoke_finding = findings
        .iter()
        .find(|f| f["kind"] == "temporary_file")
        .unwrap_or_else(|| panic!("expected temporary_file finding: {findings:?}"));
    assert_eq!(smoke_finding["tracked_status"], "untracked");
    assert_eq!(smoke_finding["path"], ".webcodex-smoke-acceptance.txt");
    assert_review_verdict_shape(&result.output["verdict"]);
    assert_eq!(result.output["verdict"]["status"], "warn");
    assert_eq!(result.output["verdict"]["blocking"], false);
    assert_reason_list_contains(
        &result.output["verdict"],
        "warning_reasons",
        "hygiene_findings_present",
    );
}

#[tokio::test]
async fn workspace_hygiene_check_detects_dirty_untracked_and_suspicious_paths() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-mixed", "demo").await;

    fs::write(tmp.path().join("README.md"), "changed\n").unwrap();
    fs::write(tmp.path().join("scratch_probe.txt"), "temporary\n").unwrap();
    fs::write(tmp.path().join(".env.local"), "DO_NOT_LEAK=this-value\n").unwrap();

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-mixed", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["kind"] == "dirty_worktree"),
        "expected dirty_worktree finding: {findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f["kind"] == "temporary_file" && f["path"] == "scratch_probe.txt"),
        "expected temporary_file finding: {findings:?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f["kind"] == "secret_like_path" && f["path"] == ".env.local"),
        "expected secret_like_path finding: {findings:?}"
    );
    let output_str = serde_json::to_string(&result.output).unwrap();
    assert!(
        !output_str.contains("DO_NOT_LEAK=this-value"),
        "output must not contain secret-like file contents"
    );
    assert_eq!(result.output["verdict"]["status"], "fail");
    assert_reason_list_contains(
        &result.output["verdict"],
        "blocking_reasons",
        "sensitive_path_risk",
    );
    assert_verdict_omits_raw_output_and_sensitive_values(
        &result.output["verdict"],
        &["DO_NOT_LEAK=this-value"],
        "mixed hygiene verdict",
    );
}

// =========================================================================
// 4. Detects secret-like path without reading content
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_detects_secret_like_path_without_content() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-secret", "demo").await;

    let secret_content = "SUPER_SECRET_API_KEY=sk-super-secret-value-12345";
    fs::write(tmp.path().join(".env.local"), secret_content).unwrap();

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-secret", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    let secret_finding = findings
        .iter()
        .find(|f| f["kind"] == "secret_like_path")
        .unwrap_or_else(|| panic!("expected secret_like_path finding: {findings:?}"));
    assert_eq!(secret_finding["path"], ".env.local");
    assert_eq!(secret_finding["tracked_status"], "untracked");
    let severity = secret_finding["severity"].as_str().unwrap();
    assert!(severity == "high" || severity == "critical");

    // The output must NOT contain the file contents.
    let output_str = serde_json::to_string(&result.output).unwrap();
    assert!(
        !output_str.contains("SUPER_SECRET_API_KEY"),
        "output must not contain secret file contents"
    );
    assert!(
        !output_str.contains("sk-super-secret-value-12345"),
        "output must not contain secret values"
    );
    assert_verdict_omits_raw_output_and_sensitive_values(
        &result.output["verdict"],
        &["SUPER_SECRET_API_KEY", "sk-super-secret-value-12345"],
        "secret hygiene verdict",
    );
}

// =========================================================================
// 5. Detects cache path
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_detects_cache_path() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-cache", "demo").await;

    fs::create_dir_all(tmp.path().join(".pytest_cache")).unwrap();
    fs::write(tmp.path().join(".pytest_cache").join("v.json"), "{}\n").unwrap();

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-cache", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    assert!(
        findings.iter().any(|f| f["kind"] == "cache_path"),
        "expected cache_path finding: {findings:?}"
    );
}

// =========================================================================
// 6. Detects large untracked file (without reading content)
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_detects_large_untracked_file() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-large", "demo").await;

    // Create a >5 MiB untracked file.
    let large_content = vec![0u8; 6 * 1024 * 1024];
    fs::write(tmp.path().join("big_blob.dat"), &large_content).unwrap();

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-large", project, None, None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    let large_finding = findings
        .iter()
        .find(|f| f["kind"] == "large_untracked_file")
        .unwrap_or_else(|| panic!("expected large_untracked_file finding: {findings:?}"));
    assert_eq!(large_finding["path"], "big_blob.dat");
    assert_eq!(result.output["verdict"]["status"], "warn");
    assert_eq!(result.output["verdict"]["blocking"], false);

    // The output must NOT contain file contents.
    let output_str = serde_json::to_string(&result.output).unwrap();
    assert!(
        !output_str.contains("big_blob.dat\\u0000") && !output_str.contains("\\u0000\\u0000"),
        "output must not contain binary file contents"
    );
}

// =========================================================================
// 7. include_tracked=false by default; true reports tracked suspicious paths
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_include_tracked_false_by_default() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-tracked", "demo").await;

    // Commit a tracked file with a suspicious name.
    commit_file(
        tmp.path(),
        ".env",
        "TRACKED_SECRET=value\n",
        "add tracked env",
    );

    // Default: include_tracked=false → tracked .env should NOT be reported.
    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-tracked", project.clone(), None, None, None)
            .await;
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("findings").is_none(),
        "default tracked-file exclusion should leave sparse clean output: {}",
        result.output
    );

    // include_tracked=true → tracked .env should be reported.
    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-tracked", project, None, Some(true), None).await;
    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    let has_tracked_secret = findings.iter().any(|f| {
        f["path"] == ".env" && f["tracked_status"] == "tracked" && f["kind"] == "secret_like_path"
    });
    assert!(
        has_tracked_secret,
        "tracked suspicious path should be reported with include_tracked=true: {findings:?}"
    );

    // Even with include_tracked=true, contents must not appear.
    let output_str = serde_json::to_string(&result.output).unwrap();
    assert!(
        !output_str.contains("TRACKED_SECRET"),
        "output must not contain file contents"
    );
}

#[tokio::test]
async fn workspace_hygiene_check_does_not_mark_tracked_auth_source_names_critical() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-source-names", "demo").await;
    for path in [
        "src/auth/tokens.rs",
        "src/auth/project_credential.rs",
        "src/runner_tokens_http.rs",
    ] {
        let full = tmp.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        commit_file(
            tmp.path(),
            path,
            "// ordinary authentication source\n",
            &format!("add {path}"),
        );
    }
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    commit_file(
        tmp.path(),
        "config/token.json",
        "{}\n",
        "add token data fixture",
    );

    let result = dispatch_hygiene_with_agent(
        &runtime,
        "hyc-source-names",
        project,
        None,
        Some(true),
        None,
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    for path in [
        "src/auth/tokens.rs",
        "src/auth/project_credential.rs",
        "src/runner_tokens_http.rs",
    ] {
        assert!(
            findings.iter().all(|finding| finding["path"] != path),
            "source name alone must not create a finding: {findings:?}"
        );
    }
    assert!(
        findings.iter().any(|finding| {
            finding["path"] == "config/token.json"
                && finding["kind"] == "secret_like_path"
                && finding["severity"] == "critical"
        }),
        "explicit token data file must remain critical: {findings:?}"
    );
}

// =========================================================================
// 8. Bounds findings (max_findings clamp + truncation)
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_bounds_findings() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-bound", "demo").await;

    // Create many untracked scratch files (each matches is_temporary_file).
    for i in 0..60 {
        fs::write(tmp.path().join(format!("scratch_{i}.txt")), "temp\n").unwrap();
    }

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-bound", project, Some(10), None, None).await;

    assert!(result.success, "{:?}", result.error);
    let findings = result.output["findings"].as_array().unwrap();
    assert_eq!(
        findings.len(),
        10,
        "findings should be bounded to max_findings=10"
    );
    assert_eq!(result.output["truncated"], true);
}

// =========================================================================
// 9. Non-git project does not fail
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_non_git_project_does_not_fail() {
    let runtime = test_runtime();
    let tmp = TempDir::new().unwrap();
    // Do NOT git init — this is a non-git project.
    fs::write(tmp.path().join("README.md"), "hello\n").unwrap();

    let project = register_runner_project_at_path(&runtime, "hyc-nongit", "demo", tmp.path()).await;

    let result =
        dispatch_hygiene_with_agent(&runtime, "hyc-nongit", project, None, None, None).await;

    assert!(
        result.success,
        "non-git project must not fail the tool: {:?}",
        result.error
    );
    assert_eq!(result.output["git_available"], false);
    assert_review_verdict_shape(&result.output["verdict"]);
    assert_eq!(result.output["verdict"]["status"], "warn");
    assert_eq!(result.output["verdict"]["blocking"], false);
    assert_reason_list_contains(
        &result.output["verdict"],
        "warning_reasons",
        "git_unavailable",
    );
    for omitted in ["counts", "findings", "truncated", "suggested_next_actions"] {
        assert!(
            result.output.get(omitted).is_none(),
            "non-git hygiene output must omit empty/default field {omitted}: {}",
            result.output
        );
    }
    let warnings = result.output["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w == "non_git_project"),
        "warnings should contain non_git_project: {warnings:?}"
    );
}

// =========================================================================
// 10. Read-only session allowed
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_read_only_session_allowed() {
    let runtime = test_runtime();
    let (_tmp, project) = setup_clean_git_repo(&runtime, "hyc-ro", "demo").await;

    // Start a read_only session.
    let session = runtime
        .sessions
        .start_session_with_options(crate::tool_runtime::sessions::SessionCreateOptions::new(
            Some(project.clone()),
            Some("hygiene read-only".to_string()),
            SessionMode::ReadOnly,
            crate::tool_runtime::sessions::SessionGuards {
                deny_write_tools: true,
                deny_shell_tools: true,
            },
        ))
        .unwrap();

    let result = dispatch_hygiene_with_agent(
        &runtime,
        "hyc-ro",
        project,
        None,
        None,
        Some(session.session_id.clone()),
    )
    .await;

    assert!(
        result.success,
        "read_only session should allow workspace_hygiene_check: {:?}",
        result.error
    );
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
}

// =========================================================================
// 11. Input summary is bounded (session_log_arguments)
// =========================================================================

#[tokio::test]
async fn workspace_hygiene_check_input_summary_is_bounded() {
    let runtime = test_runtime();
    let (tmp, project) = setup_clean_git_repo(&runtime, "hyc-summary", "demo").await;

    // Create an untracked secret-like file so findings exist.
    fs::write(tmp.path().join(".env.local"), "SECRET=abc\n").unwrap();

    let session = runtime.sessions.start_session(Some(project.clone()), None);

    let result = dispatch_hygiene_with_agent(
        &runtime,
        "hyc-summary",
        project,
        Some(5),
        Some(false),
        Some(session.session_id.clone()),
    )
    .await;

    assert!(result.success, "{:?}", result.error);

    // Fetch the session summary and find the started event (which carries
    // the input_summary). The finished event has input_summary=None.
    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(30))
        .expect("session should exist");
    let event = summary
        .events
        .iter()
        .rev()
        .find(|e| e.kind == "tool_call_started" && e.tool_name == "workspace_hygiene_check")
        .unwrap_or_else(|| {
            panic!(
                "missing started event for workspace_hygiene_check: {:?}",
                summary.events
            )
        });

    // The logged arguments should only contain project/max_findings/include_tracked.
    let args = event
        .input_summary
        .as_ref()
        .expect("input_summary should be set on started event");
    let arg_keys: std::collections::BTreeSet<&str> = args
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert!(
        arg_keys.contains("project"),
        "arguments should contain project: {arg_keys:?}"
    );
    assert!(
        arg_keys.contains("max_findings"),
        "arguments should contain max_findings: {arg_keys:?}"
    );
    assert!(
        arg_keys.contains("include_tracked"),
        "arguments should contain include_tracked: {arg_keys:?}"
    );

    // Arguments must NOT contain findings or file contents.
    assert!(
        !arg_keys.contains("findings"),
        "arguments must not contain findings: {arg_keys:?}"
    );
    let args_str = serde_json::to_string(args).unwrap();
    assert!(
        !args_str.contains("SECRET=abc"),
        "arguments must not contain file contents: {args_str}"
    );
}

// =========================================================================
// Extra: tool parses correctly
// =========================================================================

#[test]
fn workspace_hygiene_check_tool_is_known_and_parses() {
    assert!(is_known_tool_name("workspace_hygiene_check"));
    let call = ToolCall::from_tool_name(
        "workspace_hygiene_check",
        json!({
            "project": "agent:oe:webcodex",
            "max_findings": 25,
            "include_tracked": true,
            "session_id": "wc_sess_1234"
        }),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::WorkspaceHygieneCheck {
            ref project,
            max_findings: Some(25),
            include_tracked: Some(true),
            session_id: Some(ref session_id),
        } if project == "agent:oe:webcodex" && session_id == "wc_sess_1234"
    ));
    assert_eq!(call.tool_name(), "workspace_hygiene_check");
    assert_eq!(call.project(), Some("agent:oe:webcodex"));
    assert_eq!(call.session_id(), Some("wc_sess_1234"));

    let log_args = call.session_log_arguments();
    assert_eq!(log_args["project"], "agent:oe:webcodex");
    assert_eq!(log_args["max_findings"], 25);
    assert_eq!(log_args["include_tracked"], true);
    assert!(
        log_args.get("findings").is_none(),
        "session_log_arguments must not include findings"
    );
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
