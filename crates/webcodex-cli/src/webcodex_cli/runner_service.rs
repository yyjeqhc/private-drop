use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::connect::profile::read_enabled_project_count;
use super::http::{fetch_runtime_status, http_post_json_status, HttpStatusSummary};
use super::{
    control_service_for_scope, encode_exec_argument, encode_exec_path_argument,
    encode_exec_program, encode_unit_path_value, ensure_service_file_parent,
    install_unit_for_scope, local_runner_profile_marker, local_runner_state_summary,
    query_systemd_service_status_for_scope, read_optional_token, read_optional_user_api_token,
    run_local_runner_logs, run_local_runner_service, run_logs_for_scope, service_unit_name,
    shell_command, uninstall_unit_for_scope, validate_systemd_identity, LocalRunnerServiceAction,
    RUNNER_SERVICE_UNIT,
};
use crate::{
    RunnerInstallServiceOptions, RunnerStatusOptions, ServiceActionKind, ServiceActionOptions,
    ServiceScope,
};

pub(crate) fn render_runner_systemd_unit(
    opts: &RunnerInstallServiceOptions,
) -> Result<String, String> {
    match opts.scope {
        ServiceScope::User if opts.user.is_some() || opts.group.is_some() => {
            return Err(
                "user service units cannot contain User= or Group=; they run as the current user"
                    .to_string(),
            );
        }
        _ if opts.root_runner && !opts.allow_root_runner => {
            return Err(
                "refusing to render a Runner that would run as root without --allow-root-runner"
                    .to_string(),
            );
        }
        _ => {}
    }
    let binary = encode_exec_program("ExecStart", &opts.bin)?;
    let config_flag = encode_exec_argument("ExecStart option", "--config")?;
    let config = encode_exec_path_argument("ExecStart --config", &opts.config)?;
    let exec_start = format!("{binary} {config_flag} {config}");
    let working_directory = encode_unit_path_value("WorkingDirectory", &opts.working_directory)?;
    if let Some(user) = &opts.user {
        validate_systemd_identity("User", user)?;
    }
    if let Some(group) = &opts.group {
        validate_systemd_identity("Group", group)?;
    }

    let mut unit = String::new();
    if opts.root_runner {
        unit.push_str(
            "# WARNING: --allow-root-runner was explicitly accepted; project commands run as root.\n",
        );
    }
    unit.push_str("[Unit]\n");
    unit.push_str("Description=WebCodex Runner\n");
    if opts.scope == ServiceScope::System {
        unit.push_str("After=network-online.target\n");
        unit.push_str("Wants=network-online.target\n");
    }
    unit.push_str("\n[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!("ExecStart={exec_start}\n"));
    unit.push_str("ExecReload=/bin/kill -HUP $MAINPID\n");
    unit.push_str("Restart=always\n");
    unit.push_str("RestartSec=5s\n");
    unit.push_str("StandardOutput=journal\n");
    unit.push_str("StandardError=journal\n");
    unit.push_str("Environment=RUST_LOG=info\n");
    unit.push_str(&format!("WorkingDirectory={working_directory}\n"));
    if opts.scope == ServiceScope::System {
        if let Some(user) = &opts.user {
            unit.push_str(&format!("User={user}\n"));
        }
        if let Some(group) = &opts.group {
            unit.push_str(&format!("Group={group}\n"));
        }
    }
    unit.push_str("\n[Install]\n");
    unit.push_str(match opts.scope {
        ServiceScope::User => "WantedBy=default.target\n",
        ServiceScope::System => "WantedBy=multi-user.target\n",
    });
    Ok(unit)
}

pub(crate) fn run_runner_install_service(
    opts: RunnerInstallServiceOptions,
) -> Result<String, String> {
    let rendered = render_runner_systemd_unit(&opts)?;
    let unit = service_unit_name(&opts.service_file, RUNNER_SERVICE_UNIT);
    if opts.output_stdout || opts.dry_run {
        if opts.json {
            return serde_json::to_string_pretty(&json!({
                "service_file": opts.service_file.to_string_lossy(),
                "config": opts.config.to_string_lossy(),
                "bin": opts.bin.to_string_lossy(),
                "unit_name": unit,
                "scope": opts.scope.as_str(),
                "root_runner": opts.root_runner,
                "dry_run": true,
                "systemd_called": false,
                "unit": rendered,
            }))
            .map_err(|e| e.to_string());
        }
        return Ok(rendered);
    }
    if opts.scope == ServiceScope::User {
        ensure_service_file_parent(&opts.service_file)?;
    }
    let result = install_unit_for_scope(
        opts.scope,
        &opts.service_file,
        &unit,
        &rendered,
        opts.overwrite,
        opts.no_start,
    )?;
    if opts.json {
        return serde_json::to_string_pretty(&json!({
            "service_file": opts.service_file.to_string_lossy(),
            "config": opts.config.to_string_lossy(),
            "bin": opts.bin.to_string_lossy(),
            "unit": result.unit,
            "scope": opts.scope.as_str(),
            "root_runner": opts.root_runner,
            "enabled": true,
            "started": result.started,
        }))
        .map_err(|e| e.to_string());
    }
    let warning = if opts.root_runner {
        "\nWARNING: explicit --allow-root-runner accepted; this Runner executes project commands as root.\n"
    } else {
        ""
    };
    let status_command = shell_command(&[
        "webcodex".to_string(),
        "runner".to_string(),
        "status".to_string(),
        "--scope".to_string(),
        opts.scope.as_str().to_string(),
        "--config".to_string(),
        opts.config.to_string_lossy().into_owned(),
        "--service-file".to_string(),
        opts.service_file.to_string_lossy().into_owned(),
    ]);
    Ok(format!(
        "Runner {}.\n\nRunner configuration:\n  {}\n\nNext:\n  {status_command}\n\nDetails:\n  Service: {}\n  Scope:   {}\n  Started: {}\n{}",
        if result.started { "installed and started" } else { "installed" },
        opts.config.display(),
        result.unit,
        opts.scope.as_str(),
        if result.started { "yes" } else { "no (--no-start)" },
        warning,
    ))
}

pub(crate) fn run_runner_service(opts: ServiceActionOptions) -> Result<String, String> {
    if let Some(local) = &opts.local_profile {
        if local_runner_profile_marker(&local.state_dir).is_file() {
            return match &opts.kind {
                ServiceActionKind::Control(control) => run_local_runner_service(
                    match control {
                        super::ServiceControl::Start => LocalRunnerServiceAction::Start,
                        super::ServiceControl::Stop => LocalRunnerServiceAction::Stop,
                        super::ServiceControl::Restart => LocalRunnerServiceAction::Restart,
                    },
                    &local.config,
                    &local.state_dir,
                    local.runner_bin.as_deref(),
                ),
                ServiceActionKind::Logs {
                    lines,
                    since,
                    follow,
                } => run_local_runner_logs(
                    &local.state_dir,
                    *lines,
                    since.as_deref(),
                    *follow,
                ),
                ServiceActionKind::Uninstall { .. } => Err(
                    "hosted connect profiles are not system services; use `webcodex runner stop --profile <name>` and remove local profile files explicitly if desired"
                        .to_string(),
                ),
            };
        }
    }
    if opts
        .local_profile
        .as_ref()
        .is_some_and(|local| local.runner_bin.is_some())
    {
        return Err(
            "--bin requires an existing hosted connect profile; no hosted profile marker was found"
                .to_string(),
        );
    }
    match opts.kind {
        ServiceActionKind::Control(control) => {
            control_service_for_scope(opts.scope, &opts.unit, control)?;
            Ok(format!(
                "Runner service {} completed for {}.\n",
                control.as_str(),
                opts.unit
            ))
        }
        ServiceActionKind::Logs {
            lines,
            since,
            follow,
        } => run_logs_for_scope(opts.scope, &opts.unit, lines, since.as_deref(), follow),
        ServiceActionKind::Uninstall { confirm } => {
            if !confirm {
                return Err("runner uninstall requires --confirm; no changes were made".to_string());
            }
            let result = uninstall_unit_for_scope(opts.scope, &opts.service_file, &opts.unit)?;
            Ok(format!(
                "Runner service {}. Runner config, tokens, profile data and binaries were not deleted.\n",
                if result.removed { "uninstalled" } else { "was already absent" }
            ))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RunnerStatusConfig {
    #[serde(default)]
    server_url: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    project_registry_dir: Option<PathBuf>,
    #[serde(default, rename = "projects_dir")]
    legacy_projects_dir: Option<PathBuf>,
    #[serde(default)]
    policy: RunnerStatusPolicy,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RunnerStatusPolicy {
    #[serde(default)]
    allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct RunnerConfigMetadata {
    path: PathBuf,
    client_id: String,
    owner: Option<String>,
    display_name: Option<String>,
    transport: Option<String>,
    project_registry_dir: PathBuf,
    allowed_roots: Vec<PathBuf>,
    server_url: String,
    token: String,
}

fn read_runner_config_metadata(path: &Path) -> Result<RunnerConfigMetadata, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read Runner config {}: {}", path.display(), e))?;
    let cfg: RunnerStatusConfig = toml::from_str(&content)
        .map_err(|e| format!("failed to parse Runner config {}: {}", path.display(), e))?;
    let project_registry_dir = match (cfg.project_registry_dir, cfg.legacy_projects_dir) {
        (Some(_), Some(_)) => {
            return Err(
                "project_registry_dir and legacy projects_dir cannot both be configured; keep exactly one Runner project registry setting"
                    .to_string(),
            )
        }
        (Some(path), None) | (None, Some(path)) => path,
        (None, None) => {
            let base = webcodex_runner_config::paths::default_client_config_base_dir()?;
            webcodex_runner_config::paths::select_project_registry_dir(&base)?
        }
    };
    Ok(RunnerConfigMetadata {
        path: path.to_path_buf(),
        client_id: cfg.client_id,
        owner: cfg.owner,
        display_name: cfg.display_name,
        transport: cfg.transport,
        project_registry_dir,
        allowed_roots: cfg.policy.allowed_roots,
        server_url: cfg.server_url,
        token: cfg.token,
    })
}

fn allowed_roots_summary(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        "0 configured; Runner runtime defaults to $HOME when allowed_roots is omitted".to_string()
    } else {
        format!(
            "{} configured: {}",
            roots.len(),
            roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn runtime_client_entry<'a>(output: &'a Value, client_id: &str) -> Option<&'a Value> {
    output
        .pointer("/agents/clients")
        .and_then(Value::as_array)
        .and_then(|clients| {
            clients
                .iter()
                .find(|client| client.get("client_id").and_then(Value::as_str) == Some(client_id))
        })
}

fn runtime_client_online(output: &Value, client_id: &str) -> Option<bool> {
    let entry = runtime_client_entry(output, client_id)?;
    entry.get("connected").and_then(Value::as_bool).or_else(|| {
        entry
            .get("status")
            .and_then(Value::as_str)
            .map(|s| s == "online")
    })
}

fn runtime_client_project_count(output: &Value, client_id: &str) -> Option<usize> {
    runtime_client_entry(output, client_id)?
        .get("projects_count")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
}

fn render_runner_readiness_summary(
    client_online: Option<bool>,
    loaded_project_count: Option<usize>,
    configured_project_count: Option<usize>,
    config: &Path,
) -> String {
    let mut out = String::new();
    match client_online {
        Some(true) => out.push_str("Runner: connected\n"),
        Some(false) => out.push_str("Runner: offline\n"),
        None => out.push_str("Runner connection: not checked\n"),
    }
    match (
        client_online,
        loaded_project_count,
        configured_project_count,
    ) {
        (Some(true), Some(loaded), Some(configured)) if loaded == configured => {
            out.push_str(&format!("Projects: {loaded}\n"));
        }
        (Some(true), Some(loaded), Some(configured)) => {
            out.push_str(&format!("Projects loaded: {loaded}\n"));
            out.push_str(&format!("Projects configured: {configured}\n"));
        }
        (Some(true), Some(loaded), None) => {
            out.push_str(&format!("Projects loaded: {loaded}\n"));
            out.push_str("Projects configured: unknown\n");
        }
        (_, Some(loaded), Some(configured)) => {
            out.push_str(&format!("Projects configured: {configured}\n"));
            out.push_str(&format!("Projects last reported: {loaded}\n"));
        }
        (_, _, Some(configured)) => {
            out.push_str(&format!("Projects configured: {configured}\n"));
        }
        (_, Some(loaded), None) => {
            out.push_str(&format!("Projects last reported: {loaded}\n"));
            out.push_str("Projects configured: unknown\n");
        }
        (_, None, None) => out.push_str("Projects configured: unknown\n"),
    }

    let add_project = |out: &mut String| {
        let command = format!(
            "{} /path/to/project",
            shell_command(&[
                "webcodex".to_string(),
                "project".to_string(),
                "register".to_string(),
                "--config".to_string(),
                config.to_string_lossy().into_owned(),
            ])
        );
        out.push_str("\nAdd a project:\n");
        out.push_str(&format!("  {command}\n"));
    };

    if client_online == Some(true) {
        match (loaded_project_count, configured_project_count) {
            (Some(loaded), Some(configured)) if loaded == configured && loaded > 0 => {
                out.push_str("\nWebCodex is ready to use registered projects.\n");
            }
            (Some(0), Some(0)) => {
                out.push_str("\nRunner connected, but no project has been added.\n");
                add_project(&mut out);
            }
            (Some(_), Some(_)) => {
                out.push_str(
                    "\nRunner connected, but configured project changes are not fully loaded.\n",
                );
                out.push_str("\nRunner restart required.\n");
                out.push_str("\nNext:\n  Restart the existing Runner, then check `webcodex runner status` again.\n");
            }
            (_, Some(0)) => {
                out.push_str("\nNo project is configured locally.\n");
                add_project(&mut out);
            }
            _ => {
                out.push_str("\nRunner connected, but project readiness could not be verified.\n");
                out.push_str("Readiness: unknown.\n");
            }
        }
    } else if client_online == Some(false) {
        match configured_project_count {
            Some(0) => {
                out.push_str("\nNo project has been added.\n");
                add_project(&mut out);
            }
            Some(_) => {
                out.push_str("\nNext:\n  Start or restart this Runner, then check status again.\n")
            }
            None => out.push_str("\nProject registration could not be verified locally.\n"),
        }
    } else {
        match configured_project_count {
            Some(0) => {
                out.push_str("\nNo project has been added.\n");
                add_project(&mut out);
            }
            Some(_) => out.push_str("\nProject registration exists locally, but Runner connection and loaded projects were not checked.\n"),
            None => out.push_str("\nProject registration and Runner connection were not checked.\n"),
        }
    }
    out
}

pub(crate) async fn run_runner_status(opts: RunnerStatusOptions) -> Result<String, String> {
    let service_unit = service_unit_name(&opts.service_file, RUNNER_SERVICE_UNIT);
    let local = opts
        .local_state_dir
        .as_ref()
        .filter(|dir| local_runner_profile_marker(dir).is_file())
        .map(|dir| local_runner_state_summary(dir))
        .transpose()?;
    let systemd = local
        .is_none()
        .then(|| query_systemd_service_status_for_scope(opts.scope, &service_unit));
    let metadata = read_runner_config_metadata(&opts.config)?;
    let effective_server_url = opts.server_url.clone().or_else(|| {
        let url = metadata.server_url.trim().to_string();
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    });
    let user_token = if local.is_some() {
        let key = metadata.token.trim();
        if key.is_empty() {
            return Err("hosted Runner config has an empty shared key".to_string());
        }
        Some(key.to_string())
    } else {
        read_optional_user_api_token(&opts.user_token_file, "--user-token-file")?
    };
    let runner_token = if local.is_some() {
        None
    } else {
        read_optional_token(&opts.runner_token_file, "--runner-token-file")?
    };

    let mut runtime_http: Option<HttpStatusSummary> = None;
    let mut client_online: Option<bool> = None;
    let mut loaded_project_count: Option<usize> = None;
    if let (Some(server_url), Some(token)) =
        (effective_server_url.as_deref(), user_token.as_deref())
    {
        let http = fetch_runtime_status(server_url, &opts.server_http, Some(token)).await?;
        if let Some(output) = http.output.as_ref() {
            if !metadata.client_id.trim().is_empty() {
                client_online = runtime_client_online(output, &metadata.client_id);
                loaded_project_count = runtime_client_project_count(output, &metadata.client_id);
            }
        }
        runtime_http = Some(http);
    }

    let mut runner_token_boundary_status: Option<&'static str> = None;
    let mut runner_token_boundary_detail: Option<String> = None;
    if let (Some(server_url), Some(token)) =
        (effective_server_url.as_deref(), runner_token.as_deref())
    {
        match http_post_json_status(
            server_url,
            &opts.server_http,
            "/api/runtime/status",
            Some(token),
            json!({}),
        )
        .await
        {
            Ok((status, content_type, _)) if status == 401 || status == 403 => {
                runner_token_boundary_status = Some("PASS");
                runner_token_boundary_detail =
                    Some("Runner transport token cannot call /api/runtime/status".to_string());
                let _ = content_type;
            }
            Ok((status, content_type, _)) => {
                runner_token_boundary_status = Some("FAIL");
                runner_token_boundary_detail = Some(format!(
                    "unexpected HTTP {} content-type {}",
                    status, content_type
                ));
            }
            Err(e) => {
                runner_token_boundary_status = Some("FAIL");
                runner_token_boundary_detail = Some(e);
            }
        }
    }

    if opts.json {
        let summary = json!({
            "service": {
                "mode": if local.is_some() { "hosted_local" } else { "systemd" },
                "scope": if local.is_some() { Value::Null } else { json!(opts.scope.as_str()) },
                "unit": service_unit,
                "active": local.as_ref().map(|state| json!(state.running)).unwrap_or_else(|| json!(systemd.as_ref().expect("systemd status exists outside hosted mode").active)),
                "enabled": local.as_ref().map(|state| json!(state.managed)).unwrap_or_else(|| json!(systemd.as_ref().expect("systemd status exists outside hosted mode").enabled)),
                "pid": local.as_ref().and_then(|state| state.pid),
                "logs": local.as_ref().map(|state| state.log_path.to_string_lossy().to_string()),
            },
            "config": {
                "path": metadata.path.to_string_lossy(),
                "client_id": metadata.client_id,
                "owner": metadata.owner,
                "display_name": metadata.display_name,
                "transport": metadata.transport,
                "project_registry_dir": metadata.project_registry_dir.to_string_lossy().to_string(),
                "allowed_roots": {
                    "count": metadata.allowed_roots.len(),
                    "summary": allowed_roots_summary(&metadata.allowed_roots),
                    "paths": metadata.allowed_roots.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
                },
                "server_url": metadata.server_url,
            },
            "runtime": runtime_http.as_ref().map(|http| json!({
                "checked": true,
                "reachable": http.reachable,
                "status_code": http.status_code,
                "content_type": http.content_type,
                "error": http.error,
                "client_online": client_online,
            })).unwrap_or_else(|| json!({
                "checked": false,
                "reason": "requires server URL and --user-token-file",
            })),
            "agent_token_boundary": runner_token_boundary_status.map(|status| json!({
                "checked": true,
                "status": status,
                "detail": runner_token_boundary_detail,
            })).unwrap_or_else(|| json!({
                "checked": false,
                "reason": "requires server URL and --runner-token-file",
            })),
        });
        return serde_json::to_string_pretty(&summary).map_err(|e| e.to_string());
    }

    let configured_project_count = read_enabled_project_count(&metadata.project_registry_dir).ok();
    let mut out = render_runner_readiness_summary(
        client_online,
        loaded_project_count,
        configured_project_count,
        &metadata.path,
    );
    out.push_str("\nDetails:\n");
    if let Some(local) = &local {
        out.push_str("  runner mode:          hosted local process\n");
        out.push_str(&format!("  runner active:        {}\n", local.running));
        out.push_str(&format!(
            "  runner pid:           {}\n",
            local
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "none".to_string())
        ));
        out.push_str(&format!(
            "  runner logs:          {}\n",
            local.log_path.display()
        ));
    } else {
        let systemd = systemd
            .as_ref()
            .expect("systemd status exists outside hosted mode");
        out.push_str(&format!(
            "  service scope:        {}\n",
            opts.scope.as_str()
        ));
        out.push_str(&format!("  service unit:         {service_unit}\n"));
        out.push_str(&format!("  service active:       {}\n", systemd.active));
        out.push_str(&format!("  service enabled:      {}\n", systemd.enabled));
    }
    out.push_str(&format!(
        "  config:               {}\n",
        metadata.path.display()
    ));
    out.push_str(&format!(
        "  client_id:            {}\n",
        if metadata.client_id.trim().is_empty() {
            "unknown"
        } else {
            metadata.client_id.as_str()
        }
    ));
    out.push_str(&format!(
        "  owner:                {}\n",
        metadata.owner.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  display_name:         {}\n",
        metadata.display_name.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  transport:            {}\n",
        metadata.transport.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  project_registry_dir: {}\n",
        metadata.project_registry_dir.display()
    ));
    out.push_str(&format!(
        "  allowed_roots:        {}\n",
        allowed_roots_summary(&metadata.allowed_roots)
    ));
    out.push_str(&format!(
        "  server_url:           {}\n",
        if metadata.server_url.trim().is_empty() {
            "unknown"
        } else {
            metadata.server_url.as_str()
        }
    ));
    match runtime_http {
        Some(http) => {
            out.push_str(&format!(
                "  runtime reachable:    {}\n",
                if http.reachable { "yes" } else { "no" }
            ));
            if let Some(code) = http.status_code {
                out.push_str(&format!("  runtime status:       {}\n", code));
            }
            if let Some(content_type) = &http.content_type {
                out.push_str(&format!("  runtime content-type: {}\n", content_type));
            }
            if let Some(error) = &http.error {
                out.push_str(&format!("  runtime error:        {}\n", error));
            }
            out.push_str(&format!(
                "  client online:        {}\n",
                client_online
                    .map(|online| if online { "yes" } else { "no" })
                    .unwrap_or("unknown")
            ));
        }
        None => out.push_str(
            "  runtime check:        skipped (requires server URL and --user-token-file)\n",
        ),
    }
    match runner_token_boundary_status {
        Some(status) => out.push_str(&format!(
            "  Runner token boundary: {} ({})\n",
            status,
            runner_token_boundary_detail.unwrap_or_else(|| "unknown".to_string())
        )),
        None => out.push_str(
            "  Runner token boundary: skipped (requires server URL and --runner-token-file)\n",
        ),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_readiness_requires_registered_project_and_observed_connection() {
        let config = Path::new("/tmp/webcodex/runner.toml");

        let ready = render_runner_readiness_summary(Some(true), Some(1), Some(1), config);
        assert!(ready.contains("Runner: connected"), "{ready}");
        assert!(ready.contains("Projects: 1"), "{ready}");
        assert!(
            ready.contains("WebCodex is ready to use registered projects."),
            "{ready}"
        );

        let zero = render_runner_readiness_summary(Some(true), Some(0), Some(0), config);
        assert!(
            zero.contains("Runner connected, but no project has been added."),
            "{zero}"
        );
        assert!(
            zero.contains(
                "webcodex project register --config /tmp/webcodex/runner.toml /path/to/project"
            ),
            "{zero}"
        );
        assert!(!zero.contains("ready to use"), "{zero}");

        let pending_restart = render_runner_readiness_summary(Some(true), Some(0), Some(1), config);
        assert!(
            pending_restart.contains("configured project changes are not fully loaded"),
            "{pending_restart}"
        );
        assert!(
            pending_restart.contains("Runner restart required."),
            "{pending_restart}"
        );
        assert!(
            !pending_restart.contains("ready to use"),
            "{pending_restart}"
        );

        let partial_reload = render_runner_readiness_summary(Some(true), Some(1), Some(2), config);
        assert!(
            partial_reload.contains("Projects loaded: 1"),
            "{partial_reload}"
        );
        assert!(
            partial_reload.contains("Projects configured: 2"),
            "{partial_reload}"
        );
        assert!(
            partial_reload.contains("Runner restart required."),
            "{partial_reload}"
        );
        assert!(!partial_reload.contains("ready to use"), "{partial_reload}");

        let unknown = render_runner_readiness_summary(None, None, Some(1), config);
        assert!(
            unknown.contains("Runner connection: not checked"),
            "{unknown}"
        );
        assert!(!unknown.contains("ready to use"), "{unknown}");

        let online_unknown = render_runner_readiness_summary(Some(true), None, Some(1), config);
        assert!(
            online_unknown.contains("project readiness could not be verified"),
            "{online_unknown}"
        );
        assert!(
            online_unknown.contains("Readiness: unknown."),
            "{online_unknown}"
        );
        assert!(!online_unknown.contains("ready to use"), "{online_unknown}");

        let unreadable_registry =
            render_runner_readiness_summary(Some(true), Some(1), None, config);
        assert!(
            unreadable_registry.contains("Projects configured: unknown"),
            "{unreadable_registry}"
        );
        assert!(
            unreadable_registry.contains("Readiness: unknown."),
            "{unreadable_registry}"
        );
        assert!(
            !unreadable_registry.contains("ready to use"),
            "{unreadable_registry}"
        );
    }

    #[test]
    fn configured_project_count_matches_runner_enabled_project_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let project_registry_dir = tmp.path().join("project-registry");
        std::fs::create_dir_all(&project_registry_dir).unwrap();
        let enabled_path = tmp.path().join("enabled");
        let disabled_path = tmp.path().join("disabled");
        std::fs::write(
            project_registry_dir.join("enabled.toml"),
            format!(
                "id = \"enabled\"\npath = {:?}\ndisabled = false\n",
                enabled_path.to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(
            project_registry_dir.join("disabled.toml"),
            format!(
                "id = \"disabled\"\npath = {:?}\ndisabled = true\n",
                disabled_path.to_string_lossy()
            ),
        )
        .unwrap();

        assert_eq!(
            read_enabled_project_count(&project_registry_dir).unwrap(),
            1
        );
    }

    #[test]
    fn runner_status_metadata_accepts_legacy_registry_alias_and_rejects_both_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("agent.toml");
        let legacy = tmp.path().join("projects.d");
        std::fs::write(
            &config,
            format!(
                "server_url = \"https://example.test\"\ntoken = \"t\"\nclient_id = \"demo\"\nprojects_dir = {:?}\n",
                legacy.to_string_lossy()
            ),
        )
        .unwrap();
        let metadata = read_runner_config_metadata(&config).unwrap();
        assert_eq!(metadata.project_registry_dir, legacy);

        let current = tmp.path().join("project-registry");
        std::fs::write(
            &config,
            format!(
                "server_url = \"https://example.test\"\ntoken = \"t\"\nclient_id = \"demo\"\nproject_registry_dir = {:?}\nprojects_dir = {:?}\n",
                current.to_string_lossy(),
                legacy.to_string_lossy()
            ),
        )
        .unwrap();
        let error = read_runner_config_metadata(&config).unwrap_err();
        assert!(error.contains("cannot both be configured"), "{error}");
    }

    #[test]
    fn service_unit_name_tracks_the_selected_profile_unit() {
        assert_eq!(
            service_unit_name(
                Path::new("/etc/systemd/system/webcodex-runner-workstation.service"),
                RUNNER_SERVICE_UNIT,
            ),
            "webcodex-runner-workstation.service"
        );
        assert_eq!(
            service_unit_name(
                Path::new("/etc/systemd/system/webcodex-runner.service"),
                RUNNER_SERVICE_UNIT,
            ),
            RUNNER_SERVICE_UNIT
        );
    }
}
