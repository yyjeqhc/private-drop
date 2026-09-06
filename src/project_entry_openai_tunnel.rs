use super::{
    cloudflared_service, executable_name, remove_npm_wrapper_network_environment, ProductError,
};
use reqwest::header::USER_AGENT;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Read;
#[cfg(any(not(windows), test))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const TUNNEL_CLIENT_VERSION: &str = "0.0.12";
const TUNNEL_CLIENT_RELEASE_BASE: &str =
    "https://github.com/openai/tunnel-client/releases/download/v0.0.12";
const TUNNEL_CLIENT_MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;
const TUNNEL_CLIENT_MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const TUNNEL_CLIENT_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const TUNNEL_CLIENT_DOCTOR_TIMEOUT: Duration = Duration::from_secs(30);
const TUNNEL_CLIENT_CONTROL_PLANE_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const TUNNEL_CLIENT_READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const TUNNEL_CLIENT_HEALTH_URL_BYTES: usize = 512;
const TUNNEL_CLIENT_OVERRIDE: &str = "WEBCODEX_TUNNEL_CLIENT_BIN";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TunnelClientAsset {
    target: &'static str,
    file_name: &'static str,
    archive_sha256: &'static str,
    binary_sha256: &'static str,
    member_name: &'static str,
}

#[derive(Debug)]
pub(super) struct OpenAiTunnelPrerequisites {
    pub(super) binary: PathBuf,
    pub(super) tunnel_id: String,
}

#[derive(Debug)]
pub(super) struct OpenAiTunnel {
    child: Child,
}

impl OpenAiTunnel {
    pub(super) async fn wait_for_exit(&mut self) -> Result<(), ProductError> {
        let status =
            self.child.wait().await.map_err(|_| {
                tunnel_runtime_error("OpenAI tunnel-client could not be supervised")
            })?;
        Err(ProductError::new(
            "tunnel_unavailable",
            format!("OpenAI Secure MCP Tunnel stopped unexpectedly ({status})"),
            Some("Check the OpenAI tunnel-client and network connectivity, then retry webcodex share --tunnel openai."),
        ))
    }

    pub(super) async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

pub(super) async fn prepare_openai_tunnel() -> Result<OpenAiTunnelPrerequisites, ProductError> {
    let tunnel_id = required_tunnel_id()?;
    require_runtime_api_key()?;
    let binary = resolve_tunnel_client().await?;
    Ok(OpenAiTunnelPrerequisites { binary, tunnel_id })
}

pub(super) async fn start_openai_tunnel(
    prerequisites: &OpenAiTunnelPrerequisites,
    mcp_url: &str,
    authorization_file: &Path,
    session_dir: &Path,
    deadline: Instant,
) -> Result<OpenAiTunnel, ProductError> {
    run_doctor(prerequisites, mcp_url, authorization_file, deadline).await?;
    run_control_plane_probe(prerequisites, deadline).await?;

    let health_url_file = session_dir.join("openai-tunnel-health-url");
    let log_file = session_dir.join("openai-tunnel.log");
    let mut command = Command::new(&prerequisites.binary);
    suppress_windows_console(&mut command);
    command.arg("run");
    configure_runtime_command(&mut command, prerequisites, mcp_url, authorization_file);
    command
        .arg("--health.listen-addr")
        .arg("127.0.0.1:0")
        .arg("--health.url-file")
        .arg(&health_url_file)
        .arg("--log.file")
        .arg(&log_file)
        .arg("--log.format")
        .arg("json")
        .arg("--log.level")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client could not start"))?;

    if let Err(error) = wait_until_ready(&mut child, &health_url_file, deadline).await {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(error);
    }
    Ok(OpenAiTunnel { child })
}

fn configure_runtime_command(
    command: &mut Command,
    prerequisites: &OpenAiTunnelPrerequisites,
    mcp_url: &str,
    authorization_file: &Path,
) {
    remove_npm_wrapper_network_environment(command);
    command
        .env("CONTROL_PLANE_TUNNEL_ID", &prerequisites.tunnel_id)
        // The runtime key remains in the inherited environment. Explicitly keep
        // broader OpenAI authority out of the long-lived tunnel daemon.
        .env_remove("OPENAI_ADMIN_KEY")
        .env_remove("OPENAI_API_KEY")
        // The local Server bootstrap key may be a process-environment override.
        // The daemon receives only the generated Authorization file, never the
        // bootstrap credential as inherited environment state.
        .env_remove("WEBCODEX_TOKEN")
        .arg("--mcp.server-url")
        .arg(format!("url={mcp_url},channel=main"))
        .arg("--mcp.extra-headers")
        .arg(format!(
            "Authorization: file:{}",
            authorization_file.to_string_lossy()
        ));
}

async fn run_doctor(
    prerequisites: &OpenAiTunnelPrerequisites,
    mcp_url: &str,
    authorization_file: &Path,
    deadline: Instant,
) -> Result<(), ProductError> {
    let mut command = Command::new(&prerequisites.binary);
    suppress_windows_console(&mut command);
    command.arg("doctor");
    configure_runtime_command(&mut command, prerequisites, mcp_url, authorization_file);
    command
        .arg("--health.listen-addr")
        .arg("127.0.0.1:0")
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client doctor could not start"))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(tunnel_runtime_error(
            "OpenAI tunnel-client doctor had no startup budget remaining",
        ));
    }
    let budget = remaining.min(TUNNEL_CLIENT_DOCTOR_TIMEOUT);
    let status = match tokio::time::timeout(budget, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(tunnel_runtime_error(
                "OpenAI tunnel-client doctor could not be supervised",
            ));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(tunnel_runtime_error(
                "OpenAI tunnel-client doctor timed out before validating the connection",
            ));
        }
    };
    if !status.success() {
        return Err(ProductError::new(
            "tunnel_unavailable",
            "OpenAI tunnel-client doctor rejected the Secure MCP Tunnel configuration",
            Some("Check CONTROL_PLANE_TUNNEL_ID, CONTROL_PLANE_API_KEY, Tunnel workspace scope, and network access, then retry."),
        ));
    }
    Ok(())
}

async fn run_control_plane_probe(
    prerequisites: &OpenAiTunnelPrerequisites,
    deadline: Instant,
) -> Result<(), ProductError> {
    let mut command = Command::new(&prerequisites.binary);
    suppress_windows_console(&mut command);
    remove_npm_wrapper_network_environment(&mut command);
    command
        .arg("admin")
        .arg("tunnels")
        .arg("get")
        .arg(&prerequisites.tunnel_id)
        .env("CONTROL_PLANE_TUNNEL_ID", &prerequisites.tunnel_id)
        // Exercise the exact Runtime Key inherited by the daemon rather than
        // accidentally succeeding through broader operator credentials.
        .env_remove("OPENAI_ADMIN_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| {
        tunnel_runtime_error("OpenAI tunnel-client control-plane probe could not start")
    })?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(tunnel_runtime_error(
            "OpenAI tunnel-client control-plane probe had no startup budget remaining",
        ));
    }
    let budget = remaining.min(TUNNEL_CLIENT_CONTROL_PLANE_PROBE_TIMEOUT);
    let status = match tokio::time::timeout(budget, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(tunnel_runtime_error(
                "OpenAI tunnel-client control-plane probe could not be supervised",
            ));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(ProductError::new(
                "tunnel_unavailable",
                "OpenAI Secure MCP Tunnel could not reach the OpenAI control plane before the startup timeout",
                Some("Check the Tunnel proxy, api.openai.com network access, Tunnel ID, and Runtime Key permissions, then retry."),
            ));
        }
    };
    if !status.success() {
        return Err(ProductError::new(
            "tunnel_unavailable",
            "OpenAI Secure MCP Tunnel could not verify the selected Tunnel with the Runtime Key",
            Some("Check the Tunnel proxy, Tunnel workspace, and Tunnels Read + Use permissions, then retry."),
        ));
    }
    Ok(())
}

async fn wait_until_ready(
    child: &mut Child,
    health_url_file: &Path,
    deadline: Instant,
) -> Result<(), ProductError> {
    let client = reqwest::Client::builder()
        .connect_timeout(TUNNEL_CLIENT_READY_PROBE_TIMEOUT)
        .timeout(TUNNEL_CLIENT_READY_PROBE_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(|_| {
            tunnel_runtime_error("WebCodex could not initialize the local tunnel readiness probe")
        })?;
    let mut health_base = None;

    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client could not be supervised"))?
        {
            return Err(ProductError::new(
                "tunnel_unavailable",
                format!("OpenAI tunnel-client exited before becoming ready ({status})"),
                Some("Check the Tunnel ID, runtime API key permissions, local WebCodex authentication, and network access, then retry."),
            ));
        }

        if health_base.is_none() && health_url_file.is_file() {
            health_base = read_loopback_health_url(health_url_file).ok();
        }
        if let Some(base) = health_base.as_ref() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                if let Ok(Ok(response)) = tokio::time::timeout(
                    remaining.min(TUNNEL_CLIENT_READY_PROBE_TIMEOUT),
                    client.get(format!("{base}/readyz")).send(),
                )
                .await
                {
                    if response.status().is_success() {
                        return Ok(());
                    }
                }
            }
        }

        if Instant::now() >= deadline {
            return Err(ProductError::new(
                "tunnel_unavailable",
                "OpenAI Secure MCP Tunnel did not become ready before the startup timeout",
                Some("Check CONTROL_PLANE_TUNNEL_ID, CONTROL_PLANE_API_KEY, Tunnel workspace scope, and local MCP reachability, then retry."),
            ));
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )
        .await;
    }
}

fn read_loopback_health_url(path: &Path) -> Result<String, ProductError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client health URL could not be read"))?;
    if !metadata.file_type().is_file() || metadata.len() > TUNNEL_CLIENT_HEALTH_URL_BYTES as u64 {
        return Err(tunnel_runtime_error(
            "OpenAI tunnel-client health URL file is invalid",
        ));
    }
    #[cfg(windows)]
    let value = super::windows_private_state::read_private_bytes(path)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| {
            tunnel_runtime_error("OpenAI tunnel-client health URL could not be read securely")
        })?;
    #[cfg(not(windows))]
    let value = fs::read_to_string(path)
        .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client health URL could not be read"))?;
    let value = value.trim();
    let parsed = url::Url::parse(value)
        .map_err(|_| tunnel_runtime_error("OpenAI tunnel-client health URL is invalid"))?;
    let host = parsed.host_str().unwrap_or("");
    if parsed.scheme() != "http"
        || !matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
        || parsed.port().is_none()
        || !matches!(parsed.path(), "" | "/")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(tunnel_runtime_error(
            "OpenAI tunnel-client health URL is not a loopback HTTP origin",
        ));
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn required_tunnel_id() -> Result<String, ProductError> {
    let value = std::env::var("CONTROL_PLANE_TUNNEL_ID")
        .map_err(|_| missing_tunnel_configuration("CONTROL_PLANE_TUNNEL_ID is not set"))?;
    if !valid_tunnel_id(&value) {
        return Err(missing_tunnel_configuration(
            "CONTROL_PLANE_TUNNEL_ID must be tunnel_ followed by 32 lowercase hexadecimal characters",
        ));
    }
    Ok(value)
}

fn require_runtime_api_key() -> Result<(), ProductError> {
    let value = std::env::var_os("CONTROL_PLANE_API_KEY")
        .ok_or_else(|| missing_tunnel_configuration("CONTROL_PLANE_API_KEY is not set"))?;
    if value.is_empty() {
        return Err(missing_tunnel_configuration(
            "CONTROL_PLANE_API_KEY is empty",
        ));
    }
    Ok(())
}

fn valid_tunnel_id(value: &str) -> bool {
    value.strip_prefix("tunnel_").is_some_and(|suffix| {
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

async fn resolve_tunnel_client() -> Result<PathBuf, ProductError> {
    let override_bin = std::env::var_os(TUNNEL_CLIENT_OVERRIDE).map(PathBuf::from);
    if let Some(binary) = override_bin.as_deref() {
        if !binary.is_file() {
            return Err(ProductError::new(
                "tunnel_unavailable",
                "WEBCODEX_TUNNEL_CLIENT_BIN does not point to a tunnel-client file",
                Some("Fix or unset WEBCODEX_TUNNEL_CLIENT_BIN, then retry webcodex share --tunnel openai."),
            ));
        }
        verify_tunnel_client_version(binary).await?;
        return Ok(binary.to_path_buf());
    }

    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(executable_name("tunnel-client"));
            if candidate.is_file() && verify_tunnel_client_version(&candidate).await.is_ok() {
                return Ok(candidate);
            }
        }
    }

    let asset = tunnel_client_asset_for(std::env::consts::OS, std::env::consts::ARCH)?;
    let root = managed_tunnel_client_root()?;
    ensure_managed_tunnel_client_at(&root, asset).await
}

fn tunnel_client_asset_for(os: &str, arch: &str) -> Result<TunnelClientAsset, ProductError> {
    let asset = match (os, arch) {
        ("linux", "x86_64") => TunnelClientAsset {
            target: "linux-amd64",
            file_name: "tunnel-client-v0.0.12-linux-amd64.zip",
            archive_sha256: "2bb693bd7b5cd28da7ce09cd9e309529dbb33b7cc9dc0058e62a064688f92c81",
            binary_sha256: "ee9d4a75bc0b42f36f345aa96231e0db1ab00488122f34ebc99d6db055b6603e",
            member_name: "tunnel-client",
        },
        ("linux", "aarch64") => TunnelClientAsset {
            target: "linux-arm64",
            file_name: "tunnel-client-v0.0.12-linux-arm64.zip",
            archive_sha256: "6813878a3edb82ebebb32fe5a859bc6327a81cce5bc7b635a2313174d26365d6",
            binary_sha256: "0a48e6696de0df5951c013e40be81ce775e6644e209758c48795a0ecbda06406",
            member_name: "tunnel-client",
        },
        ("macos", "x86_64") => TunnelClientAsset {
            target: "darwin-amd64",
            file_name: "tunnel-client-v0.0.12-darwin-amd64.zip",
            archive_sha256: "33de53aec680faafedc795f8f8268d6861577bddb871cb2d49529c91f88c2009",
            binary_sha256: "4133dab2575223252732a998210c34b7ed96a51765cf5ea835a8e24cf2be1272",
            member_name: "tunnel-client",
        },
        ("macos", "aarch64") => TunnelClientAsset {
            target: "darwin-arm64",
            file_name: "tunnel-client-v0.0.12-darwin-arm64.zip",
            archive_sha256: "42fb3138dc9c081d5777cb7e8bd1e041cc48b67c4978dbab3c5167ca1aabca02",
            binary_sha256: "b1757220cf4722cec9085ee4a908cf0ee4c1a499a33bd99979b9a9c7669e29b1",
            member_name: "tunnel-client",
        },
        ("windows", "x86_64") => TunnelClientAsset {
            target: "windows-amd64",
            file_name: "tunnel-client-v0.0.12-windows-amd64.zip",
            archive_sha256: "2a2804933924e38a502d62b61f0266cb80d56d65744f4c29876b2bf9c1544356",
            binary_sha256: "6649169733686805ca16cccd91774594d0c017fd729c37ad4ce1cd18323d9ae8",
            member_name: "tunnel-client.exe",
        },
        ("windows", "aarch64") => TunnelClientAsset {
            target: "windows-arm64",
            file_name: "tunnel-client-v0.0.12-windows-arm64.zip",
            archive_sha256: "65ab54221554481bb1c23b6015b99abe0b7f79b08593f4fb17a9e2e25532281d",
            binary_sha256: "480684ec1031fc2985c7e87f9d669e7dfda4012a8ecdab21eabe1b5deafdd656",
            member_name: "tunnel-client.exe",
        },
        _ => {
            return Err(ProductError::new(
                "tunnel_unavailable",
                format!("automatic OpenAI tunnel-client installation is unsupported on {os}/{arch}"),
                Some("Install the pinned OpenAI tunnel-client and set WEBCODEX_TUNNEL_CLIENT_BIN, or use another WebCodex tunnel provider."),
            ))
        }
    };
    Ok(asset)
}

fn managed_tunnel_client_root() -> Result<PathBuf, ProductError> {
    let local_app_data = cfg!(windows)
        .then(|| std::env::var_os("LOCALAPPDATA"))
        .flatten();
    managed_tunnel_client_root_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        local_app_data.as_deref(),
    )
}

fn managed_tunnel_client_root_from(
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> Result<PathBuf, ProductError> {
    if let Some(path) = xdg_state_home.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(managed_user_root_error("XDG_STATE_HOME"));
        }
        return Ok(path.join("webcodex/tools/tunnel-client"));
    }
    if let Some(path) = home.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(managed_user_root_error("HOME"));
        }
        return Ok(path.join(".local/state/webcodex/tools/tunnel-client"));
    }
    if let Some(path) = local_app_data.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(managed_user_root_error("LOCALAPPDATA"));
        }
        return Ok(path.join("WebCodex/tools/tunnel-client"));
    }
    Err(ProductError::new(
        "tunnel_unavailable",
        "WebCodex cannot choose a private user directory for managed OpenAI tunnel-client",
        Some("Set HOME/XDG_STATE_HOME, ensure LOCALAPPDATA is available on Windows, set WEBCODEX_TUNNEL_CLIENT_BIN, or use another WebCodex tunnel provider."),
    ))
}

fn managed_binary_path(root: &Path, asset: TunnelClientAsset) -> PathBuf {
    root.join(TUNNEL_CLIENT_VERSION)
        .join(asset.target)
        .join(executable_name("tunnel-client"))
}

async fn ensure_managed_tunnel_client_at(
    root: &Path,
    asset: TunnelClientAsset,
) -> Result<PathBuf, ProductError> {
    let destination = managed_binary_path(root, asset);
    let install_dir = destination.parent().ok_or_else(managed_tool_path_error)?;
    create_private_tool_dir(install_dir)?;
    if managed_binary_is_valid(&destination, asset).await {
        return Ok(destination);
    }

    eprintln!(
        "WebCodex: tunnel-client was not found; downloading verified OpenAI tunnel-client {TUNNEL_CLIENT_VERSION}..."
    );
    let temporary = install_dir.join(format!(".install-{}", uuid::Uuid::new_v4().simple()));
    create_private_tool_dir(&temporary)?;
    let result = async {
        let archive = temporary.join(asset.file_name);
        let url = format!("{TUNNEL_CLIENT_RELEASE_BASE}/{}", asset.file_name);
        download_tunnel_client_asset(&url, &archive).await?;
        verify_sha256(&archive, asset.archive_sha256, "downloaded tunnel-client archive")?;
        let candidate = temporary.join(executable_name("tunnel-client"));
        extract_tunnel_client(&archive, &candidate, asset.member_name)?;
        verify_sha256(&candidate, asset.binary_sha256, "downloaded tunnel-client binary")?;
        make_private_executable(&candidate)?;
        verify_tunnel_client_version(&candidate).await?;
        fs::rename(&candidate, &destination).map_err(|_| {
            ProductError::new(
                "tunnel_unavailable",
                "WebCodex could not install its managed OpenAI tunnel-client atomically",
                Some("Check user-state filesystem permissions, then retry webcodex share --tunnel openai."),
            )
        })?;
        if !managed_binary_is_valid(&destination, asset).await {
            return Err(verification_error());
        }
        Ok(destination.clone())
    }
    .await;
    let _ = fs::remove_dir_all(&temporary);
    result
}

async fn managed_binary_is_valid(path: &Path, asset: TunnelClientAsset) -> bool {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    if super::windows_private_state::protect_private_file(path).is_err() {
        return false;
    }
    if sha256_file(path).ok().as_deref() != Some(asset.binary_sha256) {
        return false;
    }
    verify_tunnel_client_version(path).await.is_ok()
}

async fn download_tunnel_client_asset(url: &str, destination: &Path) -> Result<(), ProductError> {
    let npm = cloudflared_service::effective_npm_network_settings().await;
    let network = cloudflared_service::resolve_download_network_config_with(url, &npm, |name| {
        std::env::var(name).ok()
    })
    .map_err(|_| download_error("network/proxy configuration is invalid"))?;
    let client = cloudflared_service::build_managed_download_client(&network)
        .map_err(|_| download_error("could not initialize the download client"))?;
    let mut response = client
        .get(url)
        .header(
            USER_AGENT,
            format!("webcodex/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                download_error("request timed out")
            } else if error.is_connect() {
                download_error("connection failed")
            } else {
                download_error("request failed")
            }
        })?;
    if !response.status().is_success() {
        return Err(download_error(&format!(
            "server returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > TUNNEL_CLIENT_MAX_DOWNLOAD_BYTES as u64)
    {
        return Err(download_error("artifact exceeds the download size limit"));
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(TUNNEL_CLIENT_MAX_DOWNLOAD_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| download_error("response body failed"))?
    {
        if bytes.len().saturating_add(chunk.len()) > TUNNEL_CLIENT_MAX_DOWNLOAD_BYTES {
            return Err(download_error("artifact exceeds the download size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    write_private_file(destination, &bytes)
}

fn extract_tunnel_client(
    archive: &Path,
    destination: &Path,
    member_name: &str,
) -> Result<(), ProductError> {
    let file = File::open(archive).map_err(|_| extraction_error("archive could not be opened"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|_| extraction_error("verified archive is not a valid ZIP file"))?;
    let mut entry = archive.by_name(member_name).map_err(|_| {
        extraction_error("verified archive does not contain the expected tunnel-client executable")
    })?;
    if !entry.is_file() || entry.size() > TUNNEL_CLIENT_MAX_BINARY_BYTES {
        return Err(extraction_error("tunnel-client archive member is invalid"));
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut bytes)
        .map_err(|_| extraction_error("tunnel-client archive member could not be read"))?;
    if bytes.len() as u64 != entry.size() {
        return Err(extraction_error(
            "tunnel-client archive member size changed while reading",
        ));
    }
    write_private_file(destination, &bytes)
}

fn create_private_tool_dir(path: &Path) -> Result<(), ProductError> {
    fs::create_dir_all(path).map_err(|_| managed_tool_path_error())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| managed_tool_path_error())?;
    }
    #[cfg(windows)]
    super::windows_private_state::protect_private_directory(path)
        .map_err(|_| managed_tool_path_error())?;
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), ProductError> {
    #[cfg(windows)]
    {
        return super::windows_private_state::write_new_private_file(path, bytes)
            .map_err(|_| managed_tool_path_error());
    }
    #[cfg(not(windows))]
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|_| managed_tool_path_error())?;
        file.write_all(bytes).map_err(|_| managed_tool_path_error())
    }
}

fn make_private_executable(path: &Path) -> Result<(), ProductError> {
    #[cfg(windows)]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| managed_tool_path_error())?;
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, ProductError> {
    let mut file = File::open(path).map_err(|_| verification_error())?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| verification_error())?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn verify_sha256(path: &Path, expected: &str, label: &str) -> Result<(), ProductError> {
    if sha256_file(path)? != expected {
        return Err(ProductError::new(
            "tunnel_unavailable",
            format!("{label} failed SHA-256 verification"),
            Some("Retry webcodex share --tunnel openai; if the failure persists, set WEBCODEX_TUNNEL_CLIENT_BIN to the pinned trusted binary."),
        ));
    }
    Ok(())
}

async fn verify_tunnel_client_version(path: &Path) -> Result<(), ProductError> {
    let mut command = Command::new(path);
    suppress_windows_console(&mut command);
    command.arg("--version");
    let output = tokio::time::timeout(TUNNEL_CLIENT_VERIFY_TIMEOUT, command.output())
        .await
        .map_err(|_| verification_error())?
        .map_err(|_| verification_error())?;
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || !version_text.starts_with(TUNNEL_CLIENT_VERSION) {
        return Err(verification_error());
    }
    Ok(())
}

fn suppress_windows_console(command: &mut Command) {
    #[cfg(windows)]
    command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    let _ = command;
}

fn missing_tunnel_configuration(message: &'static str) -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        message,
        Some("Create or select an OpenAI Secure MCP Tunnel, export CONTROL_PLANE_TUNNEL_ID and a Restricted CONTROL_PLANE_API_KEY with Tunnels Read + Use, then retry."),
    )
}

fn managed_user_root_error(name: &str) -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        format!("{name} must be an absolute path for managed OpenAI tunnel-client"),
        Some("Fix the user-state environment, set WEBCODEX_TUNNEL_CLIENT_BIN, or use another WebCodex tunnel provider."),
    )
}

fn managed_tool_path_error() -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        "WebCodex could not create or protect its managed OpenAI tunnel-client files",
        Some("Check user-state filesystem permissions, then retry webcodex share --tunnel openai."),
    )
}

fn download_error(detail: &str) -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        format!("WebCodex could not download verified OpenAI tunnel-client: {detail}"),
        Some("Check network/proxy connectivity and retry, or set WEBCODEX_TUNNEL_CLIENT_BIN to the pinned trusted binary."),
    )
}

fn extraction_error(detail: &str) -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        format!("WebCodex could not unpack verified OpenAI tunnel-client: {detail}"),
        Some("Retry webcodex share --tunnel openai or set WEBCODEX_TUNNEL_CLIENT_BIN to the pinned trusted binary."),
    )
}

fn verification_error() -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        format!("OpenAI tunnel-client failed pinned {TUNNEL_CLIENT_VERSION} verification"),
        Some("Remove the managed tunnel-client file and retry, or set WEBCODEX_TUNNEL_CLIENT_BIN to the pinned trusted binary."),
    )
}

fn tunnel_runtime_error(message: &'static str) -> ProductError {
    ProductError::new(
        "tunnel_unavailable",
        message,
        Some("Check the OpenAI tunnel-client configuration and retry webcodex share --tunnel openai."),
    )
}

#[cfg(test)]
#[path = "project_entry_openai_tunnel_tests.rs"]
mod tests;
