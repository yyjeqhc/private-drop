//! Trusted-agent fluency smoke: a realistic inspect → edit → validate(fail)
//! → fix → validate(pass) → git review → finish chain in a disposable git
//! fixture, asserting zero human approval interruptions and bounded payloads.
//!
//! Records baseline counters (tool calls, poll calls, payload bytes) as test
//! output; this is test/baseline data, not product telemetry.

use super::support::*;
use crate::tool_runtime::tool_inputs::ExecutionPurpose;
use crate::tool_runtime::{ToolCall, ToolResult, ToolRuntime};
use serde_json::Value;
use sha2::Digest;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CLIENT: &str = "smoke-agent";

/// Service any pending fake-agent request locally: shell/git commands run via
/// `sh -c`; native file writes actually write into the fixture repo.
async fn dispatch_with_local_agent(
    runtime: &ToolRuntime,
    call: ToolCall,
    poll_calls: &Arc<AtomicUsize>,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime.dispatch_with_auth(call, Some(&bootstrap)).await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !task.is_finished() {
        assert!(
            Instant::now() < deadline,
            "trusted smoke dispatch did not finish within the 10-second test deadline"
        );
        poll_calls.fetch_add(1, Ordering::SeqCst);
        let request = runtime
            .runner_registry
            .poll(crate::runner_protocol::RunnerPollRequest {
                client_id: CLIENT.to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap();
        let Some(req) = request else {
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        };
        let (exit_code, stdout, stderr) = if req.kind == "file_write_project_file" {
            let payload: Value =
                serde_json::from_str(req.content.as_deref().expect("file-op payload")).unwrap();
            let path = payload["path"].as_str().expect("write path");
            let content = payload["content"].as_str().expect("write content");
            let root = std::path::PathBuf::from(req.cwd.as_deref().expect("write cwd"));
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&target, content).unwrap();
            let sha256 = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
            (
                0,
                serde_json::json!({
                    "path": path,
                    "bytes_written": content.len(),
                    "sha256": sha256,
                    "changed": true,
                    "state_changed": true,
                    "execution_state": "completed",
                })
                .to_string(),
                String::new(),
            )
        } else {
            run_runner_shell_request_locally(&req)
        };
        complete_patch_agent_request(
            runtime,
            CLIENT,
            &req.request_id,
            exit_code,
            &stdout,
            &stderr,
        )
        .await;
    }
    task.await.unwrap()
}

fn assert_no_approval_interruption(result: &ToolResult, step: &str) {
    let text = serde_json::to_string(&result.output).unwrap();
    assert!(
        !text.contains("permission_denied"),
        "{step} hit a permission interruption: {text}"
    );
    assert!(
        !text.contains("\"approval_required\""),
        "{step} hit an approval interruption: {text}"
    );
    if let Some(permission) = result.output.get("permission") {
        assert_eq!(
            permission["status"], "auto_approved",
            "{step} consequential call must auto-authorize under trusted_agent"
        );
        assert_eq!(permission["reason"], "trusted_agent_authority", "{step}");
    }
}

#[tokio::test]
async fn trusted_agent_smoke_full_chain_has_zero_approval_interruptions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fixture");
    std::fs::create_dir_all(&root).unwrap();
    init_git_repo(&root);
    commit_file(
        &root,
        "README.md",
        "# fixture\n",
        "chore: seed fixture repo",
    );
    // Pre-existing local edit: the dirty worktree must stay advisory.
    std::fs::write(root.join("NOTES.txt"), "existing user work\n").unwrap();

    let runtime = test_runtime();
    let project = register_runner_project_at_path(&runtime, CLIENT, "fixture", &root).await;
    let poll_calls = Arc::new(AtomicUsize::new(0));

    let mut total_tool_calls = 0usize;
    let mut approval_interruptions = 0usize;
    let mut returned_payload_bytes = 0usize;
    let mut maximum_single_response_bytes = 0usize;
    let mut track = |result: &ToolResult| {
        total_tool_calls += 1;
        let bytes = serde_json::to_string(&result.output)
            .map(|s| s.len())
            .unwrap_or(0);
        returned_payload_bytes += bytes;
        maximum_single_response_bytes = maximum_single_response_bytes.max(bytes);
        if serde_json::to_string(&result.output)
            .unwrap_or_default()
            .contains("permission_denied")
        {
            approval_interruptions += 1;
        }
    };

    // 1. Startup: one call, no manifest/runtime probing required afterwards.
    let start = dispatch_with_local_agent(
        &runtime,
        ToolCall::WorkOnProject {
            project: project.clone(),
            client_id: None,
            path: None,
            mode: None,
            base_ref: None,
            instruction: "trusted agent smoke".to_string(),
            session_id: None,
            include_project_instructions: true,
            include_workflow_guidance: true,
            include_extension_catalog: false,
        },
        &poll_calls,
    )
    .await;
    track(&start);
    assert!(start.success, "{:?}", start.error);
    assert_no_approval_interruption(&start, "work_on_project");
    let session_id = start.output["session_id"].as_str().unwrap().to_string();
    // Dirty worktree is advisory context, not a blocker. The canonical compact
    // entry intentionally omits the old full diagnostic startup_verdict.
    assert!(start.output.get("startup_verdict").is_none());
    assert_ne!(start.output["readiness"]["status"], "fail");

    // 2. Edit: add a script whose assertion initially fails.
    let write = dispatch_with_local_agent(
        &runtime,
        ToolCall::WriteProjectFile {
            project: project.clone(),
            path: "check.sh".to_string(),
            content: "#!/bin/sh\ntest -f marker.txt\n".to_string(),
            session_id: Some(session_id.clone()),
            overwrite: None,
            expected_sha256: None,
        },
        &poll_calls,
    )
    .await;
    track(&write);
    assert!(write.success, "{:?}", write.error);
    assert_no_approval_interruption(&write, "write_project_file");

    // 3. Generic shell validation fails first.
    let failing = dispatch_with_local_agent(
        &runtime,
        ToolCall::RunShell {
            project: project.clone(),
            command: "sh check.sh".to_string(),
            session_id: Some(session_id.clone()),
            timeout_secs: Some(30),
            cwd: None,
            purpose: Some(ExecutionPurpose::Validation),
            shell: None,
        },
        &poll_calls,
    )
    .await;
    track(&failing);
    assert_no_approval_interruption(&failing, "run_shell (failing validation)");
    assert_ne!(
        failing.output["exit_code"], 0,
        "first validation run must fail: {:?}",
        failing.output
    );

    // 4. Agent fixes the issue.
    let fix = dispatch_with_local_agent(
        &runtime,
        ToolCall::WriteProjectFile {
            project: project.clone(),
            path: "marker.txt".to_string(),
            content: "present\n".to_string(),
            session_id: Some(session_id.clone()),
            overwrite: None,
            expected_sha256: None,
        },
        &poll_calls,
    )
    .await;
    track(&fix);
    assert!(fix.success, "{:?}", fix.error);
    assert_no_approval_interruption(&fix, "write_project_file (fix)");

    // 5. Same assertion now passes.
    let passing = dispatch_with_local_agent(
        &runtime,
        ToolCall::RunShell {
            project: project.clone(),
            command: "sh check.sh".to_string(),
            session_id: Some(session_id.clone()),
            timeout_secs: Some(30),
            cwd: None,
            purpose: Some(ExecutionPurpose::Validation),
            shell: None,
        },
        &poll_calls,
    )
    .await;
    track(&passing);
    assert!(passing.success, "{:?}", passing.error);
    assert_no_approval_interruption(&passing, "run_shell (passing validation)");
    assert_eq!(passing.output["exit_code"], 0);

    // 6. Git review.
    let changes = dispatch_with_local_agent(
        &runtime,
        ToolCall::ShowChanges {
            project: project.clone(),
            session_id: Some(session_id.clone()),
            include_diff: Some(false),
            max_hunks: None,
            max_hunk_lines: None,
            session_event_limit: Some(50),
        },
        &poll_calls,
    )
    .await;
    track(&changes);
    assert!(changes.success, "{:?}", changes.error);
    assert_no_approval_interruption(&changes, "show_changes");

    // 7. Finish: one fact package usable for the final report.
    let finish = dispatch_with_local_agent(
        &runtime,
        ToolCall::FinishCodingTask {
            project: project.clone(),
            session_id: session_id.clone(),
            summary_only: false,
            include_diff: Some(false),
            include_workspace: Some(true),
            include_hygiene: Some(false),
            include_handoff: Some(true),
            include_validation_summary: Some(true),
        },
        &poll_calls,
    )
    .await;
    track(&finish);
    assert!(finish.success, "{:?}", finish.error);
    assert_no_approval_interruption(&finish, "finish_coding_task");

    // Generic shell validation is recognized as execution evidence, the
    // early failure is preserved as resolved, and nothing blocks.
    assert_eq!(
        finish.output["hard_blockers"],
        serde_json::json!([]),
        "resolved validation + dirty worktree must not produce hard blockers: {}",
        finish.output
    );
    assert_eq!(finish.output["task_outcome"]["blocking"], false);
    let finish_text = serde_json::to_string(&finish.output).unwrap();
    assert!(
        finish_text.contains("resolved"),
        "finish evidence must record the resolved earlier failure: {finish_text}"
    );
    assert!(
        finish.output.get("facts").is_some(),
        "finish must return the canonical fact package"
    );
    // Session permission summary: only auto-authorized decisions, none pending.
    assert_eq!(finish.output["permissions"]["pending_count"], 0);
    assert_eq!(finish.output["permissions"]["denied_count"], 0);
    assert_eq!(finish.output["permissions"]["policy"], "trusted_agent");

    assert_eq!(
        approval_interruptions, 0,
        "trusted_agent smoke must run with zero approval interruptions"
    );
    // Bounded conversation: 7 calls, no re-discovery, bounded payloads.
    assert_eq!(total_tool_calls, 7);
    assert!(
        maximum_single_response_bytes < 256 * 1024,
        "single response exceeded bound: {maximum_single_response_bytes}"
    );

    println!(
        "trusted_agent_smoke_baseline total_tool_calls={total_tool_calls} \
         approval_interruptions={approval_interruptions} \
         poll_calls={} returned_payload_bytes={returned_payload_bytes} \
         maximum_single_response_bytes={maximum_single_response_bytes}",
        poll_calls.load(Ordering::SeqCst)
    );
}
