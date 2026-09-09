use crate::*;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use webcodex_runner_registry::{RunnerAccess, RunnerAccessGroup, RunnerRegistry};
use webcodex_store::Database;

const GRANT: &str = "wc_pgrant_1111111111111111";

#[derive(Default)]
struct ScriptedHost {
    invokes: AtomicUsize,
    registrations: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    invoke_error: Mutex<Option<ConnectorToolFailure>>,
    register_error: Mutex<Option<ConnectorJobHostError>>,
    start_error: Mutex<Option<ConnectorJobHostError>>,
}

impl ConnectorExecutionHost for ScriptedHost {
    fn invoke_tool(
        &self,
        _request: ConnectorToolRequest,
    ) -> ConnectorHostFuture<'_, Result<Value, ConnectorToolFailure>> {
        self.invokes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.invoke_error.lock().unwrap().take() {
                Some(error) => Err(error),
                None => {
                    Ok(json!({"changed": false, "files": [], "matches": [], "truncated": false}))
                }
            }
        })
    }

    fn register_isolated_project(
        &self,
        _request: ConnectorProjectRegistration,
    ) -> ConnectorHostFuture<'_, Result<(), ConnectorJobHostError>> {
        self.registrations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match self.register_error.lock().unwrap().take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    fn start_execution_job(
        &self,
        _request: ConnectorJobRequest,
    ) -> ConnectorHostFuture<'_, Result<ConnectorJobSubmission, ConnectorJobHostError>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(error) = self.start_error.lock().unwrap().take() {
                return Err(error);
            }
            Err(ConnectorJobHostError::Rejected(Some(
                "scripted executor rejection".to_string(),
            )))
        })
    }

    fn stop_execution_job(
        &self,
        _project: String,
        _job_id: String,
    ) -> ConnectorHostFuture<'_, Result<(), ConnectorJobHostError>> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    runtime: ConnectorRuntime,
    host: Arc<ScriptedHost>,
    owner: ConnectorCallContext,
}

impl Fixture {
    async fn call(&self, capability: &str, arguments: Value) -> ConnectorCallOutcome {
        self.runtime
            .call_for_window(
                capability,
                arguments,
                Some(&self.owner),
                ConnectorTransport::Mcp,
                None,
            )
            .await
    }

    async fn call_window(
        &self,
        capability: &str,
        arguments: Value,
        window: &ConnectorWindowId,
    ) -> ConnectorCallOutcome {
        self.runtime
            .call_for_window(
                capability,
                arguments,
                Some(&self.owner),
                ConnectorTransport::Mcp,
                Some(window),
            )
            .await
    }
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    init_repo(&project);
    let state = temp.path().join("state");
    let host = Arc::new(ScriptedHost::default());
    let runtime = ConnectorRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(Database::open(&temp.path().join("connector.db")).unwrap()),
        ConnectorContext {
            project_id: "wc_proj_1234567890".into(),
            project_name: "project".into(),
            workspace_id: "wc_ws_1234567890".into(),
            executor_project: "agent:hosted:project".into(),
            executor_root: project.to_string_lossy().into_owned(),
            runs_root: state.join("runs").to_string_lossy().into_owned(),
            results_root: state.join("results").to_string_lossy().into_owned(),
            project_registry_dir: state
                .join("agent/project-registry")
                .to_string_lossy()
                .into_owned(),
            profile: "personal".into(),
            project_grant_id: GRANT.into(),
        },
    )
    .unwrap();
    let owner = call_context("project:wc_pgrant_1111111111111111", GRANT, host.clone());
    Fixture {
        _temp: temp,
        runtime,
        host,
        owner,
    }
}

fn call_context(subject: &str, grant: &str, host: Arc<ScriptedHost>) -> ConnectorCallContext {
    ConnectorCallContext {
        access: ConnectorAccess {
            principal: ConnectorPrincipalId::new(subject.to_string()).unwrap(),
            project_grant_id: Some(grant.to_string()),
            bootstrap: false,
            global_admin: false,
            permissions: ConnectorPermissions {
                runtime_read: true,
                project_read: true,
                project_write: true,
                job_run: true,
            },
            runner_access: RunnerAccess {
                global_visibility: false,
                owner_bypass: false,
                username: Some("owner".to_string()),
                group: Some(RunnerAccessGroup::ProjectGrant(grant.to_string())),
            },
        },
        execution_authority: ConnectorExecutionAuthority {
            auto_authorize: true,
            mode: "trusted_agent".to_string(),
            source: "test".to_string(),
            resolved_rule: "trusted_agent_authority".to_string(),
        },
        host,
    }
}

fn window(value: &str) -> ConnectorWindowId {
    ConnectorWindowId::new(value.to_string(), "test".to_string()).unwrap()
}

fn init_repo(project: &Path) {
    std::fs::create_dir(project).unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "core.autocrlf", "false"]);
    std::fs::write(project.join("README.md"), "fixture\n").unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    run(&["add", "."]);
    run(&[
        "-c",
        "user.name=WebCodex Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "initial",
    ]);
}

async fn start_read_only(fx: &Fixture, goal: &str) -> ConnectorCallOutcome {
    fx.call("task_start", json!({"goal": goal, "mode": "read_only"}))
        .await
}

async fn start_normal(fx: &Fixture) -> ConnectorCallOutcome {
    fx.call(
        "task_start",
        json!({"goal": "normal work", "mode": "normal"}),
    )
    .await
}

#[tokio::test]
async fn unknown_capability_is_protocol_error_before_auth() {
    let fx = fixture();
    let out = fx
        .runtime
        .call_for_window("missing", json!({}), None, ConnectorTransport::Mcp, None)
        .await;
    assert_eq!(out.http_status, 400);
    assert!(out.protocol_error);
}

#[tokio::test]
async fn known_capability_requires_authentication() {
    let fx = fixture();
    let out = fx
        .runtime
        .call_for_window("task_list", json!({}), None, ConnectorTransport::Mcp, None)
        .await;
    assert_eq!(out.http_status, 401);
    assert_eq!(out.body["error"]["code"], "authentication_required");
}

#[tokio::test]
async fn foreign_project_grant_is_rejected() {
    let fx = fixture();
    let foreign = call_context(
        "project:wc_pgrant_2222222222222222",
        "wc_pgrant_2222222222222222",
        fx.host.clone(),
    );
    let out = fx
        .runtime
        .call_for_window(
            "task_list",
            json!({}),
            Some(&foreign),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(out.http_status, 403);
    assert_eq!(out.body["error"]["code"], "project_credential_rejected");
}

#[tokio::test]
async fn missing_capability_permission_fails_before_host() {
    let fx = fixture();
    let mut access = fx.owner.clone();
    access.access.permissions.project_read = false;
    let out = fx
        .runtime
        .call_for_window(
            "task_list",
            json!({}),
            Some(&access),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(
        out.required_permission,
        Some(ConnectorPermission::ProjectRead)
    );
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn read_only_task_start_never_registers_runner_project() {
    let fx = fixture();
    let out = start_read_only(&fx, "inspect").await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(fx.host.registrations.load(Ordering::SeqCst), 0);
    assert_eq!(out.body["data"]["mode"], "read_only");
}

#[tokio::test]
async fn normal_task_start_requires_project_write() {
    let fx = fixture();
    let mut context = fx.owner.clone();
    context.access.permissions.project_write = false;
    let out = fx
        .runtime
        .call_for_window(
            "task_start",
            json!({"goal": "write", "mode": "normal"}),
            Some(&context),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(
        out.required_permission,
        Some(ConnectorPermission::ProjectWrite)
    );
    assert_eq!(fx.host.registrations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn normal_task_registers_exactly_one_isolated_project() {
    let fx = fixture();
    let out = start_normal(&fx).await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(fx.host.registrations.load(Ordering::SeqCst), 1);
    assert_eq!(out.body["data"]["mode"], "normal");
}

#[tokio::test]
async fn registration_failure_unwinds_and_allows_retry() {
    let fx = fixture();
    *fx.host.register_error.lock().unwrap() = Some(ConnectorJobHostError::Rejected(Some(
        "registration denied".to_string(),
    )));
    let first = start_normal(&fx).await;
    assert_eq!(first.body["error"]["code"], "workspace_preparation_failed");
    let second = start_normal(&fx).await;
    assert!(second.ok, "{}", second.body);
    assert_eq!(fx.host.registrations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn same_explicit_window_reuses_active_task() {
    let fx = fixture();
    let win = window("window-a");
    let first = fx
        .call_window(
            "task_start",
            json!({"goal": "first", "mode": "read_only"}),
            &win,
        )
        .await;
    let second = fx
        .call_window(
            "task_start",
            json!({"goal": "second", "mode": "read_only"}),
            &win,
        )
        .await;
    assert_eq!(first.body["task_id"], second.body["task_id"]);
    assert_eq!(second.body["data"]["continuation"], "continued");
}

#[tokio::test]
async fn different_windows_keep_independent_tasks() {
    let fx = fixture();
    let first = fx
        .call_window(
            "task_start",
            json!({"goal": "first", "mode": "read_only"}),
            &window("window-a"),
        )
        .await;
    let second = fx
        .call_window(
            "task_start",
            json!({"goal": "second", "mode": "read_only"}),
            &window("window-b"),
        )
        .await;
    assert_ne!(first.body["task_id"], second.body["task_id"]);
}

#[tokio::test]
async fn principal_boundary_blocks_same_task_id() {
    let fx = fixture();
    let started = start_read_only(&fx, "private").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let other = call_context("project:other-owner", GRANT, fx.host.clone());
    let out = fx
        .runtime
        .call_for_window(
            "task_resume",
            json!({"task_id": task_id}),
            Some(&other),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(out.http_status, 404);
    assert_eq!(out.body["error"]["code"], "task_not_found");
}

#[tokio::test]
async fn task_list_returns_only_current_principal_tasks() {
    let fx = fixture();
    let _ = start_read_only(&fx, "owner task").await;
    let other = call_context("project:other-owner", GRANT, fx.host.clone());
    let other_started = fx
        .runtime
        .call_for_window(
            "task_start",
            json!({"goal": "other task", "mode": "read_only"}),
            Some(&other),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert!(other_started.ok);
    let listed = fx.call("task_list", json!({})).await;
    assert_eq!(listed.body["data"]["count"], 1);
    assert_eq!(listed.body["data"]["tasks"][0]["goal"], "owner task");
}

#[tokio::test]
async fn task_resume_rebinds_only_supplied_window() {
    let fx = fixture();
    let started = start_read_only(&fx, "recover me").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call_window(
            "task_resume",
            json!({"task_id": task_id}),
            &window("recovery"),
        )
        .await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(out.body["data"]["continuity"]["window_rebound"], true);
}

#[tokio::test]
async fn invalid_task_id_is_rejected_without_host_dispatch() {
    let fx = fixture();
    let out = fx
        .call("task_resume", json!({"task_id": "not-a-task"}))
        .await;
    assert_eq!(out.body["error"]["code"], "invalid_arguments");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn read_only_task_denies_commands_before_host_start() {
    let fx = fixture();
    let started = start_read_only(&fx, "read").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "commands_run",
            json!({"task_id": task_id, "operation_id": "cmd-1", "command": "echo hi"}),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "read_only_task");
    assert_eq!(fx.host.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn read_only_task_denies_edits_before_host_invoke() {
    let fx = fixture();
    let started = start_read_only(&fx, "read").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "edits_apply",
            json!({
                "task_id": task_id,
                "operation_id": "edit-1",
                "changes": [{"kind": "create", "path": "x.txt", "content": "x"}]
            }),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "read_only_task");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_operation_id_is_rejected_before_edit_host() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "edits_apply",
            json!({
                "task_id": task_id,
                "operation_id": "bad id",
                "changes": [{"kind": "create", "path": "x.txt", "content": "x"}]
            }),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "invalid_arguments");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn edit_operation_exact_retry_replays_without_second_host_call() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let args = json!({
        "task_id": task_id,
        "operation_id": "edit-replay",
        "changes": [{"kind": "create", "path": "x.txt", "content": "x"}]
    });
    let first = fx.call("edits_apply", args.clone()).await;
    assert!(first.ok, "{}", first.body);
    let second = fx.call("edits_apply", args).await;
    assert!(second.ok, "{}", second.body);
    assert_eq!(second.body["data"]["idempotent_replay"], true);
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn edit_operation_same_id_different_request_conflicts() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let first = fx
        .call(
            "edits_apply",
            json!({
                "task_id": task_id,
                "operation_id": "edit-conflict",
                "changes": [{"kind": "create", "path": "a.txt", "content": "a"}]
            }),
        )
        .await;
    assert!(first.ok);
    let second = fx
        .call(
            "edits_apply",
            json!({
                "task_id": task_id,
                "operation_id": "edit-conflict",
                "changes": [{"kind": "create", "path": "b.txt", "content": "b"}]
            }),
        )
        .await;
    assert_eq!(second.body["error"]["code"], "operation_id_conflict");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restricted_authority_creates_approval_before_execution_start() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let mut restricted = fx.owner.clone();
    restricted.execution_authority.auto_authorize = false;
    restricted.execution_authority.mode = "restricted".into();
    let out = fx
        .runtime
        .call_for_window(
            "commands_run",
            json!({"task_id": task_id, "operation_id": "cmd-approval", "command": "echo hi"}),
            Some(&restricted),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(out.body["error"]["code"], "approval_required");
    assert_eq!(fx.host.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_authority_records_audit_and_reaches_execution_host() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "commands_run",
            json!({"task_id": task_id, "operation_id": "cmd-auto", "command": "echo hi"}),
        )
        .await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(fx.host.starts.load(Ordering::SeqCst), 1);
    let events = fx
        .runtime
        .db
        .connector_task_events(
            task_id,
            "wc_proj_1234567890",
            "project:wc_pgrant_1111111111111111",
            50,
        )
        .unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "authority_auto_authorized"));
}

#[tokio::test]
async fn parent_traversal_is_rejected_before_file_host() {
    let fx = fixture();
    let started = start_read_only(&fx, "read").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "files_read",
            json!({"task_id": task_id, "files": [{"path": "../secret"}]}),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "invalid_arguments");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_search_cursor_is_rejected_before_host() {
    let fx = fixture();
    let started = start_read_only(&fx, "search").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "files_search",
            json!({"task_id": task_id, "pattern": "x", "cursor": "wrong"}),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "invalid_arguments");
    assert_eq!(fx.host.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn task_list_goal_is_bounded_without_splitting_utf8() {
    let fx = fixture();
    let long = format!("{}{}", "a".repeat(199), "界".repeat(20));
    let started = start_read_only(&fx, &long).await;
    assert!(started.ok);
    let listed = fx.call("task_list", json!({})).await;
    let goal = listed.body["data"]["tasks"][0]["goal"].as_str().unwrap();
    assert!(goal.ends_with('…'));
    assert!(goal.is_char_boundary(goal.len()));
}

#[tokio::test]
async fn task_cancel_without_execution_reaches_terminal_cancelled_state() {
    let fx = fixture();
    let started = start_read_only(&fx, "cancel me").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx.call("task_cancel", json!({"task_id": task_id})).await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(out.body["data"]["status"], "cancelled");
    assert_eq!(out.body["data"]["cancellation"], "terminal");
    assert_eq!(fx.host.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn read_only_task_finish_needs_no_validation_or_writable_patch() {
    let fx = fixture();
    let started = start_read_only(&fx, "finish analysis").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "task_finish",
            json!({"task_id": task_id, "summary": "analysis complete"}),
        )
        .await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(out.body["data"]["status"], "ready_for_review");
    assert_eq!(out.body["data"]["result"]["patch_bytes"], 0);
    assert_eq!(
        out.body["data"]["result"]["validation"]["status"],
        "not_run"
    );
}

#[tokio::test]
async fn validation_plan_failure_creates_no_execution_reservation() {
    let fx = fixture();
    let started = start_normal(&fx).await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx
        .call(
            "checks_run",
            json!({
                "task_id": task_id,
                "operation_id": "check-1",
                "recipe": "go",
                "checks": ["test"]
            }),
        )
        .await;
    assert_eq!(out.body["error"]["code"], "validation_recipe_mismatch");
    let execution = fx
        .runtime
        .db
        .latest_connector_execution(
            task_id,
            "wc_proj_1234567890",
            "project:wc_pgrant_1111111111111111",
            None,
        )
        .unwrap();
    assert!(execution.is_none());
}

#[tokio::test]
async fn task_review_without_execution_is_nonblocking_and_keeps_task_identity() {
    let fx = fixture();
    let started = start_read_only(&fx, "review analysis").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let out = fx.call("task_review", json!({"task_id": task_id})).await;
    assert!(out.ok, "{}", out.body);
    assert_eq!(out.body["task_id"], task_id);
    assert_eq!(out.body["blocking"], false);
    assert!(out.body["data"]["recent_execution"].is_null());
}

#[tokio::test]
async fn task_list_rejects_limit_above_contract_bound() {
    let fx = fixture();
    let out = fx.call("task_list", json!({"limit": 21})).await;
    assert_eq!(out.body["error"]["code"], "invalid_arguments");
}

#[tokio::test]
async fn retired_inspect_mode_fails_before_workspace_or_host_activity() {
    let fx = fixture();
    let out = fx
        .call("task_start", json!({"goal": "old mode", "mode": "inspect"}))
        .await;
    assert_eq!(out.body["error"]["code"], "inspect_mode_retired");
    assert_eq!(fx.host.registrations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn normal_task_cannot_downgrade_to_read_only_in_same_window() {
    let fx = fixture();
    let win = window("mode-window");
    let first = fx
        .call_window(
            "task_start",
            json!({"goal": "write", "mode": "normal"}),
            &win,
        )
        .await;
    assert!(first.ok, "{}", first.body);
    let second = fx
        .call_window(
            "task_start",
            json!({"goal": "downgrade", "mode": "read_only"}),
            &win,
        )
        .await;
    assert_eq!(second.body["error"]["code"], "mode_transition_invalid");
}

#[tokio::test]
async fn missing_job_permission_blocks_cancel_without_mutating_task() {
    let fx = fixture();
    let started = start_read_only(&fx, "stay active").await;
    let task_id = started.body["task_id"].as_str().unwrap();
    let mut context = fx.owner.clone();
    context.access.permissions.job_run = false;
    let denied = fx
        .runtime
        .call_for_window(
            "task_cancel",
            json!({"task_id": task_id}),
            Some(&context),
            ConnectorTransport::Mcp,
            None,
        )
        .await;
    assert_eq!(
        denied.required_permission,
        Some(ConnectorPermission::JobRun)
    );
    let task = fx
        .runtime
        .db
        .connector_task(
            task_id,
            "wc_proj_1234567890",
            "project:wc_pgrant_1111111111111111",
        )
        .unwrap();
    assert_eq!(task.task_status, webcodex_store::ConnectorTaskState::Active);
}
