mod http_metadata;
mod presentation;
mod protocol;
mod resources;
mod response;
mod tasks;
mod tools;

use crate::action_audit::{ActionAudit, ActionAuditRecord};
use crate::auth::AuthContext;
use crate::connector_runtime::{ConnectorRuntime, ConnectorRuntimeSlot};
use crate::json_error;
#[cfg(test)]
use crate::model_surface::ModelSurface;
use crate::model_surface::RuntimeExposure;
use crate::tool_request_trace::{
    estimate_json_bytes, jsonrpc_id_safe, new_trace_id, scope_active_trace,
    RequestCompletionTiming, ToolRequestLifecycle,
};
use crate::tool_runtime::kernel::HostFileImportTrust;
use crate::tool_runtime::model_ergonomics_telemetry::{
    ModelErgonomicsRecord, ModelErgonomicsTimer,
};
#[cfg(test)]
use crate::tool_runtime::registered_tool_specs;
#[cfg(test)]
use crate::tool_runtime::ToolResult;
use crate::tool_runtime::ToolRuntime;
#[cfg(test)]
use crate::tool_runtime::MAX_PROJECT_ARTIFACT_BYTES;
#[cfg(test)]
use crate::tool_runtime::{
    validate_project_artifact_export_snapshot, ProjectArtifactExportSnapshot,
    MAX_PROJECT_ARTIFACT_EXPORT_BYTES, MAX_READ_PROJECT_ARTIFACT_LENGTH,
};
#[cfg(test)]
use base64::Engine as _;
use futures_util::stream;
use http_metadata::{request_header, validate_http_protocol, MCP_PROTOCOL_VERSION_HEADER};
#[cfg(test)]
use http_metadata::{
    MCP_HEADER_MISMATCH, MCP_METHOD_HEADER, MCP_NAME_HEADER, MCP_UNSUPPORTED_PROTOCOL_VERSION,
};
#[cfg(test)]
use protocol::{
    inferred_protocol_era, request_protocol_version, MCP_CHATGPT_PROTOCOL_VERSION,
    MCP_SUPPORTED_PROTOCOL_VERSIONS,
};
use protocol::{
    JsonRpcRequest, McpProtocolEra, MCP_INFO_METHODS, MCP_PROTOCOL_VERSION,
    MCP_STATELESS_PROTOCOL_VERSION,
};
use response::{rpc_error, rpc_result};
use salvo::prelude::*;
use serde_json::{json, Value};
use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};
#[cfg(test)]
use tokio::sync::Semaphore;

#[cfg(test)]
use resources::*;
#[cfg(test)]
use tasks::{
    mcp_create_task_result, request_supports_tasks, MCP_MISSING_REQUIRED_CLIENT_CAPABILITY,
    MCP_TASKS_EXTENSION,
};
#[cfg(test)]
use tools::{
    add_stateless_workflow_recorder_metadata, mcp_host_file_import_trust_decision_from_state,
    mcp_host_file_import_trust_from_state, mcp_tools_list_payload_with_compact,
    mcp_tools_list_payload_with_compact_and_app, mcp_tools_list_payload_with_features_for_auth,
    project_connector_tools_list_payload_with_compact, session_context_revision_ack_from_wire,
    strip_recording_session_id, strip_stateless_ack_session_context_revision,
    strip_stateless_ack_session_message_ids, strip_stateless_context_request,
    strip_stateless_session_message_resolution, take_last_mcp_host_file_import_trust_decision,
    HostFileImportTrustReason, McpToolCallParams,
};

/// Hard upper bound on a single MCP JSON-RPC dispatch, applied in `mcp_post`.
///
/// Chosen above every per-tool wait (sync agent waits are clamped to
/// `wait_timeout_secs <= 120` plus a few seconds of margin), so it can only
/// fire when a dispatch path hangs without its own bound. Its job is to turn
/// an otherwise-permanently-silent HTTP request into an explicit JSON-RPC
/// error the client can surface.
const MCP_DISPATCH_HARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(150);

fn runtime(depot: &Depot) -> Option<Arc<ToolRuntime>> {
    depot.obtain::<Arc<ToolRuntime>>().ok().cloned()
}

fn connector_runtime_slot(depot: &Depot) -> Option<ConnectorRuntimeSlot> {
    depot.obtain::<ConnectorRuntimeSlot>().ok().cloned()
}

fn validate_runtime_exposure_state(
    runtime_exposure: RuntimeExposure,
    connector_present: bool,
) -> Result<(), String> {
    crate::model_surface::validate_connector_runtime_presence(runtime_exposure, connector_present)
}

fn finalize_mcp_tool_observability(
    runtime: &ToolRuntime,
    audit: Option<&ActionAudit>,
    audit_event: Option<(
        ActionAuditRecord,
        crate::action_audit::ActionAuditRecordTiming,
    )>,
    model_ergonomics: Option<&ModelErgonomicsRecord>,
    live_window_request: &mut Option<crate::tool_runtime::WindowActivityGuard>,
    timing: RequestCompletionTiming,
    streaming: bool,
    continuity_eligible: bool,
    outcome_class: &'static str,
) {
    let transition = live_window_request
        .as_ref()
        .map(crate::tool_runtime::WindowActivityGuard::transition);
    let meaningful = audit_event
        .as_ref()
        .is_some_and(|(event, _)| event.window_meaningful);

    if let Some(record) = model_ergonomics {
        crate::tool_runtime::runtime_metrics::observe_tool_call(runtime.metrics.as_ref(), record);
    }
    if audit_event.is_some() {
        crate::tool_runtime::runtime_metrics::observe_mcp_call(
            runtime.metrics.as_ref(),
            crate::tool_runtime::runtime_metrics::McpCallMetricObservation {
                elapsed_ms: timing.elapsed_ms,
                outcome_class,
                meaningful,
                streaming,
            },
        );
        if meaningful {
            if let Some(transition) = transition {
                crate::tool_runtime::runtime_metrics::observe_window_transition(
                    runtime.metrics.as_ref(),
                    transition,
                );
            }
        }
    }

    if let Some(active) = live_window_request.take() {
        active.complete(timing, continuity_eligible);
    }
    if let (Some(audit), Some((event, audit_timing))) = (audit, audit_event) {
        audit.record_with_completion(
            event,
            audit_timing,
            timing,
            transition,
            streaming,
            continuity_eligible,
        );
    }
}

#[cfg(test)]
pub(crate) fn mcp_runtime_tool_result(
    tool_name: &str,
    as_image_requested: bool,
    result: ToolResult,
) -> Value {
    tools::mcp_runtime_tool_result(tool_name, as_image_requested, result)
}

/// Outcome of handling a single MCP JSON-RPC request.
///
/// Carries the JSON-RPC response body alongside the HTTP status the HTTP
/// wrapper should render. Keeping this separate from `Response` makes the
/// core protocol logic testable without a live server.
#[derive(Debug)]
enum McpOutcome {
    /// A normal JSON-RPC result. HTTP 200 with the body.
    Ok(Value),
    /// A preflighted artifact resource read whose JSON-RPC body is emitted
    /// incrementally by the HTTP wrapper without whole-file aggregation.
    ArtifactExportStream {
        id: Value,
        plan: resources::McpArtifactExportStreamPlan,
    },
    /// A JSON-RPC protocol error. HTTP 400 with the error body.
    BadRequest(Value),
    /// A modern MCP method is not implemented. HTTP 404 with JSON-RPC -32601.
    NotFound(Value),
    /// A JSON-RPC notification (request without an `id` member). Per the
    /// JSON-RPC 2.0 and MCP specs the server MUST NOT reply with a
    /// JSON-RPC response body. The HTTP wrapper acknowledges with 202 and
    /// an empty body.
    Notification,
    /// The HTTP request authenticated, but the OAuth2 bearer token lacks the
    /// delegated scope needed by this JSON-RPC method or tool.
    Forbidden {
        body: Value,
        required_scope: Option<&'static str>,
    },
}

fn log_mcp_computer_app_resource_delivery(
    uri: &str,
    protocol_era: &str,
    ui_capability_present: bool,
    http_status: u16,
    mcp_error_code: Option<i64>,
) {
    tracing::info!(
        target: "webcodex::mcp",
        uri,
        protocol_era,
        ui_capability_present,
        http_status,
        mcp_error_code = mcp_error_code.unwrap_or(-1),
        "mcp_computer_app_resource_delivery"
    );
}

fn log_mcp_computer_app_resource_outcome(
    uri: &str,
    protocol_era: McpProtocolEra,
    ui_capability_present: bool,
    outcome: &McpOutcome,
) {
    let (http_status, mcp_error_code) = match outcome {
        McpOutcome::Ok(_) | McpOutcome::ArtifactExportStream { .. } => (200, None),
        McpOutcome::BadRequest(body) => (400, body["error"]["code"].as_i64()),
        McpOutcome::NotFound(body) => (404, body["error"]["code"].as_i64()),
        McpOutcome::Notification => (202, None),
        McpOutcome::Forbidden { .. } => (403, None),
    };
    log_mcp_computer_app_resource_delivery(
        uri,
        protocol::era_label(protocol_era),
        ui_capability_present,
        http_status,
        mcp_error_code,
    );
}

fn mcp_tools_list_audit_summary(
    result: &Value,
    protocol_era: McpProtocolEra,
    runtime_exposure: RuntimeExposure,
    compact_schemas: bool,
) -> Option<Value> {
    let tools = result.get("tools")?.as_array()?;
    let serialized_tools_bytes = serde_json::to_vec(&result["tools"]).ok()?.len() as u64;
    let serialized_result_bytes = serde_json::to_vec(result).ok()?.len() as u64;
    let gateway_tool_included = tools.iter().any(|tool| {
        tool.get("name").and_then(Value::as_str) == Some(crate::mcp_gateway::MCP_TOOL_NAME)
    });
    Some(json!({
        "transport": "mcp",
        "tool_surface": {
            "schema_version": 1,
            "protocol_era": protocol::era_label(protocol_era),
            "runtime_exposure": runtime_exposure.name(),
            "compact_schemas": compact_schemas,
            "tool_count": tools.len() as u64,
            "serialized_tools_bytes": serialized_tools_bytes,
            "serialized_result_bytes": serialized_result_bytes,
            "gateway_tool_included": gateway_tool_included
        }
    }))
}

#[handler]
pub async fn mcp_info(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    if let Err((status, _, message)) = crate::auth::require_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
        res.status_code(status);
        res.render(json_error(status, message));
        return;
    }
    if request_header(req, MCP_PROTOCOL_VERSION_HEADER) == Some(MCP_STATELESS_PROTOCOL_VERSION) {
        res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        return;
    }
    let auth_required = crate::auth::get_config(depot)
        .map(|c| c.is_auth_enabled())
        .unwrap_or(false);
    let Some(runtime) = runtime(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Tool runtime not configured",
        ));
        return;
    };
    let Some(connector_slot) = connector_runtime_slot(depot) else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP runtime exposure state not configured",
        ));
        return;
    };
    let runtime_exposure = runtime.runtime_exposure();
    if let Err(error) =
        validate_runtime_exposure_state(runtime_exposure, connector_slot.0.is_some())
    {
        tracing::error!(%error, "MCP runtime exposure state mismatch");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, error));
        return;
    }
    res.render(Json(json!({
        "name": "webcodex",
        "version": env!("CARGO_PKG_VERSION"),
        "runtimeExposure": runtime_exposure.name(),
        "protocol": "mcp",
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "transport": "streamable-http-jsonrpc",
        "endpoint": "/mcp",
        "methods": MCP_INFO_METHODS,
        "auth": {
            "type": "bearer",
            "required": auth_required,
            "header": "Authorization: Bearer <shared_key_or_wc_pat>"
        }
    })));
}

#[handler]
pub async fn mcp_post(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let mut guard = ToolRequestLifecycle::new("mcp", new_trace_id(), "-", "POST /mcp", None);
    guard.received();

    if let Err((status, _, message)) = crate::auth::require_json_same_origin(req) {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        guard.parsed("http_validation_error");
        guard.response_serialized(
            status.as_u16(),
            None,
            Some(false),
            None,
            "http_validation_error",
        );
        res.status_code(status);
        res.render(json_error(status, message));
        guard.handler_returned(
            status.as_u16(),
            None,
            Some(false),
            None,
            "http_validation_error",
        );
        return;
    }

    let Some(runtime) = runtime(depot) else {
        // Size unknown without building the json_error body for measurement.
        guard.response_serialized(500, None, Some(false), None, "error_runtime_missing");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Tool runtime not configured",
        ));
        guard.handler_returned(500, None, Some(false), None, "error_runtime_missing");
        return;
    };
    let Some(connector_slot) = connector_runtime_slot(depot) else {
        guard.response_serialized(500, None, Some(false), None, "error_surface_state_missing");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "MCP runtime exposure state not configured",
        ));
        guard.handler_returned(500, None, Some(false), None, "error_surface_state_missing");
        return;
    };
    let connector = connector_slot.0;
    if let Err(error) =
        validate_runtime_exposure_state(runtime.runtime_exposure(), connector.is_some())
    {
        tracing::error!(%error, "MCP runtime exposure state mismatch");
        guard.response_serialized(500, None, Some(false), None, "error_surface_state_mismatch");
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(json_error(StatusCode::INTERNAL_SERVER_ERROR, error));
        guard.handler_returned(500, None, Some(false), None, "error_surface_state_mismatch");
        return;
    }
    let request: JsonRpcRequest = match req.parse_json().await {
        Ok(request) => request,
        Err(e) => {
            guard.set_jsonrpc_id("none");
            guard.parsed("parse_error");
            let body = rpc_error(None, -32700, format!("Parse error: {}", e));
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "parse_error");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            guard.handler_returned(400, estimated, Some(false), None, "parse_error");
            return;
        }
    };

    guard.set_jsonrpc_id(jsonrpc_id_safe(request.id.as_ref()));
    guard.set_method(request.method.clone());
    let tool_name = if request.method == "tools/call" {
        tools::tool_name_from_params(&request.params)
    } else {
        None
    };
    guard.set_tool_name(tool_name.clone());
    let computer_app_resource_uri = if request.method == "resources/read" {
        request
            .params
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| resources::is_mcp_computer_app_resource_uri(uri))
            .map(str::to_string)
    } else {
        None
    };
    let computer_app_ui_capability_present = resources::request_supports_mcp_apps(&request.params);
    // Computer App resource delivery is part of gray-card diagnosis, but its
    // durable projection must remain metadata-only. Never persist App HTML,
    // screenshot/tool-result content, tool arguments, window titles, or other
    // request payload fields here.
    let computer_app_resource_audit = computer_app_resource_uri.as_ref().and_then(|uri| {
        request.id.as_ref().map(|_| {
            (
                ActionAudit::start(req, depot, "/mcp", "resourcesRead"),
                uri.clone(),
            )
        })
    });
    let record_computer_app_resource_audit =
        |protocol_era: &str, status: StatusCode, mcp_error_code: Option<i64>| {
            if let Some((audit, uri)) = computer_app_resource_audit.as_ref() {
                audit.record(
                    ActionAuditRecord::new(
                        "computer_app_resource_read",
                        status.is_success(),
                        status,
                    )
                    .summary(json!({
                        "transport": "mcp",
                        "resource_uri": uri,
                        "resource_version": uri.rsplit('/').next().unwrap_or("unknown"),
                        "protocol_era": protocol_era,
                        "ui_capability_present": computer_app_ui_capability_present,
                        "mcp_error_code": mcp_error_code,
                    })),
                );
            }
        };
    let protocol_era = match validate_http_protocol(req, &request) {
        Ok(protocol_era) => protocol_era,
        Err(body) => {
            guard.parsed("protocol_error");
            if let Some(uri) = computer_app_resource_uri.as_deref() {
                log_mcp_computer_app_resource_delivery(
                    uri,
                    "validation_failed",
                    computer_app_ui_capability_present,
                    400,
                    body["error"]["code"].as_i64(),
                );
                record_computer_app_resource_audit(
                    "validation_failed",
                    StatusCode::BAD_REQUEST,
                    body["error"]["code"].as_i64(),
                );
            }
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "protocol_error");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            guard.handler_returned(400, estimated, Some(false), None, "protocol_error");
            return;
        }
    };
    let window = match protocol_era {
        McpProtocolEra::Legacy => {
            crate::client_window::mcp_window(req, request.method == "initialize")
        }
        McpProtocolEra::Stateless2026 => {
            crate::client_window::stateless_mcp_window(&request.params)
        }
    };
    guard.set_client_window(window.identity.as_ref());
    guard.parsed("ok");
    let server_trace_id = guard.correlation_trace_id();
    let auth = depot.obtain::<crate::auth::AuthContext>().ok().cloned();
    let live_principal = crate::tool_runtime::runtime_observation_principal(auth.as_ref()).ok();
    let window_registry = runtime.window_activity_registry();
    let mut live_window_request =
        if request.id.is_some() && matches!(request.method.as_str(), "tools/call" | "tools/list") {
            window.identity.as_ref().map(|identity| {
                window_registry.start_observed(
                    identity,
                    &server_trace_id,
                    &request.method,
                    tool_name.as_deref(),
                    live_principal
                        .as_ref()
                        .map(|(kind, id)| (kind.as_str(), id.as_str())),
                    guard.request_observed_at_ms(),
                )
            })
        } else {
            None
        };

    // Chat-window MCP tool calls must land in the action audit exactly like
    // the REST surface (they were previously invisible there). Summary-level
    // only: tool name and project — never arguments or outputs. JSON-RPC
    // notifications are acknowledged but never dispatched, so they must not be
    // represented as executed actions.
    let audit = if request.method == "tools/call" && request.id.is_some() {
        Some((
            ActionAudit::start(req, depot, "/mcp", "toolsCall")
                .with_window(window.identity.as_ref(), Some(&server_trace_id)),
            tool_name.clone().unwrap_or_else(|| "unknown".to_string()),
            tools::project_from_tool_call_params(&request.params),
        ))
    } else {
        None
    };
    let build_audit_event = |success: bool,
                             status: StatusCode,
                             error: Option<String>,
                             model_ergonomics: Option<&ModelErgonomicsRecord>,
                             correlation: &crate::tool_runtime::ToolCallCorrelation|
     -> Option<(
        ActionAuditRecord,
        crate::action_audit::ActionAuditRecordTiming,
    )> {
        if let Some((audit, tool, project)) = audit.as_ref() {
            let mut summary = json!({ "transport": "mcp" });
            if let Some(telemetry) =
                model_ergonomics.and_then(|record| serde_json::to_value(record).ok())
            {
                summary["model_ergonomics"] = telemetry;
            }
            let mut event = ActionAuditRecord::new(tool.clone(), success, status)
                .error(error)
                .summary(summary)
                .meaningful(crate::tool_runtime::is_meaningful_activity_tool(tool))
                .recorder_gap(correlation.recorder_gap_session_id.clone());
            event.project = correlation
                .resolved_project
                .clone()
                .or_else(|| project.clone());
            for link in &correlation.workflow_sessions {
                let relation = match link.relation {
                    crate::tool_runtime::WorkflowSessionCorrelationRelation::Recording => {
                        crate::action_audit_sessions::WorkflowSessionRelation::Recording
                    }
                    crate::tool_runtime::WorkflowSessionCorrelationRelation::WorkOnProject => {
                        crate::action_audit_sessions::WorkflowSessionRelation::WorkOnProject
                    }
                };
                event =
                    event.workflow_link(link.session_id.clone(), relation, link.project.clone());
            }
            Some((event, audit.capture_record_timing()))
        } else {
            None
        }
    };

    let tools_list_audit = if request.method == "tools/list" && request.id.is_some() {
        Some(
            ActionAudit::start(req, depot, "/mcp", "toolsList")
                .with_window(window.identity.as_ref(), Some(&server_trace_id)),
        )
    } else {
        None
    };
    let compact_schemas = crate::model_surface::effective_mcp_compact_schemas(
        runtime.runtime_exposure(),
        crate::config::mcp_compact_schemas_override(),
    );
    let server_mcp_apps_enabled = crate::config::mcp_apps_enabled();

    let config = crate::auth::get_config(depot);
    let db = crate::auth::get_db(depot);
    let host_file_import_trust = tools::host_file_import_trust_for_call(
        tool_name.as_deref(),
        auth.as_ref(),
        config.as_deref(),
        db.as_deref(),
    );
    // Defense-in-depth backstop: every tool bounds its own agent/subprocess
    // waits at <= 124s, so this outer limit never preempts a legitimate inner
    // timeout. It only fires if a dispatch path hangs without a bound (the
    // failure mode behind "MCP request never gets a reply"), converting a
    // silently dead HTTP request into an observable JSON-RPC error.
    let request_id = request.id.clone();
    // The shared kernel timer is authoritative for completed runtime calls. Keep
    // one outer emergency timer only so the MCP hard-timeout path does not erase
    // an otherwise established runtime invocation from ergonomics telemetry.
    let mut hard_timeout_model_ergonomics =
        if runtime.runtime_exposure() == RuntimeExposure::ProjectConnector {
            None
        } else {
            tool_name.as_deref().and_then(ModelErgonomicsTimer::start)
        };
    let mut tool_correlation = crate::tool_runtime::ToolCallCorrelation::default();
    let mut model_ergonomics = None;
    // Window liveness needs request correlation even when trace retention is off.
    let active_trace_id = Some(server_trace_id.clone());
    // Keep the complete MCP dispatch future off the current thread's stack. The
    // handler state spans every method arm (including Connector task polling),
    // so nesting it inline under tracing + timeout can exhaust the default
    // ~2 MiB libtest/Tokio worker stack even when a request takes another arm.
    let outcome = match tokio::time::timeout(
        MCP_DISPATCH_HARD_TIMEOUT,
        scope_active_trace(
            active_trace_id,
            Box::pin(handle_mcp_request_with_lifecycle(
                &runtime,
                connector.as_deref(),
                request,
                auth.as_ref(),
                protocol_era,
                host_file_import_trust,
                window.identity.as_ref(),
                Some(&mut guard),
                Some(&mut model_ergonomics),
                compact_schemas,
                server_mcp_apps_enabled,
                Some(&mut tool_correlation),
            )),
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            let body = rpc_error(
                request_id,
                -32000,
                format!(
                    "server-side dispatch exceeded {}s hard limit; the tool may still be running — check session/job status before retrying",
                    MCP_DISPATCH_HARD_TIMEOUT.as_secs()
                ),
            );
            if let Some(uri) = computer_app_resource_uri.as_deref() {
                log_mcp_computer_app_resource_delivery(
                    uri,
                    protocol::era_label(protocol_era),
                    computer_app_ui_capability_present,
                    500,
                    Some(-32000),
                );
                record_computer_app_resource_audit(
                    protocol::era_label(protocol_era),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some(-32000),
                );
            }
            let timeout_model_ergonomics = hard_timeout_model_ergonomics.take().map(|timer| {
                timer
                    .finish()
                    .record_for_pre_result_failure("dispatch_hard_timeout")
            });
            let audit_event = build_audit_event(
                false,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("mcp dispatch hard timeout".to_string()),
                timeout_model_ergonomics.as_ref(),
                &tool_correlation,
            );
            guard.capture_payload("final_response", &body);
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(500, estimated, Some(false), None, "dispatch_hard_timeout");
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(body));
            let timing =
                guard.handler_returned(500, estimated, Some(false), None, "dispatch_hard_timeout");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                timeout_model_ergonomics.as_ref(),
                &mut live_window_request,
                timing,
                false,
                false,
                "unknown",
            );
            return;
        }
    };

    if let (Some(active), Some(project)) = (
        live_window_request.as_ref(),
        tool_correlation.resolved_project.as_deref(),
    ) {
        active.update(None, Some(project));
    }

    if let Some(audit) = tools_list_audit.as_ref() {
        match &outcome {
            McpOutcome::Ok(body) => {
                let summary = body.get("result").and_then(|result| {
                    mcp_tools_list_audit_summary(
                        result,
                        protocol_era,
                        runtime.runtime_exposure(),
                        compact_schemas,
                    )
                });
                let mut event = ActionAuditRecord::new(
                    "mcp_tools_list",
                    summary.is_some(),
                    if summary.is_some() {
                        StatusCode::OK
                    } else {
                        StatusCode::INTERNAL_SERVER_ERROR
                    },
                );
                if let Some(summary) = summary {
                    event = event.summary(summary);
                }
                audit.record(event);
            }
            McpOutcome::BadRequest(_) => audit.record(
                ActionAuditRecord::new("mcp_tools_list", false, StatusCode::BAD_REQUEST)
                    .summary(json!({"transport": "mcp"})),
            ),
            McpOutcome::NotFound(_) => audit.record(
                ActionAuditRecord::new("mcp_tools_list", false, StatusCode::NOT_FOUND)
                    .summary(json!({"transport": "mcp"})),
            ),
            McpOutcome::Forbidden { .. } => audit.record(
                ActionAuditRecord::new("mcp_tools_list", false, StatusCode::FORBIDDEN)
                    .summary(json!({"transport": "mcp"})),
            ),
            McpOutcome::ArtifactExportStream { .. } | McpOutcome::Notification => {}
        }
    }

    if let Some(uri) = computer_app_resource_uri.as_deref() {
        log_mcp_computer_app_resource_outcome(
            uri,
            protocol_era,
            computer_app_ui_capability_present,
            &outcome,
        );
        let (status, mcp_error_code) = match &outcome {
            McpOutcome::Ok(_) | McpOutcome::ArtifactExportStream { .. } => (StatusCode::OK, None),
            McpOutcome::BadRequest(body) => {
                (StatusCode::BAD_REQUEST, body["error"]["code"].as_i64())
            }
            McpOutcome::NotFound(body) => (StatusCode::NOT_FOUND, body["error"]["code"].as_i64()),
            McpOutcome::Notification => (StatusCode::ACCEPTED, None),
            McpOutcome::Forbidden { .. } => (StatusCode::FORBIDDEN, None),
        };
        record_computer_app_resource_audit(
            protocol::era_label(protocol_era),
            status,
            mcp_error_code,
        );
    }

    if matches!(
        outcome,
        McpOutcome::Ok(_) | McpOutcome::ArtifactExportStream { .. }
    ) {
        if let Some(session_id) = window.issued_session_id.as_deref() {
            crate::client_window::set_mcp_session_header(res, session_id);
        }
    }

    match outcome {
        McpOutcome::Ok(body) => {
            // Protocol success: valid JSON-RPC result envelope.
            // Tool success: only meaningful for tools/call (isError / structuredContent.success).
            let tool_success = body
                .get("result")
                .and_then(|r| r.get("structuredContent"))
                .and_then(|s| s.get("success").or_else(|| s.get("ok")))
                .and_then(|v| v.as_bool());
            let audit_success = tool_success.unwrap_or(true);
            let audit_event = build_audit_event(
                audit_success,
                StatusCode::OK,
                if audit_success {
                    None
                } else {
                    body["result"]["structuredContent"]["error"]
                        .as_str()
                        .map(str::to_string)
                },
                model_ergonomics.as_ref(),
                &tool_correlation,
            );
            guard.capture_payload("final_response", &body);
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(200, estimated, Some(true), tool_success, "ok");
            res.render(Json(body));
            let timing = guard.handler_returned(200, estimated, Some(true), tool_success, "ok");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                model_ergonomics.as_ref(),
                &mut live_window_request,
                timing,
                false,
                true,
                if audit_success { "success" } else { "failure" },
            );
        }
        McpOutcome::ArtifactExportStream { id, plan } => {
            let audit_event =
                build_audit_event(true, StatusCode::OK, None, None, &tool_correlation);
            guard.response_serialized(200, None, Some(true), None, "artifact_export_stream");
            res.status_code(StatusCode::OK);
            let _ = res.add_header("content-type", "application/json", true);
            let receiver =
                resources::start_artifact_export_stream(runtime.clone(), id, auth.clone(), plan);
            let response_stream = stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|frame| (frame, receiver))
            });
            res.stream(response_stream);
            let timing =
                guard.handler_returned(200, None, Some(true), None, "artifact_export_stream");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                None,
                &mut live_window_request,
                timing,
                true,
                false,
                "success",
            );
        }
        McpOutcome::BadRequest(body) => {
            let audit_event = build_audit_event(
                false,
                StatusCode::BAD_REQUEST,
                body["error"]["message"].as_str().map(str::to_string),
                model_ergonomics.as_ref(),
                &tool_correlation,
            );
            guard.capture_payload("final_response", &body);
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(400, estimated, Some(false), None, "bad_request");
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(body));
            let timing = guard.handler_returned(400, estimated, Some(false), None, "bad_request");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                model_ergonomics.as_ref(),
                &mut live_window_request,
                timing,
                false,
                true,
                "failure",
            );
        }
        McpOutcome::NotFound(body) => {
            let audit_event = build_audit_event(
                false,
                StatusCode::NOT_FOUND,
                body["error"]["message"].as_str().map(str::to_string),
                model_ergonomics.as_ref(),
                &tool_correlation,
            );
            guard.capture_payload("final_response", &body);
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(404, estimated, Some(false), None, "not_found");
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(body));
            let timing = guard.handler_returned(404, estimated, Some(false), None, "not_found");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                model_ergonomics.as_ref(),
                &mut live_window_request,
                timing,
                false,
                true,
                "failure",
            );
        }
        McpOutcome::Forbidden {
            body,
            required_scope,
        } => {
            let audit_event = build_audit_event(
                false,
                StatusCode::FORBIDDEN,
                Some(format!(
                    "insufficient scope: {}",
                    required_scope.unwrap_or("unknown")
                )),
                model_ergonomics.as_ref(),
                &tool_correlation,
            );
            guard.capture_payload("final_response", &body);
            let estimated = estimate_json_bytes(&body);
            guard.response_serialized(403, estimated, Some(false), None, "forbidden");
            res.status_code(StatusCode::FORBIDDEN);
            if auth.as_ref().is_some_and(AuthContext::is_oauth_token) {
                let challenge = crate::auth::oauth_insufficient_scope_challenge(required_scope);
                if let Ok(val) = salvo::http::HeaderValue::from_str(&challenge) {
                    res.headers_mut().insert("www-authenticate", val);
                }
            }
            res.render(Json(body));
            let timing = guard.handler_returned(403, estimated, Some(false), None, "forbidden");
            finalize_mcp_tool_observability(
                &runtime,
                audit.as_ref().map(|(audit, _, _)| audit),
                audit_event,
                model_ergonomics.as_ref(),
                &mut live_window_request,
                timing,
                false,
                true,
                "failure",
            );
        }
        McpOutcome::Notification => {
            // JSON-RPC notifications carry no `id`; the server MUST NOT reply
            // with a JSON-RPC body. Acknowledge with 202 and an empty body.
            // Empty body size is known (0) without JSON serialization.
            guard.response_serialized(202, Some(0), Some(true), None, "notification");
            res.status_code(StatusCode::ACCEPTED);
            guard.handler_returned(202, Some(0), Some(true), None, "notification");
        }
    }
}

/// Core MCP JSON-RPC dispatch. Pure (no HTTP types) so it can be unit tested.
///
/// Business logic stays in `ToolRuntime`; this function only frames the
/// JSON-RPC envelope and translates tool results into MCP content blocks.
/// Test-friendly wrapper: no lifecycle hooks.
#[cfg(test)]
async fn handle_mcp_request(
    runtime: &ToolRuntime,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
) -> McpOutcome {
    let protocol_era = inferred_protocol_era(&request);
    let compact_schemas = crate::model_surface::effective_mcp_compact_schemas(
        runtime.runtime_exposure(),
        crate::config::mcp_compact_schemas_override(),
    );
    let server_mcp_apps_enabled = crate::config::mcp_apps_enabled();
    let outcome = handle_mcp_request_with_lifecycle(
        runtime,
        None,
        request,
        auth,
        protocol_era,
        HostFileImportTrust::Untrusted,
        None,
        None,
        None,
        compact_schemas,
        server_mcp_apps_enabled,
        None,
    )
    .await;
    match outcome {
        McpOutcome::ArtifactExportStream { id, plan } => {
            match resources::mcp_artifact_export_collect_stream_response(runtime, &id, auth, plan)
                .await
            {
                Ok(body) => McpOutcome::Ok(body),
                Err(error) => {
                    resources::mcp_artifact_export_read_error_outcome(Some(id), auth, error)
                }
            }
        }
        outcome => outcome,
    }
}

async fn handle_mcp_request_with_lifecycle(
    runtime: &ToolRuntime,
    connector: Option<&ConnectorRuntime>,
    request: JsonRpcRequest,
    auth: Option<&AuthContext>,
    protocol_era: McpProtocolEra,
    host_file_import_trust: HostFileImportTrust,
    window: Option<&crate::client_window::ClientWindow>,
    mut lifecycle: Option<&mut ToolRequestLifecycle>,
    mut model_ergonomics_out: Option<&mut Option<ModelErgonomicsRecord>>,
    compact_schemas: bool,
    server_mcp_apps_enabled: bool,
    mut correlation_out: Option<&mut crate::tool_runtime::ToolCallCorrelation>,
) -> McpOutcome {
    let stateless_2026 = protocol_era == McpProtocolEra::Stateless2026;
    let runtime_exposure = runtime.runtime_exposure();
    let resource_read_bypasses_runtime_read =
        matches!(runtime_exposure, RuntimeExposure::Runtime(_))
            && stateless_2026
            && request.method == "resources/read"
            && resources::resource_read_bypasses_runtime_read(&request.params);
    let mcp_app_enabled = match runtime_exposure {
        RuntimeExposure::Runtime(model_surface) => resources::mcp_app_enabled(
            server_mcp_apps_enabled,
            stateless_2026,
            model_surface,
            &request.params,
        ),
        RuntimeExposure::ProjectConnector => false,
    };
    let runtime_resource_method = matches!(runtime_exposure, RuntimeExposure::Runtime(_))
        && matches!(request.method.as_str(), "resources/list" | "resources/read");

    if auth.is_some()
        && (matches!(request.method.as_str(), "server/discover" | "tools/list")
            || (runtime_resource_method && !resource_read_bypasses_runtime_read)
            || (!stateless_2026
                && matches!(
                    request.method.as_str(),
                    "initialize" | "ping" | "notifications/initialized"
                )))
    {
        if let Some(outcome) = require_mcp_scope(auth, crate::auth::SCOPE_RUNTIME_READ) {
            return outcome;
        }
    }

    if auth.is_some_and(|auth| !auth.is_bootstrap())
        && !stateless_2026
        && !matches!(
            request.method.as_str(),
            "server/discover"
                | "initialize"
                | "ping"
                | "tools/list"
                | "tools/call"
                | "notifications/initialized"
        )
    {
        return scope_forbidden(
            auth,
            None,
            "authenticated caller cannot call unknown MCP methods",
        );
    }

    // A JSON-RPC request without an `id` member is a notification. Per the
    // JSON-RPC 2.0 and MCP specs the server MUST NOT reply with a JSON-RPC
    // response body, even if the method is unknown or malformed. We accept
    // the notification silently. `notifications/initialized` is the common
    // case sent by MCP clients after `initialize` completes.
    if request.id.is_none() {
        return McpOutcome::Notification;
    }

    let jsonrpc_valid = if stateless_2026 {
        request.jsonrpc.as_deref() == Some("2.0")
    } else {
        request.jsonrpc.as_deref().unwrap_or("2.0") == "2.0"
    };
    if !jsonrpc_valid {
        return McpOutcome::BadRequest(rpc_error(request.id, -32600, "jsonrpc must be '2.0'"));
    }

    if let Err(error) = validate_runtime_exposure_state(runtime_exposure, connector.is_some()) {
        return McpOutcome::BadRequest(rpc_error(request.id, -32603, error));
    }

    let id = request.id.clone();
    let response = match request.method.as_str() {
        // MCP 2026-07-28 clients discover capabilities before issuing ordinary
        // requests. WebCodex supports the stateless tools path required by
        // modern clients while retaining the initialized 2025 tool-only
        // session lifecycle used by 2025-06-18 and ChatGPT 2025-11-25 clients.
        "server/discover" if stateless_2026 => {
            let capabilities = match runtime_exposure {
                RuntimeExposure::ProjectConnector => tasks::server_capabilities(),
                RuntimeExposure::Runtime(model_surface)
                    if resources::model_surface_supports_computer_app(model_surface) =>
                {
                    resources::server_capabilities(server_mcp_apps_enabled)
                }
                RuntimeExposure::Runtime(_) => json!({ "tools": { "listChanged": false } }),
            };
            rpc_result(
                id,
                protocol::server_discover_payload(capabilities, runtime_exposure.name()),
            )
        }
        "initialize" if !stateless_2026 => rpc_result(
            id,
            protocol::legacy_initialize_payload(&request.params, runtime_exposure.name()),
        ),
        "ping" if !stateless_2026 => rpc_result(id, json!({})),
        "tools/list" => {
            return tools::handle_list(
                runtime,
                id,
                auth,
                stateless_2026,
                compact_schemas,
                mcp_app_enabled,
            )
            .await;
        }
        "resources/list" if stateless_2026 && runtime_resource_method => {
            return resources::handle_list(runtime, id, mcp_app_enabled);
        }
        "resources/read" if stateless_2026 && runtime_resource_method => {
            let RuntimeExposure::Runtime(model_surface) = runtime_exposure else {
                unreachable!("runtime_resource_method requires a runtime ModelSurface");
            };
            return resources::handle_read(
                runtime,
                request.params,
                id,
                auth,
                model_surface,
                server_mcp_apps_enabled,
            )
            .await;
        }
        method @ ("tasks/get" | "tasks/update" | "tasks/cancel")
            if stateless_2026 && tasks::runtime_exposure_supports_tasks(runtime_exposure) =>
        {
            return tasks::handle_request(
                method,
                request.params,
                id,
                auth,
                connector.expect("validated ProjectConnector runtime state"),
            )
            .await;
        }
        "tools/call" => {
            return tools::handle_call(
                runtime,
                connector,
                request.params,
                id,
                auth,
                stateless_2026,
                mcp_app_enabled,
                server_mcp_apps_enabled,
                host_file_import_trust,
                window,
                lifecycle.as_deref_mut(),
                model_ergonomics_out.as_deref_mut(),
                correlation_out.as_deref_mut(),
            )
            .await;
        }
        "notifications/initialized" if !stateless_2026 => rpc_result(id, json!({})),
        _ => {
            let body = rpc_error(id, -32601, format!("Method not found: {}", request.method));
            return if stateless_2026 {
                McpOutcome::NotFound(body)
            } else {
                McpOutcome::BadRequest(body)
            };
        }
    };
    McpOutcome::Ok(response)
}

fn require_mcp_scope(auth: Option<&AuthContext>, scope: &'static str) -> Option<McpOutcome> {
    let auth = auth?;
    if auth.has_scope(scope) {
        return None;
    }
    Some(scope_forbidden(
        Some(auth),
        Some(scope),
        format!("missing required scope: {}", scope),
    ))
}

fn scope_forbidden(
    auth: Option<&AuthContext>,
    required_scope: Option<&'static str>,
    description: impl Into<String>,
) -> McpOutcome {
    McpOutcome::Forbidden {
        body: crate::auth::scope_forbidden_body(auth, description),
        required_scope,
    }
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
