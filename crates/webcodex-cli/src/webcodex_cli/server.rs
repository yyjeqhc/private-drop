use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use webcodex::SERVER_SYSTEMD_TIMEOUT_STOP_SECS;
use webcodex_admin::ServerHttpOptions;

use crate::{
    ServerInitOptions, ServerInstallServiceOptions, ServerTunnelOptions, ServiceActionKind,
    ServiceActionOptions,
};

use super::{
    compare_build_commits, control_server_unit_pair, encode_exec_program, encode_unit_path_value,
    fetch_runtime_status, generate_bootstrap_token, install_server_unit_pair, is_effective_root,
    local_cli_build_metadata, query_systemd_service_status, query_systemd_socket_status,
    read_env_file_value, render_build_metadata_block, render_server_env, run_logs,
    runtime_build_metadata, server_status_revision_check, service_unit_name, shell_command,
    system_group_exists, system_user_exists, token_prefix, uninstall_server_unit_pair,
    validate_systemd_identity, SERVER_SERVICE_UNIT, SERVER_SOCKET_UNIT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerStatusOptions {
    pub(crate) url: String,
    pub(crate) url_explicit: bool,
    pub(crate) server_http: ServerHttpOptions,
    pub(crate) env_file: Option<PathBuf>,
    pub(crate) env_file_explicit: bool,
    pub(crate) token_file: Option<PathBuf>,
    pub(crate) service_file: PathBuf,
    pub(crate) json: bool,
}

pub(crate) async fn run_server_tunnel(opts: ServerTunnelOptions) -> Result<(), String> {
    let local_server_url = derive_regular_tunnel_server_url(&opts.env_file)?;
    let bootstrap_token = derive_regular_tunnel_bootstrap_token(&opts.env_file)?;
    let runtime_parent = opts
        .env_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    webcodex::run_regular_server_tunnel(webcodex::RegularServerTunnelOptions {
        local_server_url,
        bootstrap_token,
        runtime_parent,
    })
    .await
}

pub(crate) fn derive_regular_tunnel_bootstrap_token(env_file: &Path) -> Result<String, String> {
    let value = match std::env::var("WEBCODEX_TOKEN") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => read_env_file_value(env_file, "WEBCODEX_TOKEN")?,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("WEBCODEX_TOKEN is not valid UTF-8".to_string())
        }
    };
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "regular Server Tunnel requires the effective local Server WEBCODEX_TOKEN".to_string()
        })
}

pub(crate) fn derive_regular_tunnel_server_url(env_file: &Path) -> Result<String, String> {
    if !env_file.is_file() {
        return Err(format!("env file {} does not exist", env_file.display()));
    }
    let value = read_env_file_value(env_file, "WEBCODEX_ADDR")?
        .ok_or_else(|| format!("{} does not define WEBCODEX_ADDR", env_file.display()))?;
    let mut addr = value.trim().parse::<SocketAddr>().map_err(|error| {
        format!(
            "WEBCODEX_ADDR {:?} from {} is not a fixed IP socket address: {error}",
            value.trim(),
            env_file.display()
        )
    })?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    if !addr.ip().is_loopback() {
        return Err("server tunnel requires a loopback WEBCODEX_ADDR".to_string());
    }
    Ok(format!("http://{addr}"))
}

pub(crate) fn run_server_init(opts: ServerInitOptions) -> Result<String, String> {
    if opts.env_file.exists() && !opts.overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to update it",
            opts.env_file.display()
        ));
    }
    std::fs::create_dir_all(&opts.data_dir).map_err(|error| {
        format!(
            "failed to create Server data directory {}: {error}",
            opts.data_dir.display()
        )
    })?;
    if !opts.data_dir.is_dir() {
        return Err(format!(
            "Server data path {} is not a directory",
            opts.data_dir.display()
        ));
    }
    let existing_token = if opts.env_file.exists() {
        read_env_file_value(&opts.env_file, "WEBCODEX_TOKEN")?
            .filter(|token| !token.trim().is_empty())
    } else {
        None
    };
    let token_generated = existing_token.is_none();
    let token = existing_token.unwrap_or_else(generate_bootstrap_token);
    let env_content = render_server_env(&opts, &token);
    super::system::write_server_secret_text_file(&opts.env_file, &env_content, opts.overwrite)?;
    let foreground_command = shell_command(&[
        "webcodex".to_string(),
        "server".to_string(),
        "run".to_string(),
        "--env-file".to_string(),
        opts.env_file.to_string_lossy().into_owned(),
    ]);
    let status_command = shell_command(&[
        "webcodex".to_string(),
        "server".to_string(),
        "status".to_string(),
        "--env-file".to_string(),
        opts.env_file.to_string_lossy().into_owned(),
    ]);
    let install_command = (cfg!(target_os = "linux") && is_effective_root()).then(|| {
        shell_command(&[
            "webcodex".to_string(),
            "server".to_string(),
            "install".to_string(),
            "--env-file".to_string(),
            opts.env_file.to_string_lossy().into_owned(),
            "--working-directory".to_string(),
            opts.data_dir.to_string_lossy().into_owned(),
        ])
    });
    if opts.json {
        let mut next_steps = Vec::new();
        if let Some(command) = &install_command {
            next_steps.push(command.clone());
        }
        next_steps.push(foreground_command.clone());
        next_steps.push(status_command.clone());
        next_steps.push("configure HTTPS/public URL separately if using GPT Actions".to_string());
        let summary = json!({
            "env_file": opts.env_file.to_string_lossy(),
            "listen": opts.listen,
            "data_dir": opts.data_dir.to_string_lossy(),
            "public_url": opts.public_url,
            "open": opts.open,
            "shared_key_enabled": true,
            "token_generated": token_generated,
            "token_prefix": token_prefix(&token),
            "wrote_env_file": true,
            "next_steps": next_steps,
        });
        return serde_json::to_string_pretty(&summary).map_err(|e| e.to_string());
    }
    let mut out = String::new();
    out.push_str("WebCodex Server configured.\n\n");
    out.push_str("Data:\n");
    out.push_str(&format!("  {}\n", opts.data_dir.display()));
    out.push_str("\nNext:\n");
    if let Some(command) = &install_command {
        out.push_str(&format!("  {command}\n"));
        out.push_str("\nForeground alternative:\n");
        out.push_str(&format!("  {foreground_command}\n"));
        out.push_str("  Keep that terminal open while using the foreground Server.\n");
    } else {
        out.push_str(&format!("  {foreground_command}\n"));
        out.push_str("  Keep that terminal open while using the foreground Server.\n");
    }
    out.push_str("\nCheck:\n");
    out.push_str(&format!("  {status_command}\n"));
    out.push_str("\nDetails:\n");
    out.push_str(&format!("  Configuration: {}\n", opts.env_file.display()));
    out.push_str(&format!("  Listen:        {}\n", opts.listen));
    Ok(out)
}

fn server_socket_file(service_file: &Path) -> Result<PathBuf, String> {
    if service_file.file_name().is_none() {
        return Err(format!(
            "invalid service unit path: {}",
            service_file.display()
        ));
    }
    Ok(service_file.with_extension("socket"))
}

fn configured_socket_addr(env_file: &Path) -> Result<String, String> {
    if !env_file.exists() {
        return Err(format!("env file {} does not exist", env_file.display()));
    }
    let value = read_env_file_value(env_file, "WEBCODEX_ADDR")?
        .ok_or_else(|| format!("{} does not define WEBCODEX_ADDR", env_file.display()))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "{} defines an empty WEBCODEX_ADDR",
            env_file.display()
        ));
    }
    let addr = value.parse::<SocketAddr>().map_err(|error| {
        format!(
            "WEBCODEX_ADDR {value:?} from {} is not a fixed IP socket address valid for systemd ListenStream: {error}",
            env_file.display()
        )
    })?;
    Ok(addr.to_string())
}

fn preflight_server_install(opts: &ServerInstallServiceOptions) -> Result<(), String> {
    let binary = std::fs::metadata(&opts.bin).map_err(|_| {
        format!(
            "cannot install WebCodex Server: binary {} does not exist",
            opts.bin.display()
        )
    })?;
    if !binary.is_file() {
        return Err(format!(
            "cannot install WebCodex Server: binary {} is not a regular file",
            opts.bin.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if binary.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "cannot install WebCodex Server: binary {} is not executable",
                opts.bin.display()
            ));
        }
    }
    let working = std::fs::metadata(&opts.working_directory).map_err(|_| {
        format!(
            "cannot install WebCodex Server: WorkingDirectory {} does not exist\n\nCreate it or pass --working-directory <existing-directory>.",
            opts.working_directory.display()
        )
    })?;
    if !working.is_dir() {
        return Err(format!(
            "cannot install WebCodex Server: WorkingDirectory {} is not a directory",
            opts.working_directory.display()
        ));
    }
    let env = std::fs::metadata(&opts.env_file).map_err(|_| {
        format!(
            "cannot install WebCodex Server: env file {} does not exist",
            opts.env_file.display()
        )
    })?;
    if !env.is_file() {
        return Err(format!(
            "cannot install WebCodex Server: env file {} is not a regular file",
            opts.env_file.display()
        ));
    }
    std::fs::File::open(&opts.env_file).map_err(|error| {
        format!(
            "cannot install WebCodex Server: env file {} is not readable: {error}",
            opts.env_file.display()
        )
    })?;
    if let Some(user) = &opts.user {
        if !system_user_exists(user) {
            return Err(format!(
                "cannot install WebCodex Server: User={user} does not name a local account"
            ));
        }
    }
    if let Some(group) = &opts.group {
        if !system_group_exists(group) {
            return Err(format!(
                "cannot install WebCodex Server: Group={group} does not name a local group"
            ));
        }
    }
    Ok(())
}

pub(crate) fn run_server_install_service(
    opts: ServerInstallServiceOptions,
) -> Result<String, String> {
    let service_unit = service_unit_name(&opts.service_file, SERVER_SERVICE_UNIT);
    if !opts.output_stdout && !opts.dry_run {
        preflight_server_install(&opts)?;
    }
    let socket_file = server_socket_file(&opts.service_file)?;
    let socket_unit = service_unit_name(&socket_file, SERVER_SOCKET_UNIT);
    let rendered_service = render_systemd_unit(&opts, &socket_unit)?;
    let listen = configured_socket_addr(&opts.env_file)?;
    let rendered_socket = render_systemd_socket_unit(&listen, &service_unit)?;
    if opts.output_stdout || opts.dry_run {
        if opts.json {
            return serde_json::to_string_pretty(&json!({
                "service_file": opts.service_file.to_string_lossy(),
                "socket_file": socket_file.to_string_lossy(),
                "env_file": opts.env_file.to_string_lossy(),
                "bin": opts.bin.to_string_lossy(),
                "working_directory": opts.working_directory.to_string_lossy(),
                "service_unit": service_unit,
                "socket_unit": socket_unit,
                "listen": listen,
                "dry_run": true,
                "no_start": opts.no_start,
                "systemd_called": false,
                "units": {
                    "service": rendered_service,
                    "socket": rendered_socket,
                },
            }))
            .map_err(|e| e.to_string());
        }
        return Ok(format!(
            "# {}\n{}\n# {}\n{}",
            opts.service_file.display(),
            rendered_service,
            socket_file.display(),
            rendered_socket
        ));
    }
    let result = install_server_unit_pair(
        &opts.service_file,
        &service_unit,
        &rendered_service,
        &socket_file,
        &socket_unit,
        &rendered_socket,
        opts.overwrite,
        opts.no_start,
    )?;
    if opts.json {
        return serde_json::to_string_pretty(&json!({
            "service_file": opts.service_file.to_string_lossy(),
            "socket_file": socket_file.to_string_lossy(),
            "env_file": opts.env_file.to_string_lossy(),
            "bin": opts.bin.to_string_lossy(),
            "service_unit": service_unit,
            "working_directory": opts.working_directory.to_string_lossy(),
            "socket_unit": socket_unit,
            "listen": listen,
            "enabled": true,
            "started": result.started,
        }))
        .map_err(|e| e.to_string());
    }
    Ok(format!(
        "Server socket/service pair installed.\n\n  service file:      {}\n  socket file:       {}\n  service unit:      {}\n  socket unit:       {}\n  binary:            {}\n  env file:          {}\n  working directory: {}\n  listen:            {}\n  enabled:           yes\n  started:           {}\n",
        opts.service_file.display(),
        socket_file.display(),
        service_unit,
        socket_unit,
        opts.bin.display(),
        opts.env_file.display(),
        opts.working_directory.display(),
        listen,
        if result.started { "yes" } else { "no (--no-start)" }
    ))
}

fn render_systemd_unit(
    opts: &ServerInstallServiceOptions,
    socket_unit: &str,
) -> Result<String, String> {
    let environment_file = encode_unit_path_value("EnvironmentFile", &opts.env_file)?;
    let exec_start = encode_exec_program("ExecStart", &opts.bin)?;
    let working_directory = encode_unit_path_value("WorkingDirectory", &opts.working_directory)?;
    if let Some(user) = &opts.user {
        validate_systemd_identity("User", user)?;
    }
    if let Some(group) = &opts.group {
        validate_systemd_identity("Group", group)?;
    }

    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str("Description=WebCodex Runtime\n");
    unit.push_str(&format!("Requires={socket_unit}\n"));
    unit.push_str(&format!("After=network-online.target {socket_unit}\n"));
    unit.push_str("Wants=network-online.target\n\n");
    unit.push_str("[Service]\n");
    unit.push_str("Type=simple\n");
    unit.push_str(&format!("EnvironmentFile={environment_file}\n"));
    unit.push_str(&format!("ExecStart={exec_start}\n"));
    unit.push_str("Restart=on-failure\n");
    unit.push_str("RestartSec=3\n");
    unit.push_str(&format!(
        "TimeoutStopSec={SERVER_SYSTEMD_TIMEOUT_STOP_SECS}s\n"
    ));
    unit.push_str(&format!("WorkingDirectory={working_directory}\n"));
    if let Some(user) = &opts.user {
        unit.push_str(&format!("User={user}\n"));
    }
    if let Some(group) = &opts.group {
        unit.push_str(&format!("Group={group}\n"));
    }
    unit.push_str("\n[Install]\n");
    unit.push_str("WantedBy=multi-user.target\n");
    Ok(unit)
}

fn render_systemd_socket_unit(listen: &str, service_unit: &str) -> Result<String, String> {
    listen
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid systemd ListenStream address {listen:?}: {error}"))?;
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str("Description=WebCodex HTTP Socket\n\n");
    unit.push_str("[Socket]\n");
    unit.push_str(&format!("ListenStream={listen}\n"));
    unit.push_str(&format!("Service={service_unit}\n"));
    unit.push_str("FileDescriptorName=webcodex-http\n\n");
    unit.push_str("[Install]\n");
    unit.push_str("WantedBy=sockets.target\n");
    Ok(unit)
}

pub(crate) fn run_server_service(opts: ServiceActionOptions) -> Result<String, String> {
    match opts.kind {
        ServiceActionKind::Control(control) => {
            let socket_file = server_socket_file(&opts.service_file)?;
            let socket_unit = service_unit_name(&socket_file, SERVER_SOCKET_UNIT);
            control_server_unit_pair(&opts.unit, &socket_unit, control)?;
            Ok(format!(
                "Server {} completed for service {} and socket {}.\n",
                control.as_str(),
                opts.unit,
                socket_unit
            ))
        }
        ServiceActionKind::Logs {
            lines,
            since,
            follow,
        } => run_logs(&opts.unit, lines, since.as_deref(), follow),
        ServiceActionKind::Uninstall { confirm } => {
            if !confirm {
                return Err("server uninstall requires --confirm; no changes were made".to_string());
            }
            let socket_file = server_socket_file(&opts.service_file)?;
            let socket_unit = service_unit_name(&socket_file, SERVER_SOCKET_UNIT);
            let result = uninstall_server_unit_pair(
                &opts.service_file,
                &opts.unit,
                &socket_file,
                &socket_unit,
            )?;
            Ok(format!(
                "Server service {}. Configuration and data were not deleted.\n",
                if result.removed {
                    "uninstalled"
                } else {
                    "was already absent"
                }
            ))
        }
    }
}

fn resolve_status_token(opts: &ServerStatusOptions) -> Result<Option<String>, String> {
    if let Some(path) = &opts.token_file {
        let token = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read token file {}: {}", path.display(), e))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err("--token-file cannot be empty".to_string());
        }
        return Ok(Some(token));
    }
    if let Some(path) = &opts.env_file {
        if !path.exists() {
            if opts.env_file_explicit {
                return Err(format!("env file {} does not exist", path.display()));
            }
        } else if let Some(token) = read_env_file_value(path, "WEBCODEX_TOKEN")? {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(Some(token));
            }
        }
    }
    if let Ok(token) = std::env::var("WEBCODEX_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

pub(crate) fn derive_server_status_url(opts: &ServerStatusOptions) -> Result<String, String> {
    if opts.url_explicit {
        return Ok(opts.url.clone());
    }
    let Some(env_file) = &opts.env_file else {
        return Ok(opts.url.clone());
    };
    if !env_file.exists() {
        if opts.env_file_explicit {
            return Err(format!("env file {} does not exist", env_file.display()));
        }
        return Ok(opts.url.clone());
    }
    let Some(value) = read_env_file_value(env_file, "WEBCODEX_ADDR")? else {
        return Ok(opts.url.clone());
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "{} defines an empty WEBCODEX_ADDR",
            env_file.display()
        ));
    }
    let mut addr = value.parse::<SocketAddr>().map_err(|error| {
        format!(
            "WEBCODEX_ADDR {value:?} from {} is not a fixed IP socket address: {error}",
            env_file.display()
        )
    })?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    Ok(format!("http://{addr}"))
}

pub(crate) async fn run_server_status(opts: ServerStatusOptions) -> Result<String, String> {
    let probe_url = derive_server_status_url(&opts)?;
    let service_unit = service_unit_name(&opts.service_file, SERVER_SERVICE_UNIT);
    let socket_file = server_socket_file(&opts.service_file)?;
    let socket_unit = service_unit_name(&socket_file, SERVER_SOCKET_UNIT);
    let systemd = query_systemd_service_status(&service_unit);
    let socket = query_systemd_socket_status(&socket_unit);
    let token = resolve_status_token(&opts)?;
    let http = fetch_runtime_status(&probe_url, &opts.server_http, token.as_deref()).await?;
    let output = http.output.as_ref();
    let auth_enabled = output.and_then(|v| v.get("auth_enabled")).cloned();
    let configured_public_url = output
        .and_then(|v| v.get("configured_public_url"))
        .cloned()
        .unwrap_or(Value::Null);
    let tools_count = output
        .and_then(|v| v.pointer("/tools/count"))
        .and_then(Value::as_u64);
    let agents_online_count = output
        .and_then(|v| v.pointer("/agents/online_count"))
        .and_then(Value::as_u64);
    let server_build = runtime_build_metadata(output);
    let local_build = local_cli_build_metadata();
    let revision_comparison = compare_build_commits(
        local_build.git_commit.as_deref(),
        server_build.git_commit.as_deref(),
    );
    if opts.json {
        let summary = json!({
            "http_reachable": http.reachable,
            "probe_url": probe_url,
            "http_status_code": http.status_code,
            "http_content_type": http.content_type,
            "http_error": http.error,
            "service": {
                "unit": service_unit,
                "loaded": systemd.loaded,
                "active": systemd.active,
                "enabled": systemd.enabled,
            },
            "socket": {
                "unit": socket_unit,
                "loaded": socket.loaded,
                "active": socket.active,
                "enabled": socket.enabled,
            },
            "auth_enabled": auth_enabled.unwrap_or(Value::Null),
            "configured_public_url": configured_public_url,
            "tools": {
                "count": tools_count,
            },
            "agents": {
                "online_count": agents_online_count,
            },
            "server_build": {
                "version": server_build.version,
                "git_commit": server_build.git_commit,
                "git_dirty": server_build.git_dirty,
                "built_at": server_build.built_at,
            },
            "local_cli_build": {
                "version": local_build.version,
                "git_commit": local_build.git_commit,
                "git_dirty": local_build.git_dirty,
                "built_at": local_build.built_at,
            },
            "revision_check": server_status_revision_check(&revision_comparison),
        });
        return serde_json::to_string_pretty(&summary).map_err(|e| e.to_string());
    }
    let mut out = String::new();
    out.push_str(if http.reachable {
        "Server: running\n"
    } else {
        "Server: unreachable\n"
    });
    if let Some(count) = agents_online_count {
        out.push_str(&format!("Runners online: {count}\n"));
    }
    out.push_str("\nNext:\n");
    if !http.reachable {
        if let Some(env_file) = opts.env_file.as_ref() {
            let command = shell_command(&[
                "webcodex".to_string(),
                "server".to_string(),
                "run".to_string(),
                "--env-file".to_string(),
                env_file.to_string_lossy().into_owned(),
            ]);
            out.push_str(&format!("  {command}\n"));
        } else {
            out.push_str("  Start the WebCodex Server, then run this status command again.\n");
        }
    } else if agents_online_count == Some(0) {
        out.push_str(
            "  Create a one-time login code in another terminal with `webcodex pairing create`.\n",
        );
    } else if agents_online_count.is_some_and(|count| count > 0) {
        out.push_str(
            "  Check project readiness on the project machine with `webcodex runner status`.\n",
        );
    } else {
        out.push_str("  Continue first-run setup with `webcodex pairing create`, or check an existing Runner.\n");
    }
    out.push_str("\nDetails:\n");
    out.push_str(&format!("  HTTP probe:            {}\n", probe_url));
    out.push_str(&format!(
        "  HTTP reachable:        {}\n",
        if http.reachable { "yes" } else { "no" }
    ));
    if !http.reachable {
        if let Some(code) = http.status_code {
            out.push_str(&format!("  HTTP status:           {}\n", code));
        }
        if let Some(content_type) = &http.content_type {
            out.push_str(&format!("  HTTP content-type:     {}\n", content_type));
        }
        if let Some(error) = &http.error {
            out.push_str(&format!("  HTTP error:            {}\n", error));
        }
    }
    out.push_str(&format!("  service unit:          {}\n", service_unit));
    out.push_str(&format!("  service loaded:        {}\n", systemd.loaded));
    out.push_str(&format!("  service active:        {}\n", systemd.active));
    out.push_str(&format!("  service enabled:       {}\n", systemd.enabled));
    out.push_str(&format!("  socket unit:           {}\n", socket_unit));
    out.push_str(&format!("  socket loaded:         {}\n", socket.loaded));
    out.push_str(&format!("  socket active:         {}\n", socket.active));
    out.push_str(&format!("  socket enabled:        {}\n", socket.enabled));
    out.push_str(&format!(
        "  auth_enabled:          {}\n",
        auth_enabled
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out.push_str(&format!(
        "  configured_public_url: {}\n",
        if configured_public_url.is_null() {
            "null".to_string()
        } else {
            configured_public_url
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| configured_public_url.to_string())
        }
    ));
    out.push_str(&format!(
        "  tools.count:           {}\n",
        tools_count
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out.push_str(&format!(
        "  agents.online_count:   {}\n",
        agents_online_count
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    out.push('\n');
    out.push_str(&render_build_metadata_block("Server build", &server_build));
    out.push('\n');
    out.push_str(&render_build_metadata_block(
        "Local CLI build",
        &local_build,
    ));
    out.push('\n');
    out.push_str("Revision check:\n");
    out.push_str(&format!(
        "  {}\n",
        server_status_revision_check(&revision_comparison)
    ));
    Ok(out)
}
