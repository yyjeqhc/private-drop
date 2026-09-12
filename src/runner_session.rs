//! Shared post-register Runner session loop.
//!
//! Both long-lived Runner transports — WebSocket (`runner_ws`) and custom QUIC
//! (`runner_quic`) — run the *same* session once a connection is registered:
//! a request pump that drains the shared registry queue and pushes `Request`
//! envelopes, a reader loop that dispatches `Result`/`PersistentShellResult`/
//! `JobUpdate`/`Ping`/`Pong`/`RuntimeMetadata`/`Goodbye` envelopes into the
//! connection-scoped registry lease, and a teardown that stops the pump, joins
//! the writer, and reconciles the disconnect. The only differences are the
//! wire I/O (how a frame is read or written) and a log label. This module owns
//! that shared loop; the two
//! transport modules own transport-specific registration, auth, and I/O.
//!
//! Connection-lease scoping: every registry call below takes the
//! `connection_id` so a stale same-instance reconnect cannot consume or
//! refresh the newer connection's lease. This mirrors the polling transport's
//! `*_for_connection` discipline.

use crate::auth::{AuthContext, SCOPE_AGENT_REGISTER};
use crate::runner_http::{
    effective_register_owner, enforce_register_owner, require_runner_transport_scope,
    RunnerRegistry,
};
use crate::runner_protocol::{RunnerEnvelope, RunnerPollRequest, RunnerRegisterRequest};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;

/// Channel capacity for outgoing envelopes (requests + pongs). Provides
/// backpressure if the Runner reads slowly. Shared by both transports.
pub(crate) const OUTGOING_CHANNEL_CAPACITY: usize = 64;

/// Bound post-session joins for the request pump and transport writer.
const STREAM_TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Transport writer completion without retaining an unsent envelope or error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterExit {
    ChannelClosed,
    TransportFailed,
}

/// Request-pump completion without retaining a registry error or request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PumpExit {
    /// The shared writer feed has closed as part of session teardown.
    ChannelClosed,
    /// This concrete connection no longer owns the active registry lease.
    LeaseLost,
    /// The registry rejected the pump for another bounded internal reason.
    RegistryFailed,
}

/// Error from [`register_session_prelude`]: the Runner transport boundary
/// rejected the register. Both variants surface to the wire as the
/// `register_forbidden` error code; they are distinguished only so the caller
/// can log which gate failed.
#[derive(Debug)]
pub(crate) enum RegisterPreludeError {
    /// `require_runner_transport_scope` rejected the caller (wrong scope / not a
    /// bootstrap or agent token).
    ForbiddenScope(String),
    /// `enforce_register_owner` rejected the client_id/owner binding.
    ForbiddenOwner(String),
}

impl RegisterPreludeError {
    /// Wire error code shared by both prelude gates.
    pub(crate) const CODE: &'static str = "register_forbidden";

    /// The human-readable message to send to the Runner.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::ForbiddenScope(m) | Self::ForbiddenOwner(m) => m,
        }
    }
}

/// Enforce the Runner transport boundary shared by the WebSocket and QUIC
/// register handlers, and resolve the effective owner onto `register_payload`.
///
/// This is the transport-neutral half of registration: it mirrors the polling
/// register handler's checks — bootstrap may register any owner, an agent
/// token may register only when its `allowed_client_id` matches and its owner
/// matches the requested owner (or fills it in when absent), a direct shared
/// key registers into its hash group with no trusted owner, and other user
/// tokens are rejected. It stops **before** any wire I/O: on failure it returns
/// the reason and lets the caller send its transport-specific error envelope
/// and log the cause. On success `register_payload.owner` is set to the
/// effective owner and the caller proceeds to mutate the registry.
pub(crate) fn register_session_prelude(
    auth: Option<&AuthContext>,
    register_payload: &mut RunnerRegisterRequest,
) -> Result<(), RegisterPreludeError> {
    if let Err(e) = require_runner_transport_scope(auth, SCOPE_AGENT_REGISTER) {
        return Err(RegisterPreludeError::ForbiddenScope(e));
    }
    if let Err(e) = enforce_register_owner(
        auth,
        &register_payload.client_id,
        register_payload.owner.as_deref(),
    ) {
        return Err(RegisterPreludeError::ForbiddenOwner(e));
    }
    register_payload.owner = effective_register_owner(auth, register_payload.owner.as_deref());
    Ok(())
}

/// Outcome of a single inbound read on the shared reader loop.
#[derive(Debug)]
pub(crate) enum RecvOutcome {
    /// A decoded envelope ready to dispatch.
    Envelope(RunnerEnvelope),
    /// A frame was consumed but yielded no envelope (e.g. a non-text
    /// WebSocket frame, or a malformed envelope that was logged and skipped).
    /// The reader loop continues.
    Skip,
    /// The peer closed the connection or a fatal read error occurred. The
    /// transport logs the cause itself; the reader loop stops.
    Closed,
}

/// Transport-neutral inbound reader. Implementations wrap a WebSocket
/// `StreamExt` stream or a QUIC `RecvStream` and translate wire reads into
/// [`RecvOutcome`]s, logging transport-specific errors themselves.
pub(crate) trait RunnerReader {
    async fn recv(&mut self) -> RecvOutcome;
}

/// Shared session context handed to [`run_runner_session`] after a transport
/// has authenticated, registered, and acknowledged the Runner.
pub(crate) struct SessionContext<'a> {
    pub(crate) registry: &'a Arc<RunnerRegistry>,
    pub(crate) client_id: &'a str,
    pub(crate) runner_instance_id: &'a str,
    pub(crate) connection_id: &'a str,
    pub(crate) notify: Arc<Notify>,
    /// Exact process-local cancellation lease for this streaming connection.
    /// Successful replacement signals it only after the new connection commits.
    pub(crate) cancel: watch::Receiver<bool>,
    /// Log label: `"websocket"` or `"quic"`.
    pub(crate) transport_label: &'static str,
}

/// Drive the post-register session to completion: request pump, reader-loop,
/// replacement cancellation, writer health, and bounded teardown.
///
/// The caller owns the transport-specific **writer task** (`writer_task`),
/// which drains `out_tx` (an `RunnerEnvelope` mpsc) onto the wire. This function
/// owns the connection-scoped **request pump** and directly observes its task,
/// the reader, the writer, and the exact replacement-cancellation lease. A
/// silent pump exit therefore cannot leave a pending reader/writer registered
/// as a zombie session.
pub(crate) async fn run_runner_session(
    ctx: SessionContext<'_>,
    out_tx: mpsc::Sender<RunnerEnvelope>,
    reader: impl RunnerReader,
    writer_task: JoinHandle<WriterExit>,
) {
    let pump_task = spawn_request_pump(&ctx, out_tx.clone());
    run_runner_session_with_pump(ctx, out_tx, reader, writer_task, pump_task).await;
}

fn classify_pump_poll_error(error: &str) -> PumpExit {
    if error.contains("transport connection is no longer active")
        || error.contains("no longer the active instance")
    {
        PumpExit::LeaseLost
    } else {
        PumpExit::RegistryFailed
    }
}

fn spawn_request_pump(
    ctx: &SessionContext<'_>,
    pump_tx: mpsc::Sender<RunnerEnvelope>,
) -> JoinHandle<PumpExit> {
    let pump_registry = Arc::clone(ctx.registry);
    let pump_client_id = ctx.client_id.to_string();
    let pump_instance_id = ctx.runner_instance_id.to_string();
    let pump_connection_id = ctx.connection_id.to_string();
    let pump_notify = Arc::clone(&ctx.notify);
    tokio::spawn(async move {
        loop {
            // Create the notified future before polling so an enqueue that
            // happens while poll returns None is not missed.
            let notified = pump_notify.notified();
            let poll_req = RunnerPollRequest {
                client_id: pump_client_id.clone(),
                runner_instance_id: pump_instance_id.clone(),
            };
            match pump_registry
                .poll_for_connection(poll_req, &pump_connection_id)
                .await
            {
                Ok(Some(request)) => {
                    // Do not retain/log SendError<RunnerEnvelope>: it can include
                    // command/stdin payloads. The semantic channel exit is enough.
                    if pump_tx
                        .send(RunnerEnvelope::Request { request })
                        .await
                        .is_err()
                    {
                        return PumpExit::ChannelClosed;
                    }
                }
                Ok(None) => notified.await,
                Err(error) => return classify_pump_poll_error(&error),
            }
        }
    })
}

async fn run_runner_session_with_pump(
    ctx: SessionContext<'_>,
    out_tx: mpsc::Sender<RunnerEnvelope>,
    mut reader: impl RunnerReader,
    writer_task: JoinHandle<WriterExit>,
    pump_task: JoinHandle<PumpExit>,
) {
    let SessionContext {
        registry,
        client_id,
        runner_instance_id,
        connection_id,
        notify: _,
        mut cancel,
        transport_label,
    } = ctx;

    let mut pump_task = pump_task;
    let mut writer_task = writer_task;
    let mut pump_observed = false;
    let mut writer_observed = false;

    loop {
        tokio::select! {
            pump = &mut pump_task => {
                pump_observed = true;
                let reason_code = match pump {
                    Ok(PumpExit::ChannelClosed) => "pump_channel_closed",
                    Ok(PumpExit::LeaseLost) => "pump_lease_lost",
                    Ok(PumpExit::RegistryFailed) => "pump_registry_failed",
                    Err(error) if error.is_panic() => "pump_task_panicked",
                    Err(_) => "pump_task_cancelled",
                };
                tracing::debug!(
                    client_id = client_id,
                    reason_code,
                    "runner {} request pump ended; terminating session",
                    transport_label
                );
                break;
            }
            writer = &mut writer_task => {
                writer_observed = true;
                let reason_code = match writer {
                    Ok(WriterExit::ChannelClosed) => "writer_channel_closed",
                    Ok(WriterExit::TransportFailed) => "writer_transport_failed",
                    Err(error) if error.is_panic() => "writer_task_panicked",
                    Err(_) => "writer_task_cancelled",
                };
                tracing::debug!(
                    client_id = client_id,
                    reason_code,
                    "runner {} writer ended; terminating session",
                    transport_label
                );
                break;
            }
            cancellation = cancel.changed() => {
                if cancellation.is_ok() && !*cancel.borrow() {
                    continue;
                }
                let reason_code = if cancellation.is_ok() {
                    "connection_replaced"
                } else {
                    "connection_cancel_channel_closed"
                };
                tracing::debug!(
                    client_id = client_id,
                    reason_code,
                    "runner {} session cancellation observed; terminating session",
                    transport_label
                );
                break;
            }
            received = reader.recv() => {
                match received {
                    RecvOutcome::Envelope(env) => {
                        let is_goodbye = matches!(&env, RunnerEnvelope::Goodbye { .. });
                        dispatch_inbound(
                            env,
                            registry,
                            client_id,
                            runner_instance_id,
                            connection_id,
                            &out_tx,
                            transport_label,
                        )
                        .await;
                        if is_goodbye {
                            break;
                        }
                    }
                    RecvOutcome::Skip => continue,
                    RecvOutcome::Closed => break,
                }
            }
        }
    }

    // Unified bounded teardown. An unobserved pump is aborted and joined before
    // the writer feed is dropped, ensuring its Sender clone cannot orphan the
    // writer. Exact connection reconciliation keeps every stale-A exit harmless
    // after a successful replacement has already committed B.
    if !pump_observed {
        pump_task.abort();
        match tokio::time::timeout(STREAM_TASK_JOIN_TIMEOUT, &mut pump_task).await {
            Ok(Ok(_)) | Ok(Err(_)) => {}
            Err(_) => {
                tracing::debug!(
                    client_id = client_id,
                    reason_code = "pump_join_timeout",
                    "runner {} request pump join timed out after abort",
                    transport_label
                );
            }
        }
    }

    drop(out_tx);
    if !writer_observed {
        match tokio::time::timeout(STREAM_TASK_JOIN_TIMEOUT, &mut writer_task).await {
            Ok(Ok(WriterExit::ChannelClosed)) => {}
            Ok(Ok(WriterExit::TransportFailed)) => {
                tracing::debug!(
                    client_id = client_id,
                    reason_code = "writer_transport_failed_during_teardown",
                    "runner {} writer failed during teardown",
                    transport_label
                );
            }
            Ok(Err(error)) => {
                let reason_code = if error.is_panic() {
                    "writer_task_panicked_during_teardown"
                } else {
                    "writer_task_cancelled_during_teardown"
                };
                tracing::debug!(
                    client_id = client_id,
                    reason_code,
                    "runner {} writer join failed during teardown",
                    transport_label
                );
            }
            Err(_) => {
                tracing::debug!(
                    client_id = client_id,
                    reason_code = "writer_join_timeout",
                    "runner {} writer join timed out; aborting writer",
                    transport_label
                );
                writer_task.abort();
            }
        }
    }
    registry
        .reconcile_disconnect_for_connection(client_id, runner_instance_id, connection_id)
        .await;
}

/// Dispatch one inbound envelope into the connection-scoped registry lease.
async fn dispatch_inbound(
    env: RunnerEnvelope,
    registry: &Arc<RunnerRegistry>,
    client_id: &str,
    runner_instance_id: &str,
    connection_id: &str,
    out_tx: &mpsc::Sender<RunnerEnvelope>,
    transport_label: &'static str,
) {
    match env {
        RunnerEnvelope::Result { payload } => {
            if payload.result.client_id != client_id
                || payload.result.runner_instance_id != runner_instance_id
            {
                tracing::warn!(
                    client_id = client_id,
                    "runner {} result rejected: envelope identity does not match registered connection",
                    transport_label
                );
                return;
            }
            // `complete_for_connection` refreshes `last_seen` internally only
            // when this connection still holds the lease; a late result on a
            // stale same-instance connection is still applied but does not
            // revive the new connection's liveness.
            if let Err(e) = registry
                .complete_for_connection(payload, connection_id)
                .await
            {
                tracing::warn!(
                    client_id = client_id,
                    error = %e,
                    "runner {} result rejected",
                    transport_label
                );
            }
        }
        RunnerEnvelope::PersistentShellResult { payload } => {
            if payload.client_id != client_id || payload.runner_instance_id != runner_instance_id {
                tracing::warn!(
                    client_id = client_id,
                    "runner {} persistent shell result rejected: envelope identity does not match registered connection",
                    transport_label
                );
                return;
            }
            if let Err(e) = registry
                .complete_persistent_shell_for_connection(payload, connection_id)
                .await
            {
                tracing::warn!(
                    client_id = client_id,
                    error = %e,
                    "runner {} persistent shell result rejected",
                    transport_label
                );
            }
        }
        RunnerEnvelope::JobUpdate { payload } => {
            if payload.client_id != client_id || payload.runner_instance_id != runner_instance_id {
                tracing::warn!(
                    client_id = client_id,
                    "runner {} job_update rejected: envelope identity does not match registered connection",
                    transport_label
                );
                return;
            }
            if let Err(e) = registry
                .update_job_for_connection(payload, connection_id)
                .await
            {
                tracing::warn!(
                    client_id = client_id,
                    error = %e,
                    "runner {} job_update rejected",
                    transport_label
                );
            }
        }
        RunnerEnvelope::Ping { ts } => {
            // Keepalive: refresh liveness before replying so an idle Runner (no
            // pending requests) is not aged out of the online window. Without
            // this touch a connected-but-idle Runner decays to "stale" even
            // though its socket is healthy.
            if let Err(e) = registry
                .touch_runner_for_connection(client_id, runner_instance_id, connection_id)
                .await
            {
                tracing::warn!(
                    client_id = client_id,
                    error = %e,
                    "runner {} ping liveness touch failed",
                    transport_label
                );
            }
            // Pong is best-effort: never block the reader if the outbound
            // channel is full (a slow Runner must not stall inbound processing).
            // try_send drops the pong when saturated; the Runner treats a
            // missing pong as a soft liveness signal, not a fatal error.
            if let Err(e) = out_tx.try_send(RunnerEnvelope::Pong { ts }) {
                let reason = match e {
                    tokio::sync::mpsc::error::TrySendError::Full(_) => "full",
                    tokio::sync::mpsc::error::TrySendError::Closed(_) => "closed",
                };
                tracing::debug!(
                    client_id = client_id,
                    reason,
                    "runner {} pong send dropped",
                    transport_label
                );
            }
        }
        RunnerEnvelope::Pong { .. } => {
            // Pong is a normal keepalive response. The server does not
            // currently originate Pings, but a Pong must still count as live
            // traffic so the client does not decay to stale, and must never be
            // treated as an unexpected envelope.
            if let Err(e) = registry
                .touch_runner_for_connection(client_id, runner_instance_id, connection_id)
                .await
            {
                tracing::debug!(
                    client_id = client_id,
                    error = %e,
                    "runner {} pong liveness touch failed",
                    transport_label
                );
            }
        }
        RunnerEnvelope::RuntimeMetadata {
            tool_providers,
            mcp_gateway_providers,
        } => {
            let _ = registry
                .update_runtime_metadata_for_connection(
                    client_id,
                    runner_instance_id,
                    connection_id,
                    Some(tool_providers),
                    mcp_gateway_providers,
                )
                .await;
        }
        RunnerEnvelope::ProjectInventoryPage { page } => {
            match registry
                .apply_project_inventory_page_for_connection(
                    client_id,
                    runner_instance_id,
                    connection_id,
                    page,
                )
                .await
            {
                Ok(status) => {
                    // Bounded best-effort acknowledgement. A full outbound
                    // channel must not make project inventory a liveness fence;
                    // the Runner can restart the snapshot on reconnect.
                    let _ = out_tx.try_send(RunnerEnvelope::ProjectInventoryStatus { status });
                }
                Err(error) => {
                    tracing::debug!(
                        client_id = client_id,
                        error = %error,
                        "runner project inventory page rejected by lease fence"
                    );
                }
            }
        }
        RunnerEnvelope::ProjectInventoryStatus { .. } => {
            // Server-to-Runner only; ignore if a peer reflects it.
        }
        RunnerEnvelope::Goodbye { reason } => {
            tracing::debug!(
                client_id = client_id,
                reason = reason.as_deref().unwrap_or("unspecified"),
                "runner {} sent goodbye",
                transport_label
            );
            registry
                .reconcile_disconnect_for_connection(client_id, runner_instance_id, connection_id)
                .await;
        }
        RunnerEnvelope::Register { .. } => {
            // Ignore a redundant register mid-session.
        }
        other => {
            tracing::debug!(
                client_id = client_id,
                kind = other.kind(),
                "runner {} received unexpected envelope; ignoring",
                transport_label
            );
        }
    }
}

#[cfg(test)]
#[path = "runner_session_tests.rs"]
mod tests;
