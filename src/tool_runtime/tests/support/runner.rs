#[cfg(feature = "workspace-checkpoints")]
use super::auth::auth_context;
use super::runtime::test_runtime;
use crate::runner_http::RunnerRegistry;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerJobUpdateRequest, RunnerPolicySummary, RunnerPollRequest,
    RunnerProjectSummary, RunnerRegisterRequest, RunnerRequest, RunnerResultRequest,
    ShellProfileSummaryEntry, EXTERNAL_SEARCH_REQUEST_PREFIX,
};
#[cfg(feature = "workspace-checkpoints")]
use crate::tool_runtime::ToolResult;
use crate::tool_runtime::{RuntimeInfo, ToolCall, ToolRuntime};
#[cfg(feature = "workspace-checkpoints")]
use crate::workspace_checkpoint::{create_workspace_checkpoint, restore_workspace_checkpoint};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(in crate::tool_runtime::tests) async fn register_runner_project_at_path(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &Path,
) -> String {
    let project_path = root.to_string_lossy().to_string();
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(RunnerCapabilities {
                shell: true,
                git: true,
                file_read: true,
                file_write: true,
                internal_posix_script: true,
                ..Default::default()
            }),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![named_registered_project(
            client_id,
            project_id,
            project_id,
            &project_path,
            1,
        )],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

pub(in crate::tool_runtime::tests) async fn register_runner_project_at_path_with_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    project_id: &str,
    root: &Path,
    auth: &crate::auth::AuthContext,
) -> String {
    let project_path = root.to_string_lossy().to_string();
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(
                    RunnerCapabilities {
                        shell: true,
                        git: true,
                        file_read: true,
                        file_write: true,
                        internal_posix_script: true,
                        ..Default::default()
                    },
                ),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![named_registered_project(
            client_id,
            project_id,
            project_id,
            &project_path,
            1,
        )],
    )
    .await;
    crate::tool_runtime::runner_project_runtime_id(client_id, project_id)
}

pub(in crate::tool_runtime::tests) fn run_runner_shell_request_locally(
    req: &RunnerRequest,
) -> (i32, String, String) {
    if req.kind == "file_read" {
        return run_runner_file_read_request_locally(req);
    }
    if req.kind == "file_list" {
        return run_runner_file_list_request_locally(req);
    }
    if req.kind == "file_project_overview" {
        return run_runner_project_overview_request_locally(req);
    }
    if req.kind == "file_skill_list_packages" {
        return run_runner_skill_list_packages_locally(req);
    }
    if req.kind == "file_skill_read_file" {
        return run_runner_skill_read_file_locally(req);
    }
    let structured_process = if req.kind == "run_process" {
        assert!(req.command.is_empty());
        assert!(req.script.is_none());
        Some(
            req.process
                .as_ref()
                .expect("run_process request must carry a typed process payload"),
        )
    } else {
        None
    };
    let internal_posix = if req.kind == "run_internal_posix_script" {
        let payload = req
            .script
            .as_ref()
            .expect("internal POSIX request must carry a script payload");
        assert_eq!(
            payload.language,
            crate::runner_protocol::ShellScriptLanguage::Sh
        );
        assert!(payload.args.is_empty());
        assert!(req.command.is_empty());
        assert!(req.stdin.is_none());
        Some(payload.script.as_str())
    } else {
        None
    };
    let internal_search = req
        .command
        .strip_prefix(EXTERNAL_SEARCH_REQUEST_PREFIX)
        .and_then(|rest| rest.strip_prefix('\n'));
    let (mut command, stdin_payload) = if let Some(process) = structured_process {
        let mut command = std::process::Command::new(&process.executable);
        command.args(&process.args);
        (command, req.stdin.clone())
    } else if let Some(script) = internal_posix {
        let mut command = std::process::Command::new(crate::tool_runtime::helpers::test_shell());
        command.arg("-s");
        (command, Some(script.to_string()))
    } else if let Some(script) = internal_search {
        let mut command = std::process::Command::new(crate::tool_runtime::helpers::test_shell());
        command.arg("-s");
        let script = if cfg!(windows) {
            format!(
                "if ! command -v rg >/dev/null 2>&1 && command -v rg.exe >/dev/null 2>&1; then rg() {{ command rg.exe --path-separator / \"$@\"; }}; fi\n{script}"
            )
        } else {
            script.to_string()
        };
        (command, Some(script))
    } else {
        let mut command = std::process::Command::new(crate::tool_runtime::helpers::test_shell());
        command.arg("-c").arg(&req.command);
        (command, req.stdin.clone())
    };
    if let Some(cwd) = req.cwd.as_deref() {
        command.current_dir(cwd);
    }
    if stdin_payload.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn agent shell request");
    if let Some(stdin) = stdin_payload.as_deref() {
        use std::io::Write;
        let write_result = child
            .stdin
            .take()
            .expect("agent shell request stdin")
            .write_all(stdin.as_bytes());
        if let Err(error) = write_result {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe,
                "agent shell request stdin write failed: {error}"
            );
        }
    }
    let output = child.wait_with_output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn run_runner_file_list_request_locally(req: &RunnerRequest) -> (i32, String, String) {
    let Some(cwd) = req.cwd.as_deref() else {
        return (-1, String::new(), "file_list missing cwd".to_string());
    };
    let path = req.path.as_deref().unwrap_or(".");
    let target = Path::new(cwd).join(path);
    let entries = match std::fs::read_dir(&target) {
        Ok(entries) => entries,
        Err(error) => return (-1, String::new(), error.to_string()),
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().ok().is_some_and(|kind| kind.is_dir()) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    (0, format!("{}\n", names.join("\n")), String::new())
}

fn run_runner_file_read_request_locally(req: &RunnerRequest) -> (i32, String, String) {
    use webcodex_workspace::file_read_range::{self, EffectiveRange};

    let Some(cwd) = req.cwd.as_deref() else {
        return (-1, String::new(), "file_read missing cwd".to_string());
    };
    let Some(path) = req.path.as_deref() else {
        return (-1, String::new(), "file_read missing path".to_string());
    };
    let target = Path::new(cwd).join(path);
    // Reconstruct the shared effective range from the runner's inclusive window.
    let start_line = req.start_line.unwrap_or(1).max(1);
    let end_line = req.end_line.unwrap_or(start_line);
    let limit = end_line.saturating_sub(start_line).saturating_add(1);
    let range = EffectiveRange::new(Some(start_line), Some(limit));
    match file_read_range::read_range(&target, range) {
        Ok(result) => {
            let output = json!({
                "format": "webcodex.file_read_range.v1",
                "content": result.content,
                "sha256": result.sha256,
                "total_lines": result.total_lines,
                "start_line": result.start_line,
                "limit": result.limit,
            });
            (0, output.to_string(), String::new())
        }
        Err(error) => (-1, String::new(), error.to_string()),
    }
}

fn run_runner_skill_list_packages_locally(req: &RunnerRequest) -> (i32, String, String) {
    let root = request_root(req);
    let limit = req
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .and_then(|value| value["limit"].as_u64())
        .map(|value| value as usize)
        .unwrap_or(257);
    let entries =
        match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (
                0,
                json!({"format":"webcodex.skill_package_list.v1","entries":[],"truncated":false})
                    .to_string(),
                String::new(),
            ),
            Err(error) => return (-1, String::new(), error.to_string()),
        };
    let mut items = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let kind = entry.file_type().ok()?;
            let kind = if kind.is_dir() {
                "dir"
            } else if kind.is_symlink() {
                "symlink"
            } else {
                return None;
            };
            Some(json!({"name": entry.file_name().to_string_lossy(), "kind": kind}))
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    let truncated = items.len() > limit;
    items.truncate(limit);
    (
        0,
        json!({
            "format":"webcodex.skill_package_list.v1",
            "entries":items,
            "truncated":truncated
        })
        .to_string(),
        String::new(),
    )
}

fn run_runner_skill_read_file_locally(req: &RunnerRequest) -> (i32, String, String) {
    use webcodex_workspace::file_read_range::{self, EffectiveRange};
    let Some(cwd) = req.cwd.as_deref() else {
        return (-1, String::new(), "skill_path_invalid".to_string());
    };
    let Some(path) = req.path.as_deref() else {
        return (-1, String::new(), "skill_path_invalid".to_string());
    };
    let options = req
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .unwrap_or(Value::Null);
    let Some(package_root) = options["package_root"].as_str() else {
        return (-1, String::new(), "skill_path_invalid".to_string());
    };
    let max_file_bytes = options["max_file_bytes"].as_u64().unwrap_or(0) as usize;
    let project_root = match Path::new(cwd).canonicalize() {
        Ok(root) => root,
        Err(_) => return (-1, String::new(), "skill_path_invalid".to_string()),
    };
    let package = match project_root.join(package_root).canonicalize() {
        Ok(root) => root,
        Err(_) => return (-1, String::new(), "skill_file_not_found".to_string()),
    };
    let target = match project_root.join(path).canonicalize() {
        Ok(target) => target,
        Err(_) => return (-1, String::new(), "skill_file_not_found".to_string()),
    };
    if !package.starts_with(&project_root) || !target.starts_with(&package) || !target.is_file() {
        return (-1, String::new(), "skill_path_escape".to_string());
    }
    let canonical_relative = target
        .strip_prefix(&project_root)
        .unwrap_or(&target)
        .to_string_lossy();
    if crate::sensitive_paths::is_secret_path(path)
        || crate::sensitive_paths::is_secret_path(canonical_relative.as_ref())
    {
        return (-1, String::new(), "skill_sensitive_path".to_string());
    }
    let file_bytes = target
        .metadata()
        .map(|m| m.len() as usize)
        .unwrap_or(usize::MAX);
    if file_bytes > max_file_bytes {
        return (-1, String::new(), "skill_file_too_large".to_string());
    }
    let start_line = req.start_line.unwrap_or(1);
    let end_line = req.end_line.unwrap_or(start_line);
    let limit = end_line.saturating_sub(start_line).saturating_add(1);
    let range = EffectiveRange::new(Some(start_line), Some(limit));
    let result = match file_read_range::read_range_with_budget(
        &target,
        range,
        req.max_bytes.unwrap_or(48 * 1024),
    ) {
        Ok(result) => result,
        Err(error) => {
            let reason = if matches!(
                error.reason,
                webcodex_workspace::file_read_range::ReadFileReason::InvalidUtf8
            ) {
                "skill_invalid_utf8"
            } else {
                "skill_read_unavailable"
            };
            return (-1, String::new(), reason.to_string());
        }
    };
    (
        0,
        json!({
            "format":"webcodex.skill_file_read.v1",
            "content":result.content,
            "sha256":result.sha256,
            "file_bytes":file_bytes,
            "total_lines":result.total_lines,
            "start_line":result.start_line,
            "limit":result.limit,
            "returned_lines":result.returned_lines,
            "end_line":result.end_line,
            "has_more":result.has_more,
            "next_start_line":result.next_start_line
        })
        .to_string(),
        String::new(),
    )
}

fn run_runner_project_overview_request_locally(req: &RunnerRequest) -> (i32, String, String) {
    let Some(cwd) = req.cwd.as_deref() else {
        return (
            -1,
            String::new(),
            "project_overview missing cwd".to_string(),
        );
    };
    let requested_path = req.path.as_deref().unwrap_or(".");
    let options = req
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .unwrap_or_else(|| json!({}));
    match crate::project_overview::build_project_overview(
        Path::new(cwd),
        requested_path,
        options["max_depth"].as_u64().map(|value| value as usize),
        options["limit"].as_u64().map(|value| value as usize),
    ) {
        Ok(output) => (0, output.to_string(), String::new()),
        Err(error) => (-1, String::new(), error),
    }
}

pub(in crate::tool_runtime::tests) async fn complete_agent_request_by_running_locally(
    runtime: &ToolRuntime,
    client_id: &str,
    req: RunnerRequest,
) {
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
}

pub(in crate::tool_runtime::tests) async fn complete_project_overview_agent_request_locally(
    runtime: &ToolRuntime,
    client_id: &str,
    req: &RunnerRequest,
) {
    assert_eq!(req.kind, "file_project_overview");
    let options = req
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .expect("project_overview agent options");
    let output = crate::project_overview::build_project_overview(
        Path::new(req.cwd.as_deref().expect("project_overview cwd")),
        req.path.as_deref().expect("project_overview path"),
        options["max_depth"].as_u64().map(|value| value as usize),
        options["limit"].as_u64().map(|value| value as usize),
    )
    .expect("Runner-side project_overview scan");
    complete_patch_agent_request(
        runtime,
        client_id,
        &req.request_id,
        0,
        &output.to_string(),
        "",
    )
    .await;
}

#[cfg(feature = "workspace-checkpoints")]
pub(in crate::tool_runtime::tests) fn run_runner_checkpoint_request_locally(
    req: &RunnerRequest,
) -> (i32, String, String) {
    let root = request_root(req);
    let payload = req
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .unwrap_or_else(|| json!({}));
    let output = match req.kind.as_str() {
        "file_checkpoint_create" => {
            let include_untracked = payload
                .get("include_untracked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            create_workspace_checkpoint(&root, include_untracked)
        }
        "file_checkpoint_restore" => {
            let checkpoint = payload.get("checkpoint").unwrap_or(&Value::Null);
            restore_workspace_checkpoint(&root, checkpoint)
        }
        other => panic!("unexpected checkpoint request kind: {other}"),
    };
    (0, serde_json::to_string(&output).unwrap(), String::new())
}

fn request_root(req: &RunnerRequest) -> PathBuf {
    let path = req.path.as_deref().unwrap_or(".");
    let raw = PathBuf::from(path);
    if raw.is_absolute() {
        raw
    } else {
        req.cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(raw)
    }
}

#[cfg(feature = "workspace-checkpoints")]
pub(in crate::tool_runtime::tests) async fn dispatch_checkpoint_with_local_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    call: ToolCall,
) -> ToolResult {
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            let bootstrap = auth_context(None, true);
            runtime.dispatch_with_auth(call, Some(&bootstrap)).await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let req = loop {
        let request = probe_patch_agent_request(runtime, client_id).await;
        if request.is_some() || task.is_finished() {
            break request;
        }
        if Instant::now() >= deadline {
            panic!("checkpoint Runner request readiness failed for client {client_id} within 10 seconds");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let req = match req {
        Some(req) => req,
        None => {
            let result = task.await.unwrap();
            panic!("checkpoint did not enqueue an agent file request: {result:?}");
        }
    };
    assert!(
        matches!(
            req.kind.as_str(),
            "file_checkpoint_create" | "file_checkpoint_restore"
        ),
        "unexpected checkpoint request kind: {}",
        req.kind
    );
    assert!(
        req.command.is_empty(),
        "checkpoint native request must not use a shell command: {}",
        req.command
    );
    let (exit_code, stdout, stderr) = run_runner_checkpoint_request_locally(&req);
    complete_patch_agent_request(
        runtime,
        client_id,
        &req.request_id,
        exit_code,
        &stdout,
        &stderr,
    )
    .await;
    task.await.unwrap()
}

pub(in crate::tool_runtime::tests) fn runtime_with_agent_project(client_id: &str) -> ToolRuntime {
    let _ = client_id;
    ToolRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(RuntimeInfo::default()),
    )
}

pub(in crate::tool_runtime::tests) async fn register_agent(
    runtime: &ToolRuntime,
    client_id: &str,
    owner: Option<&str>,
    caps: RunnerCapabilities,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: owner.map(str::to_string),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(caps),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        vec![registered_project("agent-proj", "/tmp/agent-proj")],
    )
    .await;
}

pub(in crate::tool_runtime::tests) fn agent_test_project_id(client_id: &str) -> String {
    crate::tool_runtime::runner_project_runtime_id(client_id, "agent-proj")
}

/// Register an agent under an explicit `agent_instance_id`. `register_agent`
/// always uses `"inst"`; replacement scenarios need a different instance id to
/// model a new Runner process taking over the lease.
pub(in crate::tool_runtime::tests) async fn register_agent_with_instance(
    runtime: &ToolRuntime,
    client_id: &str,
    runner_instance_id: &str,
    owner: Option<&str>,
    caps: RunnerCapabilities,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: runner_instance_id.to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: owner.map(str::to_string),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(caps),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        runner_instance_id,
        vec![registered_project("agent-proj", "/tmp/agent-proj")],
    )
    .await;
}

/// Build a ToolRuntime backed by a single server-configured (local) project
/// rooted at `root`. Used to assert the runtime surface rejects
/// server-configured projects in favor of agent-registered ones.
pub(in crate::tool_runtime::tests) fn runtime_with_local_project(
    root: &Path,
    project_id: &str,
) -> ToolRuntime {
    let _ = (root, project_id);
    ToolRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(RuntimeInfo::default()),
    )
}

pub(in crate::tool_runtime::tests) fn registered_project(
    id: &str,
    path: &str,
) -> RunnerProjectSummary {
    RunnerProjectSummary {
        id: id.to_string(),
        name: Some(id.to_string()),
        path: path.to_string(),
        allow_patch: true,
        kind: Some("repo".to_string()),
        registration_source: None,
        description: None,
        hooks: Vec::new(),
        disabled: false,
        revision: None,
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at: 123,
        shell_profile: None,
    }
}

pub(in crate::tool_runtime::tests) fn named_registered_project(
    client_id: &str,
    id: &str,
    name: &str,
    path: &str,
    updated_at: i64,
) -> RunnerProjectSummary {
    let _ = client_id;
    RunnerProjectSummary {
        id: id.to_string(),
        name: Some(name.to_string()),
        path: path.to_string(),
        allow_patch: true,
        kind: Some("repo".to_string()),
        registration_source: None,
        description: None,
        hooks: Vec::new(),
        disabled: false,
        revision: None,
        git_branch: None,
        git_head: None,
        git_dirty: None,
        updated_at,
        shell_profile: None,
    }
}

pub(in crate::tool_runtime::tests) async fn register_agent_projects(
    runtime: &ToolRuntime,
    client_id: &str,
    owner: Option<&str>,
    caps: RunnerCapabilities,
    projects: Vec<RunnerProjectSummary>,
) {
    let runner_instance_id = format!("inst-{client_id}");
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: runner_instance_id.clone(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: owner.map(str::to_string),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(caps),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &runner_instance_id,
        projects,
    )
    .await;
}

pub(in crate::tool_runtime::tests) async fn register_agent_projects_for_auth(
    runtime: &ToolRuntime,
    client_id: &str,
    auth: &crate::auth::AuthContext,
    caps: RunnerCapabilities,
    projects: Vec<RunnerProjectSummary>,
) {
    let runner_instance_id = format!("inst-{client_id}");
    runtime
        .runner_registry
        .register_with_auth(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.clone(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: None,
                hostname: None,
                host_context: None,
                capabilities: crate::test_support::current_runner_capabilities(caps),
                policy: None,
            },
            Some(&crate::test_support::runner_access(auth)),
        )
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        &runner_instance_id,
        projects,
    )
    .await;
}

pub(in crate::tool_runtime::tests) async fn probe_agent_request_for_client(
    runtime: &ToolRuntime,
    client_id: &str,
) -> Option<RunnerRequest> {
    probe_agent_request_for_instance(runtime, client_id, &format!("inst-{}", client_id)).await
}

pub(in crate::tool_runtime::tests) async fn probe_agent_request_for_instance(
    runtime: &ToolRuntime,
    client_id: &str,
    runner_instance_id: &str,
) -> Option<RunnerRequest> {
    for _ in 0..20 {
        let req = runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.to_string(),
            })
            .await
            .unwrap();
        if req.is_some() {
            return req;
        }
        tokio::task::yield_now().await;
    }
    None
}

pub(in crate::tool_runtime::tests) async fn wait_for_runner_request_for_instance(
    runtime: &ToolRuntime,
    client_id: &str,
    runner_instance_id: &str,
) -> RunnerRequest {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(request) = runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: runner_instance_id.to_string(),
            })
            .await
            .unwrap()
        {
            return request;
        }
        if Instant::now() >= deadline {
            panic!(
                "Runner request readiness failed for client {client_id} instance {runner_instance_id} within 10 seconds"
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub(in crate::tool_runtime::tests) async fn wait_for_runner_request_for_client(
    runtime: &ToolRuntime,
    client_id: &str,
) -> RunnerRequest {
    wait_for_runner_request_for_instance(runtime, client_id, &format!("inst-{client_id}")).await
}

/// Start one real Runner-owned session Job and drive it to the requested
/// nonterminal projection state. This keeps handoff/finish projection tests on
/// the current Runner Job topology instead of seeding the retired Server-local
/// Job registry.
pub(in crate::tool_runtime::tests) async fn seed_session_projection_job(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    session_id: &str,
    status: &str,
    stdout: &str,
    auth: &crate::auth::AuthContext,
) -> String {
    assert!(matches!(status, "running" | "stop_requested"));
    let started = runtime
        .dispatch_with_auth(
            ToolCall::RunJob {
                project: project.to_string(),
                command: "echo projection-job".to_string(),
                session_id: Some(session_id.to_string()),
                timeout_secs: Some(60),
                cwd: None,
                purpose: Some(crate::tool_runtime::tool_inputs::ExecutionPurpose::Test),
                shell: None,
            },
            Some(auth),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    let job_id = started.output["job_id"].as_str().unwrap().to_string();
    let request = wait_for_runner_request_for_instance(runtime, client_id, "inst").await;
    assert_eq!(request.job_id.as_deref(), Some(job_id.as_str()));
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job_id.clone(),
            request_id: Some(request.request_id),
            update_seq: None,
            status: "running".to_string(),
            stdout_chunk: (!stdout.is_empty()).then(|| stdout.to_string()),
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
    if status == "stop_requested" {
        let stopped = runtime
            .runner_registry
            .stop_job(&job_id, "projection-test".to_string())
            .await
            .unwrap();
        assert_eq!(stopped.status, "stop_requested");
        let stop_request = wait_for_runner_request_for_instance(runtime, client_id, "inst").await;
        assert_eq!(stop_request.kind, "stop_job");
        assert_eq!(stop_request.job_id.as_deref(), Some(job_id.as_str()));
    }
    job_id
}

pub(in crate::tool_runtime::tests) async fn finish_session_projection_job(
    runtime: &ToolRuntime,
    client_id: &str,
    job_id: &str,
    status: &str,
) {
    assert!(matches!(
        status,
        "completed" | "failed" | "stopped" | "timeout" | "cancelled"
    ));
    let current = runtime.runner_registry.get_job(job_id).await.unwrap();
    runtime
        .runner_registry
        .update_job(RunnerJobUpdateRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            job_id: job_id.to_string(),
            request_id: current.request_id,
            update_seq: None,
            status: status.to_string(),
            stdout_chunk: None,
            stderr_chunk: None,
            stdout_tail: None,
            stderr_tail: None,
            log_snapshot: None,
            exit_code: (status == "completed").then_some(0),
            duration_ms: Some(1),
            error: None,
            command_execution_state: None,
            validation_progress: None,
            activity: None,
            finished: true,
        })
        .await
        .unwrap();
}

pub(in crate::tool_runtime::tests) async fn runtime_with_resolver_projects() -> ToolRuntime {
    let runtime = test_runtime();
    let file_caps = RunnerCapabilities {
        file_read: true,
        git: true,
        shell: true,
        internal_posix_script: true,
        ..Default::default()
    };
    register_agent_projects(
        &runtime,
        "workstation",
        None,
        file_caps.clone(),
        vec![
            named_registered_project(
                "workstation",
                "my-repo",
                "My Repo",
                "/root/git/workstation-my-repo",
                200,
            ),
            named_registered_project(
                "workstation",
                "other-repo",
                "Other Repo",
                "/root/git/workstation-other-repo",
                210,
            ),
        ],
    )
    .await;
    register_agent_projects(
        &runtime,
        "laptop",
        None,
        file_caps,
        vec![named_registered_project(
            "laptop",
            "my-repo",
            "My Repo",
            "/root/git/laptop-my-repo",
            190,
        )],
    )
    .await;
    runtime
}

//   * the working directory is supplied via the shell request `cwd` field,
//     never via a `cd <path> && ...` prefix in the command;
//   * `apply_unified_diff` performs one applicability check before one apply;
//   * the raw unified diff always travels via typed stdin, never command text;
//   * server-configured (non-agent) projects are rejected, so the server never
//     touches their filesystem directly.

pub(in crate::tool_runtime::tests) async fn probe_patch_agent_request(
    runtime: &ToolRuntime,
    client_id: &str,
) -> Option<RunnerRequest> {
    for _ in 0..20 {
        let req = runtime
            .runner_registry
            .poll(RunnerPollRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
            })
            .await
            .unwrap();
        if req.is_some() {
            return req;
        }
        tokio::task::yield_now().await;
    }
    None
}

/// Wait for a request that the test requires to be dispatched. Unlike
/// `probe_patch_agent_request`, which is intentionally a short probe for
/// negative/no-dispatch assertions, this positive readiness wait uses one
/// absolute wall-clock deadline so scheduler contention cannot turn a fixed
/// yield count into a flaky failure.
pub(in crate::tool_runtime::tests) async fn wait_for_patch_agent_request(
    runtime: &ToolRuntime,
    client_id: &str,
) -> RunnerRequest {
    wait_for_runner_request_for_instance(runtime, client_id, "inst").await
}

pub(in crate::tool_runtime::tests) fn assert_internal_posix_script_contains(
    request: &RunnerRequest,
    needle: &str,
) {
    assert_eq!(request.kind, "run_internal_posix_script");
    assert!(request.command.is_empty());
    assert!(request.stdin.is_none());
    let payload = request
        .script
        .as_ref()
        .expect("internal POSIX request must carry a typed script payload");
    assert_eq!(
        payload.language,
        crate::runner_protocol::ShellScriptLanguage::Sh
    );
    assert!(payload.args.is_empty());
    assert!(
        payload.script.contains(needle),
        "internal POSIX script did not contain {needle:?}: {}",
        payload.script
    );
}

pub(in crate::tool_runtime::tests) async fn complete_patch_agent_request(
    runtime: &ToolRuntime,
    client_id: &str,
    request_id: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) {
    complete_patch_agent_request_for_instance(
        runtime, client_id, "inst", request_id, exit_code, stdout, stderr,
    )
    .await;
}

pub(in crate::tool_runtime::tests) async fn complete_patch_agent_request_for_instance(
    runtime: &ToolRuntime,
    client_id: &str,
    runner_instance_id: &str,
    request_id: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) {
    runtime
        .runner_registry
        .complete(RunnerResultRequest {
            client_id: client_id.to_string(),
            runner_instance_id: runner_instance_id.to_string(),
            request_id: request_id.to_string(),
            exit_code: Some(exit_code),
            stdout: Some(stdout.to_string()),
            stderr: Some(stderr.to_string()),
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
}

pub(in crate::tool_runtime::tests) async fn complete_agent_ranged_file_read_request(
    runtime: &ToolRuntime,
    client_id: &str,
    request: &RunnerRequest,
    content: &str,
) {
    let start = request
        .start_line
        .expect("ToolRuntime file_read test request must include start_line");
    let end = request
        .end_line
        .expect("ToolRuntime file_read test request must include end_line");
    let limit = end.saturating_sub(start).saturating_add(1);
    complete_patch_agent_request(
        runtime,
        client_id,
        &request.request_id,
        0,
        &canonical_agent_file_read_range(content, start, limit),
        "",
    )
    .await;
}

pub(in crate::tool_runtime::tests) async fn register_agent_with_projects(
    runtime: &ToolRuntime,
    client_id: &str,
    owner: Option<&str>,
    caps: RunnerCapabilities,
    projects: Vec<RunnerProjectSummary>,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: owner.map(str::to_string),
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(caps),
            policy: None,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        projects,
    )
    .await;
}

/// Helper: register an agent carrying a sanitized shell-profiles summary
/// (inside its policy) plus a set of projects with optional per-project
/// `shell_profile`. Used by the shell-profile observability tests.
pub(in crate::tool_runtime::tests) async fn register_agent_with_shell_profiles(
    runtime: &ToolRuntime,
    client_id: &str,
    policy: Option<RunnerPolicySummary>,
    projects: Vec<RunnerProjectSummary>,
) {
    runtime
        .runner_registry
        .register(RunnerRegisterRequest {
            process_started_at: None,
            build: None,
            job_concurrency_limit: None,
            job_inventory: None,
            coding_agent_providers: None,
            coding_agent_inventory: None,
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
            runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
            display_name: None,
            owner: None,
            hostname: None,
            host_context: None,
            capabilities: crate::test_support::current_runner_capabilities(
                RunnerCapabilities::default(),
            ),
            policy,
        })
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &runtime.runner_registry,
        client_id,
        "inst",
        projects,
    )
    .await;
}

/// Build a canonical `webcodex.file_read_range.v1` envelope for the full file
/// `content` under the ToolRuntime default effective range (start_line=1,
/// limit=2000). The selected window, SHA-256, total line count, and range
/// fields are all derived through the shared range reader so the envelope is
/// exactly what a real agent produces and passes the ToolRuntime strict
/// validation. `total_lines` is ignored in favor of the shared reader's count;
/// it is kept only for call-site compatibility.
pub(in crate::tool_runtime::tests) fn canonical_agent_file_read_output(
    content: &str,
    _total_lines: usize,
) -> String {
    canonical_agent_file_read_range(content, 1, 2000)
}

/// Build a canonical `webcodex.file_read_range.v1` envelope for the full file
/// `content` under an explicit effective request range. The selected window is
/// computed by the shared range reader so content, total_lines, start_line,
/// and limit are internally consistent and match the ToolRuntime validation.
pub(in crate::tool_runtime::tests) fn canonical_agent_file_read_range(
    content: &str,
    start_line: usize,
    limit: usize,
) -> String {
    use webcodex_workspace::file_read_range::{self, EffectiveRange};
    let range = EffectiveRange::new(Some(start_line), Some(limit));
    let result = file_read_range::read_range_from(content.as_bytes(), range)
        .expect("canonical agent file read fixture range fits budget");
    serde_json::json!({
        "format": "webcodex.file_read_range.v1",
        "content": result.content,
        "sha256": result.sha256,
        "total_lines": result.total_lines,
        "start_line": result.start_line,
        "limit": result.limit,
    })
    .to_string()
}

pub(in crate::tool_runtime::tests) fn profile_summary_entry(
    name: &str,
    has_init_script: bool,
    env_keys_count: usize,
) -> ShellProfileSummaryEntry {
    ShellProfileSummaryEntry {
        dialect: None,
        name: name.to_string(),
        has_init_script,
        env_keys_count,
        program: "sh".to_string(),
        args_count: 1,
    }
}
