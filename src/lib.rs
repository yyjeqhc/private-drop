#![recursion_limit = "512"]

use crate::route_metadata::RouteId;
use salvo::cors::Cors;
use salvo::prelude::*;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
#[cfg(test)]
use uuid::Uuid;

mod action_audit;
mod action_audit_sessions;
mod admin_http;
mod admin_project_lifecycle;
#[cfg(test)]
mod agent_continuation_tests;
mod agent_wake;
mod audit_http;
mod auth;
mod client_window;
mod config;
mod connector_runtime;
mod console_web;
mod db;
mod host_console_http;
mod job_observation;
mod mcp;
mod mcp_gateway;
mod model_surface;
mod models;
mod oauth_http;
mod openapi;
mod pairing_http;
mod plugin_gateway;
mod project_entry;
mod projects;
mod route_metadata;
mod runner_http;
mod runner_quic;
mod runner_session;
mod runner_tokens_http;
mod runner_ws;
mod runtime_console_http;
mod runtime_http;
mod server_instance;
mod server_listener;
mod server_shutdown;
mod ssh_resource_gateway;
mod startup;
mod task_cli;
#[cfg(test)]
mod test_support;
mod tool_request_trace;
mod tool_runtime;
mod users_http;
mod workspace_activity_store;

#[cfg(test)]
pub(crate) use webcodex_admin as admin_cli;
pub(crate) use webcodex_core::{
    apply_edits_shared, apply_patch_shared, artifact_policy, build_info, lsp_bridge,
    runner_protocol, sensitive_paths,
};
pub(crate) use webcodex_runner_config as runner_config;
pub(crate) use webcodex_workspace::project_overview;
#[cfg(test)]
pub(crate) use webcodex_workspace::workspace_checkpoint;

pub(crate) use auth::{get_db, json_error, AuthMiddleware};
pub(crate) use config::load_startup_env_files;
#[cfg(test)]
pub(crate) use config::parse_env_file_line;
pub use config::Config;
pub use config::OAuth2Config;
pub use db::{Database, RotateResult};
pub use models::{ActionEventRecord, ActionSessionRecord};
pub(crate) use openapi::openapi_json;
pub(crate) use runner_http::{
    runner_job_update, runner_offline, runner_persistent_shell_result, runner_poll,
    runner_register, runner_result, shell_file_op, shell_job, shell_job_log, shell_job_status,
    shell_job_stop, shell_jobs_list, shell_run, RunnerRegistry,
};
pub use startup::{
    is_project_command, run_project_command, run_regular_server_tunnel, CliCommandOutput,
    RegularServerTunnelOptions,
};

// ============================================================================
// Main
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerBinaryAction {
    Run {
        stop_on_stdin_eof: bool,
    },
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
}

pub fn server_binary_action<I, S>(args: I) -> ServerBinaryAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect();
    match args.as_slice() {
        [] => ServerBinaryAction::Run {
            stop_on_stdin_eof: false,
        },
        [arg] if arg == "--stop-on-stdin-eof" => ServerBinaryAction::Run {
            stop_on_stdin_eof: true,
        },
        [arg] if matches!(arg.as_str(), "--help" | "-h") => ServerBinaryAction::Exit {
            code: 0,
            stdout: "Usage: webcodex-server [OPTIONS]\n\nRun the WebCodex server runtime.\n\nOptions:\n      --stop-on-stdin-eof  Stop when the invoking parent closes stdin\n  -h, --help               Print help and exit\n  -V, --version            Print version and exit\n".to_string(),
            stderr: String::new(),
        },
        [arg] if matches!(arg.as_str(), "--version" | "-V") => ServerBinaryAction::Exit {
            code: 0,
            stdout: build_info::version_output("webcodex-server"),
            stderr: String::new(),
        },
        _ => ServerBinaryAction::Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!(
                "unknown argument(s): {}\nRun `webcodex-server --help` for usage.\n",
                args.join(" ")
            ),
        },
    }
}

/// Whole-service HTTP request timeout (defense in depth). Must stay above the
/// MCP dispatch hard bound (150s) plus response-write margin so the inner,
/// better-reported timeouts always fire first.
const REQUEST_HARD_TIMEOUT_SECS: u64 = 300;

/// A finite request may legitimately consume the full HTTP hard timeout. Give
/// its response and connection teardown an additional bounded window before
/// Salvo escalates graceful shutdown to a forcible connection stop.
const SERVER_GRACEFUL_RESPONSE_MARGIN_SECS: u64 = 15;

/// Application-owned graceful Server drain deadline. This is deliberately
/// derived from the maximum finite HTTP request lifetime instead of being an
/// independent operational magic number.
pub const SERVER_GRACEFUL_SHUTDOWN_TIMEOUT_SECS: u64 =
    REQUEST_HARD_TIMEOUT_SECS + SERVER_GRACEFUL_RESPONSE_MARGIN_SECS;

/// systemd must outlive the application's own graceful/forced stop lifecycle
/// so PID 1 does not SIGKILL the Server before WebCodex's bounded deadline.
const SERVER_SYSTEMD_STOP_MARGIN_SECS: u64 = 15;
pub const SERVER_SYSTEMD_TIMEOUT_STOP_SECS: u64 =
    SERVER_GRACEFUL_SHUTDOWN_TIMEOUT_SECS + SERVER_SYSTEMD_STOP_MARGIN_SECS;

static PREPARED_SERVER_ENV_LOADS: std::sync::OnceLock<Vec<config::EnvFileLoad>> =
    std::sync::OnceLock::new();

/// Prepare process-global Server startup state while the binary is still
/// single-threaded. In particular this consumes systemd `LISTEN_*` metadata
/// before the Tokio runtime exists, so neither the metadata nor the activation
/// listener can leak into later child processes.
#[doc(hidden)]
pub fn prepare_server_process_environment() -> Result<(), String> {
    if PREPARED_SERVER_ENV_LOADS.get().is_some() {
        return Ok(());
    }
    let env_loads = load_startup_env_files()?;
    server_listener::prepare_activation_from_env()?;
    PREPARED_SERVER_ENV_LOADS
        .set(env_loads)
        .map_err(|_| "Server process environment was prepared concurrently".to_string())
}

pub async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    run_server_with_parent_liveness(false).await
}

#[doc(hidden)]
pub async fn run_server_with_parent_liveness(
    stop_on_stdin_eof: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let env_loads = match PREPARED_SERVER_ENV_LOADS.get() {
        Some(prepared) => prepared.clone(),
        None => load_startup_env_files().map_err(std::io::Error::other)?,
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    for load in &env_loads {
        tracing::info!(
            "Loaded env file {} ({} variables set)",
            load.path.display(),
            load.loaded_count,
        );
    }
    let config = Config::from_env();
    let (acceptor, listener_mode, listener_addr) = server_listener::server_acceptor(&config.addr)
        .await
        .map_err(std::io::Error::other)?;
    let console_asset_source = Arc::new(
        console_web::ConsoleAssetSource::from_env(&config.addr).map_err(std::io::Error::other)?,
    );
    if !config.is_auth_enabled() {
        tracing::warn!(
            "WEBCODEX_TOKEN is not set! Running in development mode without authentication. \
Use `webcodex server init` to generate a bootstrap/admin key, or set WEBCODEX_ALLOW_ANONYMOUS=true \
only for local/trusted-network demos."
        );
        tracing::warn!("Anonymous API access is rejected by default in production mode.");
    }
    let build_info = build_info::current();
    tracing::info!(
        "Starting WebCodex v{} (commit {})",
        build_info.version,
        build_info.git_commit.unwrap_or("unknown")
    );
    tracing::info!("Data directory: {:?}", config.data_dir);
    let addr = config.addr.clone();
    tracing::info!(
        "Listening on: {} (actual {}, mode {:?})",
        addr,
        listener_addr,
        listener_mode
    );
    tracing::info!("Console assets: {}", console_asset_source.mode_label());
    if let Some(directory) = console_asset_source.directory() {
        tracing::info!("Console assets directory: {}", directory.display());
    }
    std::fs::create_dir_all(&config.data_dir)?;
    let db = Database::open(&config.db_path())?;
    let server_instance_guard = server_instance::ServerInstanceGuard::acquire(&db)?;
    db.recover_agent_wakes_for_server_takeover(
        &server_instance_guard,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(anyhow::Error::from)?;
    tracing::info!("Database initialized at {:?}", config.db_path());

    // Set max payload size to 2MB for text messages
    salvo::http::request::set_global_secure_max_size(config.max_text_size);

    let cors = Cors::permissive();
    let config = Arc::new(config);
    let db = Arc::new(db);
    // First-party authorize browser session store (in-memory, short-lived).
    // Holds the opaque session id -> user mapping bridging the authorize
    // login form to the consent decision. PAT/bootstrap plaintext is never
    // stored here — only the resolved user identity.
    let authorize_session_store = Arc::new(oauth_http::AuthorizeSessionStore::new());
    let runner_registry = Arc::new(runner_http::registry_with_tool_request_trace());
    // Root HTTP admission consults this process-local state before any
    // side-effecting handler can run. It closes the small race between the
    // authoritative drain transition and Salvo consuming its stop command.
    let shutdown_coordinator = Arc::new(server_shutdown::ShutdownCoordinator::default());
    let quic_cfg = config::QuicServerConfig::from_env();
    let connector_context =
        connector_runtime::ConnectorContext::from_env().map_err(std::io::Error::other)?;
    // Resolve the top-level runtime exposure exactly once at startup, after
    // Connector configuration has been parsed and validated. Every request-time
    // projection reads this immutable enum from ToolRuntime.
    let runtime_exposure = model_surface::resolve_runtime_exposure(connector_context.as_ref())
        .map_err(std::io::Error::other)?;
    let runtime_info = Arc::new(tool_runtime::RuntimeInfo::from_config_with_quic_config(
        &config, &quic_cfg,
    ));
    let runtime_state_dir = config.runtime_state_dir();
    let mut tool_runtime_builder =
        tool_runtime::ToolRuntime::new(runner_registry.clone(), runtime_info.clone())
            .with_runtime_exposure(runtime_exposure)
            .with_window_activity_database(db.clone())
            .with_memory_database(db.clone())
            .with_communication_database(db.clone())
            .with_checkpoint_state_dir(runtime_state_dir.clone())
            .with_session_ledger(config.session_ledger_path())
            .with_persistent_coding_agent_observation_state(&runtime_state_dir)
            .map_err(std::io::Error::other)?;
    if let Some(activity_store) = db::WorkspaceActivityStore::from_env(db.clone()) {
        tool_runtime_builder =
            tool_runtime_builder.with_activity_recorder(Arc::new(activity_store));
    }
    let tool_runtime = Arc::new(tool_runtime_builder);
    let connector_runtime = connector_runtime::ConnectorRuntime::from_context(
        tool_runtime.clone(),
        db.clone(),
        connector_context,
    )
    .map_err(std::io::Error::other)?;
    if let Some(runtime) = connector_runtime.0.as_ref() {
        tracing::info!(
            project_id = %runtime.context().project_id,
            profile = %runtime.context().profile,
            capabilities = connector_runtime::surface::CAPABILITY_NAMES.len(),
            runtime_exposure = runtime_exposure.name(),
            "Project-bound Connector exposure enabled"
        );
    } else {
        tracing::info!(
            runtime_exposure = runtime_exposure.name(),
            config = "WEBCODEX_MCP_MODEL_SURFACE",
            "MCP runtime exposure enabled"
        );
    }

    // Custom QUIC Runner transport. Default disabled;
    // only starts when WEBCODEX_QUIC_ENABLED=true. Runs a separate quinn UDP
    // listener in parallel with the HTTP server. HTTP/WebSocket/polling and
    // the GPT Actions / Nginx path are completely unaffected. This is NOT
    // HTTP/3 and Nginx does not terminate QUIC.
    if quic_cfg.enabled {
        if let Err(e) = quic_cfg.validate() {
            if let Some(status) = runtime_info.quic.as_ref() {
                status
                    .lock()
                    .expect("quic runtime status mutex poisoned")
                    .mark_error(&e);
            }
            tracing::error!(
                "QUIC listener disabled due to config error: {}; check WEBCODEX_QUIC_LISTEN/CERT/KEY/ALPN",
                e
            );
        } else {
            let quic_config = config.clone();
            let quic_db = db.clone();
            let quic_registry = runner_registry.clone();
            let quic_cfg_task = quic_cfg.clone();
            let quic_status = runtime_info.quic.clone();
            tokio::spawn(async move {
                if let Err(e) = runner_quic::run_runner_quic_listener(
                    quic_config,
                    Some(quic_db),
                    quic_registry,
                    quic_cfg_task,
                    quic_status,
                )
                .await
                {
                    tracing::error!(
                        "QUIC agent listener exited with error: {}; check bind address, UDP port availability, certificate/key readability, and ALPN",
                        e
                    );
                }
            });
            tracing::info!(
                "Runner QUIC configured on UDP {} ALPN {}",
                quic_cfg.listen,
                quic_cfg.alpn
            );
        }
    }

    let authed_api_router = Router::new()
        .hoop(AuthMiddleware)
        .push(connector_runtime::http::routes())
        .push(host_console_http::routes())
        .push(runtime_console_http::routes())
        .push(admin_http::routes())
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ToolsList))
                .post(runtime_http::tools_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ToolsCall))
                .post(runtime_http::tools_call),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ArtifactsImport))
                .post(runtime_http::import_conversation_files_to_project),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::JobsStatus))
                .post(runtime_http::job_status),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::JobsLog))
                .post(runtime_http::job_log),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::JobsStop))
                .post(runtime_http::job_stop),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::JobsList))
                .post(runtime_http::jobs_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::JobsTail))
                .post(runtime_http::job_tail),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerConfigCheck))
                .post(runtime_http::runner_config_check),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerConfigReload))
                .post(runtime_http::runner_config_reload),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsList))
                .post(runtime_http::projects_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsRegister))
                .post(runtime_http::projects_register),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsCreate))
                .post(runtime_http::projects_create),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsUnregister))
                .post(runtime_http::projects_unregister),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsResolveOrRegister))
                .post(runtime_http::projects_resolve_or_register),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsGitStatus))
                .post(runtime_http::projects_git_status),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsGitDiff))
                .post(runtime_http::projects_git_diff),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsGitDiffSummary))
                .post(runtime_http::projects_git_diff_summary),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsListFiles))
                .post(runtime_http::projects_list_files),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsApplyUnifiedDiff))
                .post(runtime_http::projects_apply_unified_diff),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsRunShell))
                .post(runtime_http::projects_run_shell),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsGitRestorePaths))
                .post(runtime_http::projects_git_restore_paths),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsDiscardUntracked))
                .post(runtime_http::projects_discard_untracked),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ProjectsRunJob))
                .post(runtime_http::projects_run_job),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RuntimeStatus))
                .post(runtime_http::runtime_status),
        )
        // Phase 2e-3: first-party OAuth client management API. Behind
        // AuthMiddleware; route policy is FirstPartyOnly so OAuth2 access
        // tokens are rejected even with account:manage.
        .push(
            Router::with_path(route_metadata::api_path(RouteId::OAuthClientsCreate))
                .post(oauth_http::oauth_clients_create),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::OAuthClientsList))
                .post(oauth_http::oauth_clients_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::OAuthClientsUpdateScopes))
                .post(oauth_http::oauth_clients_update_scopes),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::OAuthClientsRevoke))
                .post(oauth_http::oauth_clients_revoke),
        )
        .push(
            Router::with_path(route_metadata::api_path(
                RouteId::OAuthSharedKeyClientProvision,
            ))
            .post(oauth_http::oauth_shared_key_client_provision),
        )
        // Phase 2 multi-user auth: user + personal API token management.
        // REST-only admin/self-management surface; intentionally NOT
        // exposed in /openapi.json (GPT Actions) because token creation is
        // sensitive. All behind the shared AuthMiddleware Bearer auth.
        .push(
            Router::with_path(route_metadata::api_path(RouteId::UsersCreate))
                .post(users_http::users_create),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::UsersList))
                .post(users_http::users_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::UsersMe))
                .post(users_http::users_me),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::TokensCreate))
                .post(users_http::tokens_create),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::TokensRegisterHash))
                .post(users_http::tokens_register_hash),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::TokensList))
                .post(users_http::tokens_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::TokensRevoke))
                .post(users_http::tokens_revoke),
        )
        // Phase 3 agent token management: REST-only admin/self-management
        // surface for agent tokens bound to an owner + allowed_client_id.
        // Intentionally NOT exposed in /openapi.json (GPT Actions) because
        // token creation is sensitive. All behind the shared AuthMiddleware
        // Bearer auth. Agent tokens themselves are rejected from these
        // endpoints so a leaked agent token cannot mint more tokens.
        .push(
            Router::with_path(route_metadata::api_path(RouteId::AgentTokensCreate))
                .post(runner_tokens_http::runner_tokens_create),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::AgentTokensRegisterHash))
                .post(runner_tokens_http::runner_tokens_register_hash),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::AgentTokensList))
                .post(runner_tokens_http::runner_tokens_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::AgentTokensRevoke))
                .post(runner_tokens_http::runner_tokens_revoke),
        )
        .push(Router::with_path(route_metadata::api_path(RouteId::ShellRun)).post(shell_run))
        .push(Router::with_path(route_metadata::api_path(RouteId::ShellFile)).post(shell_file_op))
        .push(Router::with_path(route_metadata::api_path(RouteId::ShellJob)).post(shell_job))
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ShellJobsStatus))
                .post(shell_job_status),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ShellJobsLog)).post(shell_job_log),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ShellJobsStop))
                .post(shell_job_stop),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::ShellJobsList))
                .post(shell_jobs_list),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerRegister))
                .post(runner_register),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerOffline))
                .post(runner_offline),
        )
        .push(Router::with_path(route_metadata::api_path(RouteId::RunnerPoll)).post(runner_poll))
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerResult)).post(runner_result),
        )
        .push(
            Router::with_path(route_metadata::api_path(
                RouteId::RunnerPersistentShellResult,
            ))
            .post(runner_persistent_shell_result),
        )
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerJobUpdate))
                .post(runner_job_update),
        )
        // WebSocket Runner transport (preferred long-lived connection).
        // Polling endpoints above remain as fallback. Bearer auth is
        // enforced by the shared AuthMiddleware hoop.
        .push(
            Router::with_path(route_metadata::api_path(RouteId::RunnerWs))
                .get(runner_ws::runner_ws),
        );

    let api_router = Router::with_path("api")
        .push(
            Router::with_path(route_metadata::api_path(RouteId::PairingEnroll))
                .post(pairing_http::pairing_enroll),
        )
        .push(
            authed_api_router.push(
                Router::with_path(route_metadata::api_path(RouteId::PairingCreate))
                    .post(pairing_http::pairing_create),
            ),
        );

    let openapi_router =
        Router::with_path(route_metadata::root_path(RouteId::OpenApiDocument)).get(openapi_json);

    // Read-only readiness console. Public static entry — the HTML/JS/CSS
    // bundle carries no secrets; project facts come from the protected shared
    // `POST /api/connector/readiness` application projection. Mirrors
    // `/openapi.json` being public. NOT part of the GPT Actions schema.
    let console_root = RouteId::ConsoleWebRoot;
    let console_router = Router::with_path(route_metadata::root_path(console_root))
        .get(console_web::console_html)
        .push(
            Router::with_path(route_metadata::direct_child_path(
                console_root,
                RouteId::ConsoleWebAppJs,
            ))
            .get(console_web::console_app_js),
        )
        .push(
            Router::with_path(route_metadata::direct_child_path(
                console_root,
                RouteId::ConsoleWebStylesCss,
            ))
            .get(console_web::console_styles_css),
        );

    let runtime_root = RouteId::RuntimeWebRoot;
    let runtime_console_router = Router::with_path(route_metadata::root_path(runtime_root))
        .get(console_web::runtime_html)
        .push(
            Router::with_path(route_metadata::direct_child_path(
                runtime_root,
                RouteId::RuntimeWebAppJs,
            ))
            .get(console_web::runtime_app_js),
        )
        .push(
            Router::with_path(route_metadata::direct_child_path(
                runtime_root,
                RouteId::RuntimeWebStylesCss,
            ))
            .get(console_web::runtime_styles_css),
        );

    let admin_root = RouteId::AdminWebRoot;
    let admin_router = Router::with_path(route_metadata::root_path(admin_root))
        .get(console_web::admin_html)
        .push(
            Router::with_path(route_metadata::direct_child_path(
                admin_root,
                RouteId::AdminWebAppJs,
            ))
            .get(console_web::admin_app_js),
        )
        .push(
            Router::with_path(route_metadata::direct_child_path(
                admin_root,
                RouteId::AdminWebStylesCss,
            ))
            .get(console_web::admin_styles_css),
        );

    let mut router = Router::new()
        .hoop(server_shutdown::DrainAdmission::new(
            shutdown_coordinator.clone(),
        ))
        // Whole-service backstop: no handler may hold an HTTP request open
        // forever. Sized well above every legitimate request — sync agent
        // waits are <= ~122s and MCP dispatch is hard-bounded at 150s — so it
        // only fires on a genuinely unbounded hang, converting a permanently
        // silent request into an explicit 503. Long-lived work is unaffected:
        // Runner polling replies immediately and WebSocket connections live in
        // a task spawned after the (fast) upgrade handshake completes.
        .hoop(salvo::timeout::Timeout::new(
            std::time::Duration::from_secs(REQUEST_HARD_TIMEOUT_SECS),
        ))
        .hoop(affix_state::inject(config.clone()))
        .hoop(affix_state::inject(db.clone()))
        .hoop(affix_state::inject(authorize_session_store.clone()))
        .hoop(affix_state::inject(runner_registry.clone()))
        .hoop(affix_state::inject(tool_runtime.clone()))
        .hoop(affix_state::inject(connector_runtime.clone()))
        .hoop(affix_state::inject(console_asset_source))
        .hoop(cors.into_handler())
        .push(api_router)
        .push(openapi_router)
        .push(console_router)
        .push(runtime_console_router)
        .push(admin_router)
        // OAuth2 token, revocation, and discovery endpoints — public, no
        // AuthMiddleware. Token/revoke clients authenticate via
        // client_id + client_secret in the form body.
        .push(
            Router::with_path(route_metadata::root_path(RouteId::OAuthToken))
                .post(oauth_http::oauth_token),
        )
        .push(
            Router::with_path(route_metadata::root_path(RouteId::OAuthRevoke))
                .post(oauth_http::oauth_revoke),
        )
        // /oauth/authorize is NOT behind AuthMiddleware: the handler accepts
        // either a first-party Bearer token (Bootstrap / PAT, backward
        // compatible direct code issuance) or a short-lived authorize
        // session cookie set by the login form. login/consent do their own
        // token/session validation.
        .push(
            Router::new()
                .push(
                    Router::with_path(route_metadata::root_path(RouteId::OAuthAuthorize))
                        .get(oauth_http::oauth_authorize),
                )
                .push(
                    Router::with_path(route_metadata::root_path(RouteId::OAuthAuthorizeLogin))
                        .post(oauth_http::oauth_authorize_login),
                )
                .push(
                    Router::with_path(route_metadata::root_path(RouteId::OAuthAuthorizeConsent))
                        .post(oauth_http::oauth_authorize_consent),
                )
                .push(
                    Router::with_path(route_metadata::root_path(RouteId::OAuthAuthorizeBridge))
                        .post(oauth_http::oauth_authorize_bridge),
                )
                .push(
                    Router::with_path(route_metadata::root_path(RouteId::OAuthAuthorizeProject))
                        .post(oauth_http::oauth_authorize_project),
                ),
        )
        .push(
            Router::with_path(route_metadata::root_path(
                RouteId::WellKnownProtectedResource,
            ))
            .get(oauth_http::oauth_metadata),
        )
        .push(
            Router::with_path(route_metadata::root_path(
                RouteId::WellKnownAuthorizationServer,
            ))
            .get(oauth_http::oauth_authorization_server_metadata),
        )
        .push(
            Router::with_path(route_metadata::shared_root_path(
                RouteId::McpGet,
                RouteId::McpPost,
            ))
            .hoop(AuthMiddleware)
            .get(mcp::mcp_info)
            .post(mcp::mcp_post),
        );

    // Read-only audit query API. Admin/debug surface only: NOT part of the
    // GPT Actions OpenAPI schema. All endpoints are POST + Bearer auth.
    router = router.push(
        Router::new()
            .hoop(AuthMiddleware)
            .push(
                Router::with_path(route_metadata::root_path(RouteId::AuditSessions))
                    .post(audit_http::audit_sessions),
            )
            .push(
                Router::with_path(route_metadata::root_path(RouteId::AuditSession))
                    .post(audit_http::audit_session),
            )
            .push(
                Router::with_path(route_metadata::root_path(RouteId::AuditStats))
                    .post(audit_http::audit_stats),
            ),
    );
    tracing::info!("WebCodex Server is running.");
    let port = addr.split(':').next_back().unwrap_or("8080");
    let base = format!("http://localhost:{}", port);
    tracing::info!("Runtime base: {}", base);
    tracing::info!("MCP endpoint: {}/mcp", base);
    tracing::info!(
        "Next: create a one-time login code in another terminal with `webcodex pairing create`."
    );
    tracing::info!(
        tool_request_trace = crate::config::tool_request_trace_enabled(),
        "tool_request_trace"
    );
    tracing::info!(
        mcp_compact_schemas = crate::model_surface::effective_mcp_compact_schemas(
            runtime_exposure,
            crate::config::mcp_compact_schemas_override(),
        ),
        "mcp_compact_schemas"
    );
    tracing::info!("OpenAPI (GPT Actions): {}/openapi.json", base);
    tracing::info!("MCP App console: {}/console", base);
    tracing::info!("Runtime status: {}/api/runtime/status", base);
    tracing::info!("Runner WebSocket: {}/api/agents/ws", base);
    tracing::info!("Runner polling (fallback): {}/api/shell/agent/poll", base);
    tracing::info!("Audit API (read-only): {}/api/audit/sessions", base);
    // Periodic recovery-timeout sweep for disconnected reconciliation-capable
    // runners. A job whose runner disconnected enters `recovering`; if that
    // runner never reconnects and nobody queries the job, the on-demand
    // deadline check would never run. This background task bounds `recovering`
    // to the grace window independently of request traffic. It is pure
    // in-memory, holds the registry mutex only for bounded HashMap work, and
    // dies with the process. A server restart resets the in-memory registry;
    // the deadline is re-anchored only when a runner reconnects and submits its
    // inventory. See docs/RUNNER.md (reconnect and recovery).
    let sweep_registry = runner_registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            runner_http::RECOVERY_SWEEP_INTERVAL_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the first (immediate) tick so a sweep does not race startup
        // reconciliation before any runner has had a chance to re-register.
        interval.tick().await;
        loop {
            interval.tick().await;
            runner_http::recovery_timeout_sweep(&sweep_registry).await;
        }
    });
    server_shutdown::serve_until_termination(
        Server::new(acceptor),
        router,
        shutdown_coordinator,
        std::time::Duration::from_secs(SERVER_GRACEFUL_SHUTDOWN_TIMEOUT_SECS),
        stop_on_stdin_eof,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_parent_liveness_is_explicit_opt_in() {
        assert_eq!(
            server_binary_action(std::iter::empty::<&str>()),
            ServerBinaryAction::Run {
                stop_on_stdin_eof: false,
            }
        );
        assert_eq!(
            server_binary_action(["--stop-on-stdin-eof"]),
            ServerBinaryAction::Run {
                stop_on_stdin_eof: true,
            }
        );
    }

    #[test]
    fn test_parse_env_file_line_basic() {
        let parsed = parse_env_file_line("WEBCODEX_ADDR=127.0.0.1:8080")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "WEBCODEX_ADDR");
        assert_eq!(parsed.1, "127.0.0.1:8080");
    }

    #[test]
    fn test_parse_env_file_line_quotes_and_export() {
        let parsed = parse_env_file_line("export RUST_LOG='info,codex.metrics=info'")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, "RUST_LOG");
        assert_eq!(parsed.1, "info,codex.metrics=info");
    }

    #[test]
    fn test_parse_env_file_line_ignores_empty_and_comments() {
        assert!(parse_env_file_line("").is_none());
        assert!(parse_env_file_line("  # comment").is_none());
    }

    #[test]
    fn test_parse_env_file_line_rejects_invalid_key() {
        assert!(parse_env_file_line("webcodex_token=x").unwrap().is_err());
        assert!(parse_env_file_line("DROP TOKEN=x").unwrap().is_err());
    }

    #[test]
    fn test_uuid_generation_not_empty() {
        let id = Uuid::new_v4().to_string();
        assert!(!id.is_empty());
        assert_eq!(id.len(), 36); // UUID v4 with hyphens
        assert!(id.contains('-'));
    }

    #[test]
    fn test_uuid_generation_unique() {
        let id1 = Uuid::new_v4().to_string();
        let id2 = Uuid::new_v4().to_string();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_config_from_env_defaults() {
        let mut env = crate::test_support::TestEnvGuard::new();
        // Clear env vars to test defaults; Drop restores the process environment.
        env.remove("WEBCODEX_ADDR");
        env.remove("WEBCODEX_DATA");
        env.remove("WEBCODEX_TOKEN");

        let config = Config::from_env();
        assert_eq!(config.addr, "0.0.0.0:8080");
        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.token, None);
        assert!(!config.is_auth_enabled());
        assert_eq!(config.max_text_size, 2 * 1024 * 1024);
    }

    #[test]
    fn test_config_validate_token() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: Some("secret123".to_string()),
            max_text_size: 2 * 1024 * 1024,
            oauth2: crate::OAuth2Config::default(),
        };
        assert!(config.is_auth_enabled());
        assert!(config.validate_token("secret123"));
        assert!(!config.validate_token("wrong"));
        assert!(!config.validate_token(""));
    }

    #[test]
    fn test_config_validate_token_none() {
        let config = Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            oauth2: crate::OAuth2Config::default(),
        };
        assert!(!config.is_auth_enabled());
        // When no token is set, validation always returns false
        assert!(!config.validate_token("anything"));
    }

    #[test]
    fn test_filename_sanitization() {
        // Test that path separators are stripped from display names
        let filename = "test/file\\name.txt";
        let safe: String = filename
            .chars()
            .filter(|c| !matches!(c, '/' | '\\' | '\0' | '\r' | '\n'))
            .collect();
        assert_eq!(safe, "testfilename.txt");
    }

    #[test]
    fn test_filename_sanitization_quotes() {
        let filename = "file\"name.txt";
        let safe = filename.replace('"', "_");
        assert_eq!(safe, "file_name.txt");
    }
}
