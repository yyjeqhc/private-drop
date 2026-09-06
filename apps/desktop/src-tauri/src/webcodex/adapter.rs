use super::cli::{run_json, run_json_until, ResolvedBinaries};
use super::models::{
    LoginOutput, OpsProjectsOutput, PairingCreateOutput, RunnerStatusOutput, ServerStatusOutput,
};
use crate::deadline::Deadline;
use crate::error::{DesktopError, DesktopResult};
use crate::models::ProjectSelection;
use crate::operation::CancellationContext;
use crate::platform;
use std::path::{Path, PathBuf};
use std::process::Command;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRuntimeIdentity {
    pub project_id: String,
    pub runtime_project_id: String,
    pub project_path: String,
    pub runner_config: PathBuf,
    pub user_token_file: PathBuf,
    pub server_url: String,
}

pub struct WebCodexAdapter {
    binaries: Option<ResolvedBinaries>,
    bundled_runtime_dir: Option<PathBuf>,
}

impl WebCodexAdapter {
    pub fn new(bundled_runtime_dir: Option<PathBuf>) -> Self {
        Self {
            binaries: None,
            bundled_runtime_dir,
        }
    }

    pub async fn ensure_binaries(
        &mut self,
        cancellation: &CancellationContext,
    ) -> DesktopResult<&ResolvedBinaries> {
        if self.binaries.is_none() {
            self.binaries = Some(
                ResolvedBinaries::resolve(self.bundled_runtime_dir.as_deref(), cancellation)
                    .await?,
            );
        }
        Ok(self.binaries.as_ref().expect("resolved above"))
    }

    pub async fn ensure_binaries_until(
        &mut self,
        cancellation: &CancellationContext,
        deadline: Deadline,
    ) -> DesktopResult<&ResolvedBinaries> {
        if self.binaries.is_none() {
            self.binaries = Some(
                ResolvedBinaries::resolve_until(
                    self.bundled_runtime_dir.as_deref(),
                    cancellation,
                    deadline,
                )
                .await?,
            );
        }
        Ok(self.binaries.as_ref().expect("resolved above"))
    }

    pub fn binaries(&self) -> DesktopResult<&ResolvedBinaries> {
        self.binaries.as_ref().ok_or_else(|| {
            DesktopError::new(
                "binaries_not_checked",
                "WebCodex binaries have not been verified yet",
                "Refresh Desktop diagnostics or start setup.",
            )
        })
    }

    pub async fn inspect_project(&self, path: &str) -> DesktopResult<ProjectSelection> {
        inspect_project_path(path).await
    }

    pub async fn init_local_server(
        &mut self,
        listen: &str,
        data_dir: &Path,
        env_file: &Path,
        cancellation: &CancellationContext,
    ) -> DesktopResult<ServerStatusOutput> {
        let webcodex = self.ensure_binaries(cancellation).await?.webcodex.clone();
        cancellation.check()?;
        tokio::fs::create_dir_all(
            data_dir
                .parent()
                .ok_or_else(|| invalid_runtime_path(data_dir))?,
        )
        .await
        .map_err(|_| invalid_runtime_path(data_dir))?;
        #[derive(serde::Deserialize)]
        struct InitOutput {
            env_file: String,
            listen: String,
        }
        let init: InitOutput = run_json(
            &webcodex,
            &[
                "server".into(),
                "init".into(),
                "--listen".into(),
                listen.into(),
                "--data-dir".into(),
                data_dir.to_string_lossy().to_string(),
                "--env-file".into(),
                env_file.to_string_lossy().to_string(),
                "--json".into(),
            ],
            None,
            false,
            cancellation,
        )
        .await?;
        if init.env_file.trim().is_empty() || init.listen.trim().is_empty() {
            return Err(invalid_contract("server init"));
        }
        self.server_status(None, Some(env_file), None, cancellation)
            .await
    }

    pub async fn server_status(
        &mut self,
        server_url: Option<&str>,
        env_file: Option<&Path>,
        token_file: Option<&Path>,
        cancellation: &CancellationContext,
    ) -> DesktopResult<ServerStatusOutput> {
        self.server_status_with_deadline(server_url, env_file, token_file, cancellation, None)
            .await
    }

    pub async fn server_status_until(
        &mut self,
        server_url: Option<&str>,
        env_file: Option<&Path>,
        token_file: Option<&Path>,
        cancellation: &CancellationContext,
        deadline: Deadline,
    ) -> DesktopResult<ServerStatusOutput> {
        self.server_status_with_deadline(
            server_url,
            env_file,
            token_file,
            cancellation,
            Some(deadline),
        )
        .await
    }

    async fn server_status_with_deadline(
        &mut self,
        server_url: Option<&str>,
        env_file: Option<&Path>,
        token_file: Option<&Path>,
        cancellation: &CancellationContext,
        deadline: Option<Deadline>,
    ) -> DesktopResult<ServerStatusOutput> {
        let webcodex = match deadline {
            Some(deadline) => self
                .ensure_binaries_until(cancellation, deadline)
                .await?
                .webcodex
                .clone(),
            None => self.ensure_binaries(cancellation).await?.webcodex.clone(),
        };
        let mut args = vec!["server".into(), "status".into()];
        if let Some(url) = server_url {
            args.extend(["--url".into(), url.into()]);
        }
        if let Some(path) = env_file {
            args.extend(["--env-file".into(), path.to_string_lossy().to_string()]);
        }
        if let Some(path) = token_file {
            args.extend(["--token-file".into(), path.to_string_lossy().to_string()]);
        }
        args.push("--json".into());
        let output: ServerStatusOutput = match deadline {
            Some(deadline) => {
                run_json_until(&webcodex, &args, None, false, cancellation, deadline).await?
            }
            None => run_json(&webcodex, &args, None, false, cancellation).await?,
        };
        if output.probe_url.trim().is_empty() {
            return Err(invalid_contract("server status"));
        }
        if output
            .revision_check
            .as_deref()
            .is_some_and(|value| value.starts_with("warning:"))
        {
            return Err(DesktopError::new(
                "binary_version_mismatch",
                "The running Server does not match this Desktop WebCodex CLI build",
                "Stop the old Server or point Desktop at a matching Server before continuing.",
            ));
        }
        Ok(output)
    }

    pub fn local_server_command(&self, env_file: &Path) -> DesktopResult<Command> {
        let binaries = self.binaries()?;
        let mut command = Command::new(&binaries.server);
        command.arg("--stop-on-stdin-eof");
        command.env("WEBCODEX_ENV_FILE", env_file);
        remove_tunnel_credentials(&mut command);
        Ok(command)
    }

    pub fn local_runner_command(&self, config: &Path) -> DesktopResult<Command> {
        let binaries = self.binaries()?;
        let mut command = Command::new(&binaries.runner);
        command
            .arg("--config")
            .arg(config)
            .arg("--stop-on-stdin-eof");
        remove_tunnel_credentials(&mut command);
        Ok(command)
    }

    pub fn quick_share_command(
        &self,
        project: &Path,
        provider: &str,
        tunnel_proxy: Option<&str>,
    ) -> DesktopResult<Command> {
        let binaries = self.binaries()?;
        let tunnel = match provider {
            "cloudflare" | "openai" | "none" => provider,
            _ => {
                return Err(DesktopError::new(
                    "quick_share_provider_invalid",
                    "Unsupported Quick Share provider",
                    "Choose Cloudflare, OpenAI Secure Tunnel, or no external tunnel.",
                ))
            }
        };
        let mut command = Command::new(&binaries.webcodex);
        command
            .arg("share")
            .arg("--root")
            .arg(project)
            .arg("--tunnel")
            .arg(tunnel)
            .arg("--auth")
            .arg("bearer")
            .arg("--json")
            .arg("--stop-on-stdin-eof");
        if tunnel == "openai" {
            command
                .env_remove("OPENAI_ADMIN_KEY")
                .env_remove("OPENAI_API_KEY");
            configure_tunnel_proxy_environment(&mut command, tunnel_proxy);
        } else {
            remove_tunnel_credentials(&mut command);
        }
        Ok(command)
    }

    pub fn regular_tunnel_command(
        &self,
        env_file: &Path,
        tunnel_proxy: Option<&str>,
    ) -> DesktopResult<Command> {
        let binaries = self.binaries()?;
        let mut command = Command::new(&binaries.webcodex);
        command
            .arg("server")
            .arg("tunnel")
            .arg("--provider")
            .arg("openai")
            .arg("--env-file")
            .arg(env_file)
            .arg("--json")
            .arg("--stop-on-stdin-eof")
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("OPENAI_API_KEY");
        configure_tunnel_proxy_environment(&mut command, tunnel_proxy);
        Ok(command)
    }

    pub async fn create_local_pairing(
        &mut self,
        server_url: &str,
        env_file: &Path,
        cancellation: &CancellationContext,
    ) -> DesktopResult<String> {
        let webcodex = self.ensure_binaries(cancellation).await?.webcodex.clone();
        let username = platform::current_username();
        let output: PairingCreateOutput = run_json(
            &webcodex,
            &[
                "pairing".into(),
                "create".into(),
                "--server-url".into(),
                server_url.into(),
                "--env-file".into(),
                env_file.to_string_lossy().to_string(),
                "--username".into(),
                username,
                "--ttl-secs".into(),
                "600".into(),
                "--json".into(),
            ],
            None,
            true,
            cancellation,
        )
        .await?;
        if !output.pairing_code.starts_with("wc_pair_") {
            return Err(invalid_contract("pairing create"));
        }
        Ok(output.pairing_code)
    }

    pub async fn login_with_pairing(
        &mut self,
        server_url: &str,
        pairing_code: &str,
        connections_dir: &Path,
        project: &ProjectSelection,
        cancellation: &CancellationContext,
    ) -> DesktopResult<ProjectRuntimeIdentity> {
        validate_server_url(server_url)?;
        if pairing_code.trim().is_empty() {
            return Err(DesktopError::new(
                "pairing_code_invalid",
                "One-time login code is empty",
                "Enter the wc_pair_… code issued by the Server.",
            ));
        }
        let webcodex = self.ensure_binaries(cancellation).await?.webcodex.clone();
        cancellation.check()?;
        tokio::fs::create_dir_all(connections_dir)
            .await
            .map_err(|_| {
                DesktopError::new(
                    "desktop_state_unavailable",
                    "Desktop could not prepare its protected connection directory",
                    "Check local app-data permissions and retry.",
                )
            })?;
        let args = vec![
            "login".into(),
            server_url.into(),
            "--code-stdin".into(),
            "--dir".into(),
            connections_dir.to_string_lossy().to_string(),
            // Desktop owns this connection directory and may intentionally
            // re-enroll the same device when its selected/default project
            // changes. The one-shot pairing code remains the authority for the
            // replacement; --overwrite never broadens Server authority.
            "--overwrite".into(),
            "--allowed-root".into(),
            project.allowed_root.clone(),
            "--project".into(),
            project.path.clone(),
            "--json".into(),
        ];
        let output: LoginOutput = run_json(
            &webcodex,
            &args,
            Some(pairing_code.as_bytes()),
            true,
            cancellation,
        )
        .await?;
        validate_login_output(&output, project)
    }

    pub async fn runner_ready(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
    ) -> DesktopResult<bool> {
        self.runner_ready_with_deadline(identity, cancellation, None)
            .await
    }

    pub async fn runner_ready_until(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
        deadline: Deadline,
    ) -> DesktopResult<bool> {
        self.runner_ready_with_deadline(identity, cancellation, Some(deadline))
            .await
    }

    async fn runner_ready_with_deadline(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
        deadline: Option<Deadline>,
    ) -> DesktopResult<bool> {
        let webcodex = match deadline {
            Some(deadline) => self
                .ensure_binaries_until(cancellation, deadline)
                .await?
                .webcodex
                .clone(),
            None => self.ensure_binaries(cancellation).await?.webcodex.clone(),
        };
        let args = [
            "runner".into(),
            "status".into(),
            "--config".into(),
            identity.runner_config.to_string_lossy().to_string(),
            "--server-url".into(),
            identity.server_url.clone(),
            "--user-token-file".into(),
            identity.user_token_file.to_string_lossy().to_string(),
            "--json".into(),
        ];
        let output: RunnerStatusOutput = match deadline {
            Some(deadline) => {
                run_json_until(&webcodex, &args, None, false, cancellation, deadline).await?
            }
            None => run_json(&webcodex, &args, None, false, cancellation).await?,
        };
        if output.config.path.trim().is_empty()
            || output.config.client_id.trim().is_empty()
            || output.config.server_url.trim().is_empty()
        {
            return Err(invalid_contract("runner status"));
        }
        let runtime = output
            .runtime
            .unwrap_or(super::models::RunnerRuntimeOutput {
                checked: false,
                reachable: None,
                client_online: None,
            });
        Ok(runtime.checked
            && runtime.reachable == Some(true)
            && runtime.client_online == Some(true))
    }

    pub async fn project_ready(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
    ) -> DesktopResult<bool> {
        self.project_ready_with_deadline(identity, cancellation, None)
            .await
    }

    pub async fn project_ready_until(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
        deadline: Deadline,
    ) -> DesktopResult<bool> {
        self.project_ready_with_deadline(identity, cancellation, Some(deadline))
            .await
    }

    async fn project_ready_with_deadline(
        &mut self,
        identity: &ProjectRuntimeIdentity,
        cancellation: &CancellationContext,
        deadline: Option<Deadline>,
    ) -> DesktopResult<bool> {
        let webcodex = match deadline {
            Some(deadline) => self
                .ensure_binaries_until(cancellation, deadline)
                .await?
                .webcodex
                .clone(),
            None => self.ensure_binaries(cancellation).await?.webcodex.clone(),
        };
        let args = [
            "ops".into(),
            "projects".into(),
            "--server-url".into(),
            identity.server_url.clone(),
            "--token-file".into(),
            identity.user_token_file.to_string_lossy().to_string(),
            "--json".into(),
        ];
        let output: OpsProjectsOutput = match deadline {
            Some(deadline) => {
                run_json_until(&webcodex, &args, None, false, cancellation, deadline).await?
            }
            None => run_json(&webcodex, &args, None, false, cancellation).await?,
        };
        Ok(output
            .summary
            .projects
            .iter()
            .any(|candidate| ops_project_is_ready(candidate, identity)))
    }
}

pub async fn inspect_project_path(path: &str) -> DesktopResult<ProjectSelection> {
    let requested = PathBuf::from(path);
    let canonical = tokio::fs::canonicalize(&requested).await.map_err(|_| {
        DesktopError::new(
            "project_unavailable",
            "The selected project directory could not be resolved",
            "Choose an existing directory that this account can access.",
        )
    })?;
    let metadata = tokio::fs::metadata(&canonical).await.map_err(|_| {
        DesktopError::new(
            "project_unavailable",
            "The selected project directory could not be inspected",
            "Check its filesystem permissions and retry.",
        )
    })?;
    if !metadata.is_dir() {
        return Err(DesktopError::new(
            "project_not_directory",
            "The selected project is not a directory",
            "Choose a project directory.",
        ));
    }
    let allowed_root = canonical.parent().unwrap_or(&canonical).to_path_buf();
    let is_git_repository = tokio::fs::symlink_metadata(canonical.join(".git"))
        .await
        .is_ok();
    Ok(ProjectSelection {
        path: display_path(&canonical),
        allowed_root: display_path(&allowed_root),
        is_git_repository,
        runtime_project_id: None,
    })
}

fn ops_project_is_ready(
    candidate: &super::models::OpsProject,
    identity: &ProjectRuntimeIdentity,
) -> bool {
    candidate.id == identity.runtime_project_id
        && same_path(&candidate.path, &identity.project_path)
        && candidate.connected == Some(true)
        && candidate.agent_status.as_deref() == Some("online")
}

fn validate_login_output(
    output: &LoginOutput,
    project: &ProjectSelection,
) -> DesktopResult<ProjectRuntimeIdentity> {
    if output.server_url.trim().is_empty()
        || output.runner_config.trim().is_empty()
        || output.user_token_file.trim().is_empty()
    {
        return Err(invalid_contract("login"));
    }
    let registered = output
        .registered_projects
        .iter()
        .find(|candidate| same_path(&candidate.path, &project.path))
        .ok_or_else(|| invalid_contract("login project registration"))?;
    if registered.id.trim().is_empty() || registered.runtime_project.trim().is_empty() {
        return Err(invalid_contract("login project identity"));
    }
    Ok(ProjectRuntimeIdentity {
        project_id: registered.id.clone(),
        runtime_project_id: registered.runtime_project.clone(),
        project_path: registered.path.clone(),
        runner_config: PathBuf::from(&output.runner_config),
        user_token_file: PathBuf::from(&output.user_token_file),
        server_url: output.server_url.clone(),
    })
}

pub fn validate_server_url(value: &str) -> DesktopResult<String> {
    let value = value.trim().trim_end_matches('/');
    let parsed = Url::parse(value).map_err(|_| {
        DesktopError::new(
            "server_url_invalid",
            "Server URL is not a valid absolute URL",
            "Enter a WebCodex Server URL such as https://webcodex.example.com.",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(DesktopError::new(
            "server_url_invalid",
            "Server URL must be a plain http(s) origin without credentials, path, query, or fragment",
            "Enter only the WebCodex Server origin.",
        ));
    }
    Ok(value.to_string())
}

fn configure_tunnel_proxy_environment(command: &mut Command, proxy: Option<&str>) {
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    if let Some(proxy) = proxy {
        command
            .env("HTTP_PROXY", proxy)
            .env("HTTPS_PROXY", proxy)
            .env("http_proxy", proxy)
            .env("https_proxy", proxy)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1");
    }
}

fn remove_tunnel_credentials(command: &mut Command) {
    for name in [
        "CONTROL_PLANE_API_KEY",
        "CONTROL_PLANE_TUNNEL_ID",
        "OPENAI_ADMIN_KEY",
        "OPENAI_API_KEY",
    ] {
        command.env_remove(name);
    }
}

fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        display_path(Path::new(left)).eq_ignore_ascii_case(&display_path(Path::new(right)))
    } else {
        left == right
    }
}

fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    value.into_owned()
}

fn invalid_contract(operation: &str) -> DesktopError {
    DesktopError::new(
        "webcodex_contract_invalid",
        format!("WebCodex returned an incomplete {operation} identity"),
        "Verify that Desktop and the WebCodex binaries come from the same source baseline.",
    )
}

fn invalid_runtime_path(path: &Path) -> DesktopError {
    DesktopError::new(
        "desktop_state_unavailable",
        format!("Desktop cannot prepare {}", path.display()),
        "Check local app-data permissions and retry.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_validation_rejects_credential_and_path_smuggling() {
        assert!(validate_server_url("https://example.com").is_ok());
        assert!(validate_server_url("https://user:pass@example.com").is_err());
        assert!(validate_server_url("https://example.com/admin").is_err());
        assert!(validate_server_url("file:///tmp/server").is_err());
    }

    #[test]
    fn quick_share_only_inherits_control_plane_credentials_for_openai_provider() {
        let binaries = ResolvedBinaries {
            directory: PathBuf::from("bin"),
            webcodex: PathBuf::from("webcodex"),
            server: PathBuf::from("webcodex-server"),
            runner: PathBuf::from("webcodex-runner"),
            version: "0.3.9".to_string(),
            git_commit: "0123456789abcdef".to_string(),
            source: super::super::cli::ResolvedBinarySource::Environment,
        };
        let adapter = WebCodexAdapter {
            binaries: Some(binaries),
            bundled_runtime_dir: None,
        };
        let local = adapter
            .quick_share_command(Path::new("repo"), "none", None)
            .unwrap();
        let local_env: Vec<_> = local.get_envs().collect();
        for key in [
            "CONTROL_PLANE_API_KEY",
            "CONTROL_PLANE_TUNNEL_ID",
            "OPENAI_ADMIN_KEY",
            "OPENAI_API_KEY",
        ] {
            assert!(local_env
                .iter()
                .any(|(name, value)| name.to_str() == Some(key) && value.is_none()));
        }

        let openai = adapter
            .quick_share_command(Path::new("repo"), "openai", Some("http://127.0.0.1:7890"))
            .unwrap();
        let openai_env: Vec<_> = openai.get_envs().collect();
        for key in ["OPENAI_ADMIN_KEY", "OPENAI_API_KEY"] {
            assert!(openai_env
                .iter()
                .any(|(name, value)| name.to_str() == Some(key) && value.is_none()));
        }
        for key in ["CONTROL_PLANE_API_KEY", "CONTROL_PLANE_TUNNEL_ID"] {
            assert!(!openai_env
                .iter()
                .any(|(name, _)| name.to_str() == Some(key)));
        }
        for key in ["http_proxy", "https_proxy"] {
            assert!(openai_env.iter().any(|(name, value)| {
                name.to_string_lossy().eq_ignore_ascii_case(key)
                    && value.and_then(|value| value.to_str()) == Some("http://127.0.0.1:7890")
            }));
        }
        assert!(openai_env.iter().any(|(name, value)| {
            name.to_str() == Some("NO_PROXY")
                && value.and_then(|value| value.to_str()) == Some("127.0.0.1,localhost,::1")
        }));
    }

    #[test]
    fn regular_tunnel_uses_local_server_bootstrap_auth_and_only_inherits_control_plane_credentials() {
        let binaries = ResolvedBinaries {
            directory: PathBuf::from("bin"),
            webcodex: PathBuf::from("webcodex"),
            server: PathBuf::from("webcodex-server"),
            runner: PathBuf::from("webcodex-runner"),
            version: "0.3.9".to_string(),
            git_commit: "0123456789abcdef".to_string(),
            source: super::super::cli::ResolvedBinarySource::Environment,
        };
        let adapter = WebCodexAdapter {
            binaries: Some(binaries),
            bundled_runtime_dir: None,
        };
        let command = adapter
            .regular_tunnel_command(
                Path::new("server.env"),
                Some("http://127.0.0.1:7890"),
            )
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|value| value.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "server",
                "tunnel",
                "--provider",
                "openai",
                "--env-file",
                "server.env",
                "--json",
                "--stop-on-stdin-eof",
            ]
        );
        let env: Vec<_> = command.get_envs().collect();
        for key in ["OPENAI_ADMIN_KEY", "OPENAI_API_KEY"] {
            assert!(env
                .iter()
                .any(|(name, value)| name.to_str() == Some(key) && value.is_none()));
        }
        for key in ["CONTROL_PLANE_API_KEY", "CONTROL_PLANE_TUNNEL_ID"] {
            assert!(!env.iter().any(|(name, _)| name.to_str() == Some(key)));
        }
        assert!(env.iter().any(|(name, value)| {
            name.to_str() == Some("HTTPS_PROXY")
                && value.and_then(|value| value.to_str()) == Some("http://127.0.0.1:7890")
        }));
    }

    #[test]
    fn ops_project_readiness_uses_runtime_project_identity() {
        let identity = ProjectRuntimeIdentity {
            project_id: "repo".to_string(),
            runtime_project_id: "agent:desktop:repo".to_string(),
            project_path: r"C:\repo".to_string(),
            runner_config: PathBuf::from("runner.toml"),
            user_token_file: PathBuf::from("user-token"),
            server_url: "https://example.test".to_string(),
        };
        let ready = super::super::models::OpsProject {
            id: "agent:desktop:repo".to_string(),
            path: r"C:\repo".to_string(),
            connected: Some(true),
            agent_status: Some("online".to_string()),
        };
        assert!(ops_project_is_ready(&ready, &identity));

        let short_id = super::super::models::OpsProject {
            id: "repo".to_string(),
            ..ready
        };
        assert!(!ops_project_is_ready(&short_id, &identity));
    }

    #[test]
    fn login_identity_fails_closed_when_registered_project_is_missing() {
        let output = LoginOutput {
            server_url: "https://example.com".to_string(),
            runner_config: "runner.toml".to_string(),
            user_token_file: "user-token".to_string(),
            registered_projects: Vec::new(),
        };
        let project = ProjectSelection {
            path: "C:\\repo".to_string(),
            allowed_root: "C:\\".to_string(),
            is_git_repository: true,
            runtime_project_id: None,
        };
        assert_eq!(
            validate_login_output(&output, &project).unwrap_err().code,
            "webcodex_contract_invalid"
        );
    }

    #[test]
    fn windows_extended_and_display_paths_match_the_same_project() {
        if cfg!(windows) {
            assert!(same_path(
                r"\\?\C:\Users\example\repo",
                r"C:\Users\example\repo"
            ));
        }
    }

    #[test]
    fn windows_extended_paths_are_presented_as_normal_user_paths() {
        if cfg!(windows) {
            assert_eq!(
                display_path(Path::new(r"\\?\C:\Users\example\repo")),
                r"C:\Users\example\repo"
            );
            assert_eq!(
                display_path(Path::new(r"\\?\UNC\server\share\repo")),
                r"\\server\share\repo"
            );
        }
    }
}
