//! Shared Runner-config initialization logic used by the server,
//! `webcodex-runner`, and `webcodex-cli`.
//!
//! It owns the `RunnerInitOptions` type, validation, token resolution, TOML
//! generation, and atomic 0600 file writing. Each binary keeps its own small
//! flag parser and help text.
//!
//! Default policy: when `allowed_roots` is not explicitly configured, it
//! defaults to `$HOME` (see `effective_allowed_roots`).

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use webcodex_core::runner_protocol::RunnerCapabilities;

pub mod paths;

/// Default Runner project registry selected for a new system-level install.
pub const DEFAULT_INIT_PROJECT_REGISTRY_DIR: &str = "/etc/webcodex/project-registry";
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
/// Largest idle polling floor allowed when polling can be selected. The Server
/// currently considers a Runner offline after 60 seconds without a keepalive;
/// 30 seconds leaves one full interval of scheduling/network slack.
pub const MAX_POLL_INTERVAL_MS: u64 = 30_000;
pub const DEFAULT_MAX_TIMEOUT_SECS: u64 = 3600;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Config value selecting the polling transport (HTTP `/api/shell/agent/poll`).
pub const TRANSPORT_POLLING: &str = "polling";
/// Config value selecting the WebSocket transport.
pub const TRANSPORT_WEBSOCKET: &str = "websocket";
/// Config value selecting the supported custom QUIC stream transport.
/// Requires a `[quic]` section in the Runner config with `server_addr` / `server_name`.
pub const TRANSPORT_QUIC: &str = "quic";
/// Recommended fallback mode for new deployments: try QUIC when `[quic]` is
/// configured, then WebSocket, then polling.
pub const TRANSPORT_AUTO: &str = "auto";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInitOptions {
    pub server_url: String,
    pub token: Option<String>,
    pub token_file: Option<PathBuf>,
    pub client_id: String,
    pub owner: String,
    pub display_name: Option<String>,
    pub transport: String,
    pub poll_interval_ms: u64,
    pub project_registry_dir: PathBuf,
    pub output: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub allow_cwd_anywhere: bool,
    pub overwrite: bool,
}

/// Return the per-user home as an allowed-root candidate, when determinable.
/// Windows: `USERPROFILE`; Unix: `HOME`. See [`paths::home_dir`].
pub fn home_allowed_root() -> Option<PathBuf> {
    paths::home_dir().filter(|p| !p.as_os_str().is_empty())
}

/// Resolve the effective `allowed_roots` for a Runner policy.
///
/// - If `configured` is non-empty, the configured value is used as-is
///   (overrides the HOME default).
/// - If `configured` is empty, default to `[$HOME]` when `HOME` is available
///   (regardless of `allow_cwd_anywhere` — adding HOME as an explicit root is
///   harmless when cwd-anywhere is also enabled, and makes the policy summary
///   + create/register validation predictable).
/// - If `configured` is empty, `HOME` is unavailable, and `allow_cwd_anywhere`
///   is true, return an empty vector (the cwd-anywhere policy still governs
///   access; sensitive path protections apply at validation time).
/// - If `configured` is empty, `HOME` is unavailable, and
///   `allow_cwd_anywhere` is false, surface a clear configuration error.
pub fn effective_allowed_roots(
    configured: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<Vec<PathBuf>, String> {
    if !configured.is_empty() {
        return Ok(configured.to_vec());
    }
    if let Some(home) = home_allowed_root() {
        return Ok(vec![home]);
    }
    if allow_cwd_anywhere {
        return Ok(Vec::new());
    }
    Err(
        "allowed_roots is empty and HOME is not set; set allowed_roots explicitly, \
         set --allow-cwd-anywhere true, or set the HOME environment variable"
            .to_string(),
    )
}

pub fn validate_runner_init_options(opts: &RunnerInitOptions) -> Result<(), String> {
    if opts.server_url.trim().is_empty() {
        return Err("--server-url is required".to_string());
    }
    if opts.token.is_some() && opts.token_file.is_some() {
        return Err("use only one of --token or --token-file".to_string());
    }
    if opts.client_id.trim().is_empty() {
        return Err("--client-id is required".to_string());
    }
    if opts.owner.trim().is_empty() {
        return Err("--owner is required".to_string());
    }
    if opts.poll_interval_ms == 0 {
        return Err("--poll-interval-ms must be > 0".to_string());
    }
    if !matches!(
        opts.transport.as_str(),
        TRANSPORT_WEBSOCKET | TRANSPORT_POLLING | TRANSPORT_QUIC | TRANSPORT_AUTO
    ) {
        return Err("--transport must be websocket, polling, quic, or auto".to_string());
    }
    if matches!(opts.transport.as_str(), TRANSPORT_POLLING | TRANSPORT_AUTO)
        && opts.poll_interval_ms > MAX_POLL_INTERVAL_MS
    {
        return Err(format!(
            "--poll-interval-ms must be <= {MAX_POLL_INTERVAL_MS} when polling may be used"
        ));
    }
    if opts.project_registry_dir.as_os_str().is_empty() {
        return Err("--project-registry-dir cannot be empty".to_string());
    }
    if opts.output.as_os_str().is_empty() {
        return Err("--output is required".to_string());
    }
    // Empty allowed_roots no longer errors here; the HOME default
    // is resolved at generation time via `effective_allowed_roots`. We still
    // reject empty path entries so an explicit `--allowed-root ""` is caught.
    if opts
        .allowed_roots
        .iter()
        .any(|path| path.as_os_str().is_empty())
    {
        return Err("--allowed-root cannot be empty".to_string());
    }
    Ok(())
}

pub fn resolve_runner_init_token(opts: &RunnerInitOptions) -> Result<String, String> {
    if let Some(token) = &opts.token {
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err("--token cannot be empty".to_string());
        }
        return Ok(token);
    }
    if let Some(path) = &opts.token_file {
        let token = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read token file {}: {}", path.display(), e))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err("--token-file cannot be empty".to_string());
        }
        return Ok(token);
    }
    let token = std::env::var("WEBCODEX_AGENT_TOKEN")
        .map_err(|_| "--token, --token-file, or WEBCODEX_AGENT_TOKEN is required".to_string())?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("WEBCODEX_AGENT_TOKEN cannot be empty".to_string());
    }
    Ok(token)
}

#[derive(Debug, Serialize)]
struct GeneratedRunnerConfig {
    server_url: String,
    token: String,
    client_id: String,
    display_name: Option<String>,
    owner: String,
    transport: String,
    poll_interval_ms: u64,
    project_registry_dir: PathBuf,
    capabilities: RunnerCapabilities,
    policy: GeneratedRunnerPolicy,
}

#[derive(Debug, Serialize)]
struct GeneratedRunnerPolicy {
    allow_raw_shell: bool,
    allow_cwd_anywhere: bool,
    allowed_roots: Vec<PathBuf>,
    max_timeout_secs: u64,
    max_output_bytes: usize,
}

pub fn generated_runner_config_toml(opts: &RunnerInitOptions) -> Result<String, String> {
    let effective_roots = effective_allowed_roots(&opts.allowed_roots, opts.allow_cwd_anywhere)?;
    let cfg = GeneratedRunnerConfig {
        server_url: opts.server_url.trim_end_matches('/').to_string(),
        token: resolve_runner_init_token(opts)?,
        client_id: opts.client_id.clone(),
        display_name: opts.display_name.clone(),
        owner: opts.owner.clone(),
        transport: opts.transport.clone(),
        poll_interval_ms: opts.poll_interval_ms,
        project_registry_dir: opts.project_registry_dir.clone(),
        capabilities: RunnerCapabilities {
            shell: true,
            file_read: true,
            file_write: true,
            // The running binary advertises the internal optimized export read
            // only after installing that request handler.
            artifact_export_chunk_read: false,
            // Large export metadata is likewise a running-binary capability;
            // generated config is not authoritative for registration semantics.
            artifact_export_streaming_metadata: false,
            // The running binary advertises structured deletion only after its
            // complete project-root-enforced handler is installed.
            structured_file_delete: false,
            // Exact occurrence selectors are a running-binary capability; the
            // running Runner advertises the canonical registration baseline.
            apply_text_edit_occurrence: false,
            // Scoped exact matching is likewise runtime-only and must not be
            // inferred from generated config or occurrence support.
            apply_text_edit_line_scope: false,
            // Codex Patch is a running-binary request kind and must not be
            // inferred from generated config or generic file-write support.
            apply_patch: false,
            // The current patch success metadata contract is runtime-only and
            // must be advertised by the binary that actually implements it.
            apply_patch_match_metadata: false,
            // Enum-based matching semantics are runtime-only and current
            // Servers require an explicit registration capability.
            apply_patch_matching_mode: false,
            // Strict patch matching is also runtime-only and must be explicitly
            // advertised for rolling compatibility with older Servers.
            apply_patch_strict_matching: false,
            git: true,
            jobs: true,
            async_jobs: true,
            async_shell_jobs: true,
            // The Runner performs the executable probe at registration time.
            // A generated static file never claims SSH support on its own.
            ssh_shell: false,
            // The running binary advertises process-lifetime capabilities
            // after it has installed the persistent-shell manager.
            persistent_shell: false,
            ssh_persistent_shell: false,
            structured_validation_argv: true,
            // Durable assertion metadata is advertised by the running binary,
            // not inferred from generic validation argv support.
            structured_cargo_test_count_assertion: false,
            // Explicit Cargo execution policy is a separate running-binary
            // rolling-upgrade capability and is never inferred from the older
            // count-assertion bit or protocol generation.
            structured_cargo_test_execution_policy: false,
            // The running binary advertises this process-lifetime protocol
            // capability after installing its exact Go argv boundary.
            structured_go_test_json: false,
            // Like JSON parsing, first-class durable go_test support is
            // advertised by the running binary, never by generated static config.
            structured_go_test_tool: false,
            // Focused Go package argv is likewise a running-binary capability;
            // older generated configs must never claim it implicitly.
            structured_go_test_packages: false,
            structured_process_argv: true,
            structured_script_payload: true,
            // Internal generated-program execution is a running-binary
            // capability and must fail closed across mixed-version rollout.
            internal_posix_script: false,
            structured_execution_jobs: true,
            // Detached process authority is advertised only by a running binary
            // after its durable ownership-transfer backend is installed.
            detached_process_jobs: false,
            lsp_read_only_navigation: true,
            lsp_call_hierarchy: true,
            project_lifecycle: false,
            project_path_registration: false,
            managed_worktree: false,
            // Runner-global Skill store support is runtime-only and never
            // inferred from project/file capabilities in generated config.
            // Configured live Skill roots are Runner-owned runtime config and
            // require an explicit running-binary capability.
            configured_skill_roots_read: false,
            skill_store_read: false,
            skill_store_manage: false,
            // Desktop observation is a runtime/platform capability and is never
            // claimed by generated static config.
            computer_observe: false,
            // Application discovery/launch are native runtime capabilities;
            // generated config never implies either from other Computer support.
            computer_application_discovery: false,
            computer_application_launch: false,
            // Full-display observation is a separate privacy/runtime capability.
            computer_display_observe: false,
            // Coordinate pointer control is independently advertised only by a running native implementation.
            computer_pointer_control: false,
            // Clipboard read/write are independent native runtime capabilities.
            computer_clipboard_read: false,
            computer_clipboard_write: false,
            // Region/downscale snapshot support is advertised only by the running binary.
            computer_snapshot_region: false,
            // Semantic accessibility inspection is also a runtime/platform
            // capability and remains distinct from screenshot observation.
            computer_accessibility_observe: false,
            // The running binary advertises normalized element-state support
            // only after installing the native handler.
            computer_element_state: false,
            // Control authority is runtime/platform-specific and is never
            // implied by either observation capability in generated config.
            computer_control: false,
            // Semantic scroll-to-element is an independently advertised native
            // capability and is never inferred from computer_control.
            computer_scroll_to_element: false,
            // Closed key input is independently advertised only by a running native
            // implementation and is never inferred from computer_control.
            computer_key_input: false,
            // Exact window activation is independently advertised only by the
            // running binary after the native handler is installed.
            computer_window_activate: false,
            // Text input is another runtime/platform capability and must never
            // be inferred from computer_control in generated static config.
            computer_text_input: false,
            job_state_reconciliation: false,
            // ACP autonomous coding is a runtime-only capability and must not be
            // silently enabled by generated legacy agent config.
            coding_agent_runs: false,
            // Native Tool Plugins are likewise advertised only by a Runner
            // binary that implements the typed local Plugin lifecycle.
            native_tool_plugins: false,
            // Managed SSH resource lifecycle is likewise a running-binary
            // capability and is never implied by generated static SSH config.
            managed_ssh_resources: false,
            // First-class config control is implemented by the running binary
            // against its startup-bound path and must never be inferred from a
            // generated static runner.toml capability block.
            runner_config_control: false,
        },
        policy: GeneratedRunnerPolicy {
            allow_raw_shell: true,
            allow_cwd_anywhere: opts.allow_cwd_anywhere,
            allowed_roots: effective_roots,
            max_timeout_secs: DEFAULT_MAX_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        },
    };
    toml::to_string_pretty(&cfg).map_err(|e| format!("failed to serialize Runner config: {}", e))
}

/// Generate the Runner config TOML and write it to `opts.output`.
///
/// - `--output -` writes the TOML to stdout (returned as a string). The caller
///   is responsible for printing it; this is the only path that returns the
///   token-bearing content, and only when the user explicitly asks for stdout.
/// - Refuses to overwrite an existing file unless `--overwrite` is set.
/// - Writes 0600 permissions on Unix.
pub fn run_runner_init(opts: RunnerInitOptions) -> Result<String, String> {
    let content = generated_runner_config_toml(&opts)?;
    if opts.output.as_os_str() == "-" {
        return Ok(content);
    }
    if opts.output.exists() && !opts.overwrite {
        return Err(format!(
            "{} already exists; pass --overwrite to replace it",
            opts.output.display()
        ));
    }
    if let Some(parent) = opts.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
    }
    let mut open = OpenOptions::new();
    open.write(true);
    if opts.overwrite {
        open.create(true).truncate(true);
    } else {
        open.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open
        .open(&opts.output)
        .map_err(|e| format!("failed to write {}: {}", opts.output.display(), e))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write {}: {}", opts.output.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&opts.output, std::fs::Permissions::from_mode(0o600)).map_err(
            |e| {
                format!(
                    "failed to set permissions on {}: {}",
                    opts.output.display(),
                    e
                )
            },
        )?;
    }
    Ok(format!("wrote {}\n", opts.output.display()))
}

/// Mutex serializing tests that mutate process-wide environment variables.
/// It stays exported because integration tests in each consuming package share
/// the same dependency instance.
pub static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err("boolean value must be true or false".to_string()),
    }
}

pub fn required_value<'a, I>(iter: &mut I, flag: &str) -> Result<String, String>
where
    I: Iterator<Item = &'a String>,
{
    iter.next()
        .cloned()
        .ok_or_else(|| format!("{} requires a value", flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII restore for environment variables: restores the previous value
    /// (or removes the variable) on drop, even if the test panics.
    struct EnvVarRestore {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn remove(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            std::env::remove_var(name);
            EnvVarRestore { name, previous }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn init_opts(output: PathBuf) -> RunnerInitOptions {
        RunnerInitOptions {
            server_url: "https://v4.example.test/".to_string(),
            token: Some("wc_agent_fake_test_token".to_string()),
            token_file: None,
            client_id: "alice-laptop".to_string(),
            owner: "alice".to_string(),
            display_name: Some("Alice Laptop".to_string()),
            transport: TRANSPORT_WEBSOCKET.to_string(),
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            project_registry_dir: PathBuf::from("/etc/webcodex/project-registry"),
            output,
            allowed_roots: vec![PathBuf::from("/srv/projects")],
            allow_cwd_anywhere: false,
            overwrite: false,
        }
    }

    #[test]
    fn effective_allowed_roots_uses_explicit_when_present() {
        let roots = effective_allowed_roots(&[PathBuf::from("/root/git")], false).unwrap();
        assert_eq!(roots, vec![PathBuf::from("/root/git")]);
    }

    #[test]
    fn effective_allowed_roots_defaults_to_home_when_empty() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = home_allowed_root();
        if let Some(home) = home {
            let roots = effective_allowed_roots(&[], false).unwrap();
            assert_eq!(roots, vec![home]);
        }
    }

    #[test]
    fn effective_allowed_roots_errors_when_empty_and_no_home_and_no_cwd_anywhere() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let err = effective_allowed_roots(&[], false).unwrap_err();
        assert!(err.contains("allowed_roots is empty"));
    }

    #[test]
    fn effective_allowed_roots_empty_with_cwd_anywhere_returns_empty() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let roots = effective_allowed_roots(&[], true).unwrap();
        assert!(roots.is_empty());
    }

    #[test]
    fn generated_config_uses_home_default_when_roots_empty() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = home_allowed_root();
        if let Some(home) = home {
            let mut opts = init_opts(PathBuf::from("-"));
            opts.allowed_roots.clear();
            let content = generated_runner_config_toml(&opts).unwrap();
            // Parse the output back rather than matching the serializer's
            // quoting: Windows paths contain backslashes and the TOML encoder
            // may pick basic- or literal-string forms.
            let parsed: toml::Value = toml::from_str(&content).unwrap();
            let rendered_roots = parsed["policy"]["allowed_roots"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            assert_eq!(rendered_roots, vec![home.to_string_lossy().to_string()]);
            assert!(!content.contains("structured_go_test_json"));
            assert!(!content.contains("structured_go_test_tool"));
            assert!(!content.contains("job_state_reconciliation"));
        }
    }

    #[test]
    fn polling_capable_init_rejects_interval_beyond_online_window_slack() {
        for transport in [TRANSPORT_POLLING, TRANSPORT_AUTO] {
            let mut opts = init_opts(PathBuf::from("-"));
            opts.transport = transport.to_string();
            opts.poll_interval_ms = MAX_POLL_INTERVAL_MS + 1;
            let error = validate_runner_init_options(&opts).unwrap_err();
            assert!(error.contains("must be <= 30000"), "{transport}: {error}");
            opts.poll_interval_ms = MAX_POLL_INTERVAL_MS;
            validate_runner_init_options(&opts).unwrap();
        }

        let mut websocket = init_opts(PathBuf::from("-"));
        websocket.poll_interval_ms = MAX_POLL_INTERVAL_MS + 1;
        validate_runner_init_options(&websocket).unwrap();
    }
}
