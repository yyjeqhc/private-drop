pub(crate) mod artifacts;
pub(crate) mod checkpoints;
pub(crate) mod coding_agent;
pub(crate) mod computer;
pub(crate) mod config;
pub(crate) mod configured_skills;
pub(crate) mod detached_job;
pub(crate) mod dispatch;
#[cfg(windows)]
pub(crate) mod exit_diagnostics;
pub(crate) mod external_tools;
pub(crate) mod files;
pub(crate) mod lsp;
pub(crate) mod managed_ssh;
pub(crate) mod mcp_gateway;
pub(crate) mod output;
pub(crate) mod output_text;
pub(crate) mod patches;
pub(crate) mod persistent_shell;
pub(crate) mod plugin;
pub(crate) mod projects;
// Remote persistent shells always run POSIX sh/bash on the SSH target. Their
// local child ownership is platform-specific: Unix uses a private process group,
// while Windows owns ssh.exe through ManagedChild's Job Object.
#[cfg(any(unix, windows))]
pub(crate) mod remote_shell;
pub(crate) mod shell;
pub(crate) mod shutdown;
pub(crate) mod skill_store;
pub(crate) mod ssh;
pub(crate) mod transport;
pub(crate) mod util;
pub(crate) mod validation;

pub(crate) use artifacts::handle_artifact_file_operation;
#[cfg(test)]
pub(crate) use artifacts::is_artifact_request_kind;
pub(crate) use checkpoints::handle_checkpoint_file_request;
#[cfg(test)]
pub(crate) use checkpoints::is_checkpoint_request_kind;
pub(crate) use computer::handle_computer_operation;
pub(crate) use config::SshConfig;
pub(crate) use config::{
    client_profile_runner_config, default_config_path, hostname, load_config, max_concurrent_jobs,
    project_registry_dir, validate_client_profile, HotRunnerConfig, ReloadableRunnerConfig,
    RunnerConfig, RunnerPolicy, ShellConfig,
};
#[cfg(test)]
pub(crate) use config::{
    default_quic_alpn, default_quic_connect_timeout_secs, default_quic_keepalive_interval_secs,
    default_websocket_connect_timeout_secs, QuicClientConfig, ShellProfileConfig,
    CLIENT_PROFILE_ERROR, DEFAULT_MAX_CONCURRENT_JOBS,
};
pub(crate) use configured_skills::handle_configured_skill_roots_request;
#[cfg(test)]
pub(super) use dispatch::dispatch_request;
pub(super) use dispatch::{dispatch_request_with_outcome, RunnerDispatchOutcome};
#[cfg(test)]
pub(crate) use files::is_basic_file_request_kind;
#[cfg(test)]
pub(crate) use files::sha256_hex_bytes;
pub(crate) use files::{handle_basic_file_request, resolve_requested_path};
pub(crate) use lsp::LspSupervisor;
pub(crate) use output::{err_cmd, ok_cmd, CommandResult, ShellCommandResult};
#[cfg(test)]
pub(crate) use patches::is_structured_edit_request_kind;
pub(crate) use patches::{
    handle_apply_patch_file_request, handle_apply_text_edits_file_request,
    handle_write_project_file_request, validate_structured_edit_runner_path,
};
pub(crate) use persistent_shell::PersistentShellManager;
#[cfg(test)]
pub(crate) use projects::{
    handle_prepare_managed_worktree, handle_project_lifecycle_op, handle_project_op,
    handle_resolve_or_register_project, load_runner_project_summaries_from_dir,
};
pub(crate) use projects::{
    handle_prepare_managed_worktree_operation, handle_project_lifecycle_operation,
    handle_project_operation, handle_resolve_or_register_project_operation, RunnerProjectCache,
};
#[cfg(test)]
pub(crate) use projects::{
    parse_runner_project_toml, runner_project_summary, validate_project_path_policy,
};
pub(crate) use shell::{
    configured_prepared_shell_job_command, configured_shell_job_command,
    configured_validation_job_command, cwd_allowed, prepare_detached_process_launch,
    resolve_prepared_shell_profile, run_internal_posix_script_with_profiles_and_execution_state,
    run_internal_search_script_with_profiles_and_execution_state,
    run_process_with_profiles_and_execution_state,
    run_process_with_profiles_and_execution_state_with_start_hook,
    run_script_with_profiles_and_execution_state,
    run_script_with_profiles_and_execution_state_with_start_hook,
    run_shell_with_profiles_and_execution_state, PreparedShellProfile, PreparedShellProfileCache,
};
#[cfg(test)]
pub(crate) use shell::{run_shell, run_shell_with_profiles};
pub(crate) use skill_store::handle_skill_store_request;
pub(crate) use ssh::{is_transport_failure, run_ssh_shell_with_execution_state, SshConnectionPool};
#[cfg(all(test, unix))]
pub(crate) use transport::install_reload_listener;
#[cfg(test)]
pub(crate) use transport::{
    auto_transport_plan, build_ws_request, effective_transport, non_empty_token,
    quic_client_bind_addr_for, resolve_quic_config, resolve_quic_server_addrs, server_url_to_ws,
    websocket_session, ResultSubmission, RunnerRuntimeState, WS_OUTGOING_CAPACITY,
};
pub(crate) use transport::{run_runner, HttpSendConfig, RunnerSink, SubmitResultError};
pub(crate) use util::contains_any;
