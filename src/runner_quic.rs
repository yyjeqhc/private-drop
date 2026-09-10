//! Server-side custom QUIC Runner transport.
//!
//! This is a **custom QUIC stream transport** for Runner connections, NOT
//! HTTP/3. It runs a separate `quinn` UDP listener in parallel with the HTTP
//! server (which keeps serving GPT Actions over TCP 443 via Nginx unchanged).
//! Nginx is not involved in QUIC.
//!
//! QUIC is an alternative transport for the existing Runner envelope protocol.
//! It uses a length-prefixed JSON `RunnerEnvelope` stream over QUIC and is
//! intended to mirror the WebSocket Runner flow, not introduce a separate
//! application protocol.
//!
//! Authentication reuses [`crate::auth::authenticate_bearer`], which mirrors
//! `AuthMiddleware`: bootstrap when auth is disabled, the server-wide token,
//! or a Phase 2/3 API/agent token looked up by SHA-256 hash. TLS certificates
//! are NOT trusted as authentication — the agent token is always validated.

use crate::auth::authenticate_bearer;
use crate::config::{Config, QuicRuntimeStatus, QuicServerConfig};
use crate::runner_http::{RunnerRegistry, RunnerTransport};
use crate::runner_protocol::{
    read_quic_frame, read_quic_register_frame, write_quic_frame, RunnerEnvelope,
};
#[cfg(test)]
use crate::runner_protocol::{write_quic_register_frame, QuicRegisterFrame};
use crate::Database;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// The rustls crypto provider used for the QUIC transport. The dependency tree
/// pulls *both* `aws-lc-rs` and `ring`, so rustls cannot auto-select a
/// process-level provider; we therefore pin aws-lc-rs explicitly per config
/// via `builder_with_provider`. This is thread-safe (no global install) and
/// works under parallel test execution.
fn rustls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Deadline for the Runner to send its first `Register` frame after the QUIC
/// handshake. Mirrors the WebSocket `REGISTER_TIMEOUT`.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time to keep an error-path QUIC stream open while waiting for the
/// peer to read the final `Error` envelope.
const ERROR_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum number of peer frames to discard on an error path before closing.
const ERROR_DRAIN_MAX_FRAMES: usize = 4;

/// Load a PEM cert chain from `path` into DER certificates.
fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file = File::open(path)
        .map_err(|e| format!("failed to open QUIC cert {}: {}", path.display(), e))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("failed to parse QUIC cert {}: {}", path.display(), e))?;
    if certs.is_empty() {
        return Err(format!(
            "QUIC cert {} contains no certificates",
            path.display()
        ));
    }
    Ok(certs)
}

/// Load a PEM private key from `path`. Reads the file only to parse the key;
/// never returns or logs the key *contents* (only path/parse errors).
fn load_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>, String> {
    let file = File::open(path)
        .map_err(|e| format!("failed to open QUIC key {}: {}", path.display(), e))?;
    let mut reader = BufReader::new(file);
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| format!("failed to parse QUIC key {}: {}", path.display(), e))?
        .ok_or_else(|| format!("QUIC key {} contains no private key", path.display()))?;
    Ok(key)
}

/// Build a `quinn` server crypto config from PEM cert/key paths, with the
/// given ALPN. The cert/key are read once at startup; their contents are not
/// retained beyond the rustls config. Returns the quinn-wrapped
/// `QuicServerConfig` ready for `ServerConfig::with_crypto`.
fn build_server_crypto(
    quic_cfg: &QuicServerConfig,
) -> Result<quinn::crypto::rustls::QuicServerConfig, String> {
    let certs = load_certs(&quic_cfg.cert)?;
    let key = load_key(&quic_cfg.key)?;
    let mut server_crypto = rustls::ServerConfig::builder_with_provider(rustls_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("failed to select rustls protocol versions: {}", e))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("failed to build rustls server config: {}", e))?;
    server_crypto.alpn_protocols = vec![quic_cfg.alpn.as_bytes().to_vec()];
    quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .map_err(|e| format!("failed to build quinn server crypto: {}", e))
}

/// Start the QUIC Runner listener. Loads cert/key, binds the UDP endpoint, and
/// runs an accept loop in the caller's task. Per-connection errors are logged
/// and the loop continues; only startup failures (bad cert, bind error) are
/// returned. Runs forever once started.
pub(crate) async fn run_runner_quic_listener(
    config: Arc<Config>,
    db: Option<Arc<Database>>,
    registry: Arc<RunnerRegistry>,
    quic_cfg: QuicServerConfig,
    quic_status: Option<Arc<std::sync::Mutex<QuicRuntimeStatus>>>,
) -> Result<(), String> {
    if let Err(e) = quic_cfg.validate() {
        if let Some(status) = quic_status.as_ref() {
            status
                .lock()
                .expect("quic runtime status mutex poisoned")
                .mark_error(&e);
        }
        return Err(e);
    }
    let server_crypto = match build_server_crypto(&quic_cfg) {
        Ok(config) => config,
        Err(e) => {
            if let Some(status) = quic_status.as_ref() {
                status
                    .lock()
                    .expect("quic runtime status mutex poisoned")
                    .mark_error(&e);
            }
            return Err(e);
        }
    };
    let listen: std::net::SocketAddr = match quic_cfg.listen.parse() {
        Ok(listen) => listen,
        Err(e) => {
            let error = format!("invalid WEBCODEX_QUIC_LISTEN '{}': {}", quic_cfg.listen, e);
            if let Some(status) = quic_status.as_ref() {
                status
                    .lock()
                    .expect("quic runtime status mutex poisoned")
                    .mark_error(&error);
            }
            return Err(error);
        }
    };
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    let endpoint = match quinn::Endpoint::server(server_config, listen) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            let error = format!("failed to bind QUIC listener on {}: {}", listen, e);
            if let Some(status) = quic_status.as_ref() {
                status
                    .lock()
                    .expect("quic runtime status mutex poisoned")
                    .mark_error(&error);
            }
            return Err(error);
        }
    };
    if let Some(status) = quic_status.as_ref() {
        status
            .lock()
            .expect("quic runtime status mutex poisoned")
            .mark_started();
    }
    tracing::info!(
        "Runner QUIC listener on UDP {} with ALPN {}",
        listen,
        quic_cfg.alpn
    );
    serve_quic_endpoint(endpoint, &quic_cfg.alpn, config, db, registry).await;
    Ok(())
}

/// Accept loop shared by the production listener and tests. Runs until the
/// endpoint is closed. Each connection is handled in its own task so a slow or
/// misbehaving Runner cannot block acceptance of others.
async fn serve_quic_endpoint(
    endpoint: quinn::Endpoint,
    alpn: &str,
    config: Arc<Config>,
    db: Option<Arc<Database>>,
    registry: Arc<RunnerRegistry>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let config = config.clone();
        let db = db.clone();
        let registry = registry.clone();
        let alpn = alpn.to_string();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    handle_quic_connection(conn, &alpn, config, db, registry).await;
                }
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        "QUIC Runner connection handshake failed; check UDP reachability, certificate trust/SAN, and ALPN"
                    );
                }
            }
        });
    }
}

/// Drive one QUIC Runner connection to completion: register, ack, optional
/// request dispatch, keepalive, and inbound result/job_update handling.
async fn handle_quic_connection(
    conn: quinn::Connection,
    alpn: &str,
    config: Arc<Config>,
    db: Option<Arc<Database>>,
    registry: Arc<RunnerRegistry>,
) {
    // ALPN is enforced by quinn during the TLS handshake: the server crypto
    // only offers the configured `alpn`, so a connection only completes when
    // the client advertises a matching ALPN. No explicit post-handshake check
    // is needed; a mismatch fails the handshake (logged in the accept loop).
    let _ = alpn;

    // The Runner opens one bidirectional stream for all frames. Multiplexing
    // is intentionally left to a later phase.
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(
                error = ?e,
                "QUIC Runner accept_bi failed before register frame"
            );
            return;
        }
    };

    // 1. QUIC owns its credential-bearing first-register wire. The codec keeps
    //    the current QUIC-v1 JSON shape for rolling compatibility, but the
    //    shared RunnerEnvelope lifecycle never sees the credential.
    let register_frame =
        match tokio::time::timeout(REGISTER_TIMEOUT, read_quic_register_frame(&mut recv)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(e)) => {
                tracing::debug!(
                    reason_code = "malformed_register",
                    error = %e,
                    "QUIC Runner first register frame rejected"
                );
                send_error(&mut send, &mut recv, "expected_register", &e.to_string()).await;
                return;
            }
            Err(_) => {
                tracing::debug!("QUIC Runner register timed out waiting for first frame");
                send_error(
                    &mut send,
                    &mut recv,
                    "expected_register",
                    "register timed out",
                )
                .await;
                return;
            }
        };
    let (mut register_payload, auth_token) = register_frame.into_parts();
    let client_id = register_payload.client_id.clone();
    let runner_instance_id = register_payload.runner_instance_id.clone();
    let connection_id = uuid::Uuid::new_v4().to_string();

    // 2. Authenticate the agent token exactly like the HTTP/WebSocket paths.
    //    The token is dropped immediately after auth so it is never logged.
    let auth = authenticate_bearer(&config, db.as_ref(), auth_token.as_deref()).await;
    drop(auth_token);
    let auth = match auth {
        Some(ctx) => ctx,
        None => {
            send_error(
                &mut send,
                &mut recv,
                "unauthorized",
                "invalid or missing agent token",
            )
            .await;
            tracing::warn!(client_id = %client_id, "QUIC Runner register rejected: unauthorized");
            return;
        }
    };

    // 3. Enforce the same transport scope/owner boundary as the WS handler.
    //    The shared `register_session_prelude` performs the checks and resolves
    //    the effective owner; it stops before any wire I/O so this handler sends
    //    its own error frame and logs which gate failed.
    if let Err(e) =
        crate::runner_session::register_session_prelude(Some(&auth), &mut register_payload)
    {
        let reason = match &e {
            crate::runner_session::RegisterPreludeError::ForbiddenScope(_) => "forbidden scope",
            crate::runner_session::RegisterPreludeError::ForbiddenOwner(_) => {
                "client_id/owner binding mismatch"
            }
        };
        tracing::warn!(
            client_id = %client_id,
            error = e.message(),
            "QUIC Runner register rejected: {reason}"
        );
        send_error(
            &mut send,
            &mut recv,
            crate::runner_session::RegisterPreludeError::CODE,
            e.message(),
        )
        .await;
        return;
    }

    // 4. Commit the complete QUIC session in one registry transaction. The
    //    transport credential is already gone; only its non-secret registry
    //    access projection and the registration payload enter shared state.
    let access = crate::runner_http::runner_access_from_auth(Some(&auth));
    let notify = Arc::new(Notify::new());
    let (view, cancel) = match registry
        .register_streaming_session_with_cancel(
            register_payload,
            access.as_ref(),
            &connection_id,
            RunnerTransport::Quic,
            notify.clone(),
        )
        .await
    {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(
                client_id = %client_id,
                error = %e,
                "QUIC Runner register failed in registry"
            );
            send_error(&mut send, &mut recv, "register_failed", &e).await;
            return;
        }
    };

    // 5. Acknowledge the committed registration before the writer task takes
    //    ownership of SendStream. Queueing the ack into an mpsc would only prove
    //    local admission, not that the handshake reached the QUIC stream. If the
    //    actual write fails, revoke only this exact connection lease.
    let ack = RunnerEnvelope::Registered {
        success: true,
        client: Some(view),
        error: None,
    };
    if let Err(e) = write_quic_frame(&mut send, &ack).await {
        tracing::debug!(
            client_id = %client_id,
            error = %e,
            "QUIC Runner registered ack send failed"
        );
        registry
            .reconcile_disconnect_for_connection(&client_id, &runner_instance_id, &connection_id)
            .await;
        return;
    }

    // 6. After the registration handshake, all writes go through one writer
    //    task so the request pump and keepalive replies never concurrently hold
    //    SendStream.
    let (out_tx, mut out_rx) =
        mpsc::channel::<RunnerEnvelope>(crate::runner_session::OUTGOING_CHANNEL_CAPACITY);
    let writer_task = tokio::spawn(async move {
        while let Some(env) = out_rx.recv().await {
            if write_quic_frame(&mut send, &env).await.is_err() {
                return crate::runner_session::WriterExit::TransportFailed;
            }
        }
        if send.finish().is_err() {
            crate::runner_session::WriterExit::TransportFailed
        } else {
            crate::runner_session::WriterExit::ChannelClosed
        }
    });
    tracing::info!(client_id = %client_id, "Runner QUIC connected");

    // 7-9. Pump, reader loop, and teardown are shared with the WebSocket
    //      transport. The reader adapter wraps the QUIC receive stream.
    let reader = QuicReader { recv };
    crate::runner_session::run_runner_session(
        crate::runner_session::SessionContext {
            registry: &registry,
            client_id: &client_id,
            runner_instance_id: &runner_instance_id,
            connection_id: &connection_id,
            notify,
            cancel,
            transport_label: "quic",
        },
        out_tx,
        reader,
        writer_task,
    )
    .await;
    // The shared session has already drained/stopped its stream writer and
    // reconciled the exact lease. Explicitly close this connection so a
    // graceful Goodbye or reader/writer termination does not leave connection
    // lifetime to object-drop timing.
    conn.close(quinn::VarInt::from_u32(0), b"session complete");
    tracing::info!(client_id = %client_id, "Runner QUIC disconnected");
}

/// Adapter turning a QUIC receive stream into the transport-neutral
/// [`crate::runner_session::RunnerReader`]. A clean stream end stops the reader;
/// framing errors are logged and treated as a closed connection (mirroring the
/// previous inline reader loop).
struct QuicReader {
    recv: quinn::RecvStream,
}

impl crate::runner_session::RunnerReader for QuicReader {
    async fn recv(&mut self) -> crate::runner_session::RecvOutcome {
        match read_quic_frame(&mut self.recv).await {
            Ok(env) => crate::runner_session::RecvOutcome::Envelope(env),
            Err(crate::runner_protocol::QuicFrameError::EmptyStream) => {
                crate::runner_session::RecvOutcome::Closed
            }
            Err(e) => {
                tracing::debug!(error = %e, "QUIC Runner stream read ended");
                crate::runner_session::RecvOutcome::Closed
            }
        }
    }
}
/// Read and discard a bounded number of frames. Used to keep a QUIC connection
/// alive long enough for the peer to receive a final `Error` frame without
/// allowing a bad peer to hold the connection task indefinitely.
async fn drain_quic_stream_limited(recv: &mut quinn::RecvStream, max_frames: usize) {
    for _ in 0..max_frames {
        match read_quic_frame(recv).await {
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

/// Send an `Error` envelope over the stream before tearing it down, then briefly
/// drain the peer's stream so the connection stays alive until the error is
/// received. The drain is bounded because unauthenticated or malformed peers may
/// keep their send stream open forever.
async fn send_error(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    code: &str,
    message: &str,
) {
    let env = RunnerEnvelope::Error {
        code: code.to_string(),
        message: message.to_string(),
    };
    if write_quic_frame(send, &env).await.is_ok() {
        let _ = send.finish();
        let _ = tokio::time::timeout(
            ERROR_DRAIN_TIMEOUT,
            drain_quic_stream_limited(recv, ERROR_DRAIN_MAX_FRAMES),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner_protocol::{
        RunnerCapabilities, RunnerJobUpdateRequest, RunnerProtocolGenerationNumber,
        RunnerRegisterRequest, RunnerResultRequest, ShellJobOpRequest, ShellRunRequest,
        RUNNER_PROTOCOL_GENERATION_V2, RUNNER_QUIC_ALPN_V1,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    /// ALPN used by the QUIC integration tests.
    const TEST_ALPN: &str = RUNNER_QUIC_ALPN_V1;

    async fn wait_for_quic_client_connected(
        registry: &RunnerRegistry,
        client_id: &str,
        expected: bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let view = registry.get_runner_view(client_id).await.unwrap();
            if view.connected == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "client {client_id} connected={expected} was not observed before the 3-second deadline; last status={} transport={}",
                view.status,
                view.transport
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_quic_job_status(registry: &RunnerRegistry, job_id: &str, expected: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let job = registry.get_job(job_id).await.unwrap();
            if job.status == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "job {job_id} did not reach {expected} before the 3-second deadline; last status={}",
                job.status
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Generate a self-signed cert/key for `localhost` using rcgen, returned as
    /// DER types directly consumable by rustls. Avoids PEM parsing in tests.
    fn self_signed_cert() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-signed cert");
        let cert_der = ck.cert.der().clone();
        // rcgen serializes the key as PKCS#8 DER.
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
        (cert_der, key_der)
    }

    /// Build a quinn-wrapped rustls server config from the self-signed cert.
    fn server_crypto(
        cert_der: CertificateDer<'static>,
        key_der: PrivateKeyDer<'static>,
    ) -> quinn::crypto::rustls::QuicServerConfig {
        let mut cfg = rustls::ServerConfig::builder_with_provider(rustls_provider())
            .with_safe_default_protocol_versions()
            .expect("safe default protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("rustls server config");
        cfg.alpn_protocols = vec![TEST_ALPN.as_bytes().to_vec()];
        quinn::crypto::rustls::QuicServerConfig::try_from(cfg).expect("quinn server crypto")
    }

    /// Build a quinn-wrapped rustls client config that trusts the self-signed cert.
    fn client_crypto(
        cert_der: &CertificateDer<'static>,
    ) -> quinn::crypto::rustls::QuicClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der.clone()).expect("add root cert");
        let mut cfg = rustls::ClientConfig::builder_with_provider(rustls_provider())
            .with_safe_default_protocol_versions()
            .expect("safe default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![TEST_ALPN.as_bytes().to_vec()];
        quinn::crypto::rustls::QuicClientConfig::try_from(cfg).expect("quinn client crypto")
    }

    fn register_envelope(client_id: &str, instance: &str) -> QuicRegisterFrame {
        register_envelope_with_generation(client_id, instance, RUNNER_PROTOCOL_GENERATION_V2, None)
    }

    fn register_envelope_with_generation(
        client_id: &str,
        instance: &str,
        generation: RunnerProtocolGenerationNumber,
        auth_token: Option<String>,
    ) -> QuicRegisterFrame {
        let capabilities = crate::test_support::current_runner_capabilities(RunnerCapabilities {
            shell: true,
            file_read: true,
            file_write: true,
            artifact_export_chunk_read: false,
            artifact_export_streaming_metadata: false,
            structured_file_delete: true,
            apply_text_edit_occurrence: false,
            apply_text_edit_line_scope: false,
            apply_patch: false,
            apply_patch_match_metadata: false,
            apply_patch_matching_mode: false,
            apply_patch_strict_matching: false,
            git: false,
            jobs: true,
            async_jobs: true,
            async_shell_jobs: true,
            ssh_shell: false,
            persistent_shell: false,
            ssh_persistent_shell: false,
            structured_validation_argv: true,
            structured_cargo_test_count_assertion: true,
            structured_cargo_test_execution_policy: true,
            structured_go_test_json: true,
            structured_go_test_tool: true,
            structured_go_test_packages: true,
            structured_process_argv: true,
            structured_script_payload: false,
            structured_script_javascript: false,
            internal_posix_script: false,
            structured_execution_jobs: false,
            detached_process_jobs: false,
            lsp_read_only_navigation: false,
            lsp_call_hierarchy: false,
            project_lifecycle: false,
            project_path_registration: false,
            managed_worktree: false,
            skill_store_read: false,
            skill_store_manage: false,
            computer_observe: false,
            computer_application_discovery: false,
            computer_application_launch: false,
            computer_display_observe: false,
            computer_pointer_control: false,
            computer_clipboard_read: false,
            computer_clipboard_write: false,
            computer_snapshot_region: false,
            computer_accessibility_observe: false,
            computer_element_state: false,
            computer_control: false,
            computer_scroll_to_element: false,
            computer_key_input: false,
            computer_window_activate: false,
            computer_text_input: false,
            job_state_reconciliation: false,
            coding_agent_runs: false,
            native_tool_plugins: false,
            managed_ssh_resources: false,
            runner_config_control: false,
        });
        QuicRegisterFrame::new(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: client_id.to_string(),
                runner_instance_id: instance.to_string(),
                runner_protocol_generation: generation,
                display_name: Some("quic-test".to_string()),
                owner: Some("tester".to_string()),
                hostname: None,
                host_context: None,
                capabilities: capabilities,
                policy: None,
            },
            auth_token,
        )
    }

    use crate::test_support::test_config;

    /// Bind a QUIC server endpoint on 127.0.0.1:0 and return (endpoint, addr).
    fn bind_server(
        server_crypto: quinn::crypto::rustls::QuicServerConfig,
    ) -> (quinn::Endpoint, std::net::SocketAddr) {
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
        let endpoint = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap())
            .expect("bind quic server");
        let addr = endpoint.local_addr().expect("local_addr");
        (endpoint, addr)
    }

    async fn start_quic_server(
        registry: Arc<RunnerRegistry>,
        config: Arc<Config>,
        cert_der: CertificateDer<'static>,
        key_der: PrivateKeyDer<'static>,
    ) -> std::net::SocketAddr {
        let server_crypto = server_crypto(cert_der, key_der);
        let (endpoint, addr) = bind_server(server_crypto);
        tokio::spawn(async move {
            serve_quic_endpoint(endpoint, TEST_ALPN, config, None, registry).await;
        });
        addr
    }

    async fn connect_quic_client(
        cert_der: &CertificateDer<'static>,
        addr: std::net::SocketAddr,
    ) -> (
        quinn::Endpoint,
        quinn::Connection,
        quinn::SendStream,
        quinn::RecvStream,
    ) {
        let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = client_endpoint
            .connect_with(
                quinn::ClientConfig::new(Arc::new(client_crypto(cert_der))),
                addr,
                "localhost",
            )
            .unwrap()
            .await
            .expect("quic connect");
        let (send, recv) = conn.open_bi().await.expect("open_bi");
        (client_endpoint, conn, send, recv)
    }

    #[tokio::test]
    async fn quic_bad_register_error_path_exits_without_waiting_for_peer_close() {
        let (cert_der, key_der) = self_signed_cert();
        let server_crypto = server_crypto(cert_der.clone(), key_der);
        let (endpoint, addr) = bind_server(server_crypto);
        let registry = Arc::new(RunnerRegistry::default());
        let config = test_config(None);

        let server_task = tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("accept incoming quic");
            let conn = incoming.await.expect("server quic handshake");
            handle_quic_connection(conn, TEST_ALPN, config, None, registry).await;
        });

        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_frame(&mut send, &RunnerEnvelope::Ping { ts: 1 })
            .await
            .expect("write wrong first frame");

        let error = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("error frame timeout")
            .expect("read error frame");
        match error {
            RunnerEnvelope::Error { code, message } => {
                assert_eq!(code, "expected_register");
                assert!(message.contains("register"), "message was: {message}");
                assert!(
                    !message.contains('{'),
                    "raw first-frame JSON leaked: {message}"
                );
            }
            other => panic!("expected error, got {:?}", other.kind()),
        }

        tokio::time::timeout(ERROR_DRAIN_TIMEOUT + Duration::from_secs(2), server_task)
            .await
            .expect("server handler should exit without peer close")
            .expect("server task should not panic");
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_register_requires_explicit_protocol_generation() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;
        let register = register_envelope("quic-missing-generation", "inst-missing-generation");
        let mut register = serde_json::to_value(&register).unwrap();
        register
            .as_object_mut()
            .unwrap()
            .remove("agent_protocol_generation");
        let json = serde_json::to_vec(&register).unwrap();
        send.write_all(&(json.len() as u32).to_be_bytes())
            .await
            .expect("write register length");
        send.write_all(&json).await.expect("write register body");

        let error = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("register error timeout")
            .expect("read register error");
        match error {
            RunnerEnvelope::Error { code, message } => {
                assert_eq!(code, "expected_register");
                assert!(message.contains("agent_protocol_generation"), "{message}");
            }
            other => panic!("expected register_failed, got {:?}", other.kind()),
        }
        assert!(registry
            .get_runner_view("quic-missing-generation")
            .await
            .is_none());
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_register_rejects_unsupported_protocol_generation() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;
        let register = register_envelope_with_generation(
            "quic-unsupported-generation",
            "inst-unsupported",
            RunnerProtocolGenerationNumber::new(3),
            None,
        );
        write_quic_register_frame(&mut send, &register)
            .await
            .expect("write register");

        let error = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("register error timeout")
            .expect("read register error");
        match error {
            RunnerEnvelope::Error { code, message } => {
                assert_eq!(code, "register_failed");
                assert_eq!(message, "agent_protocol_generation is unsupported");
            }
            other => panic!("expected register_failed, got {:?}", other.kind()),
        }
        assert!(registry
            .get_runner_view("quic-unsupported-generation")
            .await
            .is_none());
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_register_ack_and_ping_pong_roundtrip() {
        let (cert_der, key_der) = self_signed_cert();
        let server_crypto = server_crypto(cert_der.clone(), key_der);
        let (endpoint, addr) = bind_server(server_crypto);

        // Auth disabled -> bootstrap, so no token is required. This mirrors
        // the WebSocket integration tests which run without AuthMiddleware.
        let config = Arc::new(Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: std::path::PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            oauth2: crate::OAuth2Config::default(),
        });
        let registry = Arc::new(RunnerRegistry::default());

        // Spawn the accept loop.
        let serve_registry = registry.clone();
        let serve_config = config.clone();
        tokio::spawn(async move {
            serve_quic_endpoint(endpoint, TEST_ALPN, serve_config, None, serve_registry).await;
        });

        // Client: connect, open bi stream, register.
        let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = client_endpoint
            .connect_with(
                quinn::ClientConfig::new(Arc::new(client_crypto(&cert_der))),
                addr,
                "localhost",
            )
            .unwrap()
            .await
            .expect("quic connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");

        write_quic_register_frame(&mut send, &register_envelope("quic-rt", "inst-rt"))
            .await
            .expect("write register");

        // Read the Registered ack.
        let ack = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("ack timeout")
            .expect("read ack");
        match ack {
            RunnerEnvelope::Registered {
                success, client, ..
            } => {
                assert!(success, "register should succeed");
                let client = client.expect("client view in ack");
                assert_eq!(client.client_id, "quic-rt");
                assert_eq!(client.transport, "quic");
                assert_eq!(
                    client.runner_protocol_generation,
                    RUNNER_PROTOCOL_GENERATION_V2
                );
                assert!(client.capabilities.shell);
                assert!(client.capabilities.file_read);
                assert!(client.capabilities.file_write);
                assert!(client.capabilities.structured_file_delete);
                assert!(!client.capabilities.git);
                assert!(client.capabilities.jobs);
                assert!(client.capabilities.async_jobs);
                assert!(client.capabilities.async_shell_jobs);
            }
            other => panic!("expected registered ack, got {:?}", other.kind()),
        }

        // The registry shows the Runner online over QUIC.
        let view = registry
            .get_runner_view("quic-rt")
            .await
            .expect("client view");
        assert!(view.connected);
        assert_eq!(view.status, "online");
        assert_eq!(view.transport, "quic");
        assert_eq!(
            view.runner_protocol_generation,
            RUNNER_PROTOCOL_GENERATION_V2
        );
        assert!(view.capabilities.shell);
        assert!(view.capabilities.file_read);
        assert!(view.capabilities.file_write);
        assert!(view.capabilities.structured_file_delete);
        assert!(!view.capabilities.git);
        assert!(view.capabilities.jobs);
        assert!(view.capabilities.async_jobs);
        assert!(view.capabilities.async_shell_jobs);

        // Ping -> Pong, and liveness is refreshed.
        let before = view.last_seen;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        write_quic_frame(&mut send, &RunnerEnvelope::Ping { ts: 7 })
            .await
            .expect("write ping");
        let pong = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("pong timeout")
            .expect("read pong");
        assert!(matches!(pong, RunnerEnvelope::Pong { ts: 7 }));
        let after = registry
            .get_runner_view("quic-rt")
            .await
            .expect("client view")
            .last_seen;
        assert!(after > before, "ping must refresh last_seen");

        // Close the stream; the server reconciles the retained client offline.
        send.finish().unwrap();
        wait_for_quic_client_connected(&registry, "quic-rt", false).await;
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_request_result_roundtrip() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;

        write_quic_register_frame(
            &mut send,
            &register_envelope_with_generation(
                "quic-gen2-rt",
                "inst-v2",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .expect("write register");

        let ack = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("ack timeout")
            .expect("read ack");
        match ack {
            RunnerEnvelope::Registered {
                success, client, ..
            } => {
                assert!(success);
                let client = client.expect("client view");
                assert_eq!(client.client_id, "quic-gen2-rt");
                assert_eq!(client.transport, "quic");
                assert_eq!(
                    client.runner_protocol_generation,
                    RUNNER_PROTOCOL_GENERATION_V2
                );
                assert!(client.capabilities.shell);
                assert!(client.capabilities.file_read);
                assert!(client.capabilities.file_write);
                assert!(client.capabilities.jobs);
                assert!(client.capabilities.async_jobs);
                assert!(client.capabilities.async_shell_jobs);
            }
            other => panic!("expected registered ack, got {:?}", other.kind()),
        }

        let (request_id, rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "quic-gen2-rt".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        let req_env = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("request timeout")
            .expect("read request");
        match req_env {
            RunnerEnvelope::Request { request } => {
                assert_eq!(request.request_id, request_id);
                assert_eq!(request.kind, "run_shell");
                assert_eq!(request.command, "echo hi");
            }
            other => panic!("expected request, got {:?}", other.kind()),
        }

        write_quic_frame(
            &mut send,
            &RunnerEnvelope::Result {
                payload: RunnerResultRequest {
                    client_id: "quic-gen2-rt".to_string(),
                    runner_instance_id: "inst-v2".to_string(),
                    request_id: request_id.clone(),
                    exit_code: Some(0),
                    stdout: Some("hi\n".to_string()),
                    stderr: Some(String::new()),
                    duration_ms: Some(2),
                    error: None,
                }
                .into(),
            },
        )
        .await
        .expect("write result");

        let response = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("result timeout")
            .expect("result response");
        assert!(response.success);
        assert_eq!(response.stdout.as_deref(), Some("hi\n"));
        assert_eq!(response.exit_code, Some(0));
        assert_eq!(
            registry
                .get_runner_view("quic-gen2-rt")
                .await
                .expect("client view")
                .pending_requests,
            0
        );

        let _ = send.finish();
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_v1_job_update_updates_registry() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;

        write_quic_register_frame(
            &mut send,
            &register_envelope_with_generation(
                "quic-job",
                "inst-job",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .unwrap()
            .unwrap();

        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("quic-job".to_string()),
                    cwd: None,
                    command: Some("printf hi".to_string()),
                    timeout_secs: Some(5),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        let req_env = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("request timeout")
            .expect("read request");
        let request_id = match req_env {
            RunnerEnvelope::Request { request } => {
                assert_eq!(request.kind, "start_job");
                request.request_id
            }
            other => panic!("expected request, got {:?}", other.kind()),
        };

        write_quic_frame(
            &mut send,
            &RunnerEnvelope::JobUpdate {
                payload: RunnerJobUpdateRequest {
                    client_id: "quic-job".to_string(),
                    runner_instance_id: "inst-job".to_string(),
                    update_seq: None,
                    job_id: job.job_id.clone(),
                    request_id: Some(request_id),
                    status: "running".to_string(),
                    stdout_chunk: Some("hi".to_string()),
                    stderr_chunk: None,
                    stdout_tail: None,
                    stderr_tail: None,
                    log_snapshot: None,
                    exit_code: None,
                    duration_ms: None,
                    error: None,
                    command_execution_state: None,
                    validation_progress: None,
                    activity: None,
                    finished: false,
                },
            },
        )
        .await
        .unwrap();

        wait_for_quic_job_status(&registry, &job.job_id, "running").await;
        let updated = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(updated.status, "running");
        let (_job, stdout, _stderr, _next_stdout, _next_stderr) = registry
            .job_log(&job.job_id, None, None, None)
            .await
            .unwrap();
        assert_eq!(stdout.as_deref(), Some("hi\n"));

        let _ = send.finish();
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_v1_disconnect_reconciles_jobs_and_notifier() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;

        write_quic_register_frame(
            &mut send,
            &register_envelope_with_generation(
                "quic-disc",
                "inst-disc",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .unwrap()
            .unwrap();

        let job = registry
            .start_job(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some("quic-disc".to_string()),
                    cwd: None,
                    command: Some("sleep 10".to_string()),
                    timeout_secs: Some(10),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("request timeout")
            .expect("read request");

        let _ = send.finish();
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");

        wait_for_quic_job_status(&registry, &job.job_id, "lost").await;
        let lost = registry.get_job(&job.job_id).await.unwrap();
        assert_eq!(lost.status, "lost");
        assert!(lost.error.unwrap().contains("disconnected"));
        let view = registry.get_runner_view("quic-disc").await.unwrap();
        assert_eq!(
            view.pending_requests, 0,
            "disconnect must reconcile pending job requests"
        );
        assert!(
            !view.connected,
            "quic transport disconnect must release active lease immediately"
        );

        // Offline enqueue gate (intentionally rejects work for clients outside
        // the online window). Previously this test enqueued after disconnect
        // and asserted pending_requests == 1 as a proxy for "notifier removed
        // so the pump no longer drains the queue". That proxy is no longer
        // valid: enqueue itself must fail fast rather than silently pile up
        // against a dead Runner. Notifier removal still happens in
        // reconcile_disconnect (covered above by pending cleanup + offline
        // connected flag); this asserts the post-disconnect contract.
        let err = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "quic-disc".to_string(),
                    cwd: None,
                    command: "echo after".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .expect_err("enqueue against a disconnected Runner must fail");
        assert!(
            err.contains("offline"),
            "post-disconnect enqueue must fail as offline, got: {err}"
        );
        assert_eq!(
            registry
                .get_runner_view("quic-disc")
                .await
                .unwrap()
                .pending_requests,
            0,
            "offline gate must not leave dangling queued requests"
        );
    }

    #[tokio::test]
    async fn quic_goodbye_releases_lease_for_new_instance() {
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;
        let (endpoint_a, conn_a, mut send_a, mut recv_a) =
            connect_quic_client(&cert_der, addr).await;

        write_quic_register_frame(
            &mut send_a,
            &register_envelope_with_generation(
                "quic-goodbye",
                "inst-a",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv_a))
            .await
            .unwrap()
            .unwrap();
        assert!(
            registry
                .get_runner_view("quic-goodbye")
                .await
                .unwrap()
                .connected
        );

        write_quic_frame(
            &mut send_a,
            &RunnerEnvelope::Goodbye {
                reason: Some("test shutdown".to_string()),
            },
        )
        .await
        .unwrap();
        wait_for_quic_client_connected(&registry, "quic-goodbye", false).await;
        assert!(
            !registry
                .get_runner_view("quic-goodbye")
                .await
                .unwrap()
                .connected
        );

        let (endpoint_b, conn_b, mut send_b, mut recv_b) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(
            &mut send_b,
            &register_envelope_with_generation(
                "quic-goodbye",
                "inst-b",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .unwrap();
        let ack = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv_b))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack,
            RunnerEnvelope::Registered { success: true, .. }
        ));
        let view = registry.get_runner_view("quic-goodbye").await.unwrap();
        assert_eq!(view.runner_instance_id, "inst-b");
        assert!(view.connected);

        let _ = send_a.finish();
        endpoint_a.close(quinn::VarInt::from_u32(0), b"");
        conn_a.close(quinn::VarInt::from_u32(0), b"done");
        let _ = send_b.finish();
        endpoint_b.close(quinn::VarInt::from_u32(0), b"");
        conn_b.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_bootstrap_token_registers_and_wrong_token_is_rejected() {
        // "wrong-secret" has no wc_ prefix, so a leaked shared-key mode would
        // authenticate it and shift the failure from "unauthorized" to the
        // scope gate. Hold the auth env guard to keep the rejection exact.
        let _env = crate::auth::AuthEnvGuard::auth_required();
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(Some("bootstrap-secret")),
            cert_der.clone(),
            key_der,
        )
        .await;

        let (client_endpoint, conn, mut send, mut recv) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(
            &mut send,
            &register_envelope_with_generation(
                "quic-auth-ok",
                "inst-auth-ok",
                RUNNER_PROTOCOL_GENERATION_V2,
                Some("bootstrap-secret".to_string()),
            ),
        )
        .await
        .unwrap();
        let ack = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack,
            RunnerEnvelope::Registered { success: true, .. }
        ));
        let _ = send.finish();
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");

        let (missing_endpoint, missing_conn, mut missing_send, mut missing_recv) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(
            &mut missing_send,
            &register_envelope_with_generation(
                "quic-auth-missing",
                "inst-auth-missing",
                RUNNER_PROTOCOL_GENERATION_V2,
                None,
            ),
        )
        .await
        .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut missing_recv))
            .await
            .unwrap()
            .unwrap();
        match err {
            RunnerEnvelope::Error { code, .. } => assert_eq!(code, "unauthorized"),
            other => panic!("expected unauthorized error, got {:?}", other.kind()),
        }
        assert!(registry
            .get_runner_view("quic-auth-missing")
            .await
            .is_none());
        let _ = missing_send.finish();
        missing_endpoint.close(quinn::VarInt::from_u32(0), b"");
        missing_conn.close(quinn::VarInt::from_u32(0), b"done");

        let (bad_endpoint, bad_conn, mut bad_send, mut bad_recv) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(
            &mut bad_send,
            &register_envelope_with_generation(
                "quic-auth-bad",
                "inst-auth-bad",
                RUNNER_PROTOCOL_GENERATION_V2,
                Some("wrong-secret".to_string()),
            ),
        )
        .await
        .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut bad_recv))
            .await
            .unwrap()
            .unwrap();
        match err {
            RunnerEnvelope::Error { code, .. } => assert_eq!(code, "unauthorized"),
            other => panic!("expected unauthorized error, got {:?}", other.kind()),
        }
        assert!(registry.get_runner_view("quic-auth-bad").await.is_none());
        let _ = bad_send.finish();
        bad_endpoint.close(quinn::VarInt::from_u32(0), b"");
        bad_conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_non_register_first_frame_is_rejected() {
        let (cert_der, key_der) = self_signed_cert();
        let server_crypto = server_crypto(cert_der.clone(), key_der);
        let (endpoint, addr) = bind_server(server_crypto);
        let config = Arc::new(Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: std::path::PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            oauth2: crate::OAuth2Config::default(),
        });
        let registry = Arc::new(RunnerRegistry::default());
        let serve_registry = registry.clone();
        let serve_config = config.clone();
        tokio::spawn(async move {
            serve_quic_endpoint(endpoint, TEST_ALPN, serve_config, None, serve_registry).await;
        });

        let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = client_endpoint
            .connect_with(
                quinn::ClientConfig::new(Arc::new(client_crypto(&cert_der))),
                addr,
                "localhost",
            )
            .unwrap()
            .await
            .expect("quic connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");

        // Send a Ping instead of Register.
        write_quic_frame(&mut send, &RunnerEnvelope::Ping { ts: 1 })
            .await
            .unwrap();

        // The server sends an Error and closes the stream.
        let env = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("timeout")
            .expect("read");
        match env {
            RunnerEnvelope::Error { code, .. } => assert_eq!(code, "expected_register"),
            other => panic!("expected error, got {:?}", other.kind()),
        }

        // No client was registered.
        assert!(registry.get_runner_view("quic-reject").await.is_none());
        assert!(registry.list_runners().await.is_empty());
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    /// A QUIC-registered Runner must surface protocol generation 2 and the
    /// `quic` transport in `list_runners` (used by runtime_status / list_runners).
    #[tokio::test]
    async fn quic_runner_surfaces_transport_and_protocol_in_list() {
        let (cert_der, key_der) = self_signed_cert();
        let server_crypto = server_crypto(cert_der.clone(), key_der);
        let (endpoint, addr) = bind_server(server_crypto);
        let config = Arc::new(Config {
            addr: "0.0.0.0:8080".to_string(),
            data_dir: std::path::PathBuf::from("./data"),
            token: None,
            max_text_size: 2 * 1024 * 1024,
            oauth2: crate::OAuth2Config::default(),
        });
        let registry = Arc::new(RunnerRegistry::default());
        let serve_registry = registry.clone();
        let serve_config = config.clone();
        tokio::spawn(async move {
            serve_quic_endpoint(endpoint, TEST_ALPN, serve_config, None, serve_registry).await;
        });

        let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = client_endpoint
            .connect_with(
                quinn::ClientConfig::new(Arc::new(client_crypto(&cert_der))),
                addr,
                "localhost",
            )
            .unwrap()
            .await
            .expect("quic connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        write_quic_register_frame(&mut send, &register_envelope("quic-list", "inst-list"))
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv))
            .await
            .expect("ack timeout")
            .expect("read ack");

        let clients = registry.list_runners().await;
        assert_eq!(clients.len(), 1);
        let c = &clients[0];
        assert_eq!(c.client_id, "quic-list");
        assert_eq!(c.transport, "quic");
        assert_eq!(c.runner_protocol_generation, RUNNER_PROTOCOL_GENERATION_V2);
        assert!(c.connected);
        assert!(c.capabilities.shell);
        assert!(c.capabilities.file_read);
        assert!(c.capabilities.file_write);
        assert!(!c.capabilities.git);
        assert!(c.capabilities.jobs);
        assert!(c.capabilities.async_jobs);
        assert!(c.capabilities.async_shell_jobs);

        send.finish().unwrap();
        client_endpoint.close(quinn::VarInt::from_u32(0), b"");
        conn.close(quinn::VarInt::from_u32(0), b"done");
    }

    #[tokio::test]
    async fn quic_stale_connection_pump_cannot_steal_new_request() {
        // Same runner instance connects over QUIC stream A, then reconnects
        // over stream B (same agent_instance_id, new connection_id lease). A
        // request enqueued after B's register must be delivered to B's pump
        // only — A's pump is bound to the stale connection lease and the
        // connection-scoped poll rejects it, so A never receives the request.
        let (cert_der, key_der) = self_signed_cert();
        let registry = Arc::new(RunnerRegistry::default());
        let addr = start_quic_server(
            registry.clone(),
            test_config(None),
            cert_der.clone(),
            key_der,
        )
        .await;

        // Connection A registers.
        let (client_endpoint_a, conn_a, mut send_a, mut recv_a) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(&mut send_a, &register_envelope("quic-steal", "inst-x"))
            .await
            .expect("write register A");
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv_a))
            .await
            .expect("ack A timeout")
            .expect("read ack A");

        // Same instance reconnects over B (reconnect/refresh: accepted).
        let (client_endpoint_b, conn_b, mut send_b, mut recv_b) =
            connect_quic_client(&cert_der, addr).await;
        write_quic_register_frame(&mut send_b, &register_envelope("quic-steal", "inst-x"))
            .await
            .expect("write register B");
        let _ = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv_b))
            .await
            .expect("ack B timeout")
            .expect("read ack B");

        // Enqueue a request after B's register: it belongs to B's lease.
        let (request_id, _rx) = registry
            .enqueue_run(
                ShellRunRequest {
                    client_id: "quic-steal".to_string(),
                    cwd: None,
                    command: "echo hi".to_string(),
                    stdin: None,
                    timeout_secs: 5,
                    wait_timeout_secs: 0,
                },
                "tester".to_string(),
            )
            .await
            .unwrap();

        // B's pump receives the request.
        let req_env = tokio::time::timeout(Duration::from_secs(5), read_quic_frame(&mut recv_b))
            .await
            .expect("request timeout on B")
            .expect("read request on B");
        match req_env {
            RunnerEnvelope::Request { request } => assert_eq!(request.request_id, request_id),
            other => panic!("expected request on B, got {:?}", other.kind()),
        }

        // P5a actively cancels A once B commits. The stale connection must
        // terminate promptly rather than remaining open/quiet behind a dead
        // request pump. Any successfully decoded application envelope here
        // would mean A stayed authoritative long enough to receive traffic.
        let stale_exit =
            tokio::time::timeout(Duration::from_millis(500), read_quic_frame(&mut recv_a))
                .await
                .expect("replaced QUIC connection A must terminate promptly");
        assert!(
            stale_exit.is_err(),
            "replaced QUIC connection A must close instead of receiving an application envelope"
        );

        send_a.finish().unwrap();
        send_b.finish().unwrap();
        client_endpoint_a.close(quinn::VarInt::from_u32(0), b"");
        client_endpoint_b.close(quinn::VarInt::from_u32(0), b"");
        conn_a.close(quinn::VarInt::from_u32(0), b"done");
        conn_b.close(quinn::VarInt::from_u32(0), b"done");
    }
}
