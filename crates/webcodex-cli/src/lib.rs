//! `webcodex` — standalone management/setup binary for WebCodex.
//!
//! Provides canonical users / tokens / runner-tokens management (reusing the
//! shared `admin_cli` module) and low-level `runner init` (reusing the shared
//! `runner_config` module).
//!
//! This binary intentionally does NOT start a server and does NOT print real
//! tokens, Authorization headers, or full Runner config contents with secrets
//! (except explicit stdout materialization paths such as `runner init --output -`,
//! which the user requests deliberately). Server initialization never prints the
//! full bootstrap token.
//!
//! This package implements the single public `webcodex` CLI; background runtime
//! execution remains in the separate `webcodex-server` and `webcodex-runner`
//! binaries.

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

mod webcodex_cli;

use webcodex_admin as admin_cli;
use webcodex_core::build_info;
use webcodex_runner_config as runner_config;

use admin_cli::{
    parse_admin_cli, run_admin_command, AdminCliCommand, AdminOptions, ServerHttpOptions,
};
use runner_config::{
    run_runner_init, RunnerInitOptions, DEFAULT_INIT_PROJECT_REGISTRY_DIR,
    DEFAULT_POLL_INTERVAL_MS, TRANSPORT_WEBSOCKET,
};
use webcodex_cli::ops::ops_exit_code;
use webcodex_cli::{
    base_dir_or_default, client_profile_project_registry_dir, client_profile_runner_config,
    client_profile_runner_token_file, client_profile_runner_token_file_for_scope,
    client_profile_state_dir, client_profile_user_token_file,
    client_profile_user_token_file_for_scope, connect_usage, current_user_home,
    default_device_name, default_server_paths, disconnect_usage, discover_internal_binary,
    is_effective_root, login_usage, logout_usage, ops_projects_usage, ops_runner_usage,
    ops_runners_usage, ops_smoke_preflight_usage, ops_status_usage, ops_usage, ops_windows_usage,
    pairing_create_usage, pairing_usage, project_activate_usage, project_register_usage,
    read_env_file_value, render_token_generate, run_connect, run_disconnect, run_hosted_log_writer,
    run_internal_binary, run_login, run_logout, run_ops_command, run_pairing_create,
    run_project_activate, run_project_register, run_runner_install_service, run_runner_service,
    run_runner_status, run_runner_token_create_local, run_server_init, run_server_install_service,
    run_server_service, run_server_status, run_server_tunnel, run_status, run_token_create_local,
    runner_config_for_scope, runner_init_usage, runner_install_service_usage,
    runner_service_file_for_scope, runner_status_usage, runner_usage, server_init_usage,
    server_install_service_usage, server_status_usage, server_tunnel_usage, server_usage,
    service_unit_name, status_usage, system_user_home, system_user_is_root, usage,
    validate_client_profile, validate_service_file_scope, write_connect_result, ConnectAuth,
    ConnectOptions, DisconnectOptions, LoginOptions, LogoutOptions, OpsCommand, OpsCommonOptions,
    OpsRunnerOptions, OpsSmokePreflightOptions, OpsWindowsOptions, ProjectActivateOptions,
    ProjectRegisterOptions, ServerStatusOptions, ServiceControl, StatusOptions, DEFAULT_LOG_LINES,
    RUNNER_SERVICE_UNIT, SERVER_SERVICE_FILE, SERVER_SERVICE_UNIT,
};
const SETUP_GPT_SCOPES: &[&str] = &[
    "runtime:read",
    "session:collaborate",
    "project:read",
    "project:write",
    "job:run",
];
const SETUP_RUNNER_SCOPES: &[&str] = &[
    "agent:register",
    "agent:poll",
    "agent:result",
    "agent:job_update",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceScope {
    User,
    System,
}

impl ServiceScope {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "user" => Ok(Self::User),
            "system" => Ok(Self::System),
            _ => Err("--scope must be 'user' or 'system'".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

fn default_runner_service_scope(effective_root: bool) -> ServiceScope {
    if effective_root {
        ServiceScope::System
    } else {
        ServiceScope::User
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    Project(Vec<String>),
    ProjectRegister(ProjectRegisterOptions),
    ProjectActivate(ProjectActivateOptions),
    Connect(ConnectOptions),
    Disconnect(DisconnectOptions),
    HostedLogWriter(PathBuf),
    Admin(AdminCliCommand),
    TokenGenerate(TokenGenerateOptions),
    TokenCreateLocal(TokenCreateLocalOptions),
    RunnerTokenCreateLocal(RunnerTokenCreateLocalOptions),
    RunnerInit(RunnerInitOptions),
    PairingCreate(PairingCreateOptions),
    Login(LoginOptions),
    Logout(LogoutOptions),
    Status(StatusOptions),
    Ops(OpsCommand),
    RunnerInstall(RunnerInstallServiceOptions),
    RunnerStatus(RunnerStatusOptions),
    RunnerRun(InternalRunOptions),
    RunnerService(ServiceActionOptions),
    ServerInit(ServerInitOptions),
    ServerInstall(ServerInstallServiceOptions),
    ServerStatus(ServerStatusOptions),
    ServerTunnel(ServerTunnelOptions),
    ServerRun(InternalRunOptions),
    ServerService(ServiceActionOptions),
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenGenerateOptions {
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct TokenCreateLocalOptions {
    server_url: String,
    server_http: ServerHttpOptions,
    username: String,
    credential: Option<String>,
    credential_env: Option<String>,
    name: Option<String>,
    scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct RunnerTokenCreateLocalOptions {
    admin: AdminOptions,
    username: String,
    client_id: String,
    name: Option<String>,
    scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PairingCreateOptions {
    server_url: String,
    server_http: ServerHttpOptions,
    env_file: Option<PathBuf>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    username: String,
    client_id: String,
    display_name: Option<String>,
    ttl_secs: i64,
    user_token_name: Option<String>,
    runner_token_name: Option<String>,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerInitOptions {
    listen: String,
    data_dir: PathBuf,
    env_file: PathBuf,
    public_url: Option<String>,
    open: bool,
    overwrite: bool,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerTunnelOptions {
    env_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerInstallServiceOptions {
    env_file: PathBuf,
    bin: PathBuf,
    service_file: PathBuf,
    user: Option<String>,
    group: Option<String>,
    working_directory: PathBuf,
    overwrite: bool,
    dry_run: bool,
    output_stdout: bool,
    no_start: bool,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerInstallServiceOptions {
    scope: ServiceScope,
    config: PathBuf,
    bin: PathBuf,
    service_file: PathBuf,
    user: Option<String>,
    group: Option<String>,
    working_directory: PathBuf,
    root_runner: bool,
    allow_root_runner: bool,
    overwrite: bool,
    dry_run: bool,
    output_stdout: bool,
    no_start: bool,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalRunOptions {
    bin: PathBuf,
    args: Vec<String>,
    env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceActionKind {
    Control(ServiceControl),
    Logs {
        lines: u32,
        since: Option<String>,
        follow: bool,
    },
    Uninstall {
        confirm: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceActionOptions {
    scope: ServiceScope,
    service_file: PathBuf,
    unit: String,
    kind: ServiceActionKind,
    local_profile: Option<LocalProfileOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalProfileOptions {
    config: PathBuf,
    state_dir: PathBuf,
    runner_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunnerStatusOptions {
    scope: ServiceScope,
    config: PathBuf,
    service_file: PathBuf,
    local_state_dir: Option<PathBuf>,
    server_url: Option<String>,
    server_http: ServerHttpOptions,
    user_token_file: Option<PathBuf>,
    runner_token_file: Option<PathBuf>,
    json: bool,
}

fn first_run_default_args(
    args: Vec<String>,
    cwd: Option<&Path>,
    platform: &str,
    interactive: bool,
) -> Vec<String> {
    if !args.is_empty()
        || !interactive
        || !matches!(platform, "linux" | "macos")
        || !cwd.is_some_and(path_is_inside_git_checkout)
    {
        return args;
    }
    vec!["share".to_string()]
}

fn path_is_inside_git_checkout(path: &Path) -> bool {
    path.ancestors().any(|ancestor| {
        let marker = ancestor.join(".git");
        marker.is_dir() || marker.is_file()
    })
}

fn cli_action<I, S>(args: I) -> CliAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string()).collect();
    if args.is_empty() {
        return CliAction::Exit {
            code: 0,
            stdout: usage().to_string(),
            stderr: String::new(),
        };
    }
    match args[0].as_str() {
        "--help" | "-h" => CliAction::Exit {
            code: 0,
            stdout: usage().to_string(),
            stderr: String::new(),
        },
        "--version" | "-V" => CliAction::Exit {
            code: 0,
            stdout: build_info::version_output("webcodex"),
            stderr: String::new(),
        },
        "status" | "doctor" | "run" | "share" | "task" => CliAction::Project(args),
        "setup" if args.get(1).map(String::as_str) == Some("single-user") => cli_parse_error(
            "`webcodex setup single-user` was removed; use `webcodex pairing create` followed by `webcodex login`"
                .to_string(),
        ),
        "setup" => CliAction::Project(args),
        "connect" => parse_connect(&args[1..]),
        "disconnect" => parse_disconnect(&args[1..]),
        "project" => parse_project_subcommand(&args[1..]),
        "__hosted-log-writer" => {
            if args.len() == 2 {
                CliAction::HostedLogWriter(PathBuf::from(&args[1]))
            } else {
                cli_parse_error("invalid hosted log writer arguments".to_string())
            }
        }
        "server" => parse_server_subcommand(&args[1..]),
        "pairing" => parse_pairing_subcommand(&args[1..]),
        "client" if args.get(1).map(String::as_str) == Some("enroll") => cli_parse_error(
            "`webcodex client enroll` was removed; use `webcodex login <server-url> --code <code>`"
                .to_string(),
        ),
        "login" => parse_login(&args[1..]),
        "logout" => parse_logout(&args[1..]),
        "auth" => parse_auth_subcommand(&args[1..]),
        "ops" => parse_ops_subcommand(&args[1..]),
        "runner" => parse_runner_subcommand(&args[1..]),
        "agent-token" => cli_parse_error(
            "`webcodex agent-token` was removed; use `webcodex runner-tokens ...`".to_string(),
        ),
        "runner-tokens" | "agent-tokens" => parse_runner_token_subcommand(&args[1..]),
        "token" => cli_parse_error(
            "`webcodex token` was removed; use `webcodex tokens ...`".to_string(),
        ),
        "tokens" => parse_token_subcommand(&args[1..]),
        group if admin_cli::is_admin_group(group) => {
            // users / tokens / Runner transport-token management: reuse admin_cli parser.
            match parse_admin_cli(&args) {
                Ok(cmd) => CliAction::Admin(cmd),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        command => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("unknown command: {command}\n\n{}", usage()),
        },
    }
}

fn parse_connect(args: &[String]) -> CliAction {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return CliAction::Exit {
            code: 0,
            stdout: connect_usage().to_string(),
            stderr: String::new(),
        };
    }
    let mut server_url = None;
    let mut server_http = ServerHttpOptions::default();
    let mut key = None;
    let mut key_file = None;
    let mut auth = ConnectAuth::SharedKey;
    let mut oauth_redirect_uri = None;
    let mut oauth_computer_permissions = false;
    let mut oauth_local_mcp = false;
    let mut oauth_local_plugins = false;
    let mut oauth_local_ssh = false;
    let mut oauth_coding_agent = false;
    let mut username = None;
    let mut project = PathBuf::from(".");
    let mut profile = None;
    let mut client_id = None;
    let mut project_id = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let take = |index: &mut usize| -> Option<String> {
            *index += 1;
            args.get(*index).cloned()
        };
        match arg.as_str() {
            "--proxy" => match take(&mut index) {
                Some(value) => server_http.proxy = Some(value),
                None => return cli_parse_error("--proxy requires a value".to_string()),
            },
            "--no-system-proxy" => server_http.no_system_proxy = true,
            "--auth" => match take(&mut index) {
                Some(value) => {
                    auth = match value.as_str() {
                        "bearer" | "shared-key" => ConnectAuth::SharedKey,
                        "oauth" => ConnectAuth::SharedKeyOAuth,
                        "managed-oauth" => ConnectAuth::ManagedOAuth,
                        _ => {
                            return cli_parse_error(
                                "--auth must be 'bearer', 'oauth', or 'managed-oauth'".to_string(),
                            )
                        }
                    }
                }
                None => return cli_parse_error("--auth requires a value".to_string()),
            },
            "--oauth-redirect-uri" => match take(&mut index) {
                Some(value) => oauth_redirect_uri = Some(value),
                None => {
                    return cli_parse_error("--oauth-redirect-uri requires a value".to_string())
                }
            },
            "--oauth-computer-permissions" => oauth_computer_permissions = true,
            "--oauth-local-mcp" => oauth_local_mcp = true,
            "--oauth-local-plugins" => oauth_local_plugins = true,
            "--oauth-local-ssh" => oauth_local_ssh = true,
            "--oauth-coding-agent" => oauth_coding_agent = true,
            "--user" => match take(&mut index) {
                Some(value) => username = Some(value),
                None => return cli_parse_error(format!("{arg} requires a value")),
            },
            "--key" => match take(&mut index) {
                Some(value) => key = Some(value),
                None => return cli_parse_error("--key requires a value".to_string()),
            },
            "--key-file" => match take(&mut index) {
                Some(value) => key_file = Some(PathBuf::from(value)),
                None => return cli_parse_error("--key-file requires a value".to_string()),
            },
            "--project" => match take(&mut index) {
                Some(value) => project = PathBuf::from(value),
                None => return cli_parse_error("--project requires a value".to_string()),
            },
            "--profile" => match take(&mut index) {
                Some(value) => profile = Some(value),
                None => return cli_parse_error("--profile requires a value".to_string()),
            },
            "--client-id" => match take(&mut index) {
                Some(value) => client_id = Some(value),
                None => return cli_parse_error("--client-id requires a value".to_string()),
            },
            "--project-id" => match take(&mut index) {
                Some(value) => project_id = Some(value),
                None => return cli_parse_error("--project-id requires a value".to_string()),
            },
            other if other.starts_with('-') => {
                return cli_parse_error(format!("unknown connect option: {other}"))
            }
            other => {
                if server_url.is_some() {
                    return cli_parse_error(format!("unexpected connect argument: {other}"));
                }
                server_url = Some(other.to_string());
            }
        }
        index += 1;
    }
    if let Err(error) = server_http.validate() {
        return cli_parse_error(error);
    }
    if key.is_some() && key_file.is_some() {
        return cli_parse_error("--key and --key-file are mutually exclusive".to_string());
    }
    match auth {
        ConnectAuth::SharedKeyOAuth => {
            if oauth_redirect_uri
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return cli_parse_error(
                    "--auth oauth requires --oauth-redirect-uri <URL>".to_string(),
                );
            }
            if username.is_some() {
                return cli_parse_error("--user requires --auth managed-oauth".to_string());
            }
            // This flag only widens the ordinary shared-key OAuth client ceiling;
            // actual optional grants are still selected in browser consent.
        }
        ConnectAuth::ManagedOAuth => {
            if oauth_computer_permissions {
                return cli_parse_error(
                    "--oauth-computer-permissions requires --auth oauth".to_string(),
                );
            }
            if oauth_local_mcp {
                return cli_parse_error("--oauth-local-mcp requires --auth oauth".to_string());
            }
            if oauth_local_plugins {
                return cli_parse_error("--oauth-local-plugins requires --auth oauth".to_string());
            }
            if oauth_local_ssh {
                return cli_parse_error("--oauth-local-ssh requires --auth oauth".to_string());
            }
            if oauth_coding_agent {
                return cli_parse_error("--oauth-coding-agent requires --auth oauth".to_string());
            }
            if key.is_some() || key_file.is_some() {
                return cli_parse_error(
                    "--auth managed-oauth cannot be combined with --key or --key-file".to_string(),
                );
            }
            if oauth_redirect_uri
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                return cli_parse_error(
                    "--auth managed-oauth requires --oauth-redirect-uri <URL>".to_string(),
                );
            }
        }
        ConnectAuth::SharedKey => {
            if oauth_computer_permissions {
                return cli_parse_error(
                    "--oauth-computer-permissions requires --auth oauth".to_string(),
                );
            }
            if oauth_local_mcp {
                return cli_parse_error("--oauth-local-mcp requires --auth oauth".to_string());
            }
            if oauth_local_plugins {
                return cli_parse_error("--oauth-local-plugins requires --auth oauth".to_string());
            }
            if oauth_local_ssh {
                return cli_parse_error("--oauth-local-ssh requires --auth oauth".to_string());
            }
            if oauth_coding_agent {
                return cli_parse_error("--oauth-coding-agent requires --auth oauth".to_string());
            }
            if oauth_redirect_uri.is_some() || username.is_some() {
                return cli_parse_error(
                    "--oauth-redirect-uri requires --auth oauth or managed-oauth; --user requires --auth managed-oauth"
                        .to_string(),
                );
            }
        }
    }
    let Some(server_url) = server_url else {
        return cli_parse_error(
            "connect needs a Server URL, e.g. `webcodex connect https://example.com --project .`"
                .to_string(),
        );
    };
    if let Some(value) = profile.as_deref() {
        if let Err(error) = validate_client_profile(value) {
            return cli_parse_error(error);
        }
    }
    CliAction::Connect(ConnectOptions {
        server_url,
        server_http,
        key,
        key_file,
        auth,
        oauth_redirect_uri,
        oauth_computer_permissions,
        oauth_local_mcp,
        oauth_local_plugins,
        oauth_local_ssh,
        oauth_coding_agent,
        username,
        project,
        profile,
        client_id,
        project_id,
        config_base: None,
        state_base: None,
        runner_bin: None,
        wait_timeout_ms: 15_000,
    })
}

fn parse_disconnect(args: &[String]) -> CliAction {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return CliAction::Exit {
            code: 0,
            stdout: disconnect_usage().to_string(),
            stderr: String::new(),
        };
    }
    let mut project = PathBuf::from(".");
    let mut profile = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let take = |index: &mut usize| -> Option<String> {
            *index += 1;
            args.get(*index).cloned()
        };
        match arg.as_str() {
            "--project" => match take(&mut index) {
                Some(value) => project = PathBuf::from(value),
                None => return cli_parse_error("--project requires a value".to_string()),
            },
            "--profile" => match take(&mut index) {
                Some(value) => profile = Some(value),
                None => return cli_parse_error("--profile requires a value".to_string()),
            },
            other if other.starts_with('-') => {
                return cli_parse_error(format!("unknown disconnect option: {other}"))
            }
            other => return cli_parse_error(format!("unexpected disconnect argument: {other}")),
        }
        index += 1;
    }
    if let Some(value) = profile.as_deref() {
        if let Err(error) = validate_client_profile(value) {
            return cli_parse_error(error);
        }
    }
    CliAction::Disconnect(DisconnectOptions {
        project,
        profile,
        config_base: None,
        state_base: None,
        server_http: ServerHttpOptions::default(),
    })
}

fn parse_project_subcommand(args: &[String]) -> CliAction {
    match args.first().map(String::as_str) {
        Some("activate") => {
            if args[1..]
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                return CliAction::Exit {
                    code: 0,
                    stdout: project_activate_usage().to_string(),
                    stderr: String::new(),
                };
            }
            let mut config = None;
            let mut user_token_file = None;
            let mut project = None;
            let mut json = false;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--config" => {
                        index += 1;
                        match args.get(index) {
                            Some(value) => config = Some(PathBuf::from(value)),
                            None => return cli_parse_error("--config requires a value".to_string()),
                        }
                    }
                    "--user-token-file" => {
                        index += 1;
                        match args.get(index) {
                            Some(value) => user_token_file = Some(PathBuf::from(value)),
                            None => return cli_parse_error("--user-token-file requires a value".to_string()),
                        }
                    }
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return cli_parse_error(format!("unknown project activate option: {other}"))
                    }
                    value => {
                        if project.is_some() {
                            return cli_parse_error(format!("unexpected project activate argument: {value}"));
                        }
                        project = Some(PathBuf::from(value));
                    }
                }
                index += 1;
            }
            let Some(config) = config else {
                return cli_parse_error("project activate requires --config PATH".to_string());
            };
            let Some(user_token_file) = user_token_file else {
                return cli_parse_error("project activate requires --user-token-file PATH".to_string());
            };
            let Some(project) = project else {
                return cli_parse_error("project activate requires an existing workspace path".to_string());
            };
            CliAction::ProjectActivate(ProjectActivateOptions {
                config,
                user_token_file,
                project,
                json,
            })
        }
        Some("register") => {
            if args[1..]
                .iter()
                .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
            {
                return CliAction::Exit {
                    code: 0,
                    stdout: project_register_usage().to_string(),
                    stderr: String::new(),
                };
            }
            let mut config = None;
            let mut project = None;
            let mut json = false;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--config" => {
                        index += 1;
                        match args.get(index) {
                            Some(value) => config = Some(PathBuf::from(value)),
                            None => {
                                return cli_parse_error("--config requires a value".to_string())
                            }
                        }
                    }
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return cli_parse_error(format!("unknown project register option: {other}"))
                    }
                    value => {
                        if project.is_some() {
                            return cli_parse_error(format!(
                                "unexpected project register argument: {value}"
                            ));
                        }
                        project = Some(PathBuf::from(value));
                    }
                }
                index += 1;
            }
            let Some(config) = config else {
                return cli_parse_error("project register requires --config PATH".to_string());
            };
            let Some(project) = project else {
                return cli_parse_error(
                    "project register requires an existing workspace path".to_string(),
                );
            };
            CliAction::ProjectRegister(ProjectRegisterOptions {
                config,
                project,
                json,
            })
        }
        Some("--help" | "-h") => CliAction::Exit {
            code: 0,
            stdout: "Usage: webcodex project <COMMAND>\n\nCommands:\n  activate    Activate a project on the current Runner without restarting it\n  register    Add a project for a stopped/legacy Runner\n".to_string(),
            stderr: String::new(),
        },
        Some(other) => cli_parse_error(format!("unknown project subcommand: {other}")),
        None => cli_parse_error(
            "missing project subcommand; try `webcodex project --help`".to_string(),
        ),
    }
}

fn parse_auth_subcommand(args: &[String]) -> CliAction {
    match args.first().map(String::as_str) {
        Some("status") => parse_status(&args[1..]),
        Some("--help" | "-h") => CliAction::Exit {
            code: 0,
            stdout: "Usage: webcodex auth <COMMAND>\n\nCommands:\n  status    Show which servers this device is logged in to\n".to_string(),
            stderr: String::new(),
        },
        Some(other) => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("unknown auth subcommand: {other}\n"),
        },
        None => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: "missing auth subcommand\n".to_string(),
        },
    }
}

/// `login <server-url> --code CODE`. Everything else has a default: the device
/// name comes from the hostname and the destination from the username the
/// server returns, so neither has to be agreed on both sides beforehand.
fn parse_login(args: &[String]) -> CliAction {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return CliAction::Exit {
            code: 0,
            stdout: login_usage().to_string(),
            stderr: String::new(),
        };
    }
    let mut server_url: Option<String> = None;
    let mut server_http = ServerHttpOptions::default();
    let mut code: Option<String> = None;
    let mut code_stdin = false;
    let mut device: Option<String> = None;
    let mut device_explicit = false;
    let mut base_dir: Option<PathBuf> = None;
    let mut transport = TRANSPORT_WEBSOCKET.to_string();
    let mut allowed_roots: Vec<PathBuf> = Vec::new();
    let mut project: Option<PathBuf> = None;
    let mut overwrite = false;
    let mut json = false;
    let mut print_mcp_config = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        let take = |index: &mut usize| -> Option<String> {
            *index += 1;
            args.get(*index).cloned()
        };
        match arg.as_str() {
            "--proxy" => match take(&mut index) {
                Some(value) => server_http.proxy = Some(value),
                None => return cli_parse_error("--proxy requires a value".to_string()),
            },
            "--no-system-proxy" => server_http.no_system_proxy = true,
            "--code" | "--pairing-code" => match take(&mut index) {
                Some(value) => code = Some(value),
                None => return cli_parse_error(format!("{arg} requires a value")),
            },
            "--code-stdin" => code_stdin = true,
            "--device" | "--device-name" => match take(&mut index) {
                Some(value) => {
                    device = Some(value);
                    device_explicit = true;
                }
                None => return cli_parse_error(format!("{arg} requires a value")),
            },
            "--dir" => match take(&mut index) {
                Some(value) => base_dir = Some(PathBuf::from(value)),
                None => return cli_parse_error("--dir requires a value".to_string()),
            },
            "--transport" => match take(&mut index) {
                Some(value) => transport = value,
                None => return cli_parse_error("--transport requires a value".to_string()),
            },
            "--allowed-root" => match take(&mut index) {
                Some(value) => allowed_roots.push(PathBuf::from(value)),
                None => return cli_parse_error("--allowed-root requires a value".to_string()),
            },
            "--project" => match take(&mut index) {
                Some(value) => project = Some(PathBuf::from(value)),
                None => return cli_parse_error("--project requires a value".to_string()),
            },
            "--overwrite" => overwrite = true,
            "--json" => json = true,
            "--print-mcp-config" => print_mcp_config = true,
            other if other.starts_with('-') => {
                return cli_parse_error(format!("unknown flag {other}"))
            }
            other => {
                if server_url.is_some() {
                    return cli_parse_error(format!("unexpected argument {other}"));
                }
                server_url = Some(other.to_string());
            }
        }
        index += 1;
    }
    if let Err(error) = server_http.validate() {
        return cli_parse_error(error);
    }
    if json && print_mcp_config {
        return cli_parse_error(
            "--json and --print-mcp-config are mutually exclusive; --print-mcp-config emits a credential"
                .to_string(),
        );
    }
    if code.is_some() && code_stdin {
        return cli_parse_error("use only one of --code or --code-stdin".to_string());
    }
    let Some(server_url) = server_url else {
        return cli_parse_error(
            "login needs a server URL, e.g. `webcodex login https://example.com --code wc_pair_...`"
                .to_string(),
        );
    };
    let code = match (code, code_stdin) {
        (Some(code), false) => code,
        (None, true) => match read_login_pairing_code_from_stdin() {
            Ok(code) => code,
            Err(message) => return cli_parse_error(message),
        },
        (None, false) => return cli_parse_error(
            "login needs --code with the pairing code (or --code-stdin for process integrations)"
                .to_string(),
        ),
        (Some(_), true) => unreachable!("pairing code source conflict handled above"),
    };
    let base_dir = match base_dir_or_default(base_dir) {
        Ok(dir) => dir,
        Err(message) => return cli_parse_error(message),
    };
    CliAction::Login(LoginOptions {
        server_url,
        server_http,
        code,
        device: device.unwrap_or_else(default_device_name),
        device_explicit,
        base_dir,
        transport,
        allowed_roots,
        project,
        overwrite,
        json,
        print_mcp_config,
    })
}

const MAX_LOGIN_PAIRING_CODE_STDIN_BYTES: u64 = 4096;

fn read_login_pairing_code_from_stdin() -> Result<String, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_LOGIN_PAIRING_CODE_STDIN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read pairing code from stdin".to_string())?;
    if bytes.len() as u64 > MAX_LOGIN_PAIRING_CODE_STDIN_BYTES {
        return Err("pairing code from stdin exceeds the 4096-byte limit".to_string());
    }
    let value = String::from_utf8(bytes)
        .map_err(|_| "pairing code from stdin must be UTF-8".to_string())?;
    let code = value.trim();
    if code.is_empty() {
        return Err("pairing code from stdin is empty".to_string());
    }
    Ok(code.to_string())
}

fn parse_logout(args: &[String]) -> CliAction {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return CliAction::Exit {
            code: 0,
            stdout: logout_usage().to_string(),
            stderr: String::new(),
        };
    }
    let mut server_url: Option<String> = None;
    let mut username: Option<String> = None;
    let mut all = false;
    let mut base_dir: Option<PathBuf> = None;
    let mut yes = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        match arg.as_str() {
            "--user" => {
                index += 1;
                match args.get(index) {
                    Some(value) => username = Some(value.clone()),
                    None => return cli_parse_error(format!("{arg} requires a value")),
                }
            }
            "--dir" => {
                index += 1;
                match args.get(index) {
                    Some(value) => base_dir = Some(PathBuf::from(value)),
                    None => return cli_parse_error("--dir requires a value".to_string()),
                }
            }
            "--yes" | "-y" => yes = true,
            "--all" => all = true,
            "--json" => json = true,
            other if other.starts_with('-') => {
                return cli_parse_error(format!("unknown flag {other}"))
            }
            other => {
                if server_url.is_some() {
                    return cli_parse_error(format!("unexpected argument {other}"));
                }
                server_url = Some(other.to_string());
            }
        }
        index += 1;
    }
    if username.is_some() && all {
        return cli_parse_error("--user and --all are mutually exclusive".to_string());
    }
    let Some(server_url) = server_url else {
        return cli_parse_error("logout needs a server URL".to_string());
    };
    let base_dir = match base_dir_or_default(base_dir) {
        Ok(dir) => dir,
        Err(message) => return cli_parse_error(message),
    };
    CliAction::Logout(LogoutOptions {
        server_url,
        username,
        base_dir,
        all,
        yes,
        json,
    })
}

fn parse_status(args: &[String]) -> CliAction {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return CliAction::Exit {
            code: 0,
            stdout: status_usage().to_string(),
            stderr: String::new(),
        };
    }
    let mut base_dir: Option<PathBuf> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dir" => {
                index += 1;
                match args.get(index) {
                    Some(value) => base_dir = Some(PathBuf::from(value)),
                    None => return cli_parse_error("--dir requires a value".to_string()),
                }
            }
            "--json" => json = true,
            other => return cli_parse_error(format!("unknown argument {other}")),
        }
        index += 1;
    }
    let base_dir = match base_dir_or_default(base_dir) {
        Ok(dir) => dir,
        Err(message) => return cli_parse_error(message),
    };
    CliAction::Status(StatusOptions { base_dir, json })
}

fn cli_parse_error(message: String) -> CliAction {
    CliAction::Exit {
        code: 2,
        stdout: String::new(),
        stderr: format!("{message}\n"),
    }
}

fn parse_token_subcommand(args: &[String]) -> CliAction {
    if args.is_empty() {
        return CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: "missing token subcommand\n".to_string(),
        };
    }
    match args[0].as_str() {
        "generate" => match parse_token_generate(&args[1..]) {
            Ok(opts) => CliAction::TokenGenerate(opts),
            Err(e) => local_token_parse_error(e),
        },
        "create-local" => match parse_token_create_local(&args[1..]) {
            Ok(opts) => CliAction::TokenCreateLocal(opts),
            Err(e) => local_token_parse_error(e),
        },
        _ => forward_to_admin_cli("tokens", args),
    }
}

/// `generate` and `create-local` are handled locally; every other action is a
/// canonical plural admin API call.
fn forward_to_admin_cli(group: &str, args: &[String]) -> CliAction {
    let mut forwarded = vec![group.to_string()];
    forwarded.extend_from_slice(args);
    match parse_admin_cli(&forwarded) {
        Ok(cmd) => CliAction::Admin(cmd),
        Err(e) => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("{}\n", e),
        },
    }
}

fn local_token_parse_error(error: String) -> CliAction {
    CliAction::Exit {
        code: 2,
        stdout: String::new(),
        stderr: format!("{}\n", error),
    }
}

fn parse_token_generate(args: &[String]) -> Result<TokenGenerateOptions, String> {
    let mut kind = "api".to_string();
    let mut p = SimpleFlagParser::new(args);
    while let Some(flag) = p.next() {
        match flag.as_str() {
            "--kind" => kind = p.value(&flag)?,
            "-h" | "--help" => {
                return Err("Usage: webcodex tokens generate --kind api|runner".to_string())
            }
            _ => return Err(format!("unknown tokens generate flag: {}", flag)),
        }
    }
    if kind != "api" && kind != "runner" {
        return Err("--kind must be 'api' or 'runner'".to_string());
    }
    Ok(TokenGenerateOptions { kind })
}

fn parse_runner_token_subcommand(args: &[String]) -> CliAction {
    if args.is_empty() {
        return CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: "missing runner-tokens subcommand\n".to_string(),
        };
    }
    match args[0].as_str() {
        "create-local" => match parse_runner_token_create_local(&args[1..]) {
            Ok(opts) => CliAction::RunnerTokenCreateLocal(opts),
            Err(e) => local_token_parse_error(e),
        },
        _ => forward_to_admin_cli("runner-tokens", args),
    }
}

fn parse_runner_token_create_local(
    args: &[String],
) -> Result<RunnerTokenCreateLocalOptions, String> {
    let mut opts = RunnerTokenCreateLocalOptions::default();
    let mut p = SimpleFlagParser::new(args);
    while let Some(flag) = p.next() {
        match flag.as_str() {
            "--server-url" => opts.admin.server_url = p.value(&flag)?,
            "--proxy" => opts.admin.server_http.proxy = Some(p.value(&flag)?),
            "--no-system-proxy" => opts.admin.server_http.no_system_proxy = true,
            "--username" => opts.username = p.value(&flag)?,
            "--client-id" => opts.client_id = p.value(&flag)?,
            "--credential" => opts.admin.credential = Some(p.value(&flag)?),
            "--credential-env" => opts.admin.credential_env = Some(p.value(&flag)?),
            "--token" => opts.admin.token = Some(p.value(&flag)?),
            "--token-env" => opts.admin.token_env = Some(p.value(&flag)?),
            "--token-file" => opts.admin.token_file = Some(PathBuf::from(p.value(&flag)?)),
            "--name" => opts.name = Some(p.value(&flag)?),
            "--scope" => opts.scopes.push(p.value(&flag)?),
            "--scopes" => {
                opts.scopes.extend(
                    p.value(&flag)?
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
            }
            "-h" | "--help" => return Err("Usage: webcodex runner-tokens create-local --server-url URL --username USER --credential CRED --client-id ID [--proxy http://HOST:PORT|--no-system-proxy] [--name NAME] [--scopes S1,S2]".to_string()),
            _ => return Err(format!("unknown runner-tokens create-local flag: {}", flag)),
        }
    }
    opts.admin.server_http.validate()?;
    if opts.admin.server_url.trim().is_empty() {
        return Err("--server-url is required".to_string());
    }
    if opts.username.trim().is_empty() {
        return Err("--username is required".to_string());
    }
    if opts.client_id.trim().is_empty() {
        return Err("--client-id is required".to_string());
    }
    if opts.scopes.is_empty() {
        opts.scopes = SETUP_RUNNER_SCOPES.iter().map(|s| s.to_string()).collect();
    }
    Ok(opts)
}

fn parse_token_create_local(args: &[String]) -> Result<TokenCreateLocalOptions, String> {
    let mut opts = TokenCreateLocalOptions::default();
    let mut p = SimpleFlagParser::new(args);
    while let Some(flag) = p.next() {
        match flag.as_str() {
            "--server-url" => opts.server_url = p.value(&flag)?,
            "--proxy" => opts.server_http.proxy = Some(p.value(&flag)?),
            "--no-system-proxy" => opts.server_http.no_system_proxy = true,
            "--username" => opts.username = p.value(&flag)?,
            "--credential" => opts.credential = Some(p.value(&flag)?),
            "--credential-env" => opts.credential_env = Some(p.value(&flag)?),
            "--name" => opts.name = Some(p.value(&flag)?),
            "--scope" => opts.scopes.push(p.value(&flag)?),
            "--scopes" => {
                opts.scopes.extend(
                    p.value(&flag)?
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                );
            }
            "-h" | "--help" => {
                return Err("Usage: webcodex tokens create-local --server-url URL --username USER --credential CRED [--proxy http://HOST:PORT|--no-system-proxy] [--name NAME] [--scopes S1,S2]".to_string())
            }
            _ => return Err(format!("unknown tokens create-local flag: {}", flag)),
        }
    }
    opts.server_http.validate()?;
    if opts.server_url.trim().is_empty() {
        return Err("--server-url is required".to_string());
    }
    if opts.username.trim().is_empty() {
        return Err("--username is required".to_string());
    }
    if opts.scopes.is_empty() {
        opts.scopes = SETUP_GPT_SCOPES.iter().map(|s| s.to_string()).collect();
    }
    Ok(opts)
}

#[derive(Debug)]
struct SimpleFlagParser {
    args: Vec<String>,
    idx: usize,
}

impl SimpleFlagParser {
    fn new(args: &[String]) -> Self {
        Self {
            args: args.to_vec(),
            idx: 0,
        }
    }

    fn next(&mut self) -> Option<String> {
        let value = self.args.get(self.idx).cloned();
        if value.is_some() {
            self.idx += 1;
        }
        value
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        self.next()
            .ok_or_else(|| format!("{} requires a value", flag))
    }
}

fn exit_help(text: &str) -> CliAction {
    CliAction::Exit {
        code: 0,
        stdout: text.to_string(),
        stderr: String::new(),
    }
}

fn exit_error(text: &str) -> CliAction {
    CliAction::Exit {
        code: 2,
        stdout: String::new(),
        stderr: text.to_string(),
    }
}

fn result_action<T>(result: Result<T, String>, action: impl FnOnce(T) -> CliAction) -> CliAction {
    match result {
        Ok(value) => action(value),
        Err(error) => exit_error(&format!("{error}\n")),
    }
}

fn parse_runner_subcommand(args: &[String]) -> CliAction {
    let Some(command) = args.first().map(String::as_str) else {
        return exit_error(runner_usage());
    };
    if matches!(command, "--help" | "-h") {
        return exit_help(runner_usage());
    }
    if command == "run" && args.len() == 2 && matches!(args[1].as_str(), "--version" | "-V") {
        return exit_help(&build_info::version_output("webcodex-runner"));
    }
    if args
        .get(1)
        .is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        let help = match command {
            "init" => runner_init_usage(),
            "install" => runner_install_service_usage(),
            "run" => "Usage: webcodex runner run [--profile NAME|--config PATH]\n\nRun webcodex-runner directly in the foreground.\n",
            "restart" => "Usage: webcodex runner restart [--profile NAME] [--bin PATH] [--scope user|system] [--service-file PATH]\n\nWith a profile created by `webcodex connect`, omitting --scope manages its user-level background Runner; --bin selects an explicit Runner binary for that hosted profile. An explicit scope manages the matching systemd service and does not accept --bin.\n",
            "start" | "stop" => "Usage: webcodex runner <start|stop> [--profile NAME] [--scope user|system] [--service-file PATH]\n\nWith a profile created by `webcodex connect`, omitting --scope manages its user-level background Runner. An explicit scope manages the matching systemd service.\n",
            "status" => runner_status_usage(),
            "logs" => "Usage: webcodex runner logs [--profile NAME] [--scope user|system] [--service-file PATH] [--lines N] [--since VALUE] [--follow]\n",
            "uninstall" => "Usage: webcodex runner uninstall [--profile NAME] [--scope user|system] [--service-file PATH] --confirm\n",
            "install-service" => "`webcodex runner install-service` was removed; use `webcodex runner install`.\n",
            _ => runner_usage(),
        };
        return exit_help(help);
    }
    match command {
        "init" => result_action(parse_cli_runner_init(&args[1..]), CliAction::RunnerInit),
        "install" => result_action(
            parse_runner_install_service(&args[1..]),
            CliAction::RunnerInstall,
        ),
        "run" => result_action(parse_runner_run(&args[1..]), CliAction::RunnerRun),
        "status" => result_action(parse_runner_status(&args[1..]), CliAction::RunnerStatus),
        "start" | "stop" | "restart" | "logs" | "uninstall" => result_action(
            parse_runner_service_action(command, &args[1..]),
            CliAction::RunnerService,
        ),
        "install-service" => exit_error(
            "`webcodex runner install-service` was removed; use `webcodex runner install`.\n",
        ),
        other => exit_error(&format!(
            "unknown runner subcommand: {other}\n\n{}",
            runner_usage()
        )),
    }
}

fn parse_pairing_subcommand(args: &[String]) -> CliAction {
    if args.is_empty() {
        return CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("{}\n", pairing_usage()),
        };
    }
    match args[0].as_str() {
        "--help" | "-h" => CliAction::Exit {
            code: 0,
            stdout: pairing_usage().to_string(),
            stderr: String::new(),
        },
        "create" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: pairing_create_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_pairing_create(&args[1..]) {
                Ok(opts) => CliAction::PairingCreate(opts),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        other => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("unknown pairing subcommand: {}\n", other),
        },
    }
}

fn parse_ops_subcommand(args: &[String]) -> CliAction {
    if args.is_empty() {
        return CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("{}\n", ops_usage()),
        };
    }
    match args[0].as_str() {
        "--help" | "-h" => CliAction::Exit {
            code: 0,
            stdout: ops_usage().to_string(),
            stderr: String::new(),
        },
        "status" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_status_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_common(&args[1..], "status") {
                Ok(opts) => CliAction::Ops(OpsCommand::Status(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        "runners" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_runners_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_common(&args[1..], "runners") {
                Ok(opts) => CliAction::Ops(OpsCommand::Runners(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        "runner" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_runner_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_runner(&args[1..]) {
                Ok(opts) => CliAction::Ops(OpsCommand::Runner(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        "projects" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_projects_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_common(&args[1..], "projects") {
                Ok(opts) => CliAction::Ops(OpsCommand::Projects(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        "windows" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_windows_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_windows(&args[1..]) {
                Ok(opts) => CliAction::Ops(OpsCommand::Windows(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        "smoke-preflight" => {
            if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
                return CliAction::Exit {
                    code: 0,
                    stdout: ops_smoke_preflight_usage().to_string(),
                    stderr: String::new(),
                };
            }
            match parse_ops_smoke_preflight(&args[1..]) {
                Ok(opts) => CliAction::Ops(OpsCommand::SmokePreflight(opts)),
                Err(e) => CliAction::Exit {
                    code: 2,
                    stdout: String::new(),
                    stderr: format!("{}\n", e),
                },
            }
        }
        other => CliAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("unknown ops subcommand: {}\n", other),
        },
    }
}

fn default_ops_common_options() -> OpsCommonOptions {
    OpsCommonOptions {
        server_url: "http://127.0.0.1:8080".to_string(),
        server_http: ServerHttpOptions::default(),
        env_file: None,
        token_file: None,
        token: None,
        json: false,
        strict: false,
    }
}

fn parse_ops_common(args: &[String], command: &str) -> Result<OpsCommonOptions, String> {
    let mut opts = default_ops_common_options();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server-url" => opts.server_url = next_value(&mut iter, arg)?,
            "--proxy" => opts.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => opts.server_http.no_system_proxy = true,
            "--env-file" => opts.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token-file" => opts.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token" => opts.token = Some(next_value(&mut iter, arg)?),
            "--json" => opts.json = true,
            "--strict" => opts.strict = true,
            other => return Err(format!("unknown ops {} flag: {}", command, other)),
        }
    }
    validate_ops_common(&opts)
}

fn validate_ops_common(opts: &OpsCommonOptions) -> Result<OpsCommonOptions, String> {
    opts.server_http.validate()?;
    if opts.server_url.trim().is_empty() {
        return Err("--server-url cannot be empty".to_string());
    }
    if opts
        .token
        .as_ref()
        .is_some_and(|token| token.trim().is_empty())
    {
        return Err("--token cannot be empty".to_string());
    }
    Ok(opts.clone())
}

fn parse_ops_runner(args: &[String]) -> Result<OpsRunnerOptions, String> {
    let mut common = default_ops_common_options();
    let mut client_id = String::new();
    let mut request_timeout_ms = webcodex_cli::ops::DEFAULT_RUNNER_REQUEST_TIMEOUT_MS;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--client-id" => client_id = next_value(&mut iter, arg)?,
            "--request-timeout-ms" => {
                request_timeout_ms = next_value(&mut iter, arg)?
                    .parse::<u64>()
                    .map_err(|_| "--request-timeout-ms must be an integer".to_string())?;
            }
            "--server-url" => common.server_url = next_value(&mut iter, arg)?,
            "--proxy" => common.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => common.server_http.no_system_proxy = true,
            "--env-file" => common.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token-file" => common.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token" => common.token = Some(next_value(&mut iter, arg)?),
            "--json" => common.json = true,
            "--strict" => common.strict = true,
            other => return Err(format!("unknown ops runner flag: {}", other)),
        }
    }
    common = validate_ops_common(&common)?;
    let client_id = client_id.trim().to_string();
    if client_id.is_empty() {
        return Err("--client-id is required".to_string());
    }
    if client_id.chars().count() > 128 {
        return Err("--client-id must contain 1..=128 characters".to_string());
    }
    if !(1..=30_000).contains(&request_timeout_ms) {
        return Err("--request-timeout-ms must be within 1..=30000".to_string());
    }
    Ok(OpsRunnerOptions {
        common,
        client_id,
        request_timeout_ms,
    })
}

fn parse_ops_windows(args: &[String]) -> Result<OpsWindowsOptions, String> {
    let mut common = default_ops_common_options();
    let mut project = String::new();
    let mut limit = 64usize;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => project = next_value(&mut iter, arg)?,
            "--limit" => {
                limit = next_value(&mut iter, arg)?
                    .parse::<usize>()
                    .map_err(|_| "--limit must be an integer".to_string())?;
            }
            "--server-url" => common.server_url = next_value(&mut iter, arg)?,
            "--proxy" => common.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => common.server_http.no_system_proxy = true,
            "--env-file" => common.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token-file" => common.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token" => common.token = Some(next_value(&mut iter, arg)?),
            "--json" => common.json = true,
            "--strict" => common.strict = true,
            other => return Err(format!("unknown ops windows flag: {}", other)),
        }
    }
    common = validate_ops_common(&common)?;
    let project = project.trim().to_string();
    if project.is_empty() {
        return Err("--project is required".to_string());
    }
    if !(1..=64).contains(&limit) {
        return Err("--limit must be within 1..=64".to_string());
    }
    Ok(OpsWindowsOptions {
        common,
        project,
        limit,
    })
}

fn parse_ops_smoke_preflight(args: &[String]) -> Result<OpsSmokePreflightOptions, String> {
    let mut common = default_ops_common_options();
    let mut project = String::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--project" => project = next_value(&mut iter, arg)?,
            "--server-url" => common.server_url = next_value(&mut iter, arg)?,
            "--proxy" => common.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => common.server_http.no_system_proxy = true,
            "--env-file" => common.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token-file" => common.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token" => common.token = Some(next_value(&mut iter, arg)?),
            "--json" => common.json = true,
            "--strict" => common.strict = true,
            other => return Err(format!("unknown ops smoke-preflight flag: {}", other)),
        }
    }
    common = validate_ops_common(&common)?;
    if project.trim().is_empty() {
        return Err("--project is required".to_string());
    }
    Ok(OpsSmokePreflightOptions { common, project })
}

fn parse_server_subcommand(args: &[String]) -> CliAction {
    let Some(command) = args.first().map(String::as_str) else {
        return exit_error(server_usage());
    };
    if matches!(command, "--help" | "-h") {
        return exit_help(server_usage());
    }
    if command == "run" && args.len() == 2 && matches!(args[1].as_str(), "--version" | "-V") {
        return exit_help(&build_info::version_output("webcodex-server"));
    }
    if args
        .get(1)
        .is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        let help = match command {
            "init" => server_init_usage(),
            "install" => server_install_service_usage(),
            "run" => "Usage: webcodex server run [--env-file PATH] [--help|--version]\n\nRun webcodex-server directly in the foreground. --env-file passes the exact path through WEBCODEX_ENV_FILE; the Server remains the authoritative env-file parser.\n",
            "tunnel" => server_tunnel_usage(),
            "start" | "stop" | "restart" => "Usage: webcodex server <start|stop|restart>\n",
            "status" => server_status_usage(),
            "logs" => "Usage: webcodex server logs [--lines N] [--since VALUE] [--follow]\n",
            "uninstall" => "Usage: webcodex server uninstall --confirm\n",
            "up" => "`webcodex server up` was removed; use `webcodex server init`.\n",
            "install-service" => "`webcodex server install-service` was removed; use `webcodex server install`.\n",
            _ => server_usage(),
        };
        return exit_help(help);
    }
    match command {
        "init" => result_action(parse_server_init(&args[1..]), CliAction::ServerInit),
        "install" => result_action(
            parse_server_install_service(&args[1..]),
            CliAction::ServerInstall,
        ),
        "run" => result_action(parse_server_run(&args[1..]), CliAction::ServerRun),
        "tunnel" => result_action(parse_server_tunnel(&args[1..]), CliAction::ServerTunnel),
        "status" => result_action(parse_server_status(&args[1..]), CliAction::ServerStatus),
        "start" | "stop" | "restart" | "logs" | "uninstall" => result_action(
            parse_server_service_action(command, &args[1..]),
            CliAction::ServerService,
        ),
        "up" => exit_error("`webcodex server up` was removed; use `webcodex server init`.\n"),
        "install-service" => exit_error(
            "`webcodex server install-service` was removed; use `webcodex server install`.\n",
        ),
        other => exit_error(&format!(
            "unknown server subcommand: {other}\n\n{}",
            server_usage()
        )),
    }
}

pub(crate) fn foreground_run_banner(component: &str) -> String {
    format!(
        "Starting WebCodex {component} in the foreground.\nKeep this terminal open. Ctrl-C stops the {component}.\n\n"
    )
}

fn parse_server_run(args: &[String]) -> Result<InternalRunOptions, String> {
    let mut env_file: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--env-file" => {
                if env_file.is_some() {
                    return Err("--env-file may be specified only once".to_string());
                }
                let path = PathBuf::from(next_value(&mut iter, arg)?);
                if path.as_os_str().is_empty() {
                    return Err("--env-file cannot be empty".to_string());
                }
                env_file = Some(path);
            }
            other => return Err(format!("unknown server run option: {other}")),
        }
    }
    let bin = discover_internal_binary("webcodex-server").ok_or_else(|| {
        "webcodex-server was not found beside webcodex or in an absolute PATH entry".to_string()
    })?;
    Ok(InternalRunOptions {
        bin,
        args: Vec::new(),
        env: env_file
            .map(|path| vec![(OsString::from("WEBCODEX_ENV_FILE"), path.into_os_string())])
            .unwrap_or_default(),
    })
}

fn parse_server_tunnel(args: &[String]) -> Result<ServerTunnelOptions, String> {
    let mut provider: Option<String> = None;
    let mut env_file: Option<PathBuf> = None;
    let mut json = false;
    let mut stop_on_stdin_eof = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--provider" => provider = Some(next_value(&mut iter, arg)?),
            "--env-file" => env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--json" => json = true,
            "--stop-on-stdin-eof" => stop_on_stdin_eof = true,
            other => return Err(format!("unknown server tunnel option: {other}")),
        }
    }
    if provider.as_deref() != Some("openai") {
        return Err("--provider openai is required for regular Server Tunnel".to_string());
    }
    let env_file = env_file.ok_or_else(|| "--env-file is required".to_string())?;
    if !json {
        return Err("server tunnel currently requires --json".to_string());
    }
    if !stop_on_stdin_eof {
        return Err("server tunnel currently requires --stop-on-stdin-eof".to_string());
    }
    Ok(ServerTunnelOptions { env_file })
}

fn parse_runner_run(args: &[String]) -> Result<InternalRunOptions, String> {
    let mut profile: Option<String> = None;
    let mut config: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => profile = Some(next_value(&mut iter, arg)?),
            "--config" => config = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            other => return Err(format!("unknown runner run option: {other}")),
        }
    }
    let profile = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let config = match config {
        Some(config) => config,
        None => match profile.as_deref() {
            Some(profile) => client_profile_runner_config(profile)?,
            None => webcodex_runner_config::paths::resolve_runner_config_path(Path::new(
                "/etc/webcodex",
            ))?,
        },
    };
    let bin = discover_internal_binary("webcodex-runner").ok_or_else(|| {
        "webcodex-runner was not found beside webcodex or in an absolute PATH entry".to_string()
    })?;
    Ok(InternalRunOptions {
        bin,
        args: vec!["--config".to_string(), config.display().to_string()],
        env: Vec::new(),
    })
}

fn service_control(command: &str) -> Result<ServiceControl, String> {
    match command {
        "start" => Ok(ServiceControl::Start),
        "stop" => Ok(ServiceControl::Stop),
        "restart" => Ok(ServiceControl::Restart),
        _ => Err(format!("unsupported service action: {command}")),
    }
}

fn parse_service_kind(command: &str, args: &[String]) -> Result<ServiceActionKind, String> {
    if matches!(command, "start" | "stop" | "restart") {
        if let Some(flag) = args.first() {
            if flag == "--root" || flag == "--state-dir" || flag == "--console-assets-dir" {
                return Err(format!(
                    "`webcodex runner {command}` manages the installed service; use `webcodex run` for project runtime options"
                ));
            }
            return Err(format!("unknown {command} option: {flag}"));
        }
        return Ok(ServiceActionKind::Control(service_control(command)?));
    }
    if command == "logs" {
        let mut lines = DEFAULT_LOG_LINES;
        let mut since = None;
        let mut follow = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--lines" => {
                    lines = next_value(&mut iter, arg)?
                        .parse::<u32>()
                        .map_err(|_| "--lines must be a positive integer".to_string())?;
                    if lines == 0 {
                        return Err("--lines must be greater than zero".to_string());
                    }
                }
                "--since" => {
                    let value = next_value(&mut iter, arg)?;
                    if value.trim().is_empty() {
                        return Err("--since cannot be empty".to_string());
                    }
                    since = Some(value);
                }
                "--follow" => follow = true,
                other => return Err(format!("unknown logs option: {other}")),
            }
        }
        return Ok(ServiceActionKind::Logs {
            lines,
            since,
            follow,
        });
    }
    if command == "uninstall" {
        let mut confirm = false;
        for arg in args {
            match arg.as_str() {
                "--confirm" => confirm = true,
                other => return Err(format!("unknown uninstall option: {other}")),
            }
        }
        return Ok(ServiceActionKind::Uninstall { confirm });
    }
    Err(format!("unsupported service action: {command}"))
}

fn parse_server_service_action(
    command: &str,
    args: &[String],
) -> Result<ServiceActionOptions, String> {
    let mut service_file = PathBuf::from(SERVER_SERVICE_FILE);
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--service-file" => service_file = PathBuf::from(next_value(&mut iter, arg)?),
            _ => remaining.push(arg.clone()),
        }
    }
    if service_file.as_os_str().is_empty() {
        return Err("--service-file cannot be empty".to_string());
    }
    let unit = service_unit_name(&service_file, SERVER_SERVICE_UNIT);
    Ok(ServiceActionOptions {
        scope: ServiceScope::System,
        service_file,
        unit,
        kind: parse_service_kind(command, &remaining)?,
        local_profile: None,
    })
}

fn parse_runner_service_action(
    command: &str,
    args: &[String],
) -> Result<ServiceActionOptions, String> {
    let mut profile: Option<String> = None;
    let mut scope: Option<ServiceScope> = None;
    let mut service_file: Option<PathBuf> = None;
    let mut runner_bin: Option<PathBuf> = None;
    let mut remaining = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => profile = Some(next_value(&mut iter, arg)?),
            "--scope" => {
                if scope.is_some() {
                    return Err("--scope may be specified only once".to_string());
                }
                scope = Some(ServiceScope::parse(&next_value(&mut iter, arg)?)?);
            }
            "--service-file" => service_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--bin" => runner_bin = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            _ => remaining.push(arg.clone()),
        }
    }
    let scope_explicit = scope.is_some();
    if let Some(bin) = runner_bin.as_ref() {
        if bin.as_os_str().is_empty() {
            return Err("--bin cannot be empty".to_string());
        }
        if scope_explicit {
            return Err(
                "--bin is supported only for hosted connect profiles; omit --scope".to_string(),
            );
        }
        if profile.is_none() {
            return Err("--bin requires --profile for a hosted connect profile".to_string());
        }
        if command != "restart" {
            return Err(
                "--bin is valid only with `webcodex runner restart --profile <name>`".to_string(),
            );
        }
    }
    let scope = scope.unwrap_or_else(|| default_runner_service_scope(is_effective_root()));
    let profile = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let service_file = service_file
        .map(Ok)
        .unwrap_or_else(|| runner_service_file_for_scope(scope, profile.as_deref()))?;
    validate_service_file_scope(scope, &service_file)?;
    let unit = service_file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(RUNNER_SERVICE_UNIT)
        .to_string();
    let local_profile = if scope_explicit {
        None
    } else {
        match profile.as_deref() {
            Some(profile) => Some(LocalProfileOptions {
                config: client_profile_runner_config(profile)?,
                state_dir: client_profile_state_dir(profile)?,
                runner_bin,
            }),
            None => None,
        }
    };
    Ok(ServiceActionOptions {
        scope,
        service_file,
        unit,
        kind: parse_service_kind(command, &remaining)?,
        local_profile,
    })
}

fn parse_server_init(args: &[String]) -> Result<ServerInitOptions, String> {
    let defaults = default_server_paths()?;
    let mut opts = ServerInitOptions {
        listen: "127.0.0.1:8080".to_string(),
        data_dir: defaults.data_dir,
        env_file: defaults.env_file,
        public_url: None,
        open: false,
        overwrite: false,
        json: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--listen" => opts.listen = next_value(&mut iter, arg)?,
            "--data-dir" => opts.data_dir = PathBuf::from(next_value(&mut iter, arg)?),
            "--env-file" => opts.env_file = PathBuf::from(next_value(&mut iter, arg)?),
            "--public-url" => opts.public_url = Some(next_value(&mut iter, arg)?),
            "--open" => opts.open = true,
            "--overwrite" => opts.overwrite = true,
            "--json" => opts.json = true,
            _ => return Err(format!("unknown server init flag: {}", arg)),
        }
    }
    if opts.listen.trim().is_empty() {
        return Err("--listen cannot be empty".to_string());
    }
    if opts.data_dir.as_os_str().is_empty() {
        return Err("--data-dir cannot be empty".to_string());
    }
    if opts.env_file.as_os_str().is_empty() {
        return Err("--env-file cannot be empty".to_string());
    }
    if let Some(url) = &opts.public_url {
        if url.trim().is_empty() {
            return Err("--public-url cannot be empty".to_string());
        }
    }
    Ok(opts)
}

fn parse_runner_install_service(args: &[String]) -> Result<RunnerInstallServiceOptions, String> {
    parse_runner_install_service_with_identity(args, is_effective_root())
}

fn parse_runner_install_service_with_identity(
    args: &[String],
    effective_root: bool,
) -> Result<RunnerInstallServiceOptions, String> {
    let mut profile: Option<String> = None;
    let mut scope: Option<ServiceScope> = None;
    let mut config: Option<PathBuf> = None;
    let mut bin: Option<PathBuf> = None;
    let mut service_file: Option<PathBuf> = None;
    let mut working_directory: Option<PathBuf> = None;
    let mut user = None;
    let mut group = None;
    let mut allow_root_runner = false;
    let mut overwrite = false;
    let mut dry_run = false;
    let mut output_stdout = false;
    let mut no_start = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => profile = Some(next_value(&mut iter, arg)?),
            "--scope" => {
                if scope.is_some() {
                    return Err("--scope may be specified only once".to_string());
                }
                scope = Some(ServiceScope::parse(&next_value(&mut iter, arg)?)?);
            }
            "--config" => config = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--bin" => bin = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--service-file" => service_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--working-directory" => {
                working_directory = Some(PathBuf::from(next_value(&mut iter, arg)?))
            }
            "--user" => user = Some(next_value(&mut iter, arg)?),
            "--group" => group = Some(next_value(&mut iter, arg)?),
            "--allow-root-runner" => allow_root_runner = true,
            "--overwrite" => overwrite = true,
            "--dry-run" => dry_run = true,
            "--no-start" => no_start = true,
            "--output" => {
                let value = next_value(&mut iter, arg)?;
                if value != "-" {
                    return Err("--output only supports '-' for stdout".to_string());
                }
                output_stdout = true;
            }
            "--json" => json = true,
            _ => return Err(format!("unknown runner install flag: {}", arg)),
        }
    }
    let profile = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    let scope = scope.unwrap_or_else(|| default_runner_service_scope(effective_root));
    let config = config
        .map(Ok)
        .unwrap_or_else(|| runner_config_for_scope(scope, profile.as_deref()))?;
    let service_file = service_file
        .map(Ok)
        .unwrap_or_else(|| runner_service_file_for_scope(scope, profile.as_deref()))?;
    validate_service_file_scope(scope, &service_file)?;
    let bin = match bin.or_else(|| discover_internal_binary("webcodex-runner")) {
        Some(path) => path,
        None => {
            return Err(
                "--bin is required because webcodex-runner was not found in PATH".to_string(),
            )
        }
    };
    if config.as_os_str().is_empty() {
        return Err("--config cannot be empty".to_string());
    }
    if bin.as_os_str().is_empty() {
        return Err("--bin cannot be empty".to_string());
    }
    if service_file.as_os_str().is_empty() {
        return Err("--service-file cannot be empty".to_string());
    }
    let root_runner = match scope {
        ServiceScope::User => effective_root,
        ServiceScope::System => user.as_deref().is_none_or(system_user_is_root),
    };
    match scope {
        ServiceScope::User => {
            if user.is_some() || group.is_some() {
                return Err(
                    "--user and --group are valid only with --scope system; user services run as the current user"
                        .to_string(),
                );
            }
        }
        ServiceScope::System if !root_runner && user.is_none() => {
            return Err("--scope system requires an explicit non-root --user".to_string());
        }
        ServiceScope::System => {}
    }
    if root_runner && !allow_root_runner {
        return Err(
            "refusing to install a Runner that would run as root; pass a non-root --user with --scope system, or explicitly opt in with --allow-root-runner"
                .to_string(),
        );
    }
    if !root_runner && allow_root_runner {
        return Err(
            "--allow-root-runner is only valid when the selected Runner would run as root"
                .to_string(),
        );
    }
    let working_directory = match working_directory {
        Some(path) => path,
        None if scope == ServiceScope::User => current_user_home()?,
        None => {
            let selected_user = user.as_deref().unwrap_or("root");
            system_user_home(selected_user).ok_or_else(|| {
                format!(
                    "could not determine the home directory for system Runner user '{selected_user}'; pass --working-directory explicitly"
                )
            })?
        }
    };
    if working_directory.as_os_str().is_empty() {
        return Err("--working-directory cannot be empty".to_string());
    }
    if !working_directory.is_absolute() {
        return Err("--working-directory must be an absolute path".to_string());
    }
    if !root_runner && working_directory.starts_with("/root") {
        return Err(
            "a non-root Runner cannot use /root as its WorkingDirectory; use that user's home or another accessible directory"
                .to_string(),
        );
    }
    Ok(RunnerInstallServiceOptions {
        scope,
        config,
        bin,
        service_file,
        user,
        group,
        working_directory,
        root_runner,
        allow_root_runner,
        overwrite,
        dry_run,
        output_stdout,
        no_start,
        json,
    })
}

fn parse_runner_status(args: &[String]) -> Result<RunnerStatusOptions, String> {
    parse_runner_status_with_identity(args, is_effective_root())
}

fn parse_runner_status_with_identity(
    args: &[String],
    effective_root: bool,
) -> Result<RunnerStatusOptions, String> {
    let mut profile: Option<String> = None;
    let mut scope: Option<ServiceScope> = None;
    let mut config: Option<PathBuf> = None;
    let mut service_file: Option<PathBuf> = None;
    let mut opts = RunnerStatusOptions {
        scope: ServiceScope::System,
        config: PathBuf::new(),
        service_file: PathBuf::new(),
        local_state_dir: None,
        server_url: None,
        server_http: ServerHttpOptions::default(),
        user_token_file: None,
        runner_token_file: None,
        json: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--profile" => profile = Some(next_value(&mut iter, arg)?),
            "--scope" => {
                if scope.is_some() {
                    return Err("--scope may be specified only once".to_string());
                }
                scope = Some(ServiceScope::parse(&next_value(&mut iter, arg)?)?);
            }
            "--config" => config = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--service-file" => service_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--server-url" => opts.server_url = Some(next_value(&mut iter, arg)?),
            "--proxy" => opts.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => opts.server_http.no_system_proxy = true,
            "--user-token-file" => {
                opts.user_token_file = Some(PathBuf::from(next_value(&mut iter, arg)?))
            }
            "--runner-token-file" | "--agent-token-file" => {
                opts.runner_token_file = Some(PathBuf::from(next_value(&mut iter, arg)?))
            }
            "--json" => opts.json = true,
            _ => return Err(format!("unknown runner status flag: {}", arg)),
        }
    }
    opts.server_http.validate()?;
    let scope_explicit = scope.is_some();
    let scope = scope.unwrap_or_else(|| default_runner_service_scope(effective_root));
    opts.scope = scope;
    let profile = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?;
    opts.config = match (config, profile.as_deref(), scope_explicit) {
        (Some(config), _, _) => config,
        (None, Some(profile), false) => client_profile_runner_config(profile)?,
        (None, profile, _) => runner_config_for_scope(scope, profile)?,
    };
    opts.service_file = service_file
        .map(Ok)
        .unwrap_or_else(|| runner_service_file_for_scope(scope, profile.as_deref()))?;
    validate_service_file_scope(scope, &opts.service_file)?;
    if let Some(profile) = profile {
        if !scope_explicit {
            opts.local_state_dir = Some(client_profile_state_dir(&profile)?);
        }
        if opts.user_token_file.is_none() {
            opts.user_token_file = Some(if scope_explicit {
                client_profile_user_token_file_for_scope(scope, &profile)?
            } else {
                client_profile_user_token_file(&profile)?
            });
        }
        if opts.runner_token_file.is_none() {
            opts.runner_token_file = Some(if scope_explicit {
                client_profile_runner_token_file_for_scope(scope, &profile)?
            } else {
                client_profile_runner_token_file(&profile)?
            });
        }
    }
    if opts.config.as_os_str().is_empty() {
        return Err("--config cannot be empty".to_string());
    }
    if opts
        .server_url
        .as_ref()
        .is_some_and(|url| url.trim().is_empty())
    {
        return Err("--server-url cannot be empty".to_string());
    }
    Ok(opts)
}

fn parse_server_install_service(args: &[String]) -> Result<ServerInstallServiceOptions, String> {
    let defaults = default_server_paths()?;
    let mut env_file = defaults.env_file;
    let mut bin: Option<PathBuf> = None;
    let mut service_file = PathBuf::from("/etc/systemd/system/webcodex.service");
    let mut user = None;
    let mut group = None;
    let mut working_directory: Option<PathBuf> = None;
    let mut overwrite = false;
    let mut dry_run = false;
    let mut output_stdout = false;
    let mut no_start = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--env-file" => env_file = PathBuf::from(next_value(&mut iter, arg)?),
            "--bin" => bin = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--service-file" => service_file = PathBuf::from(next_value(&mut iter, arg)?),
            "--user" => user = Some(next_value(&mut iter, arg)?),
            "--group" => group = Some(next_value(&mut iter, arg)?),
            "--working-directory" => {
                working_directory = Some(PathBuf::from(next_value(&mut iter, arg)?))
            }
            "--overwrite" => overwrite = true,
            "--dry-run" => dry_run = true,
            "--no-start" => no_start = true,
            "--output" => {
                let value = next_value(&mut iter, arg)?;
                if value != "-" {
                    return Err("--output only supports '-' for stdout".to_string());
                }
                output_stdout = true;
            }
            "--json" => json = true,
            _ => return Err(format!("unknown server install flag: {}", arg)),
        }
    }
    let bin = match bin.or_else(|| discover_internal_binary("webcodex-server")) {
        Some(path) => path,
        None => return Err("--bin is required because webcodex-server was not found beside webcodex or in an absolute PATH entry".to_string()),
    };
    let working_directory = match working_directory {
        Some(path) => path,
        None if env_file.exists() => match read_env_file_value(&env_file, "WEBCODEX_DATA")? {
            Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
            Some(_) => {
                return Err(format!(
                    "{} defines an empty WEBCODEX_DATA; pass --working-directory explicitly",
                    env_file.display()
                ))
            }
            None => defaults.data_dir,
        },
        None => defaults.data_dir,
    };
    if env_file.as_os_str().is_empty() {
        return Err("--env-file cannot be empty".to_string());
    }
    if bin.as_os_str().is_empty() {
        return Err("--bin cannot be empty".to_string());
    }
    if service_file.as_os_str().is_empty() {
        return Err("--service-file cannot be empty".to_string());
    }
    if working_directory.as_os_str().is_empty() {
        return Err("--working-directory cannot be empty".to_string());
    }
    if !working_directory.is_absolute() {
        return Err("--working-directory must be an absolute path".to_string());
    }
    Ok(ServerInstallServiceOptions {
        env_file,
        bin,
        service_file,
        user,
        group,
        working_directory,
        overwrite,
        dry_run,
        output_stdout,
        no_start,
        json,
    })
}

fn parse_server_status(args: &[String]) -> Result<ServerStatusOptions, String> {
    let mut opts = ServerStatusOptions {
        url: "http://127.0.0.1:8080".to_string(),
        url_explicit: false,
        server_http: ServerHttpOptions::default(),
        env_file: Some(default_server_paths()?.env_file),
        env_file_explicit: false,
        token_file: None,
        service_file: PathBuf::from(SERVER_SERVICE_FILE),
        json: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--url" => {
                opts.url = next_value(&mut iter, arg)?;
                opts.url_explicit = true;
            }
            "--proxy" => opts.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => opts.server_http.no_system_proxy = true,
            "--env-file" => {
                opts.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?));
                opts.env_file_explicit = true;
            }
            "--token-file" => opts.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--service-file" => opts.service_file = PathBuf::from(next_value(&mut iter, arg)?),
            "--json" => opts.json = true,
            _ => return Err(format!("unknown server status flag: {}", arg)),
        }
    }
    opts.server_http.validate()?;
    if opts.url.trim().is_empty() {
        return Err("--url cannot be empty".to_string());
    }
    if opts.service_file.as_os_str().is_empty() {
        return Err("--service-file cannot be empty".to_string());
    }
    Ok(opts)
}

fn parse_pairing_create(args: &[String]) -> Result<PairingCreateOptions, String> {
    let mut opts = PairingCreateOptions {
        ttl_secs: 600,
        ..PairingCreateOptions::default()
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server-url" => opts.server_url = next_value(&mut iter, arg)?,
            "--proxy" => opts.server_http.proxy = Some(next_value(&mut iter, arg)?),
            "--no-system-proxy" => opts.server_http.no_system_proxy = true,
            "--env-file" => opts.env_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--token" => opts.token = Some(next_value(&mut iter, arg)?),
            "--token-file" => opts.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--username" => opts.username = next_value(&mut iter, arg)?,
            "--client-id" => opts.client_id = next_value(&mut iter, arg)?,
            "--display-name" => opts.display_name = Some(next_value(&mut iter, arg)?),
            "--ttl-secs" => {
                opts.ttl_secs = next_value(&mut iter, arg)?
                    .parse::<i64>()
                    .map_err(|_| "--ttl-secs must be an integer".to_string())?;
            }
            "--user-token-name" => opts.user_token_name = Some(next_value(&mut iter, arg)?),
            "--runner-token-name" | "--agent-token-name" => {
                opts.runner_token_name = Some(next_value(&mut iter, arg)?)
            }
            "--json" => opts.json = true,
            _ => return Err(format!("unknown pairing create flag: {}", arg)),
        }
    }
    opts.server_http.validate()?;
    if opts.server_url.trim().is_empty() {
        return Err("--server-url is required".to_string());
    }
    if opts.username.trim().is_empty() {
        return Err("--username is required".to_string());
    }
    if !(60..=3600).contains(&opts.ttl_secs) {
        return Err("--ttl-secs must be between 60 and 3600".to_string());
    }
    let auth_sources = opts.token.is_some() as u8
        + opts.token_file.is_some() as u8
        + opts.env_file.is_some() as u8;
    if auth_sources > 1 {
        return Err("use only one of --token, --token-file, or --env-file".to_string());
    }
    Ok(opts)
}

/// Small flag parser for `webcodex runner init`. Produces an
/// `RunnerInitOptions` consumed by the shared `runner_config::run_runner_init`.
fn parse_cli_runner_init(args: &[String]) -> Result<RunnerInitOptions, String> {
    let mut opts = RunnerInitOptions {
        server_url: String::new(),
        token: None,
        token_file: None,
        client_id: String::new(),
        owner: String::new(),
        display_name: None,
        transport: TRANSPORT_WEBSOCKET.to_string(),
        poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
        project_registry_dir: PathBuf::new(),
        output: PathBuf::new(),
        allowed_roots: Vec::new(),
        allow_cwd_anywhere: false,
        overwrite: false,
    };
    let mut profile: Option<String> = None;
    let mut output_explicit = false;
    let mut project_registry_dir_explicit = false;
    let mut legacy_projects_dir_explicit = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server-url" => opts.server_url = next_value(&mut iter, arg)?,
            "--token" => opts.token = Some(next_value(&mut iter, arg)?),
            "--token-file" => opts.token_file = Some(PathBuf::from(next_value(&mut iter, arg)?)),
            "--client-id" => opts.client_id = next_value(&mut iter, arg)?,
            "--profile" => profile = Some(next_value(&mut iter, arg)?),
            "--owner" => opts.owner = next_value(&mut iter, arg)?,
            "--display-name" => opts.display_name = Some(next_value(&mut iter, arg)?),
            "--transport" => opts.transport = next_value(&mut iter, arg)?,
            "--poll-interval-ms" => {
                let v = next_value(&mut iter, arg)?;
                opts.poll_interval_ms = v
                    .parse::<u64>()
                    .map_err(|_| "--poll-interval-ms must be an integer".to_string())?;
            }
            "--project-registry-dir" => {
                if legacy_projects_dir_explicit {
                    return Err(
                        "use only one of --project-registry-dir or legacy --projects-dir"
                            .to_string(),
                    );
                }
                opts.project_registry_dir = PathBuf::from(next_value(&mut iter, arg)?);
                project_registry_dir_explicit = true;
            }
            "--projects-dir" => {
                if project_registry_dir_explicit {
                    return Err(
                        "use only one of --project-registry-dir or legacy --projects-dir"
                            .to_string(),
                    );
                }
                opts.project_registry_dir = PathBuf::from(next_value(&mut iter, arg)?);
                legacy_projects_dir_explicit = true;
            }
            "--allowed-root" => opts
                .allowed_roots
                .push(PathBuf::from(next_value(&mut iter, arg)?)),
            "--allow-cwd-anywhere" => {
                opts.allow_cwd_anywhere = runner_config::parse_bool(&next_value(&mut iter, arg)?)?;
            }
            "--output" => {
                opts.output = PathBuf::from(next_value(&mut iter, arg)?);
                output_explicit = true;
            }
            "--overwrite" => opts.overwrite = true,
            "--help" | "-h" => return Err(usage().to_string()),
            _ => return Err(format!("unknown runner init flag: {}", arg)),
        }
    }
    if let Some(profile) = profile
        .as_deref()
        .map(validate_client_profile)
        .transpose()?
    {
        if !output_explicit {
            opts.output = client_profile_runner_config(&profile)?;
        }
        if !project_registry_dir_explicit && !legacy_projects_dir_explicit {
            opts.project_registry_dir = client_profile_project_registry_dir(&profile)?;
        }
    } else {
        if !output_explicit && opts.output.as_os_str().is_empty() {
            let profile = validate_client_profile(&opts.client_id)?;
            opts.output = client_profile_runner_config(&profile)?;
            if !project_registry_dir_explicit && !legacy_projects_dir_explicit {
                opts.project_registry_dir = client_profile_project_registry_dir(&profile)?;
            }
        } else if !project_registry_dir_explicit && !legacy_projects_dir_explicit {
            let default = Path::new(DEFAULT_INIT_PROJECT_REGISTRY_DIR);
            let base = default.parent().ok_or_else(|| {
                "default Runner project registry path has no parent directory".to_string()
            })?;
            opts.project_registry_dir = runner_config::paths::select_project_registry_dir(base)?;
        }
    }
    runner_config::validate_runner_init_options(&opts)?;
    Ok(opts)
}

fn next_value<'a, I>(iter: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = &'a String>,
{
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{} requires a value", flag))
}

/// Windows release boundary, evaluated before any command dispatch.
///
/// Windows supports explicit local foreground Server initialization/execution
/// and the platform-neutral project `share` path. Service-managed Server lifecycle
/// operations and Runner service install remain unsupported and fail before
/// platform-specific service logic. `--help` is exempt so help still renders.
#[cfg(windows)]
fn windows_unsupported_platform_action(args: &[String]) -> Option<&'static str> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return None;
    }
    match args.first().map(String::as_str) {
        Some("server") => match args.get(1).map(String::as_str) {
            Some("init") | Some("run") => None,
            Some("install" | "start" | "stop" | "restart" | "logs" | "uninstall") | None => Some(
                "Windows service-managed Server lifecycle is not supported yet.\n\
                 Use `webcodex server run` for foreground operation.",
            ),
            _ => None,
        },
        Some("runner") if args.get(1).map(String::as_str) == Some("install") => Some(
            "Automatic Windows Runner startup is not supported yet.\n\
             Use `webcodex connect` or `webcodex runner start --profile <name>.",
        ),
        _ => None,
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().ok();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let args = first_run_default_args(raw_args, cwd.as_deref(), std::env::consts::OS, interactive);
    #[cfg(windows)]
    if let Some(message) = windows_unsupported_platform_action(&args) {
        eprintln!("{message}");
        std::process::exit(1);
    }
    match cli_action(args) {
        CliAction::Project(args) => {
            let output = webcodex::run_project_command(args).await;
            if !output.stdout.is_empty() {
                print!("{}", output.stdout);
            }
            if !output.stderr.is_empty() {
                eprint!("{}", output.stderr);
            }
            std::process::exit(output.code);
        }
        CliAction::ProjectRegister(opts) => match run_project_register(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::ProjectActivate(opts) => match run_project_activate(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Connect(opts) => match run_connect(opts).await {
            Ok(result) => {
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                let mut stdout = stdout.lock();
                let mut stderr = stderr.lock();
                match write_connect_result(result, &mut stdout, &mut stderr) {
                    Ok(()) => std::process::exit(0),
                    Err(error) => {
                        let _ = writeln!(stderr, "{error}");
                        let _ = stderr.flush();
                        std::process::exit(1);
                    }
                }
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Disconnect(opts) => match run_disconnect(opts).await {
            Ok(result) => {
                print!("{}", result.render());
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::HostedLogWriter(state_dir) => {
            let stdin = std::io::stdin();
            match run_hosted_log_writer(&state_dir, &mut stdin.lock()) {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        CliAction::Admin(cmd) => match run_admin_command(cmd).await {
            Ok(stdout) => {
                println!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::TokenGenerate(opts) => {
            print!("{}", render_token_generate(opts));
            std::process::exit(0);
        }
        CliAction::TokenCreateLocal(opts) => match run_token_create_local(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::RunnerTokenCreateLocal(opts) => {
            match run_runner_token_create_local(opts).await {
                Ok(stdout) => {
                    print!("{}", stdout);
                    std::process::exit(0);
                }
                Err(stderr) => {
                    eprintln!("{}", stderr);
                    std::process::exit(1);
                }
            }
        }
        CliAction::RunnerInit(opts) => match run_runner_init(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::PairingCreate(opts) => match run_pairing_create(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Login(opts) => match run_login(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Logout(opts) => match run_logout(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Status(opts) => match run_status(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Ops(command) => {
            let strict = command.strict();
            match run_ops_command(command).await {
                Ok(output) => {
                    print!("{}", output.stdout);
                    if !output.stdout.ends_with('\n') {
                        println!();
                    }
                    std::process::exit(ops_exit_code(strict, output.status));
                }
                Err(stderr) => {
                    eprintln!("{}", stderr);
                    std::process::exit(1);
                }
            }
        }
        CliAction::RunnerInstall(opts) => match run_runner_install_service(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::RunnerRun(opts) => {
            print!("{}", foreground_run_banner("Runner"));
            let _ = std::io::stdout().flush();
            match run_internal_binary(&opts.bin, &opts.args, &opts.env) {
                Ok(code) => std::process::exit(code),
                Err(stderr) => {
                    eprintln!("{}", stderr);
                    std::process::exit(1);
                }
            }
        }
        CliAction::ServerRun(opts) => {
            print!("{}", foreground_run_banner("Server"));
            let _ = std::io::stdout().flush();
            match run_internal_binary(&opts.bin, &opts.args, &opts.env) {
                Ok(code) => std::process::exit(code),
                Err(stderr) => {
                    eprintln!("{}", stderr);
                    std::process::exit(1);
                }
            }
        }
        CliAction::ServerTunnel(opts) => match run_server_tunnel(opts).await {
            Ok(()) => std::process::exit(0),
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::RunnerService(opts) => match run_runner_service(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::RunnerStatus(opts) => match run_runner_status(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::ServerInit(opts) => match run_server_init(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::ServerInstall(opts) => match run_server_install_service(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::ServerService(opts) => match run_server_service(opts) {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::ServerStatus(opts) => match run_server_status(opts).await {
            Ok(stdout) => {
                print!("{}", stdout);
                if !stdout.ends_with('\n') {
                    println!();
                }
                std::process::exit(0);
            }
            Err(stderr) => {
                eprintln!("{}", stderr);
                std::process::exit(1);
            }
        },
        CliAction::Exit {
            code,
            stdout,
            stderr,
        } => {
            if !stdout.is_empty() {
                print!("{}", stdout);
            }
            if !stderr.is_empty() {
                eprint!("{}", stderr);
            }
            std::process::exit(code);
        }
    }
}

#[cfg(test)]
#[path = "webcodex_cli/tests/mod.rs"]
mod tests;
