pub(crate) mod connect;
pub(crate) mod connections;
pub(crate) mod env;
pub(crate) mod http;
pub(crate) mod login;
pub(crate) mod ops;
pub(crate) mod output;
pub(crate) mod pairing;
pub(crate) mod profiles;
pub(crate) mod project;
pub(crate) mod runner_service;
pub(crate) mod server;
pub(crate) mod service;
pub(crate) mod system;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod token_commands;
pub(crate) mod tokens;
pub(crate) mod usage;

/// Quote one argument for display as a copyable POSIX shell command.
///
/// Keep common inert characters readable and single-quote everything else.
/// Embedded single quotes use the standard close/escape/reopen form. This is
/// display-only: actual child processes continue to receive argv directly.
pub(crate) fn shell_quote_arg(value: &str) -> String {
    let safe = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        });
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub(crate) fn shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

// Only the Unix systemd service tests consume this re-export.
pub(crate) use connect::{
    local_runner_profile_marker, local_runner_state_summary, run_connect, run_disconnect,
    run_hosted_log_writer, run_local_runner_logs, run_local_runner_service, write_connect_result,
    ConnectAuth, ConnectOptions, DisconnectOptions, LocalRunnerServiceAction,
};
pub(crate) use env::is_effective_root;
#[cfg(test)]
pub(crate) use env::parse_env_content_value;
pub(crate) use env::{
    default_server_paths, read_env_file_value, read_pairing_server_env_file_value,
    render_server_env,
};
#[cfg(test)]
pub(crate) use http::format_error_body;
#[cfg(test)]
pub(crate) use http::post_json_unauthed;
pub(crate) use http::{fetch_runtime_status, http_post_json_status, post_json_authed, ApiCall};
pub(crate) use login::{
    base_dir_or_default, default_device_name, run_login, run_logout, run_status, LoginOptions,
    LogoutOptions, StatusOptions,
};
pub(crate) use ops::{
    run_ops_command, OpsCommand, OpsCommonOptions, OpsRunnerOptions, OpsSmokePreflightOptions,
    OpsWindowsOptions,
};
#[cfg(test)]
pub(crate) use output::RevisionComparison;
pub(crate) use output::{
    compare_build_commits, local_cli_build_metadata, render_build_metadata_block,
    runtime_build_metadata, server_status_revision_check,
};
pub(crate) use pairing::run_pairing_create;
#[cfg(test)]
pub(crate) use profiles::{client_output_dir_for_profile, CLIENT_PROFILE_ERROR};
pub(crate) use profiles::{
    client_profile_project_registry_dir, client_profile_runner_config,
    client_profile_runner_token_file, client_profile_runner_token_file_for_scope,
    client_profile_state_dir, client_profile_user_token_file,
    client_profile_user_token_file_for_scope, current_user_home, runner_config_for_scope,
    runner_service_file_for_scope, validate_client_profile, validate_service_file_scope,
};
pub(crate) use project::{
    register_existing_project, run_project_activate, run_project_register, ProjectActivateOptions,
    ProjectRegisterOptions, ProjectRegistration,
};
#[cfg(all(test, unix))]
pub(crate) use runner_service::render_runner_systemd_unit;
pub(crate) use runner_service::{
    run_runner_install_service, run_runner_service, run_runner_status,
};
pub(crate) use server::{
    run_server_init, run_server_install_service, run_server_service, run_server_status,
    run_server_tunnel, ServerStatusOptions,
};
pub(crate) use service::{
    control_server_unit_pair, control_service_for_scope, encode_exec_argument,
    encode_exec_path_argument, encode_exec_program, encode_unit_path_value,
    ensure_service_file_parent, install_server_unit_pair, install_unit_for_scope,
    query_systemd_service_status, query_systemd_service_status_for_scope,
    query_systemd_socket_status, run_internal_binary, run_logs, run_logs_for_scope,
    service_unit_name, uninstall_server_unit_pair, uninstall_unit_for_scope,
    validate_systemd_identity, ServiceControl, DEFAULT_LOG_LINES, RUNNER_SERVICE_UNIT,
    SERVER_SERVICE_FILE, SERVER_SERVICE_UNIT, SERVER_SOCKET_UNIT,
};
pub(crate) use system::{
    discover_internal_binary, read_optional_token, read_optional_user_api_token,
    system_group_exists, system_user_exists, system_user_home, system_user_is_root,
    validate_user_api_token,
};
#[cfg(test)]
pub(crate) use token_commands::resolve_account_credential;
pub(crate) use token_commands::{run_runner_token_create_local, run_token_create_local};
pub(crate) use tokens::{
    generate_bootstrap_token, generate_local_api_token, generate_local_runner_token,
    hash_local_token, local_token_prefix, render_token_generate, token_prefix,
};
pub(crate) use usage::{
    connect_usage, disconnect_usage, login_usage, logout_usage, ops_projects_usage,
    ops_runner_usage, ops_runners_usage, ops_smoke_preflight_usage, ops_status_usage, ops_usage,
    ops_windows_usage, pairing_create_usage, pairing_usage, project_activate_usage,
    project_register_usage, runner_init_usage, runner_install_service_usage, runner_status_usage,
    runner_usage, server_init_usage, server_install_service_usage, server_status_usage,
    server_tunnel_usage, server_usage, status_usage, usage,
};

#[cfg(test)]
mod shell_command_tests {
    use super::shell_quote_arg;

    #[test]
    fn shell_quote_arg_handles_shell_metacharacters() {
        assert_eq!(shell_quote_arg("webcodex-runner"), "webcodex-runner");
        assert_eq!(shell_quote_arg("/tmp/runner.toml"), "/tmp/runner.toml");
        assert_eq!(shell_quote_arg(""), "''");
        assert_eq!(shell_quote_arg("path with spaces"), "'path with spaces'");
        assert_eq!(shell_quote_arg("it's"), "'it'\\''s'");
        assert_eq!(shell_quote_arg("$HOME"), "'$HOME'");
        assert_eq!(shell_quote_arg("`id`"), "'`id`'");
        assert_eq!(shell_quote_arg("one;two"), "'one;two'");
    }
}
