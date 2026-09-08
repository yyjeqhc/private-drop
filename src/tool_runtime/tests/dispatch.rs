//! Dispatch tests for tool_runtime.

use super::super::helpers::*;
use super::super::*;
use super::support::*;
use crate::runner_protocol::{RunnerCapabilities, RunnerResultRequest};
use serde_json::json;

#[test]
fn structured_validation_tools_are_known_and_parse() {
    for name in ["cargo_fmt", "cargo_check", "cargo_test", "go_test"] {
        assert!(is_known_tool_name(name), "{name} missing");
    }
    assert!(matches!(
        ToolCall::from_tool_name(
            "cargo_fmt",
            json!({"project":"agent:oe:webcodex","check":true,"cwd":"crates/app","sync_wait_secs":60})
        )
        .unwrap(),
        ToolCall::CargoFmt {
            check: Some(true),
            sync_wait_secs: Some(60),
            ..
        }
    ));
    assert!(matches!(
        ToolCall::from_tool_name(
            "cargo_check",
            json!({"project":"agent:oe:webcodex","sync_wait_secs":1})
        )
        .unwrap(),
        ToolCall::CargoCheck {
            all_targets: None,
            sync_wait_secs: Some(1),
            ..
        }
    ));
    assert!(matches!(
        ToolCall::from_tool_name(
            "cargo_test",
            json!({
                "project":"agent:oe:webcodex",
                "filter":"tool_runtime",
                "require_tests": true,
                "min_tests": 6
                ,"sync_wait_secs": 1
            })
        )
        .unwrap(),
        ToolCall::CargoTest {
            filter: Some(filter),
            require_tests: Some(true),
            min_tests: Some(6),
            sync_wait_secs: Some(1),
            ..
        } if filter == "tool_runtime"
    ));
    assert!(matches!(
        ToolCall::from_tool_name(
            "go_test",
            json!({"project":"agent:oe:webcodex","cwd":"internal/nodeapp","sync_wait_secs":1})
        )
        .unwrap(),
        ToolCall::GoTest { cwd: Some(cwd), sync_wait_secs: Some(1), .. } if cwd == "internal/nodeapp"
    ));
}

#[tokio::test]
async fn cargo_tools_reject_unsafe_cwd_before_project_dispatch() {
    let runtime = test_runtime();
    let fmt = runtime
        .cargo_fmt(
            "agent:oe:webcodex".to_string(),
            Some("../outside".to_string()),
            None,
            None,
        )
        .await;
    assert!(!fmt.success);
    assert!(fmt.error.unwrap().contains("parent traversal"));

    let check = runtime
        .cargo_check(
            "agent:oe:webcodex".to_string(),
            Some("/tmp".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!check.success);
    assert!(check.error.unwrap().contains("project-relative"));

    let test = runtime
        .cargo_test(
            "agent:oe:webcodex".to_string(),
            Some("src\0bad".to_string()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!test.success);
    assert!(test.error.unwrap().contains("NUL"));

    let go_parent = runtime
        .go_test(
            "agent:oe:webcodex".to_string(),
            Some("../outside".to_string()),
            None,
        )
        .await;
    assert!(!go_parent.success);
    assert!(go_parent.error.unwrap().contains("parent traversal"));

    let go_absolute = runtime
        .go_test(
            "agent:oe:webcodex".to_string(),
            Some("/tmp".to_string()),
            None,
        )
        .await;
    assert!(!go_absolute.success);
    assert!(go_absolute.error.unwrap().contains("project-relative"));
}

#[tokio::test]
async fn agent_run_shell_resolves_relative_cwd_from_registered_project_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let frontend = root.join("frontend");
    std::fs::create_dir_all(&frontend).unwrap();
    let runtime = runtime_with_agent_project("cwd-agent");
    let project =
        register_runner_project_at_path(&runtime, "cwd-agent", "cwd-project", &root).await;

    for (cwd, expected) in [
        (Some("frontend".to_string()), frontend.clone()),
        (Some(".".to_string()), root.clone()),
        (
            Some(frontend.to_string_lossy().to_string()),
            frontend.clone(),
        ),
    ] {
        let task = tokio::spawn({
            let runtime = runtime.clone();
            let project = project.clone();
            async move {
                runtime
                    .run_shell(project, "pwd".to_string(), Some(10), cwd)
                    .await
            }
        });
        let request = wait_for_patch_agent_request(&runtime, "cwd-agent").await;
        assert_eq!(
            request.cwd.as_deref(),
            Some(expected.to_string_lossy().as_ref())
        );
        complete_patch_agent_request(&runtime, "cwd-agent", &request.request_id, 0, "", "").await;
        assert!(task.await.unwrap().success);
    }

    for cwd in [
        "../outside".to_string(),
        temp.path().to_string_lossy().to_string(),
    ] {
        let result = runtime
            .run_shell(project.clone(), "pwd".to_string(), Some(10), Some(cwd))
            .await;
        assert!(!result.success);
        assert_eq!(result.output["failure_kind"], "permission_denied");
    }
    assert!(
        probe_patch_agent_request(&runtime, "cwd-agent")
            .await
            .is_none(),
        "unsafe cwd must be rejected before Agent enqueue"
    );
}

#[tokio::test]
async fn cargo_check_failure_includes_stderr_tail_or_guidance() {
    let runtime = runtime_with_agent_project("cargo-checker");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-checker", None, caps).await;
    let project = agent_test_project_id("cargo-checker");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_check(project, None, None, None, None, None, None, Some(60))
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-checker").await;
    assert_eq!(req.command, "cargo check --all-targets");
    complete_patch_agent_request(
        &runtime,
        "cargo-checker",
        &req.request_id,
        101,
        "",
        "error: simulated compile failure\n",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or("");
    assert!(error.contains("structured validation command failed"));
    assert!(error.contains("command was started"));
    assert!(error.contains("bounded validation evidence"));
    assert_eq!(result.output["passed"], false);
    assert_eq!(result.output["execution_state"], "completed");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], true);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    assert!(result.output["stderr_tail"]
        .as_str()
        .unwrap_or("")
        .contains("simulated compile failure"));
}

#[tokio::test]
async fn cargo_test_failure_includes_stderr_tail_or_guidance() {
    let runtime = runtime_with_agent_project("cargo-tester");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-tester", None, caps).await;
    let project = agent_test_project_id("cargo-tester");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_test(
                project,
                None,
                Some("failing".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(60),
            )
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-tester").await;
    assert_eq!(req.command, "cargo test 'failing'");
    complete_patch_agent_request(
        &runtime,
        "cargo-tester",
        &req.request_id,
        101,
        "test result: FAILED. 0 passed; 1 failed\ncargo-test-stdout-tail\n",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or("");
    assert!(error.contains("structured validation command failed"));
    assert!(error.contains("command was started"));
    assert!(error.contains("bounded validation evidence"));
    assert_eq!(result.output["passed"], false);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    assert!(result.output["stdout_tail"]
        .as_str()
        .unwrap_or("")
        .contains("cargo-test-stdout-tail"));
}

#[tokio::test]
async fn cargo_test_output_includes_bounded_failed_test_diagnostics() {
    let runtime = runtime_with_agent_project("cargo-diag");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-diag", None, caps).await;
    let project = agent_test_project_id("cargo-diag");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_test(
                project,
                None,
                Some("multi_fail".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(60),
            )
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-diag").await;
    assert_eq!(req.command, "cargo test 'multi_fail'");
    complete_patch_agent_request(
        &runtime,
        "cargo-diag",
        &req.request_id,
        101,
        "running 10 tests\n\
test tests::first_failure ... FAILED\n\
test tests::second_failure ... FAILED\n\
test tests::third_failure ... FAILED\n\
\n\
failures:\n\
\n\
---- tests::first_failure stdout ----\n\
thread 'tests::first_failure' panicked at 'TOKEN=secret-value'\n\
assertion failed: left == right\n\
\n\
test result: FAILED. 7 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out\n",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["passed"], false);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    assert_eq!(result.output["tests_passed"], 7);
    assert_eq!(result.output["tests_failed"], 3);
    assert_eq!(result.output["tests_detected"], true);
    assert_eq!(result.output["tests_run_count"], 10);
    assert_eq!(result.output["zero_tests_run"], false);
    assert!(result.output["stdout_truncated"].is_boolean());
    assert!(result.output["stderr_truncated"].is_boolean());

    let diagnostics = &result.output["diagnostics"];
    assert_eq!(diagnostics["available"], true);
    assert_eq!(diagnostics["parser"], "structured_validation_parser");
    assert_eq!(diagnostics["diagnostic_count"], 3);
    assert_eq!(diagnostics["test_summary"]["passed"], 7);
    assert_eq!(diagnostics["test_summary"]["failed"], 3);
    assert_eq!(diagnostics["test_summary"]["ignored"], 1);
    assert_eq!(
        diagnostics["failed_test_details"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        diagnostics["failed_test_details"][0]["name"],
        "tests::first_failure"
    );
    assert_eq!(
        diagnostics["failed_test_details"][1]["name"],
        "tests::second_failure"
    );
    assert_eq!(
        diagnostics["failed_test_details"][2]["name"],
        "tests::third_failure"
    );
    assert_eq!(
        diagnostics["failed_test_details"][0]["failure_kind"],
        "panic"
    );
    assert!(diagnostics["failed_test_details"][0]["file"].is_null());
    assert_eq!(diagnostics["failed_test_details_truncated"], false);
    assert_eq!(diagnostics["truncated"], false);

    let diagnostics_json = diagnostics.to_string();
    for raw in ["TOKEN=secret-value", "assertion failed", "left == right"] {
        assert!(
            !diagnostics_json.contains(raw),
            "cargo_test diagnostics must not include unsafe text {raw:?}: {diagnostics_json}"
        );
    }
}

#[tokio::test]
async fn cargo_test_passing_output_includes_empty_failed_test_details_diagnostics() {
    let runtime = runtime_with_agent_project("cargo-pass-diag");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-pass-diag", None, caps).await;
    let project = agent_test_project_id("cargo-pass-diag");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_test(
                project,
                None,
                Some("all_ok".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(60),
            )
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-pass-diag").await;
    complete_patch_agent_request(
        &runtime,
        "cargo-pass-diag",
        &req.request_id,
        0,
        "running 14 tests\n\
test result: ok. 12 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out\n",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["passed"], true);
    assert_eq!(result.output["tests_passed"], 12);
    assert_eq!(result.output["tests_failed"], 0);
    assert_eq!(result.output["tests_detected"], true);
    assert_eq!(result.output["tests_run_count"], 12);
    assert_eq!(result.output["zero_tests_run"], false);

    let diagnostics = &result.output["diagnostics"];
    assert_eq!(diagnostics["available"], true);
    assert_eq!(diagnostics["diagnostic_count"], 0);
    assert_eq!(diagnostics["failed_test_details"], json!([]));
    assert_eq!(diagnostics["failed_test_details_truncated"], false);
    assert_eq!(diagnostics["test_summary"]["passed"], 12);
    assert_eq!(diagnostics["test_summary"]["failed"], 0);
}

#[tokio::test]
async fn cargo_test_multi_harness_counts_match_diagnostics_summary() {
    let runtime = runtime_with_agent_project("cargo-multi-harness");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-multi-harness", None, caps).await;
    let project = agent_test_project_id("cargo-multi-harness");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_test(
                project,
                None,
                Some("multi_harness".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(60),
            )
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-multi-harness").await;
    complete_patch_agent_request(
        &runtime,
        "cargo-multi-harness",
        &req.request_id,
        101,
        "running 2 tests\n\
test result: ok. 2 passed; 0 failed; 1 ignored\n\
running 4 tests\n\
test tests::broken ... FAILED\n\
test result: FAILED. 3 passed; 1 failed; 0 ignored\n\
running 0 tests\n\
test result: ok. 0 passed; 0 failed; 2 ignored\n",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    // Top-level counts (full combined output) and diagnostics (bounded tails)
    // must agree when every summary is still present in the tails.
    assert_eq!(result.output["tests_run_count"], 6);
    assert_eq!(result.output["tests_passed"], 5);
    assert_eq!(result.output["tests_failed"], 1);
    let diagnostics = &result.output["diagnostics"];
    assert_eq!(diagnostics["available"], true);
    assert_eq!(diagnostics["test_summary"]["passed"], 5);
    assert_eq!(diagnostics["test_summary"]["failed"], 1);
    assert_eq!(diagnostics["test_summary"]["ignored"], 3);
    assert_eq!(diagnostics["diagnostic_count"], 1);
    assert_eq!(
        diagnostics["failed_test_details"][0]["name"],
        "tests::broken"
    );
    assert_eq!(diagnostics["failed_test_details_truncated"], false);
}

#[tokio::test]
async fn cargo_test_agent_timeout_is_not_validation_failed() {
    let runtime = runtime_with_agent_project("cargo-timeout");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-timeout", None, caps).await;
    let project = agent_test_project_id("cargo-timeout");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_test(
                project,
                None,
                Some("slow".to_string()),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(60),
            )
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-timeout").await;
    assert_eq!(req.command, "cargo test 'slow'");
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: "cargo-timeout".to_string(),
            runner_instance_id: "inst".to_string(),
            request_id: req.request_id,
            exit_code: Some(-1),
            stdout: Some("partial cargo output\n".to_string()),
            stderr: Some("Command timed out after 60 seconds".to_string()),
            duration_ms: Some(60_000),
            error: Some("command timed out".to_string()),
        })
        .await
        .unwrap();

    let result = task.await.unwrap();
    assert!(!result.success);
    assert_eq!(result.output["execution_state"], "timed_out");
    assert_eq!(result.output["command_started"], true);
    assert_eq!(result.output["command_completed"], false);
    assert_eq!(result.output["failure_kind"], "timeout");
}

#[tokio::test]
async fn cargo_fmt_failure_includes_stderr_tail_or_guidance() {
    let runtime = runtime_with_agent_project("cargo-formatter");
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "cargo-formatter", None, caps).await;
    let project = agent_test_project_id("cargo-formatter");
    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .cargo_fmt(project, None, Some(true), Some(60))
            .await
    });
    let req = wait_for_patch_agent_request(&runtime, "cargo-formatter").await;
    assert_eq!(req.command, "cargo fmt -- --check");
    complete_patch_agent_request(
        &runtime,
        "cargo-formatter",
        &req.request_id,
        1,
        "Diff in src/lib.rs\n",
        "",
    )
    .await;
    let result = task.await.unwrap();
    assert!(!result.success);
    let error = result.error.as_deref().unwrap_or("");
    assert!(error.contains("structured validation command failed"));
    assert!(error.contains("command was started"));
    assert!(error.contains("bounded validation evidence"));
    assert_eq!(result.output["passed"], false);
    assert_eq!(result.output["failure_kind"], "validation_failed");
    assert!(result.output["stdout_tail"].is_string());
    assert!(result.output["stderr_tail"].is_string());
}

#[test]
fn cleanup_paths_match_sensitive_directories_by_complete_component() {
    let root = vec![".".to_string()];
    assert!(validate_limited_cleanup_paths(&root, true).is_err());

    for path in [
        "runner.toml",
        "agent.toml",
        ".env",
        ".git/config",
        "target",
        "target/debug",
        "foo/target/cache",
        "./target/release",
        "TARGET/release/output",
    ] {
        let paths = vec![path.to_string()];
        assert!(
            validate_limited_cleanup_paths(&paths, true).is_err(),
            "sensitive path should be rejected: {path}"
        );
    }

    for path in [
        "SMOKE_TARGET.txt",
        "targeting.rs",
        "build_target.md",
        "my-target.txt",
        "not_target/file.rs",
        "target-file.txt",
        "src/targeting/mod.rs",
    ] {
        let paths = vec![path.to_string()];
        assert_eq!(
            validate_limited_cleanup_paths(&paths, true).unwrap(),
            paths,
            "ordinary target substring should be allowed: {path}"
        );
    }
}

#[test]
fn project_management_tools_require_expected_fields() {
    for spec in registered_tool_specs() {
        let expected: &[&str] = match spec.name.as_str() {
            "register_project" | "create_project" => &["client_id", "id", "name", "path"],
            "unregister_project" => &["project", "expected_revision"],
            _ => continue,
        };
        let required = spec.input_schema["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{} must have required array", spec.name));
        for field in expected {
            assert!(
                required.iter().any(|v| v == field),
                "{} input_schema must require '{}'",
                spec.name,
                field
            );
        }
    }
}

#[tokio::test]
async fn register_project_crosses_historical_64_threshold_and_is_immediately_resolvable() {
    let runtime = test_runtime();
    let client_id = "project-scale-mutation";
    let existing = (0..64)
        .map(|index| {
            registered_project(
                &format!("project-{index:04}"),
                &format!("/tmp/project-{index:04}"),
            )
        })
        .collect::<Vec<_>>();
    register_agent_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities::default(),
        existing,
    )
    .await;

    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        let bootstrap = bootstrap_auth_context();
        runtime_for_task
            .register_project(
                client_id.to_string(),
                "project-0064".to_string(),
                "Project 0064".to_string(),
                "/tmp/project-0064".to_string(),
                None,
                true,
                false,
                Some(&bootstrap),
            )
            .await
    });
    let request =
        wait_for_runner_request_for_instance(&runtime, client_id, &format!("inst-{client_id}"))
            .await;
    assert_eq!(request.kind, "register_project");
    let authoritative = json!({
        "outcome": "registered",
        "changed": true,
        "client_id": client_id,
        "agent_project_id": "project-0064",
        "name": "Project 0064",
        "path": "/tmp/project-0064",
        "allow_patch": true,
        "revision": format!("sha256:{}", "a".repeat(64))
    });
    complete_patch_agent_request_for_instance(
        &runtime,
        client_id,
        &format!("inst-{client_id}"),
        &request.request_id,
        0,
        &authoritative.to_string(),
        "",
    )
    .await;

    let result = task.await.unwrap();
    assert!(
        result.success,
        "authoritative projection should commit: {result:?}"
    );
    let projects = runtime
        .runner_registry
        .list_runner_projects(client_id)
        .await
        .unwrap();
    assert_eq!(projects.len(), 65);
    let projected = projects
        .iter()
        .find(|project| project.id == "project-0064")
        .expect("successful register_project must be immediately Server-resolvable");
    assert_eq!(
        projected.revision,
        authoritative["revision"].as_str().map(str::to_string)
    );
    assert!(
        probe_agent_request_for_client(&runtime, client_id)
            .await
            .is_none(),
        "successful projection must not replay the mutation"
    );
}

#[tokio::test]
async fn register_project_projection_failure_returns_reconcile_required_without_retrying_mutation()
{
    let runtime = test_runtime();
    let client_id = "project-projection-failure";
    register_agent_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities::default(),
        Vec::new(),
    )
    .await;

    let runtime_for_task = runtime.clone();
    let task = tokio::spawn(async move {
        let bootstrap = bootstrap_auth_context();
        runtime_for_task
            .register_project(
                client_id.to_string(),
                "persisted-on-runner".to_string(),
                "Persisted on Runner".to_string(),
                "/tmp/persisted-on-runner".to_string(),
                None,
                true,
                false,
                Some(&bootstrap),
            )
            .await
    });
    let request =
        wait_for_runner_request_for_instance(&runtime, client_id, &format!("inst-{client_id}"))
            .await;
    assert_eq!(request.kind, "register_project");
    let authoritative = json!({
        "outcome": "registered",
        "changed": true,
        "client_id": client_id,
        "agent_project_id": "persisted-on-runner",
        "name": "Persisted on Runner",
        // Simulate a successful Runner persistence followed by a Server-side
        // projection validation failure. The mutation itself must not be replayed.
        "path": format!("/{}", "x".repeat(4096)),
        "allow_patch": true,
        "revision": format!("sha256:{}", "b".repeat(64))
    });
    complete_patch_agent_request_for_instance(
        &runtime,
        client_id,
        &format!("inst-{client_id}"),
        &request.request_id,
        0,
        &authoritative.to_string(),
        "",
    )
    .await;

    let result = task.await.unwrap();
    assert!(
        !result.success,
        "projection failure must never return ordinary success"
    );
    assert_eq!(
        result.error.as_deref(),
        Some("project_projection_reconcile_required")
    );
    assert_eq!(result.output["failure_kind"], "reconcile_required");
    assert_eq!(
        result.output["reason_code"],
        "server_project_projection_failed"
    );
    assert_eq!(result.output["state_changed"], true);
    assert_eq!(result.output["agent_project_id"], "persisted-on-runner");
    assert_eq!(result.output["revision"], authoritative["revision"]);
    assert_eq!(
        result.output["reconcile"]["action"],
        "observe_exact_project_revision_before_retry"
    );
    assert!(
        runtime
            .runner_registry
            .list_runner_projects(client_id)
            .await
            .unwrap()
            .is_empty(),
        "failed Server projection must not falsely advertise routing"
    );
    assert!(
        probe_agent_request_for_client(&runtime, client_id)
            .await
            .is_none(),
        "uncertain post-persist state must not blind-retry register_project"
    );
}

#[tokio::test]
async fn dispatch_register_project_rejects_unknown_client_id() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::RegisterProject {
            client_id: "no-such-agent".to_string(),
            id: "my-project".to_string(),
            name: "My Project".to_string(),
            path: "/root/git/my-project".to_string(),
            description: None,
            allow_patch: true,
            overwrite: false,
        })
        .await;
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unknown Runner"),
        "register_project should reject unknown client_id: {:?}",
        result.error
    );
}

#[tokio::test]
async fn dispatch_unregister_project_reuses_lifecycle_validation_without_project_preresolution() {
    let runtime = test_runtime();
    let revision = format!("sha256:{}", "a".repeat(64));
    let result = runtime
        .dispatch(ToolCall::UnregisterProject {
            project: "agent:no-such-agent:demo".to_string(),
            expected_revision: revision,
        })
        .await;
    assert!(!result.success);
    assert_eq!(result.output["error"]["code"], "agent_unavailable");

    let invalid = runtime
        .dispatch(ToolCall::UnregisterProject {
            project: "agent:no-such-agent:demo".to_string(),
            expected_revision: "stale".to_string(),
        })
        .await;
    assert!(!invalid.success);
    assert_eq!(invalid.output["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn dispatch_unregister_project_removes_server_inventory_after_terminal_runner_success() {
    let runtime = test_runtime();
    let client_id = "unregister-success";
    let revision = format!("sha256:{}", "a".repeat(64));
    let mut summary = registered_project("demo", "/tmp/demo");
    summary.revision = Some(revision.clone());
    register_agent_projects(
        &runtime,
        client_id,
        None,
        RunnerCapabilities {
            project_lifecycle: true,
            ..Default::default()
        },
        vec![summary],
    )
    .await;
    let project = crate::tool_runtime::runner_project_runtime_id(client_id, "demo");

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let revision = revision.clone();
        async move {
            runtime
                .dispatch(ToolCall::UnregisterProject {
                    project,
                    expected_revision: revision,
                })
                .await
        }
    });
    let request = wait_for_runner_request_for_client(&runtime, client_id).await;
    assert_eq!(request.kind, "project_lifecycle_unregister");
    let payload: serde_json::Value =
        serde_json::from_str(request.stdin.as_deref().unwrap()).unwrap();
    assert_eq!(payload["project_id"], "demo");
    assert_eq!(payload["expected_revision"], revision);
    complete_patch_agent_request_for_instance(
        &runtime,
        client_id,
        &format!("inst-{client_id}"),
        &request.request_id,
        0,
        &json!({
            "operation": "unregister",
            "agent_project_id": "demo",
            "outcome": "unregistered",
            "changed": true,
            "revision": serde_json::Value::Null
        })
        .to_string(),
        "",
    )
    .await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], project);
    assert_eq!(result.output["outcome"], "unregistered");
    assert_eq!(result.output["changed"], true);
    let client = runtime
        .runner_registry
        .get_runner_view(client_id)
        .await
        .expect("Runner should remain registered");
    assert!(
        client.projects.iter().all(|entry| entry.id != "demo"),
        "terminal unregister must remove only the Server project inventory entry"
    );
}

#[tokio::test]
async fn dispatch_create_project_rejects_unknown_client_id() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::CreateProject {
            client_id: "no-such-agent".to_string(),
            id: "hello".to_string(),
            name: "Hello".to_string(),
            path: "/root/git/hello".to_string(),
            description: None,
            allow_patch: true,
            template: None,
            git_init: false,
            adopt_existing_empty: false,
            overwrite: false,
        })
        .await;
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unknown Runner"),
        "create_project should reject unknown client_id: {:?}",
        result.error
    );
}

#[tokio::test]
async fn dispatch_register_project_rejects_unsafe_id() {
    let runtime = test_runtime();
    for bad_id in ["", "a/b", "a\\b", "..", "a..b", "a\0b"] {
        let result = runtime
            .dispatch(ToolCall::RegisterProject {
                client_id: "oe".to_string(),
                id: bad_id.to_string(),
                name: "Test".to_string(),
                path: "/root/git/test".to_string(),
                description: None,
                allow_patch: true,
                overwrite: false,
            })
            .await;
        assert!(
            !result.success,
            "register_project should reject unsafe id '{:?}'",
            bad_id
        );
    }
}

#[tokio::test]
async fn dispatch_create_project_rejects_relative_path() {
    let runtime = test_runtime();
    let result = runtime
        .dispatch(ToolCall::CreateProject {
            client_id: "oe".to_string(),
            id: "hello".to_string(),
            name: "Hello".to_string(),
            path: "relative/path".to_string(),
            description: None,
            allow_patch: true,
            template: None,
            git_init: false,
            adopt_existing_empty: false,
            overwrite: false,
        })
        .await;
    assert!(!result.success);
    assert!(
        result.error.as_deref().unwrap_or("").contains("absolute"),
        "create_project should reject relative path: {:?}",
        result.error
    );
}

#[tokio::test]
async fn mutating_dispatch_feeds_the_activity_recorder() {
    #[derive(Default)]
    struct CapturingRecorder(
        #[allow(clippy::type_complexity)]
        std::sync::Mutex<
            Vec<(
                String,
                bool,
                Option<String>,
                Vec<String>,
                String,
                Option<String>,
                Option<String>,
                crate::tool_runtime::activity::ActivityScope,
            )>,
        >,
    );
    impl crate::tool_runtime::activity::ActivityRecorder for CapturingRecorder {
        fn record(&self, record: crate::tool_runtime::activity::ActivityRecord<'_>) {
            self.0.lock().unwrap().push((
                record.tool.to_string(),
                record.success,
                record.command.map(str::to_string),
                record.paths.clone(),
                record.surface.to_string(),
                record.project.map(str::to_string),
                record.client.map(str::to_string),
                record.scope,
            ));
        }
    }
    let recorder = std::sync::Arc::new(CapturingRecorder::default());
    let runtime =
        runtime_with_agent_project("activity-shell").with_activity_recorder(recorder.clone());
    let caps = RunnerCapabilities {
        shell: true,
        ..Default::default()
    };
    register_agent(&runtime, "activity-shell", None, caps).await;
    let project = agent_test_project_id("activity-shell");

    // A successful mutating execution lands in the ledger with its raw
    // command (the durable store truncates it to a preview and honors the
    // operator's config switch).
    let shell_task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project,
                        command: "echo activity-probe".to_string(),
                        session_id: None,
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "activity-shell").await;
    complete_patch_agent_request(&runtime, "activity-shell", &request.request_id, 0, "ok", "")
        .await;
    let shell = shell_task.await.unwrap();
    assert!(shell.success, "{:?}", shell.error);

    // The runtime also accepts a unique short project id. Activity must record
    // the canonical agent project and client, not the raw alias; otherwise the
    // project-scoped console would hide this successful call as client-less.
    let alias_task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::RunShell {
                        project: "agent-proj".to_string(),
                        command: "echo activity-alias".to_string(),
                        session_id: None,
                        timeout_secs: Some(30),
                        cwd: None,
                        purpose: None,
                        shell: None,
                    },
                    Some(&bootstrap),
                )
                .await
        }
    });
    let request = wait_for_patch_agent_request(&runtime, "activity-shell").await;
    complete_patch_agent_request(&runtime, "activity-shell", &request.request_id, 0, "ok", "")
        .await;
    let alias = alias_task.await.unwrap();
    assert!(alias.success, "{:?}", alias.error);

    // Read-only calls never reach the recorder.
    let list = runtime
        .dispatch(ToolCall::from_tool_name("list_tools", json!({})).unwrap())
        .await;
    assert!(list.success);

    let records = recorder.0.lock().unwrap();
    assert_eq!(records.len(), 2, "only the two mutating calls are recorded");
    for (index, expected_command) in ["echo activity-probe", "echo activity-alias"]
        .into_iter()
        .enumerate()
    {
        let (tool, success, command, paths, surface, recorded_project, client, scope) =
            &records[index];
        assert_eq!(tool, "run_shell");
        assert!(success);
        assert_eq!(command.as_deref(), Some(expected_command));
        assert!(paths.is_empty());
        assert_eq!(surface, "api");
        assert_eq!(recorded_project.as_deref(), Some(project.as_str()));
        assert_eq!(client.as_deref(), Some("activity-shell"));
        assert_eq!(
            scope,
            &crate::tool_runtime::activity::ActivityScope::HostGlobal,
            "activity scope must come from the verified bootstrap auth"
        );
    }

    // Path extraction and mutating classification are pinned at the capture
    // level: edits carry their sanitized paths, reads yield no context.
    let edit = ToolCall::from_tool_name(
        "delete_project_files",
        json!({"project": "agent:oe:demo", "paths": ["a.rs", "b.rs"]}),
    )
    .unwrap();
    assert_eq!(edit.command_text(), None);
    let shell_call = ToolCall::from_tool_name(
        "run_shell",
        json!({"project": "demo", "command": "echo hi"}),
    )
    .unwrap();
    assert_eq!(shell_call.command_text(), Some("echo hi"));
}
