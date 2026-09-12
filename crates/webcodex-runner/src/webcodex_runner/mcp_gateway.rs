//! Persistent Runner-owned stdio MCP providers for the built-in MCP gateway.
//!
//! Each configured provider has one process-lifetime logical identity. Its stdio
//! connection is initialized lazily and reused while healthy. A fatal protocol/
//! transport failure retires only that connection: the failed request is never
//! replayed, while a later explicit request may establish a fresh connection
//! under the same provider identity and revalidate tool schema before dispatch.

use super::config::{McpGatewayConfig, McpGatewayProviderConfig, MCP_GATEWAY_MAX_CWD_BYTES};
use super::shell::{env_keys_equal, is_sensitive_env_key};
use crate::mcp_gateway::{
    validate_json_value, validate_request, validate_tool_result, validate_tools, McpGatewayContent,
    McpGatewayDispatchState, McpGatewayProvider, McpGatewayProviderState, McpGatewayRequest,
    McpGatewayResponse, McpGatewayResponsePayload, McpGatewayTool, McpGatewayToolResult,
    MCP_GATEWAY_MAX_MESSAGE_BYTES,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Mutex, TryLockError};
use std::time::{Duration, Instant};
use webcodex_process::ManagedChild;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MCP_GATEWAY_MAX_IGNORED_NOTIFICATIONS: usize = 32;
const PROVIDER_NEVER_STARTED: u8 = 0;
const PROVIDER_HEALTHY: u8 = 1;
const PROVIDER_CONNECTION_RETIRED: u8 = 2;

pub(crate) struct McpGatewayManager {
    providers: BTreeMap<String, ProviderEntry>,
    request_timeout: Duration,
    stopping: AtomicBool,
}

struct ProviderEntry {
    config: McpGatewayProviderConfig,
    instance_id: String,
    lifecycle: AtomicU8,
    session: Mutex<Option<ProviderConnection>>,
}

struct ProviderConnection {
    child: ManagedChild,
    stdin: ChildStdin,
    incoming: mpsc::Receiver<ReaderEvent>,
    next_id: u64,
}

enum ReaderEvent {
    Message(Result<Value, ReaderFault>),
    Eof,
}

#[derive(Debug, Clone, Copy)]
enum ReaderFault {
    Malformed,
    TooLarge,
    Io,
}

struct ProviderFailure {
    code: &'static str,
    dispatch_state: McpGatewayDispatchState,
    fatal: bool,
}

impl ProviderFailure {
    fn before_send(code: &'static str) -> Self {
        Self {
            code,
            dispatch_state: McpGatewayDispatchState::NotStarted,
            fatal: true,
        }
    }

    fn after_send(code: &'static str) -> Self {
        Self {
            code,
            dispatch_state: McpGatewayDispatchState::OutcomeUnknown,
            fatal: true,
        }
    }

    fn completed(code: &'static str) -> Self {
        Self {
            code,
            dispatch_state: McpGatewayDispatchState::Completed,
            // A correlated JSON-RPC response proves this request-response
            // exchange completed. Result-level V1 incompatibility must not
            // poison the whole provider instance; only transport/protocol
            // failures that lose correlation retire the session.
            fatal: false,
        }
    }

    fn not_started(code: &'static str) -> Self {
        Self {
            code,
            dispatch_state: McpGatewayDispatchState::NotStarted,
            fatal: false,
        }
    }

    fn preflight(mut error: Self) -> Self {
        // A schema preflight may itself have been dispatched upstream, but the
        // model-requested effectful tools/call has definitely not started.
        error.dispatch_state = McpGatewayDispatchState::NotStarted;
        error
    }

    fn rpc_error() -> Self {
        Self {
            code: "provider_rpc_error",
            dispatch_state: McpGatewayDispatchState::Completed,
            fatal: false,
        }
    }
}

impl McpGatewayManager {
    pub(crate) fn new(config: &McpGatewayConfig) -> Self {
        let providers = config
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.id.clone(),
                    ProviderEntry {
                        config: provider.clone(),
                        instance_id: uuid::Uuid::new_v4().simple().to_string(),
                        lifecycle: AtomicU8::new(PROVIDER_NEVER_STARTED),
                        session: Mutex::new(None),
                    },
                )
            })
            .collect();
        Self {
            providers,
            request_timeout: Duration::from_secs(config.request_timeout_secs.clamp(1, 120)),
            stopping: AtomicBool::new(false),
        }
    }

    /// Exact process-lifetime provider inventory projected in normal Runner
    /// registration. Reading it does not start a provider process and does not
    /// expose executable, argv, environment, PID, stderr, or secret material.
    pub(crate) fn provider_inventory(&self) -> Vec<McpGatewayProvider> {
        self.providers
            .values()
            .map(ProviderEntry::advertisement)
            .collect()
    }

    pub(crate) fn handle(&self, request: McpGatewayRequest) -> McpGatewayResponse {
        if self.stopping.load(Ordering::SeqCst) {
            return bridge_error(
                McpGatewayDispatchState::NotStarted,
                "runner_stopping",
                "Runner is stopping; provider request was not started",
            );
        }
        if validate_request(&request).is_err() {
            return bridge_error(
                McpGatewayDispatchState::NotStarted,
                "invalid_bridge_request",
                "Bridge request failed bounded validation",
            );
        }
        match request {
            McpGatewayRequest::ProviderStatus {
                provider_id,
                provider_instance_id,
            } => {
                let Some(provider) = self.exact_provider(&provider_id, &provider_instance_id)
                else {
                    return stale_provider();
                };
                McpGatewayResponse::success(McpGatewayResponsePayload::ProviderStatus {
                    state: provider.lifecycle_state(),
                })
            }
            McpGatewayRequest::ToolsList {
                provider_id,
                provider_instance_id,
            } => {
                let Some(provider) = self.exact_provider(&provider_id, &provider_instance_id)
                else {
                    return stale_provider();
                };
                match provider.with_connection(
                    provider.request_timeout(self.request_timeout),
                    |connection, timeout| connection.tools_list(timeout),
                ) {
                    Ok(tools) => {
                        McpGatewayResponse::success(McpGatewayResponsePayload::Tools { tools })
                    }
                    Err(error) => provider_failure_response(error),
                }
            }
            McpGatewayRequest::ToolsCall {
                provider_id,
                provider_instance_id,
                name,
                arguments,
                expected_schema,
            } => {
                let Some(provider) = self.exact_provider(&provider_id, &provider_instance_id)
                else {
                    return stale_provider();
                };
                match provider.with_connection(
                    provider.request_timeout(self.request_timeout),
                    |connection, timeout| {
                        let started = Instant::now();
                        let tools = connection
                            .tools_list(timeout)
                            .map_err(ProviderFailure::preflight)?;
                        let Some(current) = tools.iter().find(|tool| tool.name == name) else {
                            return Err(ProviderFailure::not_started("provider_tool_missing"));
                        };
                        if current.schema_observation() != expected_schema {
                            return Err(ProviderFailure::not_started("provider_schema_changed"));
                        }
                        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                            return Err(ProviderFailure::not_started("provider_timeout"));
                        };
                        if remaining.is_zero() {
                            return Err(ProviderFailure::not_started("provider_timeout"));
                        }
                        connection.tools_call(&name, arguments, remaining)
                    },
                ) {
                    Ok(result) => {
                        McpGatewayResponse::success(McpGatewayResponsePayload::ToolResult {
                            result,
                        })
                    }
                    Err(error) => provider_failure_response(error),
                }
            }
        }
    }

    fn exact_provider(
        &self,
        provider_id: &str,
        provider_instance_id: &str,
    ) -> Option<&ProviderEntry> {
        self.providers
            .get(provider_id)
            .filter(|provider| provider.instance_id == provider_instance_id)
    }

    pub(crate) fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        for provider in self.providers.values() {
            let mut session = provider
                .session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drop(session.take());
        }
    }
}

impl Drop for McpGatewayManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ProviderEntry {
    fn advertisement(&self) -> McpGatewayProvider {
        McpGatewayProvider {
            provider_id: self.config.id.clone(),
            provider_instance_id: self.instance_id.clone(),
            name: self.config.name.clone(),
        }
    }

    fn request_timeout(&self, default: Duration) -> Duration {
        self.config
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(default)
    }

    fn lifecycle_state(&self) -> McpGatewayProviderState {
        let mut session = match self.session.try_lock() {
            Ok(session) => session,
            Err(TryLockError::WouldBlock) => return McpGatewayProviderState::Busy,
            Err(TryLockError::Poisoned(_)) => return McpGatewayProviderState::ConnectionRetired,
        };
        if let Some(connection) = session.as_mut() {
            match connection.child.try_wait() {
                Ok(None) => {
                    self.lifecycle.store(PROVIDER_HEALTHY, Ordering::SeqCst);
                    return McpGatewayProviderState::Healthy;
                }
                Ok(Some(_)) | Err(_) => {
                    // Reap/drop the dead owned connection without starting a
                    // replacement. A later explicit interaction owns reconnect.
                    drop(session.take());
                    self.lifecycle
                        .store(PROVIDER_CONNECTION_RETIRED, Ordering::SeqCst);
                    return McpGatewayProviderState::ConnectionRetired;
                }
            }
        }
        match self.lifecycle.load(Ordering::SeqCst) {
            PROVIDER_NEVER_STARTED => McpGatewayProviderState::NeverStarted,
            PROVIDER_HEALTHY | PROVIDER_CONNECTION_RETIRED => {
                McpGatewayProviderState::ConnectionRetired
            }
            _ => McpGatewayProviderState::ConnectionRetired,
        }
    }

    fn with_connection<T>(
        &self,
        timeout: Duration,
        operation: impl FnOnce(&mut ProviderConnection, Duration) -> Result<T, ProviderFailure>,
    ) -> Result<T, ProviderFailure> {
        let mut session = match self.session.try_lock() {
            Ok(session) => session,
            Err(TryLockError::WouldBlock) => {
                return Err(ProviderFailure {
                    code: "provider_busy",
                    dispatch_state: McpGatewayDispatchState::NotStarted,
                    fatal: false,
                })
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(ProviderFailure::before_send("provider_unavailable"));
            }
        };
        let started = Instant::now();
        if session.is_none() {
            match ProviderConnection::spawn(&self.config, timeout) {
                Ok(connection) => {
                    self.lifecycle.store(PROVIDER_HEALTHY, Ordering::SeqCst);
                    *session = Some(connection);
                }
                Err(mut error) => {
                    self.lifecycle
                        .store(PROVIDER_CONNECTION_RETIRED, Ordering::SeqCst);
                    // Initialization is provider lifecycle setup, not the
                    // requested tools/list or tools/call. Even if initialize
                    // reached the child, the caller's operation did not. A
                    // later explicit request may attempt a fresh connection.
                    error.dispatch_state = McpGatewayDispatchState::NotStarted;
                    return Err(error);
                }
            }
        }
        let Some(remaining) = timeout
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
        else {
            self.lifecycle
                .store(PROVIDER_CONNECTION_RETIRED, Ordering::SeqCst);
            if let Some(mut connection) = session.take() {
                connection.terminate();
            }
            return Err(ProviderFailure::before_send("provider_timeout"));
        };
        let result = operation(
            session.as_mut().expect("provider connection initialized"),
            remaining,
        );
        if result.as_ref().is_err_and(|error| error.fatal) {
            self.lifecycle
                .store(PROVIDER_CONNECTION_RETIRED, Ordering::SeqCst);
            // Retire only the desynchronized connection. Never replay the
            // failed request here; a later explicit request may spawn a fresh
            // connection under the same logical provider identity.
            if let Some(mut connection) = session.take() {
                connection.terminate();
            }
        }
        result
    }
}

fn resolve_provider_environment(
    config: &McpGatewayProviderConfig,
) -> Result<Vec<(String, std::ffi::OsString)>, ProviderFailure> {
    let mut resolved = Vec::with_capacity(config.env_from_env.len() + usize::from(cfg!(windows)));
    #[cfg(windows)]
    if !config
        .env_from_env
        .keys()
        .any(|destination| env_keys_equal(destination, "SYSTEMROOT"))
    {
        // Keep Windows process bootstrap usable after env_clear() without
        // inheriting PATH, user profile data, proxy settings, or credentials.
        // Operators may still explicitly map SYSTEMROOT to another source.
        if let Some(system_root) = std::env::var_os("SYSTEMROOT") {
            resolved.push(("SYSTEMROOT".to_string(), system_root));
        }
    }
    for (destination, source) in &config.env_from_env {
        // Keep the Runner transport/account secret invariant authoritative even
        // if a caller constructs config without going through load_config.
        if is_sensitive_env_key(destination) || is_sensitive_env_key(source) {
            return Err(ProviderFailure::before_send("provider_env_forbidden"));
        }
        let Some(value) = std::env::var_os(source) else {
            return Err(ProviderFailure::before_send("provider_env_missing"));
        };
        resolved.push((destination.clone(), value));
    }
    Ok(resolved)
}

fn resolve_provider_cwd(
    config: &McpGatewayProviderConfig,
) -> Result<Option<&std::path::Path>, ProviderFailure> {
    let Some(cwd) = config.cwd.as_deref() else {
        return Ok(None);
    };
    let path = std::path::Path::new(cwd);
    if cwd.is_empty()
        || cwd.len() > MCP_GATEWAY_MAX_CWD_BYTES
        || cwd.contains('\0')
        || !path.is_absolute()
    {
        return Err(ProviderFailure::before_send("provider_cwd_invalid"));
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(path)),
        _ => Err(ProviderFailure::before_send("provider_cwd_unavailable")),
    }
}

impl ProviderConnection {
    fn spawn(
        config: &McpGatewayProviderConfig,
        timeout: Duration,
    ) -> Result<Self, ProviderFailure> {
        // Resolve the complete operator-declared execution context before
        // creating the child. Missing env sources or unavailable cwd therefore
        // cannot produce a partially initialized provider process.
        let environment = resolve_provider_environment(config)?;
        let cwd = resolve_provider_cwd(config)?;
        let mut command = Command::new(&config.executable);
        command
            .args(&config.args)
            // Never inherit the Runner process environment implicitly. The
            // resolved environment below is limited to explicit env_from_env
            // mappings plus the minimal non-secret Windows OS bootstrap.
            .env_clear();
        for (destination, value) in environment {
            command.env(destination, value);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ManagedChild::spawn(&mut command)
            .map_err(|_| ProviderFailure::before_send("provider_spawn_failed"))?;
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| ProviderFailure::before_send("provider_stdio_unavailable"))?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| ProviderFailure::before_send("provider_stdio_unavailable"))?;
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| ProviderFailure::before_send("provider_stdio_unavailable"))?;
        let (sender, incoming) = mpsc::sync_channel(8);
        std::thread::Builder::new()
            .name(format!("wc-mcp-{}", config.id))
            .spawn(move || provider_stdout_reader(stdout, sender))
            .map_err(|_| ProviderFailure::before_send("provider_reader_unavailable"))?;
        std::thread::Builder::new()
            .name(format!("wc-mcp-stderr-{}", config.id))
            .spawn(move || {
                // Stderr is diagnostic-only and intentionally discarded. It
                // never enters protocol parsing, HTTP results, or audit data.
                let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
            })
            .map_err(|_| ProviderFailure::before_send("provider_reader_unavailable"))?;

        let mut connection = Self {
            child,
            stdin,
            incoming,
            next_id: 1,
        };
        let initialized = connection.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "webcodex-runner-mcp-gateway",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            timeout,
        )?;
        validate_initialize_result(&initialized)?;
        connection.send_notification("notifications/initialized", json!({}))?;
        Ok(connection)
    }

    fn tools_list(&mut self, timeout: Duration) -> Result<Vec<McpGatewayTool>, ProviderFailure> {
        let result = self.request("tools/list", json!({}), timeout)?;
        validate_json_value(
            &result,
            MCP_GATEWAY_MAX_MESSAGE_BYTES,
            "provider tools response",
        )
        .map_err(|_| ProviderFailure::completed("invalid_provider_tools"))?;
        let object = result
            .as_object()
            .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))?;
        if object
            .get("nextCursor")
            .is_some_and(|cursor| !cursor.is_null())
        {
            return Err(ProviderFailure::completed(
                "provider_pagination_unsupported",
            ));
        }
        let raw_tools = object
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))?;
        let mut tools = Vec::with_capacity(raw_tools.len());
        for raw in raw_tools {
            let object = raw
                .as_object()
                .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))?;
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))?;
            let title = object
                .get("title")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))
                })
                .transpose()?;
            let description = object
                .get("description")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))
                })
                .transpose()?;
            let input_schema = object
                .get("inputSchema")
                .cloned()
                .ok_or_else(|| ProviderFailure::completed("invalid_provider_tools"))?;
            let output_schema = object.get("outputSchema").cloned();
            let annotations = object.get("annotations").cloned();
            let meta = object.get("_meta").cloned();
            tools.push(McpGatewayTool {
                name: name.to_string(),
                title,
                description,
                input_schema,
                output_schema,
                annotations,
                meta,
            });
        }
        validate_tools(&tools).map_err(|_| ProviderFailure::completed("invalid_provider_tools"))?;
        Ok(tools)
    }

    fn tools_call(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<McpGatewayToolResult, ProviderFailure> {
        // Outer MCP caller metadata deliberately stops at the WebCodex trust
        // boundary. Provider tools/call receives only gateway-owned fields.
        let params = json!({"name": name, "arguments": arguments});
        let result = self.request("tools/call", params, timeout)?;
        validate_json_value(
            &result,
            MCP_GATEWAY_MAX_MESSAGE_BYTES,
            "provider tool result",
        )
        .map_err(|_| ProviderFailure::completed("invalid_provider_result"))?;
        let object = result
            .as_object()
            .ok_or_else(|| ProviderFailure::completed("invalid_provider_result"))?;
        let raw_content = object
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderFailure::completed("invalid_provider_result"))?;
        let mut content = Vec::with_capacity(raw_content.len());
        for item in raw_content {
            let item = item
                .as_object()
                .ok_or_else(|| ProviderFailure::completed("invalid_provider_result"))?;
            if item.get("type").and_then(Value::as_str) != Some("text") {
                return Err(ProviderFailure::completed("unsupported_provider_content"));
            }
            let text = item
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderFailure::completed("invalid_provider_result"))?;
            content.push(McpGatewayContent::Text {
                text: text.to_string(),
            });
        }
        let result = McpGatewayToolResult {
            content,
            structured_content: object.get("structuredContent").cloned(),
            is_error: object
                .get("isError")
                .map(|value| {
                    value
                        .as_bool()
                        .ok_or_else(|| ProviderFailure::completed("invalid_provider_result"))
                })
                .transpose()?
                .unwrap_or(false),
        };
        validate_tool_result(&result)
            .map_err(|_| ProviderFailure::completed("invalid_provider_result"))?;
        Ok(result)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ProviderFailure> {
        let mut ignored_notifications = 0usize;
        loop {
            match self.incoming.try_recv() {
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) | Ok(ReaderEvent::Eof) => {
                    return Err(ProviderFailure::before_send("provider_eof"));
                }
                Ok(ReaderEvent::Message(Ok(message))) => {
                    if is_provider_notification(&message) {
                        ignored_notifications += 1;
                        if ignored_notifications > MCP_GATEWAY_MAX_IGNORED_NOTIFICATIONS {
                            return Err(ProviderFailure::before_send(
                                "provider_notification_flood",
                            ));
                        }
                        continue;
                    }
                    if message.get("method").is_some() {
                        return Err(ProviderFailure::before_send(
                            "provider_callbacks_unsupported",
                        ));
                    }
                    let response_id = message
                        .get("id")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX);
                    return Err(ProviderFailure::before_send(
                        if response_id < self.next_id {
                            "provider_duplicate_response_id"
                        } else {
                            "provider_unknown_response_id"
                        },
                    ));
                }
                Ok(ReaderEvent::Message(Err(ReaderFault::Malformed))) => {
                    return Err(ProviderFailure::before_send("provider_malformed_json"));
                }
                Ok(ReaderEvent::Message(Err(ReaderFault::TooLarge))) => {
                    return Err(ProviderFailure::before_send("provider_message_too_large"));
                }
                Ok(ReaderEvent::Message(Err(ReaderFault::Io))) => {
                    return Err(ProviderFailure::before_send("provider_stdout_failed"));
                }
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&message)
            .map_err(|_| ProviderFailure::before_send("provider_request_invalid"))?;
        if encoded.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
            return Err(ProviderFailure::before_send("provider_request_too_large"));
        }
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .and_then(|_| self.stdin.flush())
            .map_err(|_| ProviderFailure::after_send("provider_stdin_failed"))?;

        let deadline = Instant::now() + timeout;
        let mut ignored_notifications = 0usize;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ProviderFailure::after_send("provider_timeout"));
            }
            let response = match self.incoming.recv_timeout(remaining) {
                Ok(ReaderEvent::Message(Ok(response))) => response,
                Ok(ReaderEvent::Message(Err(ReaderFault::Malformed))) => {
                    return Err(ProviderFailure::after_send("provider_malformed_json"));
                }
                Ok(ReaderEvent::Message(Err(ReaderFault::TooLarge))) => {
                    return Err(ProviderFailure::after_send("provider_message_too_large"));
                }
                Ok(ReaderEvent::Message(Err(ReaderFault::Io))) => {
                    return Err(ProviderFailure::after_send("provider_stdout_failed"));
                }
                Ok(ReaderEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderFailure::after_send("provider_eof"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(ProviderFailure::after_send("provider_timeout"));
                }
            };
            if is_provider_notification(&response) {
                ignored_notifications += 1;
                if ignored_notifications > MCP_GATEWAY_MAX_IGNORED_NOTIFICATIONS {
                    return Err(ProviderFailure::after_send("provider_notification_flood"));
                }
                continue;
            }
            if response.get("method").is_some() {
                return Err(ProviderFailure::after_send(
                    "provider_callbacks_unsupported",
                ));
            }
            return validate_rpc_response(response, id);
        }
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<(), ProviderFailure> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&message)
            .map_err(|_| ProviderFailure::before_send("provider_request_invalid"))?;
        if encoded.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
            return Err(ProviderFailure::before_send("provider_request_too_large"));
        }
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .and_then(|_| self.stdin.flush())
            .map_err(|_| ProviderFailure::before_send("provider_stdin_failed"))
    }

    fn terminate(&mut self) {
        // Never let provider cleanup turn Runner shutdown into an unbounded
        // wait. ManagedChild owns the complete process tree; force termination
        // first, then spend one shared bounded deadline confirming tree exit
        // and reaping the direct child. Drop remains the final fail-safe.
        let deadline = Instant::now() + Duration::from_secs(1);
        let _ = self.child.terminate_tree();
        let _ = self
            .child
            .wait_tree_exit(deadline.saturating_duration_since(Instant::now()));
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10).min(remaining));
                }
            }
        }
    }
}

impl Drop for ProviderConnection {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn provider_stdout_reader(
    stdout: std::process::ChildStdout,
    sender: mpsc::SyncSender<ReaderEvent>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_bounded_line(&mut reader) {
            Ok(Some(message)) => ReaderEvent::Message(
                serde_json::from_slice(&message).map_err(|_| ReaderFault::Malformed),
            ),
            Ok(None) => ReaderEvent::Eof,
            Err(fault) => ReaderEvent::Message(Err(fault)),
        };
        let terminal = !matches!(&message, ReaderEvent::Message(Ok(_)));
        if sender.send(message).is_err() || terminal {
            return;
        }
    }
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ReaderFault> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take((MCP_GATEWAY_MAX_MESSAGE_BYTES + 2) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| ReaderFault::Io)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
            ReaderFault::TooLarge
        } else {
            ReaderFault::Malformed
        });
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.is_empty() || line.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
        return Err(if line.len() > MCP_GATEWAY_MAX_MESSAGE_BYTES {
            ReaderFault::TooLarge
        } else {
            ReaderFault::Malformed
        });
    }
    Ok(Some(line))
}

fn is_provider_notification(message: &Value) -> bool {
    let Some(object) = message.as_object() else {
        return false;
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.contains_key("id")
        || object.contains_key("result")
        || object.contains_key("error")
    {
        return false;
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return false;
    };
    if method.is_empty() || method.len() > 256 || method.chars().any(char::is_control) {
        return false;
    }
    match object.get("params") {
        None => true,
        Some(params) => params.is_object() || params.is_array(),
    }
}

fn validate_rpc_response(response: Value, expected_id: u64) -> Result<Value, ProviderFailure> {
    let object = response
        .as_object()
        .ok_or_else(|| ProviderFailure::after_send("provider_protocol_error"))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ProviderFailure::after_send("provider_protocol_error"));
    }
    if object.contains_key("method") {
        return Err(ProviderFailure::after_send(
            "provider_callbacks_unsupported",
        ));
    }
    let response_id = object
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderFailure::after_send("provider_response_id_invalid"))?;
    if response_id < expected_id {
        return Err(ProviderFailure::after_send(
            "provider_duplicate_response_id",
        ));
    }
    if response_id != expected_id {
        return Err(ProviderFailure::after_send("provider_unknown_response_id"));
    }
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(Value::Object(error)))
            if error.get("code").and_then(Value::as_i64).is_some()
                && error
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| {
                        !message.is_empty()
                            && message.len() <= 512
                            && !message.chars().any(|character| {
                                character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                            })
                    }) =>
        {
            Err(ProviderFailure::rpc_error())
        }
        _ => Err(ProviderFailure::after_send("provider_protocol_error")),
    }
}

fn validate_initialize_result(result: &Value) -> Result<(), ProviderFailure> {
    validate_json_value(
        result,
        MCP_GATEWAY_MAX_MESSAGE_BYTES,
        "provider initialize response",
    )
    .map_err(|_| ProviderFailure::after_send("provider_initialize_invalid"))?;
    let object = result
        .as_object()
        .ok_or_else(|| ProviderFailure::after_send("provider_initialize_invalid"))?;
    let server_info = object.get("serverInfo").and_then(Value::as_object);
    let implementation_field_valid = |field: &str| {
        server_info
            .and_then(|info| info.get(field))
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
            })
    };
    if object.get("protocolVersion").and_then(Value::as_str) != Some(MCP_PROTOCOL_VERSION)
        || !object.get("capabilities").is_some_and(Value::is_object)
        || !object
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("tools"))
            .is_some_and(Value::is_object)
        || !implementation_field_valid("name")
        || !implementation_field_valid("version")
    {
        return Err(ProviderFailure::after_send("provider_initialize_invalid"));
    }
    Ok(())
}

fn provider_failure_response(error: ProviderFailure) -> McpGatewayResponse {
    let state = error.dispatch_state;
    let message = match state {
        McpGatewayDispatchState::NotStarted => {
            "Provider request was not started; no downstream effect was dispatched"
        }
        McpGatewayDispatchState::OutcomeUnknown => {
            "Provider request may have been dispatched; outcome is unknown and must not be retried automatically"
        }
        McpGatewayDispatchState::Completed => {
            "Provider completed the request-response exchange but returned a downstream error or unsupported/invalid bounded result"
        }
    };
    bridge_error(state, error.code, message)
}

fn bridge_error(
    state: McpGatewayDispatchState,
    code: &'static str,
    message: &'static str,
) -> McpGatewayResponse {
    McpGatewayResponse::error(state, code, message)
}

fn stale_provider() -> McpGatewayResponse {
    bridge_error(
        McpGatewayDispatchState::NotStarted,
        "stale_provider",
        "Exact provider instance is unavailable; request was not routed elsewhere",
    )
}

#[cfg(test)]
#[path = "mcp_gateway_tests.rs"]
mod tests;
