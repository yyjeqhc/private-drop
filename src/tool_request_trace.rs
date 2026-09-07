//! Gated lifecycle and forensic tracing for model-facing tool invocations.
//!
//! `WEBCODEX_TOOL_REQUEST_TRACE=true|metadata` preserves the historical
//! metadata-only behavior. `WEBCODEX_TOOL_REQUEST_TRACE=full` additionally
//! persists semantic JSON request/argument/result payloads on the Server host.
//! Full payloads are zstd-compressed files under a bounded trace directory; they
//! are deliberately not stored in the canonical runtime database. Compression,
//! filesystem persistence, reconciliation, and pruning run on a bounded dedicated
//! writer thread so diagnostic trace maintenance never blocks tool request workers.
//!
//! Full tracing is an explicit self-hosted operator diagnostic mode. It may
//! contain file contents, command input/output, user messages, or other tool
//! payload data. The trace path never reads WebCodex ingress HTTP Authorization
//! headers; credential-like values that are themselves part of a tool/Runner
//! payload are captured like any other payload field. Trace persistence is
//! fail-open: storage, compression, pruning, correlation failures, or writer
//! saturation never change tool execution correctness; saturated queues drop
//! diagnostic records instead of backpressuring tool execution.
//!
//! `*_tool_handler_returned` means the handler constructed a response and handed
//! it to the HTTP framework. It does **not** prove the client received the body;
//! combine with reverse-proxy `status` / `body_bytes_sent` / `request_time` for
//! that transport boundary.

use crate::client_window::ClientWindow;
use crate::config::ToolRequestTraceMode;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
#[cfg(unix)]
use std::{os::unix::fs::OpenOptionsExt, os::unix::fs::PermissionsExt};
use uuid::Uuid;

tokio::task_local! {
    static ACTIVE_TOOL_TRACE_ID: String;
}

const TRACE_CORRELATION_TTL_SECS: i64 = 24 * 60 * 60;
const MAX_TRACE_CORRELATIONS: usize = 8_192;
// Ordinary writes update accounting incrementally. A full recursive scan exists
// only to discover out-of-process drift, so keep it deliberately low-frequency.
const TRACE_STORE_RECONCILE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const TRACE_WRITER_QUEUE_CAPACITY: usize = 64;

static TRACE_IO_STATE: OnceLock<Mutex<TraceStoreAccounting>> = OnceLock::new();
static TRACE_CORRELATIONS: OnceLock<Mutex<TraceCorrelations>> = OnceLock::new();
static TRACE_WRITER: OnceLock<Option<TraceWriter>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraceCorrelation {
    trace_id: String,
    request_id: String,
    job_id: Option<String>,
    runner_kind: String,
    created_at: i64,
}

#[derive(Default)]
struct TraceCorrelations {
    requests: HashMap<String, TraceCorrelation>,
    jobs: HashMap<String, TraceCorrelation>,
}

#[derive(Debug, Clone)]
struct TraceDirInfo {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    evictable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraceStoreConfig {
    root: PathBuf,
    retention: Duration,
    budget: u64,
}

#[derive(Debug)]
enum TraceWrite {
    Metadata {
        trace_id: String,
        phase: String,
        event: Value,
        config: TraceStoreConfig,
    },
    Payload {
        trace_id: String,
        phase: String,
        value: Value,
        event: Value,
        config: TraceStoreConfig,
    },
    Flush(mpsc::Sender<()>),
}

const MAX_MODEL_TRACE_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_MODEL_TRACE_COMPRESSED_BYTES: usize = 512 * 1024;
const MAX_TRACE_EVENTS_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TRACE_PAYLOAD_ENTRIES: usize = 4096;
const DEFAULT_TRACE_INDEX_LIMIT: usize = 20;
const MAX_TRACE_INDEX_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct TraceReadError {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

impl TraceReadError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct TraceWriter {
    sender: mpsc::SyncSender<TraceWrite>,
}

#[derive(Debug)]
struct TraceStoreAccounting {
    config: Option<TraceStoreConfig>,
    initialized: bool,
    total_bytes: u64,
    traces: HashMap<String, TraceDirInfo>,
    last_reconcile: Option<Instant>,
    #[cfg(test)]
    filesystem_scans: u64,
}

impl Default for TraceStoreAccounting {
    fn default() -> Self {
        Self {
            config: None,
            initialized: false,
            total_bytes: 0,
            traces: HashMap::new(),
            last_reconcile: None,
            #[cfg(test)]
            filesystem_scans: 0,
        }
    }
}

impl TraceStoreAccounting {
    fn invalidate(&mut self) {
        self.initialized = false;
        self.last_reconcile = None;
    }
}

/// Whether tool-request lifecycle tracing is enabled.
pub fn tool_request_trace_enabled() -> bool {
    crate::config::tool_request_trace_enabled()
}

fn full_trace_enabled() -> bool {
    crate::config::tool_request_trace_mode() == ToolRequestTraceMode::Full
}

/// Generate a server-side trace id for one inbound handler invocation.
pub fn new_trace_id() -> String {
    Uuid::new_v4().to_string()
}

/// Scope the current async tool dispatch so deeper Server→Runner enqueue code can
/// correlate its durable Runner `request_id` with this model-facing trace.
pub async fn scope_active_trace<F>(trace_id: Option<String>, future: F) -> F::Output
where
    F: Future,
{
    match trace_id {
        Some(trace_id) => ACTIVE_TOOL_TRACE_ID.scope(trace_id, future).await,
        None => future.await,
    }
}

fn current_active_trace_id() -> Option<String> {
    ACTIVE_TOOL_TRACE_ID.try_with(Clone::clone).ok()
}

/// Opaque reference for the currently executing inbound request, but only when
/// the Server is actually retaining full payload traces. This never exposes the
/// native trace root or a filesystem path.
pub(crate) fn current_full_trace_ref() -> Option<String> {
    full_trace_enabled().then(current_active_trace_id).flatten()
}

/// Safe JSON-RPC id summary: type + length + short digest. Never the raw string.
pub fn jsonrpc_id_safe(id: Option<&serde_json::Value>) -> String {
    use serde_json::Value;
    match id {
        None => "none".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(v)) => format!("bool:{v}"),
        Some(Value::Number(n)) => format!("number:{n}"),
        Some(Value::String(s)) => {
            let digest = Sha256::digest(s.as_bytes());
            format!(
                "string:len={}:sha256_8={:02x}{:02x}{:02x}{:02x}",
                s.len(),
                digest[0],
                digest[1],
                digest[2],
                digest[3]
            )
        }
        Some(Value::Array(a)) => {
            let raw = serde_json::to_vec(a).unwrap_or_default();
            let digest = Sha256::digest(&raw);
            format!(
                "array:len={}:sha256_8={:02x}{:02x}{:02x}{:02x}",
                a.len(),
                digest[0],
                digest[1],
                digest[2],
                digest[3]
            )
        }
        Some(Value::Object(o)) => {
            let raw = serde_json::to_vec(o).unwrap_or_default();
            let digest = Sha256::digest(&raw);
            format!(
                "object:keys={}:sha256_8={:02x}{:02x}{:02x}{:02x}",
                o.len(),
                digest[0],
                digest[1],
                digest[2],
                digest[3]
            )
        }
    }
}

/// Estimate serialized JSON byte length for diagnostics.
///
/// Returns `None` when tracing is disabled so callers never pay for a size-only
/// serialization of the response body.
pub fn estimate_json_bytes(value: &serde_json::Value) -> Option<usize> {
    if !tool_request_trace_enabled() {
        return None;
    }
    serde_json::to_vec(value).ok().map(|bytes| bytes.len())
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn safe_phase(phase: &str) -> String {
    let safe: String = phase
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    if safe.is_empty() {
        "payload".to_string()
    } else {
        safe
    }
}

fn trace_root() -> PathBuf {
    crate::config::tool_request_trace_dir()
}

fn trace_retention() -> Duration {
    Duration::from_secs(crate::config::tool_request_trace_retention_hours().saturating_mul(60 * 60))
}

fn trace_budget() -> u64 {
    crate::config::tool_request_trace_max_total_bytes()
}

fn trace_store_config() -> TraceStoreConfig {
    TraceStoreConfig {
        root: trace_root(),
        retention: trace_retention(),
        budget: trace_budget(),
    }
}

fn trace_io_state() -> &'static Mutex<TraceStoreAccounting> {
    TRACE_IO_STATE.get_or_init(|| Mutex::new(TraceStoreAccounting::default()))
}

impl TraceWriter {
    fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(TRACE_WRITER_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("webcodex-tool-trace-writer".to_string())
            .spawn(move || trace_writer_loop(receiver))?;
        Ok(Self { sender })
    }
}

fn trace_writer() -> Option<&'static TraceWriter> {
    TRACE_WRITER
        .get_or_init(|| match TraceWriter::start() {
            Ok(writer) => Some(writer),
            Err(error) => {
                tracing::warn!(
                    event = "tool_trace_writer_unavailable",
                    error = %error,
                    "tool_trace_writer_unavailable"
                );
                None
            }
        })
        .as_ref()
}

fn trace_writer_loop(receiver: mpsc::Receiver<TraceWrite>) {
    while let Ok(write) = receiver.recv() {
        match write {
            TraceWrite::Metadata {
                trace_id,
                phase,
                event,
                config,
            } => match persist_metadata_event_with_config(&trace_id, event, &config) {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    event = "tool_trace_capture_omitted",
                    server_trace_id = %trace_id,
                    phase = %phase,
                    reason = "trace_disk_budget_exceeded",
                    "tool_trace_capture_omitted"
                ),
                Err(error) => tracing::warn!(
                    event = "tool_trace_capture_failed",
                    server_trace_id = %trace_id,
                    phase = %phase,
                    error = %error,
                    "tool_trace_capture_failed"
                ),
            },
            TraceWrite::Payload {
                trace_id,
                phase,
                value,
                event,
                config,
            } => match persist_payload_with_config(&trace_id, &phase, &value, event, &config) {
                Ok(Some((payload_bytes, compressed_bytes, digest, path))) => tracing::info!(
                    event = "tool_trace_payload_persisted",
                    server_trace_id = %trace_id,
                    phase = %phase,
                    payload_bytes = payload_bytes as u64,
                    compressed_bytes = compressed_bytes as u64,
                    payload_sha256 = %digest,
                    payload_path = %path,
                    "tool_trace_payload_persisted"
                ),
                Ok(None) => tracing::warn!(
                    event = "tool_trace_capture_omitted",
                    server_trace_id = %trace_id,
                    phase = %phase,
                    reason = "trace_disk_budget_exceeded",
                    "tool_trace_capture_omitted"
                ),
                Err(error) => tracing::warn!(
                    event = "tool_trace_capture_failed",
                    server_trace_id = %trace_id,
                    phase = %phase,
                    error = %error,
                    "tool_trace_capture_failed"
                ),
            },
            TraceWrite::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn flush_trace_writer_for_read() -> Result<(), TraceReadError> {
    let Some(writer) = trace_writer() else {
        return Err(TraceReadError::new(
            "trace_store_unavailable",
            "full trace writer is unavailable",
        ));
    };
    let (done_tx, done_rx) = mpsc::channel();
    match writer.sender.try_send(TraceWrite::Flush(done_tx)) {
        Ok(()) => done_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            TraceReadError::new("trace_store_busy", "full trace writer flush timed out")
        }),
        Err(mpsc::TrySendError::Full(_)) => Err(TraceReadError::new(
            "trace_store_busy",
            "full trace writer queue is busy; retry the diagnostic read",
        )),
        Err(mpsc::TrySendError::Disconnected(_)) => Err(TraceReadError::new(
            "trace_store_unavailable",
            "full trace writer is disconnected",
        )),
    }
}

#[derive(Debug, Clone)]
struct IndexedTracePayload {
    phase: String,
    payload_bytes: usize,
    compressed_bytes: usize,
    payload_sha256: String,
    file_name: String,
}

fn invalid_trace_ref() -> TraceReadError {
    TraceReadError::new(
        "invalid_trace_ref",
        "trace_ref must be the canonical UUID returned by an eligible failed tool call",
    )
}

fn validate_trace_ref(trace_ref: &str) -> Result<(), TraceReadError> {
    let parsed = Uuid::parse_str(trace_ref).map_err(|_| invalid_trace_ref())?;
    if parsed.to_string() != trace_ref {
        return Err(invalid_trace_ref());
    }
    Ok(())
}

fn require_private_regular_file(
    path: &Path,
    kind: &'static str,
) -> Result<fs::Metadata, TraceReadError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            TraceReadError::new(
                "trace_not_found",
                "trace data is unavailable or has expired",
            )
        } else {
            TraceReadError::new(kind, "trace storage could not be inspected safely")
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TraceReadError::new(
            kind,
            "trace storage failed the regular-file safety check",
        ));
    }
    Ok(metadata)
}

fn require_private_directory(
    path: &Path,
    not_found_kind: &'static str,
) -> Result<(), TraceReadError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            TraceReadError::new(not_found_kind, "trace data is unavailable or has expired")
        } else {
            TraceReadError::new(
                "trace_store_unavailable",
                "trace storage could not be inspected safely",
            )
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TraceReadError::new(
            "trace_corrupt",
            "trace storage failed the directory safety check",
        ));
    }
    Ok(())
}

fn parse_trace_payload_index(
    trace_ref: &str,
    trace_dir: &Path,
) -> Result<Vec<IndexedTracePayload>, TraceReadError> {
    require_private_regular_file(&trace_dir.join(TRACE_OWNER_MARKER), "trace_corrupt")?;
    let events_path = trace_dir.join("events.jsonl");
    let metadata = require_private_regular_file(&events_path, "trace_corrupt")?;
    if metadata.len() > MAX_TRACE_EVENTS_FILE_BYTES {
        return Err(TraceReadError::new(
            "trace_too_large",
            "trace event index exceeds the diagnostic read ceiling",
        ));
    }
    let text = fs::read_to_string(events_path)
        .map_err(|_| TraceReadError::new("trace_corrupt", "trace event index is unreadable"))?;
    let mut payloads = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line).map_err(|_| {
            TraceReadError::new("trace_corrupt", "trace event index contains invalid JSON")
        })?;
        if event.get("event").and_then(Value::as_str) != Some("tool_trace_payload_captured") {
            continue;
        }
        if event.get("server_trace_id").and_then(Value::as_str) != Some(trace_ref)
            || event.get("encoding").and_then(Value::as_str) != Some("json+zstd")
        {
            return Err(TraceReadError::new(
                "trace_corrupt",
                "trace payload index correlation is invalid",
            ));
        }
        let phase = event
            .get("phase")
            .and_then(Value::as_str)
            .filter(|phase| !phase.is_empty() && phase.len() <= 80 && safe_phase(phase) == *phase)
            .ok_or_else(|| TraceReadError::new("trace_corrupt", "trace payload phase is invalid"))?
            .to_string();
        let payload_bytes = event
            .get("payload_bytes")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| TraceReadError::new("trace_corrupt", "trace payload size is invalid"))?;
        let compressed_bytes = event
            .get("compressed_bytes")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                TraceReadError::new("trace_corrupt", "trace compressed size is invalid")
            })?;
        let payload_sha256 = event
            .get("payload_sha256")
            .and_then(Value::as_str)
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| TraceReadError::new("trace_corrupt", "trace payload digest is invalid"))?
            .to_string();
        let payload_path = event
            .get("payload_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TraceReadError::new("trace_corrupt", "trace payload location is invalid")
            })?;
        let Some(file_name) = payload_path.strip_prefix("payloads/") else {
            return Err(TraceReadError::new(
                "trace_corrupt",
                "trace payload location is outside the owned payload directory",
            ));
        };
        if file_name.is_empty()
            || file_name.contains('/')
            || file_name.contains('\\')
            || file_name.contains("..")
            || !file_name.ends_with(".json.zst")
        {
            return Err(TraceReadError::new(
                "trace_corrupt",
                "trace payload filename failed the safety check",
            ));
        }
        payloads.push(IndexedTracePayload {
            phase,
            payload_bytes,
            compressed_bytes,
            payload_sha256,
            file_name: file_name.to_string(),
        });
        if payloads.len() > MAX_TRACE_PAYLOAD_ENTRIES {
            return Err(TraceReadError::new(
                "trace_too_large",
                "trace payload index exceeds the diagnostic entry ceiling",
            ));
        }
    }
    Ok(payloads)
}

fn trace_payload_metadata(index: usize, payload: &IndexedTracePayload) -> Value {
    json!({
        "payload_index": index,
        "phase": payload.phase,
        "payload_bytes": payload.payload_bytes,
        "compressed_bytes": payload.compressed_bytes,
        "payload_sha256": payload.payload_sha256,
        "payload_available": payload.payload_bytes <= MAX_MODEL_TRACE_PAYLOAD_BYTES,
    })
}

pub(crate) fn read_full_trace(
    trace_ref: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    payload_index: Option<usize>,
) -> Result<Value, TraceReadError> {
    if !full_trace_enabled() {
        return Err(TraceReadError::new(
            "trace_mode_not_full",
            "full tool-request tracing is not enabled on this Server",
        ));
    }
    validate_trace_ref(trace_ref)?;
    if payload_index.is_some() && (offset.is_some() || limit.is_some()) {
        return Err(TraceReadError::new(
            "invalid_trace_request",
            "offset and limit cannot be combined with payload_index",
        ));
    }
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(DEFAULT_TRACE_INDEX_LIMIT);
    if offset > 4095 || !(1..=MAX_TRACE_INDEX_LIMIT).contains(&limit) {
        return Err(TraceReadError::new(
            "invalid_trace_request",
            "trace listing bounds are outside the supported range",
        ));
    }
    if payload_index.is_some_and(|index| index > 4095) {
        return Err(TraceReadError::new(
            "invalid_trace_request",
            "payload_index is outside the supported range",
        ));
    }

    flush_trace_writer_for_read()?;
    let trace_dir = trace_root().join(trace_ref);
    require_private_directory(&trace_dir, "trace_not_found")?;
    let payloads = parse_trace_payload_index(trace_ref, &trace_dir)?;

    let Some(payload_index) = payload_index else {
        let returned = payloads
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(index, payload)| trace_payload_metadata(index, payload))
            .collect::<Vec<_>>();
        let next_offset =
            (offset + returned.len() < payloads.len()).then_some(offset + returned.len());
        return Ok(json!({
            "trace_ref": trace_ref,
            "trace_mode": "full",
            "payload_count": payloads.len(),
            "returned_count": returned.len(),
            "offset": offset,
            "next_offset": next_offset,
            "payloads": returned,
            "max_payload_bytes": MAX_MODEL_TRACE_PAYLOAD_BYTES,
        }));
    };

    let Some(indexed) = payloads.get(payload_index) else {
        return Err(TraceReadError::new(
            "invalid_payload_index",
            "payload_index does not identify a retained payload in this trace",
        ));
    };
    if indexed.payload_bytes > MAX_MODEL_TRACE_PAYLOAD_BYTES {
        return Ok(json!({
            "trace_ref": trace_ref,
            "trace_mode": "full",
            "payload_index": payload_index,
            "phase": indexed.phase,
            "payload_bytes": indexed.payload_bytes,
            "payload_sha256": indexed.payload_sha256,
            "payload_available": false,
            "max_payload_bytes": MAX_MODEL_TRACE_PAYLOAD_BYTES,
            "reason": "payload_exceeds_model_read_limit",
        }));
    }
    if indexed.compressed_bytes > MAX_MODEL_TRACE_COMPRESSED_BYTES {
        return Err(TraceReadError::new(
            "trace_corrupt",
            "trace payload compressed size exceeds the diagnostic read ceiling",
        ));
    }

    let payload_dir = trace_dir.join("payloads");
    require_private_directory(&payload_dir, "trace_corrupt")?;
    let payload_path = payload_dir.join(&indexed.file_name);
    let metadata = require_private_regular_file(&payload_path, "trace_corrupt")?;
    if metadata.len() != indexed.compressed_bytes as u64 {
        return Err(TraceReadError::new(
            "trace_corrupt",
            "trace payload compressed size does not match its index",
        ));
    }
    let file = File::open(payload_path)
        .map_err(|_| TraceReadError::new("trace_corrupt", "trace payload is unreadable"))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|_| TraceReadError::new("trace_corrupt", "trace payload decompression failed"))?;
    let mut raw = Vec::with_capacity(indexed.payload_bytes);
    decoder
        .take((MAX_MODEL_TRACE_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|_| TraceReadError::new("trace_corrupt", "trace payload decompression failed"))?;
    if raw.len() != indexed.payload_bytes || sha256_hex(&raw) != indexed.payload_sha256 {
        return Err(TraceReadError::new(
            "trace_corrupt",
            "trace payload failed its size or digest check",
        ));
    }
    let payload: Value = serde_json::from_slice(&raw)
        .map_err(|_| TraceReadError::new("trace_corrupt", "trace payload is not valid JSON"))?;
    Ok(json!({
        "trace_ref": trace_ref,
        "trace_mode": "full",
        "payload_index": payload_index,
        "phase": indexed.phase,
        "payload_bytes": indexed.payload_bytes,
        "payload_sha256": indexed.payload_sha256,
        "payload_available": true,
        "max_payload_bytes": MAX_MODEL_TRACE_PAYLOAD_BYTES,
        "payload": payload,
    }))
}

fn enqueue_trace_write(write: TraceWrite, trace_id: &str, phase: &str) {
    let Some(writer) = trace_writer() else {
        tracing::warn!(
            event = "tool_trace_capture_failed",
            server_trace_id = %trace_id,
            phase = phase,
            error = "trace writer unavailable",
            "tool_trace_capture_failed"
        );
        return;
    };
    match writer.sender.try_send(write) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => tracing::warn!(
            event = "tool_trace_capture_omitted",
            server_trace_id = %trace_id,
            phase = phase,
            reason = "trace_writer_queue_full",
            "tool_trace_capture_omitted"
        ),
        Err(mpsc::TrySendError::Disconnected(_)) => tracing::warn!(
            event = "tool_trace_capture_failed",
            server_trace_id = %trace_id,
            phase = phase,
            error = "trace writer disconnected",
            "tool_trace_capture_failed"
        ),
    }
}

fn enqueue_metadata_event(trace_id: &str, phase: &str, event: Value) {
    enqueue_trace_write(
        TraceWrite::Metadata {
            trace_id: trace_id.to_string(),
            phase: phase.to_string(),
            event,
            config: trace_store_config(),
        },
        trace_id,
        phase,
    );
}

#[cfg(test)]
pub(crate) fn flush_full_trace_writer() {
    let Some(writer) = trace_writer() else {
        panic!("full trace writer unavailable");
    };
    let (done_tx, done_rx) = mpsc::channel();
    writer
        .sender
        .send(TraceWrite::Flush(done_tx))
        .expect("full trace writer disconnected before flush");
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("full trace writer flush timed out");
}

fn directory_stats(path: &Path) -> io::Result<(u64, SystemTime)> {
    let metadata = fs::metadata(path)?;
    let mut bytes = if metadata.is_file() {
        metadata.len()
    } else {
        0
    };
    let mut modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let (child_bytes, child_modified) = directory_stats(&entry.path())?;
            bytes = bytes.saturating_add(child_bytes);
            if child_modified > modified {
                modified = child_modified;
            }
        }
    }
    Ok((bytes, modified))
}

const TRACE_OWNER_MARKER: &str = ".webcodex-tool-trace";

fn trace_dir_owned_by_store(path: &Path, active_trace_id: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == active_trace_id || path.join(TRACE_OWNER_MARKER).is_file()
}

fn trace_dirs(root: &Path, active_trace_id: &str) -> io::Result<Vec<TraceDirInfo>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if !trace_dir_owned_by_store(&path, active_trace_id) {
            continue;
        }
        let (bytes, modified) = directory_stats(&path)?;
        let evictable = path.join(TRACE_OWNER_MARKER).is_file();
        dirs.push(TraceDirInfo {
            path,
            bytes,
            modified,
            evictable,
        });
    }
    Ok(dirs)
}

fn create_private_trace_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn ensure_trace_owner_marker(trace_dir: &Path) -> io::Result<()> {
    let marker = trace_dir.join(TRACE_OWNER_MARKER);
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(marker)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn open_private_append(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn remove_accounted_trace(accounting: &mut TraceStoreAccounting, trace_id: &str) {
    if let Some(info) = accounting.traces.remove(trace_id) {
        accounting.total_bytes = accounting.total_bytes.saturating_sub(info.bytes);
    }
}

fn scan_trace_store(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    active_trace_id: &str,
) -> io::Result<(HashMap<String, TraceDirInfo>, u64)> {
    #[cfg(test)]
    {
        accounting.filesystem_scans = accounting.filesystem_scans.saturating_add(1);
    }

    let previous = accounting
        .config
        .as_ref()
        .is_some_and(|previous| previous.root == config.root)
        .then(|| accounting.traces.clone());
    let mut traces = HashMap::new();
    let mut total_bytes = 0_u64;
    for info in trace_dirs(&config.root, active_trace_id)? {
        let Some(trace_id) = info
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        total_bytes = total_bytes.saturating_add(info.bytes);
        traces.insert(trace_id, info);
    }

    // A failed cleanup can remove the owner marker before remove_dir_all reports
    // failure. Keep any process-known residue in the budget until it disappears;
    // never make it evictable without a current owner marker.
    if let Some(previous) = previous {
        for (trace_id, mut info) in previous {
            if traces.contains_key(&trace_id) || !info.path.exists() {
                continue;
            }
            match directory_stats(&info.path) {
                Ok((bytes, modified)) => {
                    info.bytes = bytes;
                    info.modified = modified;
                    info.evictable = false;
                    total_bytes = total_bytes.saturating_add(bytes);
                    traces.insert(trace_id, info);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }

    Ok((traces, total_bytes))
}

fn prune_expired_traces(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    active_trace_id: &str,
) -> io::Result<()> {
    let now = SystemTime::now();
    let expired = accounting
        .traces
        .iter()
        .filter(|(trace_id, info)| {
            trace_id.as_str() != active_trace_id
                && info.evictable
                && now
                    .duration_since(info.modified)
                    .map(|age| age > config.retention)
                    .unwrap_or(false)
        })
        .map(|(trace_id, _)| trace_id.clone())
        .collect::<Vec<_>>();

    for trace_id in expired {
        let Some(path) = accounting
            .traces
            .get(&trace_id)
            .map(|info| info.path.clone())
        else {
            continue;
        };
        if !path.join(TRACE_OWNER_MARKER).is_file() {
            if let Some(info) = accounting.traces.get_mut(&trace_id) {
                info.evictable = false;
            }
            accounting.invalidate();
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => remove_accounted_trace(accounting, &trace_id),
            Err(_) => {
                // Keep the old byte count rather than assuming any partial
                // cleanup succeeded. The next foreground capture reconciles it.
                accounting.invalidate();
            }
        }
    }
    Ok(())
}

fn reconcile_trace_store(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    active_trace_id: &str,
) -> io::Result<()> {
    fs::create_dir_all(&config.root)?;
    let (traces, total_bytes) = scan_trace_store(accounting, config, active_trace_id)?;
    accounting.config = Some(config.clone());
    accounting.initialized = true;
    accounting.total_bytes = total_bytes;
    accounting.traces = traces;
    accounting.last_reconcile = Some(Instant::now());
    prune_expired_traces(accounting, config, active_trace_id)?;
    Ok(())
}

fn accounting_requires_reconcile(
    accounting: &TraceStoreAccounting,
    config: &TraceStoreConfig,
) -> bool {
    !accounting.initialized
        || accounting.config.as_ref() != Some(config)
        || accounting
            .last_reconcile
            .map(|last| last.elapsed() >= TRACE_STORE_RECONCILE_INTERVAL)
            .unwrap_or(true)
}

/// Enforce age and total-byte bounds before one new file/event is persisted.
/// The first operation, configuration changes, invalidation, and low-frequency
/// foreground maintenance rebuild accounting from the filesystem. Ordinary
/// captures use the cached total and trace index without recursively scanning
/// the store. The active trace is never deleted out from under its own write.
fn reserve_trace_capacity(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    trace_id: &str,
    incoming: u64,
) -> io::Result<bool> {
    fs::create_dir_all(&config.root)?;
    if accounting_requires_reconcile(accounting, config) {
        reconcile_trace_store(accounting, config, trace_id)?;
    }
    if accounting.total_bytes.saturating_add(incoming) <= config.budget {
        return Ok(true);
    }

    let mut candidates = accounting
        .traces
        .iter()
        .filter(|(candidate_id, info)| candidate_id.as_str() != trace_id && info.evictable)
        .map(|(candidate_id, info)| (candidate_id.clone(), info.modified))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(_, modified)| *modified);

    for (candidate_id, _) in candidates {
        if accounting.total_bytes.saturating_add(incoming) <= config.budget {
            break;
        }
        let Some(path) = accounting
            .traces
            .get(&candidate_id)
            .map(|info| info.path.clone())
        else {
            continue;
        };
        // Revalidate ownership immediately before destructive cleanup. This is
        // O(1) and closes the external/manual marker-removal race without
        // reintroducing recursive hot-path traversal.
        if !path.join(TRACE_OWNER_MARKER).is_file() {
            if let Some(info) = accounting.traces.get_mut(&candidate_id) {
                info.evictable = false;
            }
            accounting.invalidate();
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => remove_accounted_trace(accounting, &candidate_id),
            Err(error) => {
                // Do not deduct a failed eviction. The unchanged cached byte
                // count is conservative even if remove_dir_all made partial
                // progress; force a filesystem rebuild before the next capture.
                accounting.invalidate();
                return Err(error);
            }
        }
    }

    Ok(accounting.total_bytes.saturating_add(incoming) <= config.budget)
}

fn commit_trace_write(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    trace_id: &str,
    bytes: u64,
) {
    let now = SystemTime::now();
    let entry = accounting
        .traces
        .entry(trace_id.to_string())
        .or_insert_with(|| TraceDirInfo {
            path: config.root.join(trace_id),
            bytes: 0,
            modified: now,
            evictable: true,
        });
    entry.bytes = entry.bytes.saturating_add(bytes);
    entry.modified = now;
    entry.evictable = true;
    accounting.total_bytes = accounting.total_bytes.saturating_add(bytes);
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn base_event(trace_id: &str, event: &str) -> Value {
    let build = crate::build_info::current();
    json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "event": event,
        "server_trace_id": trace_id,
        "server_version": build.version,
        "server_git_commit": build.git_commit,
        "server_git_dirty": build.git_dirty,
    })
}

fn merge_event_fields(event: &mut Value, fields: Value) {
    let (Some(event), Value::Object(fields)) = (event.as_object_mut(), fields) else {
        return;
    };
    event.extend(fields);
}

fn append_event_locked(
    accounting: &mut TraceStoreAccounting,
    config: &TraceStoreConfig,
    trace_id: &str,
    event: &Value,
) -> io::Result<bool> {
    let mut line = serde_json::to_vec(event).map_err(io::Error::other)?;
    line.push(b'\n');
    if !reserve_trace_capacity(accounting, config, trace_id, line.len() as u64)? {
        return Ok(false);
    }
    let trace_dir = config.root.join(trace_id);
    create_private_trace_dir(&trace_dir)?;
    ensure_trace_owner_marker(&trace_dir)?;
    let mut file = open_private_append(&trace_dir.join("events.jsonl"))?;
    if let Err(error) = file.write_all(&line) {
        // write_all may have appended a prefix. Force the next foreground
        // operation to rescan rather than undercount an uncertain file length.
        accounting.invalidate();
        return Err(error);
    }
    commit_trace_write(accounting, config, trace_id, line.len() as u64);
    Ok(true)
}

fn persist_metadata_event_with_config(
    trace_id: &str,
    event: Value,
    config: &TraceStoreConfig,
) -> io::Result<bool> {
    let mut accounting = trace_io_state()
        .lock()
        .map_err(|_| io::Error::other("tool trace I/O lock poisoned"))?;
    append_event_locked(&mut accounting, config, trace_id, &event)
}

#[cfg(test)]
fn persist_metadata_event(trace_id: &str, event: Value) -> io::Result<bool> {
    let config = trace_store_config();
    persist_metadata_event_with_config(trace_id, event, &config)
}

fn persist_payload_with_config(
    trace_id: &str,
    phase: &str,
    value: &Value,
    mut event: Value,
    config: &TraceStoreConfig,
) -> io::Result<Option<(usize, usize, String, String)>> {
    let raw = serde_json::to_vec(value).map_err(io::Error::other)?;
    let digest = sha256_hex(&raw);
    let compressed = zstd::stream::encode_all(&raw[..], 3)?;
    let file_name = format!("{}-{}.json.zst", Uuid::new_v4(), safe_phase(phase));
    let relative_path = format!("payloads/{file_name}");
    merge_event_fields(
        &mut event,
        json!({
            "phase": phase,
            "payload_bytes": raw.len(),
            "compressed_bytes": compressed.len(),
            "payload_sha256": digest,
            "payload_path": relative_path,
            "encoding": "json+zstd",
        }),
    );
    let mut event_line = serde_json::to_vec(&event).map_err(io::Error::other)?;
    event_line.push(b'\n');
    let incoming = (compressed.len() + event_line.len()) as u64;

    let mut accounting = trace_io_state()
        .lock()
        .map_err(|_| io::Error::other("tool trace I/O lock poisoned"))?;
    if !reserve_trace_capacity(&mut accounting, config, trace_id, incoming)? {
        return Ok(None);
    }
    let trace_dir = config.root.join(trace_id);
    let payload_dir = trace_dir.join("payloads");
    create_private_trace_dir(&trace_dir)?;
    ensure_trace_owner_marker(&trace_dir)?;
    create_private_trace_dir(&payload_dir)?;
    let final_path = payload_dir.join(&file_name);
    let temp_path = payload_dir.join(format!(".{file_name}.tmp"));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut temp_file = options.open(&temp_path)?;
    #[cfg(unix)]
    if let Err(error) = temp_file.set_permissions(fs::Permissions::from_mode(0o600)) {
        drop(temp_file);
        if remove_file_if_present(&temp_path).is_err() {
            accounting.invalidate();
        }
        return Err(error);
    }
    if let Err(error) = temp_file.write_all(&compressed) {
        drop(temp_file);
        if remove_file_if_present(&temp_path).is_err() {
            accounting.invalidate();
        }
        return Err(error);
    }
    if let Err(error) = temp_file.flush() {
        drop(temp_file);
        if remove_file_if_present(&temp_path).is_err() {
            accounting.invalidate();
        }
        return Err(error);
    }
    drop(temp_file);
    if let Err(error) = fs::rename(&temp_path, &final_path) {
        if remove_file_if_present(&temp_path).is_err() {
            accounting.invalidate();
        }
        return Err(error);
    }

    let mut event_file = match open_private_append(&trace_dir.join("events.jsonl")) {
        Ok(file) => file,
        Err(error) => {
            if remove_file_if_present(&final_path).is_err() {
                accounting.invalidate();
            }
            return Err(error);
        }
    };
    if let Err(error) = event_file.write_all(&event_line) {
        // The append may have written a prefix. Even if payload cleanup succeeds,
        // the store total is uncertain until the next reconciliation.
        let _ = remove_file_if_present(&final_path);
        accounting.invalidate();
        return Err(error);
    }

    commit_trace_write(&mut accounting, config, trace_id, incoming);
    Ok(Some((raw.len(), compressed.len(), digest, relative_path)))
}

#[cfg(test)]
fn persist_payload(
    trace_id: &str,
    phase: &str,
    value: &Value,
) -> io::Result<Option<(usize, usize, String, String)>> {
    let config = trace_store_config();
    persist_payload_with_config(
        trace_id,
        phase,
        value,
        base_event(trace_id, "tool_trace_payload_captured"),
        &config,
    )
}

fn capture_payload_for_trace(trace_id: &str, phase: &str, value: &Value) {
    if !full_trace_enabled() {
        return;
    }
    enqueue_trace_write(
        TraceWrite::Payload {
            trace_id: trace_id.to_string(),
            phase: phase.to_string(),
            value: value.clone(),
            event: base_event(trace_id, "tool_trace_payload_captured"),
            config: trace_store_config(),
        },
        trace_id,
        phase,
    );
}

fn correlations() -> &'static Mutex<TraceCorrelations> {
    TRACE_CORRELATIONS.get_or_init(|| Mutex::new(TraceCorrelations::default()))
}

fn prune_correlations(correlations: &mut TraceCorrelations) {
    let cutoff = now_ts().saturating_sub(TRACE_CORRELATION_TTL_SECS);
    correlations
        .requests
        .retain(|_, correlation| correlation.created_at >= cutoff);
    correlations
        .jobs
        .retain(|_, correlation| correlation.created_at >= cutoff);
    while correlations.requests.len() > MAX_TRACE_CORRELATIONS {
        let Some(oldest) = correlations
            .requests
            .iter()
            .min_by_key(|(_, correlation)| correlation.created_at)
            .map(|(request_id, _)| request_id.clone())
        else {
            break;
        };
        correlations.requests.remove(&oldest);
    }
    while correlations.jobs.len() > MAX_TRACE_CORRELATIONS {
        let Some(oldest) = correlations
            .jobs
            .iter()
            .min_by_key(|(_, correlation)| correlation.created_at)
            .map(|(job_id, _)| job_id.clone())
        else {
            break;
        };
        correlations.jobs.remove(&oldest);
    }
}

/// Record the exact Server→Runner request identity selected for the current
/// model-facing tool call. This keeps only a bounded in-memory correlation index;
/// full request bodies remain in the file-backed trace store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_runner_request_enqueued<T: Serialize>(
    request_payload: &T,
    request_id: &str,
    client_id: &str,
    kind: &str,
    job_id: Option<&str>,
    runner_instance_id: Option<&str>,
    runner_transport: Option<&str>,
    runner_version: Option<&str>,
    runner_git_commit: Option<&str>,
) {
    let Some(trace_id) = current_active_trace_id() else {
        return;
    };
    if full_trace_enabled() {
        match serde_json::to_value(request_payload) {
            Ok(value) => capture_payload_for_trace(&trace_id, "runner_request", &value),
            Err(error) => tracing::warn!(
                event = "tool_trace_capture_failed",
                server_trace_id = %trace_id,
                phase = "runner_request",
                error = %error,
                "tool_trace_capture_failed"
            ),
        }
    }
    tracing::info!(
        event = "tool_runner_request_enqueued",
        server_trace_id = %trace_id,
        runner_request_id = request_id,
        runner_client_id = client_id,
        runner_request_kind = kind,
        runner_job_id = job_id.unwrap_or("-"),
        runner_agent_instance_id = runner_instance_id.unwrap_or("-"),
        runner_transport = runner_transport.unwrap_or("-"),
        runner_version = runner_version.unwrap_or("-"),
        runner_git_commit = runner_git_commit.unwrap_or("-"),
        "tool_runner_request_enqueued"
    );

    if let Ok(mut correlations) = correlations().lock() {
        prune_correlations(&mut correlations);
        let correlation = TraceCorrelation {
            trace_id: trace_id.clone(),
            request_id: request_id.to_string(),
            job_id: job_id.map(str::to_string),
            runner_kind: kind.to_string(),
            created_at: now_ts(),
        };
        correlations
            .requests
            .insert(request_id.to_string(), correlation.clone());
        if let Some(job_id) = job_id {
            correlations.jobs.insert(job_id.to_string(), correlation);
        }
    }

    if full_trace_enabled() {
        let mut event = base_event(&trace_id, "tool_runner_request_enqueued");
        merge_event_fields(
            &mut event,
            json!({
                "runner_request_id": request_id,
                "runner_client_id": client_id,
                "runner_request_kind": kind,
                "runner_job_id": job_id,
                "runner_agent_instance_id": runner_instance_id,
                "runner_transport": runner_transport,
                "runner_version": runner_version,
                "runner_git_commit": runner_git_commit,
            }),
        );
        enqueue_metadata_event(&trace_id, "runner_request_enqueued", event);
    }
}

fn lookup_request_correlation(request_id: &str) -> Option<TraceCorrelation> {
    let mut correlations = correlations().lock().ok()?;
    prune_correlations(&mut correlations);
    correlations.requests.get(request_id).cloned()
}

fn resolve_job_correlation(
    correlations: &TraceCorrelations,
    request_id: Option<&str>,
    job_id: &str,
) -> Option<TraceCorrelation> {
    let job_correlation = correlations.jobs.get(job_id).cloned()?;
    let Some(request_id) = request_id else {
        return Some(job_correlation);
    };
    let request_correlation = correlations.requests.get(request_id).cloned()?;
    (request_correlation == job_correlation).then_some(request_correlation)
}

fn lookup_job_correlation(request_id: Option<&str>, job_id: &str) -> Option<TraceCorrelation> {
    let mut correlations = correlations().lock().ok()?;
    prune_correlations(&mut correlations);
    resolve_job_correlation(&correlations, request_id, job_id)
}

fn remove_correlation(correlation: &TraceCorrelation) {
    if let Ok(mut correlations) = correlations().lock() {
        correlations.requests.remove(&correlation.request_id);
        if let Some(job_id) = correlation.job_id.as_deref() {
            correlations.jobs.remove(job_id);
        }
    }
}

/// Capture one authoritative Runner result after the shell-client layer has
/// accepted its client / instance / request ownership. Capture never consumes
/// correlation; finalization is a separate post-acceptance step.
pub(crate) fn capture_runner_result<T: Serialize>(request_id: &str, payload: &T) {
    let Some(correlation) = lookup_request_correlation(request_id) else {
        return;
    };
    match serde_json::to_value(payload) {
        Ok(value) => {
            let value = runner_result_trace_payload(&correlation.runner_kind, &value);
            capture_payload_for_trace(&correlation.trace_id, "runner_result", &value)
        }
        Err(error) => tracing::warn!(
            event = "tool_trace_capture_failed",
            server_trace_id = %correlation.trace_id,
            phase = "runner_result",
            error = %error,
            "tool_trace_capture_failed"
        ),
    }
}

fn runner_result_trace_payload(kind: &str, payload: &Value) -> Value {
    if kind != "ssh_resource" {
        return payload.clone();
    }
    let result = payload.get("result");
    json!({
        "kind": "ssh_resource",
        "exit_code": result
            .and_then(|value| value.get("exit_code"))
            .cloned()
            .unwrap_or(Value::Null),
        "stdout_present": result
            .and_then(|value| value.get("stdout"))
            .is_some_and(|value| !value.is_null()),
        "stderr_present": result
            .and_then(|value| value.get("stderr"))
            .is_some_and(|value| !value.is_null()),
        "error_present": result
            .and_then(|value| value.get("error"))
            .is_some_and(|value| !value.is_null()),
        "command_execution_state": payload
            .get("command_execution_state")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

/// Finalize a non-Job Runner result correlation only after authoritative result
/// acceptance. Job-backed requests stay correlated until authoritative Job
/// terminal state is accepted.
pub(crate) fn finalize_runner_result_correlation(request_id: &str) {
    let Some(correlation) = lookup_request_correlation(request_id) else {
        return;
    };
    if correlation.job_id.is_none() {
        remove_correlation(&correlation);
    }
}

/// Capture one authoritative Runner Job update into the trace that originally
/// admitted the Job. Updates without request_id may use job_id correlation; when
/// both identities are present they must resolve to the same TraceCorrelation.
/// Capture never consumes correlation.
pub(crate) fn capture_runner_job_update<T: Serialize>(
    request_id: Option<&str>,
    job_id: &str,
    payload: &T,
) {
    let Some(correlation) = lookup_job_correlation(request_id, job_id) else {
        return;
    };
    match serde_json::to_value(payload) {
        Ok(value) => capture_payload_for_trace(&correlation.trace_id, "runner_job_update", &value),
        Err(error) => tracing::warn!(
            event = "tool_trace_capture_failed",
            server_trace_id = %correlation.trace_id,
            phase = "runner_job_update",
            error = %error,
            "tool_trace_capture_failed"
        ),
    }
}

/// Consume Job correlation only after the shell-client layer has accepted a
/// terminal Server-authoritative Job state. A mismatched request_id + job_id
/// pair never resolves and therefore cannot consume either trace.
pub(crate) fn finalize_runner_job_correlation(request_id: Option<&str>, job_id: &str) {
    let Some(correlation) = lookup_job_correlation(request_id, job_id) else {
        return;
    };
    remove_correlation(&correlation);
}

/// Lifecycle guard shared by MCP `/mcp` and API `/api/tools/call` handlers.
pub struct ToolRequestLifecycle {
    prefix: &'static str,
    mode: ToolRequestTraceMode,
    trace_id: String,
    jsonrpc_id: String,
    method: String,
    tool_name: Option<String>,
    client_window: Option<ClientWindow>,
    suppress_payload_capture: bool,
    started: Instant,
    completed: AtomicBool,
}

impl ToolRequestLifecycle {
    pub fn new(
        prefix: &'static str,
        trace_id: String,
        jsonrpc_id: impl Into<String>,
        method: impl Into<String>,
        tool_name: Option<String>,
    ) -> Self {
        let suppress_payload_capture = tool_name.as_deref() == Some("read_tool_trace");
        Self {
            prefix,
            mode: crate::config::tool_request_trace_mode(),
            trace_id,
            jsonrpc_id: jsonrpc_id.into(),
            method: method.into(),
            tool_name,
            client_window: None,
            suppress_payload_capture,
            started: Instant::now(),
            completed: AtomicBool::new(false),
        }
    }

    pub fn enabled(&self) -> bool {
        self.mode != ToolRequestTraceMode::Off
    }

    pub fn full_enabled(&self) -> bool {
        self.mode == ToolRequestTraceMode::Full
    }

    pub fn active_trace_id(&self) -> Option<String> {
        self.enabled().then(|| self.trace_id.clone())
    }

    pub fn capture_payload(&self, phase: &str, value: &Value) {
        if self.full_enabled() && !self.suppress_payload_capture {
            capture_payload_for_trace(&self.trace_id, phase, value);
        }
    }

    pub fn set_method(&mut self, method: impl Into<String>) {
        self.method = method.into();
    }

    pub fn set_tool_name(&mut self, tool_name: Option<String>) {
        self.suppress_payload_capture = tool_name.as_deref() == Some("read_tool_trace");
        self.tool_name = tool_name;
    }

    pub fn set_jsonrpc_id(&mut self, jsonrpc_id: impl Into<String>) {
        self.jsonrpc_id = jsonrpc_id.into();
    }

    /// Attach an adapter-resolved client window for diagnostic correlation only.
    /// The lifecycle deliberately accepts `ClientWindow` rather than a raw host
    /// identifier so tracing cannot become another opaque-identity parser.
    pub(crate) fn set_client_window(&mut self, window: Option<&ClientWindow>) {
        self.client_window = window.cloned();
    }

    pub fn duration_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn mark_completed(&self) {
        self.completed.store(true, Ordering::SeqCst);
    }

    fn event_name(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.prefix)
    }

    pub fn log(
        &self,
        suffix: &str,
        http_status: Option<u16>,
        estimated_json_bytes: Option<usize>,
        protocol_success: Option<bool>,
        tool_success: Option<bool>,
        category: &str,
    ) {
        if !self.enabled() {
            return;
        }
        let event = self.event_name(suffix);
        let mode = match self.mode {
            ToolRequestTraceMode::Off => "off",
            ToolRequestTraceMode::Metadata => "metadata",
            ToolRequestTraceMode::Full => "full",
        };
        tracing::info!(
            event = %event,
            server_trace_id = %self.trace_id,
            trace_mode = mode,
            jsonrpc_id = %self.jsonrpc_id,
            method = %self.method,
            tool_name = self.tool_name.as_deref().unwrap_or("-"),
            client_window_key = self.client_window.as_ref().map(ClientWindow::key).unwrap_or("-"),
            client_window_source = self.client_window.as_ref().map(ClientWindow::source).unwrap_or("-"),
            duration_ms = self.duration_ms(),
            estimated_json_bytes = estimated_json_bytes.map(|b| b as i64).unwrap_or(-1),
            http_status = http_status.map(|s| s as i32).unwrap_or(-1),
            protocol_success = protocol_success
                .map(|s| if s { 1_i32 } else { 0_i32 })
                .unwrap_or(-1),
            tool_success = tool_success
                .map(|s| if s { 1_i32 } else { 0_i32 })
                .unwrap_or(-1),
            category = category,
            "{event}"
        );
        if self.full_enabled() {
            let mut stored = base_event(&self.trace_id, &event);
            merge_event_fields(
                &mut stored,
                json!({
                    "trace_mode": mode,
                    "jsonrpc_id": self.jsonrpc_id.as_str(),
                    "method": self.method.as_str(),
                    "tool_name": self.tool_name.as_deref(),
                    "client_window_key": self.client_window.as_ref().map(ClientWindow::key),
                    "client_window_source": self.client_window.as_ref().map(ClientWindow::source),
                    "duration_ms": self.duration_ms(),
                    "estimated_json_bytes": estimated_json_bytes,
                    "http_status": http_status,
                    "protocol_success": protocol_success,
                    "tool_success": tool_success,
                    "category": category,
                }),
            );
            enqueue_metadata_event(&self.trace_id, &event, stored);
        }
    }

    pub fn received(&self) {
        self.log("tool_request_received", None, None, None, None, "received");
    }

    pub fn parsed(&self, category: &str) {
        self.log("tool_request_parsed", None, None, None, None, category);
    }

    pub fn dispatch_started(&self) {
        self.log("tool_dispatch_started", None, None, None, None, "started");
    }

    pub fn dispatch_finished(
        &self,
        protocol_success: bool,
        tool_success: Option<bool>,
        category: &str,
    ) {
        self.log(
            "tool_dispatch_finished",
            None,
            None,
            Some(protocol_success),
            tool_success,
            category,
        );
    }

    pub fn dispatch_failed(&self, category: &str) {
        self.log(
            "tool_dispatch_failed",
            None,
            None,
            Some(false),
            Some(false),
            category,
        );
    }

    pub fn response_serialized(
        &self,
        http_status: u16,
        estimated_json_bytes: Option<usize>,
        protocol_success: Option<bool>,
        tool_success: Option<bool>,
        category: &str,
    ) {
        self.log(
            "tool_response_serialized",
            Some(http_status),
            estimated_json_bytes,
            protocol_success,
            tool_success,
            category,
        );
    }

    /// Response constructed and handed to the HTTP framework (not client ACK).
    pub fn handler_returned(
        &self,
        http_status: u16,
        estimated_json_bytes: Option<usize>,
        protocol_success: Option<bool>,
        tool_success: Option<bool>,
        category: &str,
    ) {
        self.log(
            "tool_handler_returned",
            Some(http_status),
            estimated_json_bytes,
            protocol_success,
            tool_success,
            category,
        );
        self.mark_completed();
    }
}

impl Drop for ToolRequestLifecycle {
    fn drop(&mut self) {
        if !self.enabled() || self.completed.load(Ordering::SeqCst) {
            return;
        }
        self.log(
            "tool_handler_incomplete_drop",
            None,
            None,
            None,
            None,
            "handler_dropped_before_response",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload_files(root: &Path, trace_id: &str) -> Vec<PathBuf> {
        let payload_dir = root.join(trace_id).join("payloads");
        let Ok(entries) = fs::read_dir(payload_dir) else {
            return Vec::new();
        };
        entries.flatten().map(|entry| entry.path()).collect()
    }

    fn reset_trace_store_accounting() {
        flush_full_trace_writer();
        *trace_io_state().lock().unwrap() = TraceStoreAccounting::default();
    }

    fn event_line_len(event: &Value) -> u64 {
        serde_json::to_vec(event).unwrap().len() as u64 + 1
    }

    #[test]
    fn job_trace_correlation_requires_request_and_job_to_agree() {
        let request_correlation = TraceCorrelation {
            trace_id: "trace-a".to_string(),
            request_id: "request-a".to_string(),
            job_id: Some("job-a".to_string()),
            runner_kind: "run_process".to_string(),
            created_at: 1,
        };
        let other_correlation = TraceCorrelation {
            trace_id: "trace-b".to_string(),
            request_id: "request-b".to_string(),
            job_id: Some("job-b".to_string()),
            runner_kind: "run_process".to_string(),
            created_at: 1,
        };
        let mut correlations = TraceCorrelations::default();
        correlations
            .requests
            .insert("request-a".to_string(), request_correlation.clone());
        correlations
            .jobs
            .insert("job-b".to_string(), other_correlation.clone());

        assert!(resolve_job_correlation(&correlations, Some("request-a"), "job-b").is_none());
        assert!(resolve_job_correlation(&correlations, Some("missing"), "job-b").is_none());
        assert_eq!(
            resolve_job_correlation(&correlations, None, "job-b"),
            Some(other_correlation)
        );

        correlations
            .jobs
            .insert("job-a".to_string(), request_correlation.clone());
        assert_eq!(
            resolve_job_correlation(&correlations, Some("request-a"), "job-a"),
            Some(request_correlation)
        );
    }

    fn accounting_snapshot() -> (PathBuf, u64, usize, u64) {
        let accounting = trace_io_state().lock().unwrap();
        let config = accounting.config.as_ref().expect("trace accounting config");
        (
            config.root.clone(),
            accounting.total_bytes,
            accounting.traces.len(),
            accounting.filesystem_scans,
        )
    }

    #[test]
    fn jsonrpc_id_safe_never_echoes_raw_string() {
        let secret = "very-secret-request-id-value";
        let summary = jsonrpc_id_safe(Some(&json!(secret)));
        assert!(!summary.contains(secret));
        assert!(summary.starts_with("string:len="));
        assert!(summary.contains("sha256_8="));
    }

    #[test]
    fn estimate_json_bytes_is_none_when_trace_disabled() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
        assert!(estimate_json_bytes(&json!({"a": 1})).is_none());
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "true");
        assert!(estimate_json_bytes(&json!({"a": 1})).is_some());
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
    }

    #[test]
    fn metadata_mode_never_creates_raw_payload_store() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "true");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        let guard = ToolRequestLifecycle::new(
            "mcp",
            "trace-metadata".into(),
            "none",
            "tools/call",
            Some("write_project_file".into()),
        );
        guard.capture_payload("raw_arguments", &json!({"content": "private"}));
        assert!(!temp.path().join("trace-metadata").exists());
    }

    #[test]
    fn full_mode_lifecycle_persists_only_hashed_client_window_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let raw_window = "chatgpt-window-opaque-secret";
        let params = json!({
            "name": "runtime_status",
            "arguments": {},
            "_meta": {"openai/session": raw_window}
        });
        let resolved = crate::client_window::stateless_mcp_window(&params);
        let window = resolved.identity.expect("valid OpenAI session window");
        let expected_key = window.key().to_string();
        let trace_ids = ["window-trace-a", "window-trace-b"];

        for trace_id in trace_ids {
            let mut guard = ToolRequestLifecycle::new(
                "mcp",
                trace_id.into(),
                "none",
                "tools/call",
                Some("runtime_status".into()),
            );
            guard.set_client_window(Some(&window));
            guard.parsed("ok");
            guard.mark_completed();
        }

        let absent_trace_id = "window-trace-absent";
        let absent = ToolRequestLifecycle::new(
            "mcp",
            absent_trace_id.into(),
            "none",
            "tools/call",
            Some("runtime_status".into()),
        );
        absent.parsed("ok");
        absent.mark_completed();
        flush_full_trace_writer();

        for trace_id in trace_ids {
            let events =
                fs::read_to_string(temp.path().join(trace_id).join("events.jsonl")).unwrap();
            assert!(!events.contains(raw_window));
            let event: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
            assert_eq!(event["client_window_key"], expected_key);
            assert_eq!(event["client_window_source"], "openai-session");
            assert_eq!(event["server_trace_id"], trace_id);
        }

        let absent_events =
            fs::read_to_string(temp.path().join(absent_trace_id).join("events.jsonl")).unwrap();
        let absent_event: Value =
            serde_json::from_str(absent_events.lines().next().unwrap()).unwrap();
        assert!(absent_event["client_window_key"].is_null());
        assert!(absent_event["client_window_source"].is_null());
    }

    #[test]
    fn full_mode_persists_complete_compressed_payload() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        let guard = ToolRequestLifecycle::new(
            "mcp",
            "trace-full".into(),
            "none",
            "tools/call",
            Some("write_project_file".into()),
        );
        let payload = json!({
            "ack_session_context_revision": 42,
            "content": "large-body-".repeat(100_000),
        });
        guard.capture_payload("raw_arguments", &payload);
        drop(guard);
        flush_full_trace_writer();
        let files = payload_files(temp.path(), "trace-full");
        assert_eq!(files.len(), 1);
        let compressed = fs::read(&files[0]).unwrap();
        let raw = zstd::stream::decode_all(&compressed[..]).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), payload);
        let events = fs::read_to_string(temp.path().join("trace-full/events.jsonl")).unwrap();
        assert!(events.contains("raw_arguments"));
        assert!(events.contains("payload_sha256"));
    }

    #[test]
    fn full_mode_trace_reader_lists_then_reads_verified_payload_without_native_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let trace_id = Uuid::new_v4().to_string();
        let payload = json!({
            "private_diagnostic": "visible only through the side channel",
            "nested": [1, 2, 3]
        });
        assert!(persist_payload(&trace_id, "runner_result", &payload)
            .unwrap()
            .is_some());

        let listing = read_full_trace(&trace_id, None, None, None).unwrap();
        assert_eq!(listing["payload_count"], 1);
        assert_eq!(listing["returned_count"], 1);
        assert_eq!(listing["payloads"][0]["payload_index"], 0);
        assert_eq!(listing["payloads"][0]["phase"], "runner_result");
        assert_eq!(listing["payloads"][0]["payload_available"], true);
        let serialized_listing = serde_json::to_string(&listing).unwrap();
        assert!(!serialized_listing.contains("payload_path"));
        assert!(!serialized_listing.contains(temp.path().to_string_lossy().as_ref()));
        assert!(!serialized_listing.contains("private_diagnostic"));

        let selected = read_full_trace(&trace_id, None, None, Some(0)).unwrap();
        assert_eq!(selected["payload_index"], 0);
        assert_eq!(selected["phase"], "runner_result");
        assert_eq!(selected["payload_available"], true);
        assert_eq!(selected["payload"], payload);
    }

    #[test]
    fn full_mode_trace_reader_rejects_unsafe_refs_and_never_returns_oversize_payload() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let unsafe_ref = read_full_trace("../etc/passwd", None, None, None).unwrap_err();
        assert_eq!(unsafe_ref.kind, "invalid_trace_ref");
        let canonical = Uuid::new_v4().to_string();
        let uppercase = canonical.to_ascii_uppercase();
        let uppercase_error = read_full_trace(&uppercase, None, None, None).unwrap_err();
        assert_eq!(uppercase_error.kind, "invalid_trace_ref");

        let large_payload = json!({"body": "x".repeat(MAX_MODEL_TRACE_PAYLOAD_BYTES + 1024)});
        assert!(
            persist_payload(&canonical, "final_response", &large_payload)
                .unwrap()
                .is_some()
        );
        let selected = read_full_trace(&canonical, None, None, Some(0)).unwrap();
        assert_eq!(selected["payload_available"], false);
        assert_eq!(selected["reason"], "payload_exceeds_model_read_limit");
        assert_eq!(selected["max_payload_bytes"], MAX_MODEL_TRACE_PAYLOAD_BYTES);
        assert!(selected.get("payload").is_none());
    }

    #[test]
    fn read_tool_trace_lifecycle_never_recursively_captures_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let trace_id = Uuid::new_v4().to_string();
        let mut guard = ToolRequestLifecycle::new(
            "mcp",
            trace_id.clone(),
            "none",
            "tools/call",
            Some("call_runtime_tool".into()),
        );
        // Adaptive routing begins at the gateway and later identifies the real
        // target. The setter must suppress every subsequent forensic payload.
        guard.set_tool_name(Some("read_tool_trace".into()));
        guard.capture_payload(
            "effective_arguments",
            &json!({"trace_ref": Uuid::new_v4().to_string()}),
        );
        guard.capture_payload("final_response", &json!({"payload": "PRIVATE_RAW_TRACE"}));
        drop(guard);
        flush_full_trace_writer();
        assert!(payload_files(temp.path(), &trace_id).is_empty());
    }

    #[test]
    fn full_mode_capture_does_not_wait_for_trace_io_lock() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let io_guard = trace_io_state().lock().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let capture = thread::spawn(move || {
            let _trace_env = crate::test_support::TestToolRequestTraceEnvReaderGuard::new();
            let guard = ToolRequestLifecycle::new(
                "mcp",
                "trace-background-writer".into(),
                "none",
                "tools/call",
                Some("read_file".into()),
            );
            guard.capture_payload("raw_arguments", &json!({"path": "README.md"}));
            let _ = done_tx.send(());
        });
        let returned_without_io = done_rx.recv_timeout(Duration::from_millis(250)).is_ok();
        drop(io_guard);
        capture.join().unwrap();
        assert!(
            returned_without_io,
            "full trace capture must enqueue without waiting for trace-store I/O"
        );
        flush_full_trace_writer();
        assert_eq!(
            payload_files(temp.path(), "trace-background-writer").len(),
            1
        );
    }

    #[test]
    fn full_mode_disk_failure_is_fail_open() {
        let temp = tempfile::tempdir().unwrap();
        let not_a_directory = temp.path().join("trace-file");
        fs::write(&not_a_directory, b"occupied").unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            not_a_directory.to_string_lossy().as_ref(),
        );
        let guard = ToolRequestLifecycle::new(
            "api",
            "trace-fail-open".into(),
            "-",
            "POST /api/tools/call",
            Some("read_file".into()),
        );
        guard.capture_payload("raw_arguments", &json!({"path": "README.md"}));
        drop(guard);
        flush_full_trace_writer();
        assert_eq!(fs::read(&not_a_directory).unwrap(), b"occupied");
    }

    #[test]
    fn full_mode_omits_payload_that_cannot_fit_disk_budget() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "1");
        let guard = ToolRequestLifecycle::new(
            "mcp",
            "trace-budget".into(),
            "none",
            "tools/call",
            Some("write_project_file".into()),
        );
        guard.capture_payload("raw_arguments", &json!({"content": "must-not-truncate"}));
        drop(guard);
        flush_full_trace_writer();
        assert!(payload_files(temp.path(), "trace-budget").is_empty());
    }

    #[test]
    fn full_mode_accounting_tracks_writes_without_rescanning_hot_path() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let trace_id = "trace-accounting-hot-path";
        assert!(persist_metadata_event(trace_id, json!({"event": "first"})).unwrap());
        let scans_after_first = accounting_snapshot().3;
        assert_eq!(scans_after_first, 1);

        assert!(persist_payload(
            trace_id,
            "raw_arguments",
            &json!({"content": "payload".repeat(512)})
        )
        .unwrap()
        .is_some());
        assert!(persist_metadata_event(trace_id, json!({"event": "last"})).unwrap());

        let (_, cached_total, trace_count, scans_after_writes) = accounting_snapshot();
        let actual_total = directory_stats(&temp.path().join(trace_id)).unwrap().0;
        assert_eq!(trace_count, 1);
        assert_eq!(cached_total, actual_total);
        assert_eq!(scans_after_writes, scans_after_first);
    }

    #[test]
    fn full_mode_accounting_rebuilds_when_trace_root_changes() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            first_root.path().to_string_lossy().as_ref(),
        );
        reset_trace_store_accounting();
        assert!(persist_metadata_event("trace-first-root", json!({"event": "first"})).unwrap());
        let first_scans = accounting_snapshot().3;

        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            second_root.path().to_string_lossy().as_ref(),
        );
        assert!(persist_metadata_event("trace-second-root", json!({"event": "second"})).unwrap());
        let (root, _, trace_count, scans) = accounting_snapshot();
        assert_eq!(root, second_root.path());
        assert_eq!(trace_count, 1);
        assert_eq!(scans, first_scans + 1);
        assert!(second_root
            .path()
            .join("trace-second-root/events.jsonl")
            .exists());
    }

    #[test]
    fn full_mode_accounting_rebuilds_when_same_root_config_changes() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS", "2");
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();
        assert!(persist_metadata_event("trace-config", json!({"event": "first"})).unwrap());
        let first_scans = accounting_snapshot().3;

        env.set("WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS", "3");
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "16777216");
        assert!(persist_metadata_event("trace-config", json!({"event": "second"})).unwrap());
        let accounting = trace_io_state().lock().unwrap();
        assert_eq!(accounting.filesystem_scans, first_scans + 1);
        assert_eq!(
            accounting.config.as_ref().unwrap().retention,
            Duration::from_secs(3 * 60 * 60)
        );
        assert_eq!(accounting.config.as_ref().unwrap().budget, 16_777_216);
    }

    #[test]
    fn full_mode_due_maintenance_reconciles_external_owned_drift() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();
        assert!(persist_metadata_event("trace-known", json!({"event": "first"})).unwrap());
        let first_scans = accounting_snapshot().3;

        let external = temp.path().join("trace-external-owned");
        create_private_trace_dir(&external).unwrap();
        ensure_trace_owner_marker(&external).unwrap();
        fs::write(external.join("events.jsonl"), vec![b'x'; 311]).unwrap();
        {
            let mut accounting = trace_io_state().lock().unwrap();
            accounting.last_reconcile = Some(Instant::now() - TRACE_STORE_RECONCILE_INTERVAL);
        }

        assert!(persist_metadata_event("trace-known", json!({"event": "second"})).unwrap());
        let (_, cached_total, trace_count, scans) = accounting_snapshot();
        assert_eq!(scans, first_scans + 1);
        assert_eq!(trace_count, 2);
        assert_eq!(cached_total, directory_stats(temp.path()).unwrap().0);
    }

    #[test]
    fn full_mode_invalidated_accounting_rebuilds_on_next_write() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();
        let trace_id = "trace-rebuild-invalidated";
        assert!(persist_metadata_event(trace_id, json!({"event": "first"})).unwrap());
        let first_scans = accounting_snapshot().3;
        trace_io_state().lock().unwrap().invalidate();

        assert!(persist_metadata_event(trace_id, json!({"event": "second"})).unwrap());
        let (_, cached_total, _, scans) = accounting_snapshot();
        assert_eq!(scans, first_scans + 1);
        assert_eq!(
            cached_total,
            directory_stats(&temp.path().join(trace_id)).unwrap().0
        );
    }

    #[test]
    fn full_mode_budget_eviction_updates_cached_total_without_rescan() {
        let temp = tempfile::tempdir().unwrap();
        let event = json!({"event": "budget", "padding": "x".repeat(128)});
        let line_len = event_line_len(&event);
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES",
            &(line_len * 2).to_string(),
        );
        reset_trace_store_accounting();

        assert!(persist_metadata_event("trace-oldest", event.clone()).unwrap());
        assert!(persist_metadata_event("trace-newer", event.clone()).unwrap());
        let scans_before_eviction = accounting_snapshot().3;
        {
            let mut accounting = trace_io_state().lock().unwrap();
            accounting.traces.get_mut("trace-oldest").unwrap().modified = SystemTime::UNIX_EPOCH;
            accounting.traces.get_mut("trace-newer").unwrap().modified =
                SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        }

        assert!(persist_metadata_event("trace-current", event).unwrap());
        let (_, cached_total, trace_count, scans_after_eviction) = accounting_snapshot();
        assert!(!temp.path().join("trace-oldest").exists());
        assert!(temp.path().join("trace-newer").exists());
        assert!(temp.path().join("trace-current").exists());
        assert_eq!(trace_count, 2);
        assert_eq!(cached_total, line_len * 2);
        assert_eq!(cached_total, directory_stats(temp.path()).unwrap().0);
        assert_eq!(scans_after_eviction, scans_before_eviction);
    }

    #[test]
    fn full_mode_active_trace_is_not_evicted_for_its_own_write() {
        let temp = tempfile::tempdir().unwrap();
        let event = json!({"event": "active", "padding": "x".repeat(64)});
        let line_len = event_line_len(&event);
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES",
            &line_len.to_string(),
        );
        reset_trace_store_accounting();

        assert!(persist_metadata_event("trace-active", event.clone()).unwrap());
        assert!(!persist_metadata_event("trace-active", event).unwrap());
        let events = fs::read_to_string(temp.path().join("trace-active/events.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 1);
        assert_eq!(accounting_snapshot().1, line_len);
    }

    #[test]
    fn full_mode_cached_trace_losing_owner_marker_is_never_evicted() {
        let temp = tempfile::tempdir().unwrap();
        let event = json!({"event": "ownership", "padding": "x".repeat(64)});
        let line_len = event_line_len(&event);
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES",
            &line_len.to_string(),
        );
        reset_trace_store_accounting();

        assert!(persist_metadata_event("trace-owned", event.clone()).unwrap());
        let owned_dir = temp.path().join("trace-owned");
        fs::remove_file(owned_dir.join(TRACE_OWNER_MARKER)).unwrap();

        assert!(!persist_metadata_event("trace-current", event).unwrap());
        assert!(owned_dir.join("events.jsonl").exists());
        assert!(!temp.path().join("trace-current/events.jsonl").exists());
        let accounting = trace_io_state().lock().unwrap();
        assert_eq!(accounting.total_bytes, line_len);
        assert!(!accounting.initialized);
        assert!(!accounting.traces["trace-owned"].evictable);
    }

    #[test]
    fn full_mode_retention_prunes_owned_non_active_cached_trace() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_RETENTION_HOURS", "1");
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();
        assert!(persist_metadata_event("trace-expired", json!({"event": "old"})).unwrap());

        let config = trace_store_config();
        let mut accounting = trace_io_state().lock().unwrap();
        accounting.traces.get_mut("trace-expired").unwrap().modified = SystemTime::UNIX_EPOCH;
        prune_expired_traces(&mut accounting, &config, "trace-active").unwrap();
        assert_eq!(accounting.total_bytes, 0);
        assert!(!accounting.traces.contains_key("trace-expired"));
        drop(accounting);
        assert!(!temp.path().join("trace-expired").exists());
    }

    #[test]
    fn full_mode_fresh_accounting_rebuild_counts_existing_owned_trace() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("trace-existing");
        create_private_trace_dir(&existing).unwrap();
        ensure_trace_owner_marker(&existing).unwrap();
        fs::write(existing.join("events.jsonl"), vec![b'x'; 257]).unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        reset_trace_store_accounting();

        let event = json!({"event": "after-restart"});
        let line_len = event_line_len(&event);
        assert!(persist_metadata_event("trace-new", event).unwrap());
        let (_, cached_total, trace_count, scans) = accounting_snapshot();
        assert_eq!(scans, 1);
        assert_eq!(trace_count, 2);
        assert_eq!(cached_total, 257 + line_len);
        assert_eq!(cached_total, directory_stats(temp.path()).unwrap().0);
    }

    #[test]
    fn full_mode_pruning_never_deletes_unowned_sibling_directories() {
        let temp = tempfile::tempdir().unwrap();
        let unrelated = temp.path().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.bin"), vec![b'x'; 16 * 1024]).unwrap();

        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8192");
        let trace_id = new_trace_id();
        let guard = ToolRequestLifecycle::new(
            "mcp",
            trace_id.clone(),
            "none",
            "tools/call",
            Some("list_tools".into()),
        );
        guard.capture_payload("raw_arguments", &json!({"probe": true}));
        drop(guard);
        flush_full_trace_writer();

        assert!(unrelated.join("keep.bin").exists());
        assert_eq!(payload_files(temp.path(), &trace_id).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn full_mode_trace_storage_is_private_on_unix() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        env.set("WEBCODEX_TOOL_REQUEST_TRACE_MAX_TOTAL_BYTES", "8388608");
        let trace_id = new_trace_id();
        let guard = ToolRequestLifecycle::new(
            "mcp",
            trace_id.clone(),
            "none",
            "tools/call",
            Some("write_project_file".into()),
        );
        guard.capture_payload("raw_arguments", &json!({"content": "private"}));
        drop(guard);
        flush_full_trace_writer();

        let trace_dir = temp.path().join(&trace_id);
        let payload_dir = trace_dir.join("payloads");
        let payload = payload_files(temp.path(), &trace_id)
            .into_iter()
            .next()
            .expect("full trace payload");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&trace_dir), 0o700);
        assert_eq!(mode(&payload_dir), 0o700);
        assert_eq!(mode(&trace_dir.join("events.jsonl")), 0o600);
        assert_eq!(mode(&payload), 0o600);
    }

    #[tokio::test]
    async fn runner_correlation_survives_original_dispatch_scope() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
        env.set(
            "WEBCODEX_TOOL_REQUEST_TRACE_DIR",
            temp.path().to_string_lossy().as_ref(),
        );
        let guard = ToolRequestLifecycle::new(
            "mcp",
            "trace-runner".into(),
            "none",
            "tools/call",
            Some("run_process".into()),
        );
        scope_active_trace(guard.active_trace_id(), async {
            record_runner_request_enqueued(
                &json!({"kind": "run_process", "argv": ["cargo", "test"]}),
                "request-1",
                "runner-1",
                "run_process",
                None,
                Some("instance-1"),
                Some("quic"),
                Some("0.3.8"),
                Some("abc123"),
            );
        })
        .await;
        capture_runner_result("request-1", &json!({"exit_code": 0, "stdout": "ok"}));
        drop(guard);
        flush_full_trace_writer();
        let events = fs::read_to_string(temp.path().join("trace-runner/events.jsonl")).unwrap();
        assert!(events.contains("tool_runner_request_enqueued"));
        let events = events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let read_phase = |phase: &str| {
            let relative = events
                .iter()
                .find(|event| {
                    event["event"] == "tool_trace_payload_captured" && event["phase"] == phase
                })
                .and_then(|event| event["payload_path"].as_str())
                .unwrap_or_else(|| panic!("missing trace payload phase {phase}"));
            let compressed = fs::read(temp.path().join("trace-runner").join(relative)).unwrap();
            let raw = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap()
        };
        assert_eq!(
            read_phase("runner_request"),
            json!({"kind": "run_process", "argv": ["cargo", "test"]})
        );
        assert_eq!(
            read_phase("runner_result"),
            json!({"exit_code": 0, "stdout": "ok"})
        );
    }

    #[test]
    fn ssh_resource_runner_result_trace_never_persists_result_bodies() {
        let target = "17724@w10";
        let payload = json!({
            "result": {
                "exit_code": 0,
                "stdout": format!("{{\"action\":\"register\",\"target\":\"{target}\"}}"),
                "stderr": target,
                "error": target
            },
            "command_execution_state": "completed"
        });
        let sanitized = runner_result_trace_payload("ssh_resource", &payload);
        let serialized = serde_json::to_string(&sanitized).unwrap();
        assert!(!serialized.contains(target));
        assert_eq!(sanitized["kind"], "ssh_resource");
        assert_eq!(sanitized["stdout_present"], true);
        assert_eq!(sanitized["stderr_present"], true);
        assert_eq!(sanitized["error_present"], true);
        assert_eq!(sanitized["exit_code"], 0);

        assert_eq!(
            runner_result_trace_payload("run_process", &payload),
            payload
        );
    }

    #[test]
    fn incomplete_drop_is_safe_when_disabled() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
        let guard = ToolRequestLifecycle::new(
            "mcp",
            "trace-test".into(),
            "none",
            "tools/call",
            Some("list_projects".into()),
        );
        assert!(!guard.enabled());
        guard.received();
        drop(guard);
    }

    #[test]
    fn completed_drop_is_silent() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set("WEBCODEX_TOOL_REQUEST_TRACE", "true");
        let guard =
            ToolRequestLifecycle::new("api", "trace-ok".into(), "-", "POST /api/tools/call", None);
        guard.handler_returned(200, Some(12), Some(true), Some(true), "ok");
        drop(guard);
        env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
    }
}
