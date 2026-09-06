use super::client_handoff_service::{copy_text_to_clipboard, mcp_url, ClipboardCopyOutcome};
use super::openai_tunnel_service::{prepare_openai_tunnel, start_openai_tunnel};
use super::setup_service::{create_private_dir, write_new_private};
use super::ProductError;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REGULAR_TUNNEL_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegularServerTunnelOptions {
    pub(crate) local_server_url: String,
    pub(crate) bootstrap_token: String,
    pub(crate) runtime_parent: PathBuf,
}

struct RegularTunnelSession {
    directory: PathBuf,
}

impl RegularTunnelSession {
    fn create(runtime_parent: &Path) -> Result<Self, ProductError> {
        let root = runtime_parent.join("regular-tunnel-runtime");
        create_private_dir(&root)?;
        let directory = root.join(format!("openai-{}", uuid::Uuid::new_v4().simple()));
        create_private_dir(&directory)?;
        Ok(Self { directory })
    }

    fn write_authorization_file(&self, bootstrap_token: &str) -> Result<PathBuf, ProductError> {
        let token = bootstrap_token.trim();
        if token.is_empty() {
            return Err(tunnel_auth_error(
                "the local Server bootstrap credential is unavailable",
            ));
        }
        let path = self.directory.join("openai-mcp-authorization");
        write_new_private(&path, format!("Bearer {token}").as_bytes())?;
        Ok(path)
    }
}

impl Drop for RegularTunnelSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub(crate) async fn run_regular_server_tunnel(
    options: &RegularServerTunnelOptions,
) -> Result<(), ProductError> {
    let local_server_url = validate_local_server_url(&options.local_server_url)?;
    let session = RegularTunnelSession::create(&options.runtime_parent)?;
    let authorization_file = session.write_authorization_file(&options.bootstrap_token)?;
    let prerequisites = prepare_openai_tunnel().await?;
    let deadline = Instant::now() + REGULAR_TUNNEL_STARTUP_TIMEOUT;
    let mut tunnel = start_openai_tunnel(
        &prerequisites,
        &mcp_url(&local_server_url),
        &authorization_file,
        &session.directory,
        deadline,
    )
    .await?;

    let clipboard = copy_text_to_clipboard(&prerequisites.tunnel_id, true).await;
    let ready = machine_regular_tunnel_ready_event(clipboard);
    let encoded = serde_json::to_string(&ready).map_err(|_| {
        ProductError::new(
            "machine_output_failed",
            "WebCodex could not encode regular Tunnel readiness",
            Some("Retry the OpenAI Secure Tunnel."),
        )
    })?;
    println!("{encoded}");

    let outcome = tokio::select! {
        _ = wait_for_regular_tunnel_stop_signal() => Ok(()),
        result = tunnel.wait_for_exit() => result,
    };
    tunnel.stop().await;
    outcome
}

fn machine_regular_tunnel_ready_event(clipboard: ClipboardCopyOutcome) -> Value {
    let clipboard_state = match clipboard {
        ClipboardCopyOutcome::Copied => "copied",
        ClipboardCopyOutcome::Unavailable => "unavailable",
        ClipboardCopyOutcome::Disabled => "disabled",
    };
    json!({
        "event": "ready",
        "schema_version": 1,
        "provider": "openai",
        "ready_for_chatgpt": clipboard == ClipboardCopyOutcome::Copied,
        "connection": {
            "kind": "openai_tunnel",
            "clipboard_state": clipboard_state,
            "clipboard_contains": "tunnel_id",
        }
    })
}

fn validate_local_server_url(value: &str) -> Result<String, ProductError> {
    let value = value.trim().trim_end_matches('/');
    let parsed = url::Url::parse(value).map_err(|_| {
        ProductError::new(
            "unsupported_topology",
            "Regular OpenAI Tunnel requires a valid local WebCodex Server URL",
            Some("Start the local WebCodex runtime before starting the Tunnel."),
        )
    })?;
    let host = parsed.host_str().unwrap_or("");
    if parsed.scheme() != "http"
        || !matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(ProductError::new(
            "unsupported_topology",
            "Regular OpenAI Tunnel only exposes a loopback local WebCodex Server",
            Some("Use this command with Local Full Runtime; remote Server exposure is managed remotely."),
        ));
    }
    Ok(value.to_string())
}

async fn wait_for_regular_tunnel_stop_signal() {
    tokio::select! {
        _ = wait_for_platform_stop_signal() => {},
        _ = wait_for_stdin_eof() => {},
    }
}

async fn wait_for_stdin_eof() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = std::thread::Builder::new()
        .name("webcodex-server-tunnel-stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buffer = [0_u8; 256];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = tx.send(());
        });
    let _ = rx.await;
}

#[cfg(not(windows))]
async fn wait_for_platform_stop_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(windows)]
async fn wait_for_platform_stop_signal() {
    let mut ctrl_break = match tokio::signal::windows::ctrl_break() {
        Ok(signal) => signal,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = ctrl_break.recv() => {},
    }
}

fn tunnel_auth_error(message: &str) -> ProductError {
    ProductError::new(
        "tunnel_auth_invalid",
        message,
        Some("Restore the local Server bootstrap configuration, then retry."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_ready_event_contains_only_safe_handoff_metadata() {
        let event = machine_regular_tunnel_ready_event(ClipboardCopyOutcome::Copied);
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("\"provider\":\"openai\""));
        assert!(encoded.contains("\"clipboard_contains\":\"tunnel_id\""));
        assert!(!encoded.contains("CONTROL_PLANE_API_KEY"));
        assert!(!encoded.contains("Authorization"));
        assert!(!encoded.contains("Bearer"));
        assert!(!encoded.contains("wc_pat_"));
        assert!(!encoded.contains("wc_boot_"));
    }

    #[test]
    fn regular_tunnel_rejects_non_loopback_server_origins() {
        assert!(validate_local_server_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_local_server_url("http://localhost:8080").is_ok());
        assert!(validate_local_server_url("https://example.test").is_err());
        assert!(validate_local_server_url("http://0.0.0.0:8080").is_err());
    }

    #[test]
    fn authorization_file_is_private_distinct_and_cleaned_up() {
        let temp = tempfile::tempdir().unwrap();
        let bootstrap_token = "wc_boot_test_secret";
        let session = RegularTunnelSession::create(temp.path()).unwrap();
        let session_dir = session.directory.clone();
        let authorization_file = session.write_authorization_file(bootstrap_token).unwrap();
        assert_eq!(
            std::fs::read_to_string(&authorization_file).unwrap(),
            format!("Bearer {bootstrap_token}")
        );
        drop(session);
        assert!(!session_dir.exists());
        assert!(!authorization_file.exists());
    }

    #[test]
    fn authorization_file_rejects_empty_bootstrap_credentials() {
        let temp = tempfile::tempdir().unwrap();
        for value in ["", "   "] {
            let session = RegularTunnelSession::create(temp.path()).unwrap();
            let error = session.write_authorization_file(value).unwrap_err();
            assert_eq!(error.code, "tunnel_auth_invalid");
            assert_eq!(
                error.message,
                "the local Server bootstrap credential is unavailable"
            );
            drop(session);
        }
    }
}
