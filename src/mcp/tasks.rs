use super::protocol::request_client_capabilities;
use super::response::{
    connector_call_tool_result, mcp_stateless_result, rpc_error, rpc_error_with_data, rpc_result,
};
use super::{require_mcp_scope, scope_forbidden, McpOutcome};
use crate::auth::{AuthContext, SCOPE_JOB_RUN};
use crate::connector_runtime::{ConnectorCallOutcome, ConnectorRuntime};
use crate::db::ConnectorExecution;
use crate::model_surface::RuntimeExposure;
use serde::Deserialize;
use serde_json::{json, Value};
use webcodex_store::ConnectorExecutionState;

pub(super) const MCP_TASKS_EXTENSION: &str = "io.modelcontextprotocol/tasks";
pub(super) const MCP_MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
const MCP_TASK_POLL_INTERVAL_MS: u64 = 2_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpTaskParams {
    task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpTaskUpdateParams {
    task_id: String,
    input_responses: Value,
}

pub(super) fn request_supports_tasks(params: &Value) -> bool {
    request_client_capabilities(params)
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(MCP_TASKS_EXTENSION))
        .is_some_and(Value::is_object)
}

pub(super) fn runtime_exposure_supports_tasks(exposure: RuntimeExposure) -> bool {
    matches!(exposure, RuntimeExposure::ProjectConnector)
}

#[cfg(test)]
mod exposure_tests {
    use super::*;
    use crate::model_surface::ModelSurface;

    #[test]
    fn mcp_tasks_are_owned_only_by_project_connector_exposure() {
        assert!(runtime_exposure_supports_tasks(
            RuntimeExposure::ProjectConnector
        ));
        for surface in [
            ModelSurface::LocalCoding,
            ModelSurface::AdaptiveRuntime,
            ModelSurface::FullOperatorRuntime,
        ] {
            assert!(!runtime_exposure_supports_tasks(RuntimeExposure::Runtime(
                surface
            )));
        }
    }
}

pub(super) fn server_capabilities() -> Value {
    json!({
        "tools": { "listChanged": false },
        "extensions": {
            MCP_TASKS_EXTENSION: {}
        }
    })
}

fn mcp_task_timestamp(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn mcp_task_id_is_valid(task_id: &str) -> bool {
    task_id.strip_prefix("wc_exec_").is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn mcp_task_status(execution: &ConnectorExecution) -> &'static str {
    if execution.state == ConnectorExecutionState::Cancelled {
        "cancelled"
    } else if execution.is_terminal() {
        "completed"
    } else {
        "working"
    }
}

fn mcp_task_base(execution: &ConnectorExecution, result_type: &str, status: &str) -> Value {
    let created_at = execution
        .mcp_task_materialized_at
        .unwrap_or(execution.submitted_at);
    let last_updated_at = execution
        .mcp_task_result_finalized_at
        .or(execution.finished_at)
        .or(execution.last_output_at)
        .or(execution.started_at)
        .or(execution.queued_at)
        .unwrap_or(created_at)
        .max(created_at);
    json!({
        "resultType": result_type,
        "taskId": execution.execution_id,
        "status": status,
        "createdAt": mcp_task_timestamp(created_at),
        "lastUpdatedAt": mcp_task_timestamp(last_updated_at),
        "ttlMs": null,
        "pollIntervalMs": MCP_TASK_POLL_INTERVAL_MS
    })
}

pub(super) fn mcp_create_task_result(execution: &ConnectorExecution) -> Value {
    mcp_task_base(execution, "task", mcp_task_status(execution))
}

fn mcp_detailed_task_result(
    execution: &ConnectorExecution,
    call_tool_result: Option<Value>,
) -> Value {
    let status = mcp_task_status(execution);
    let mut result = mcp_task_base(execution, "complete", status);
    if status == "completed" {
        if let (Some(object), Some(call_tool_result)) = (result.as_object_mut(), call_tool_result) {
            object.insert("result".to_string(), call_tool_result);
        }
    }
    result
}

fn missing_tasks_capability(id: Option<Value>) -> McpOutcome {
    McpOutcome::BadRequest(rpc_error_with_data(
        id,
        MCP_MISSING_REQUIRED_CLIENT_CAPABILITY,
        "Missing required client capability",
        json!({
            "requiredCapabilities": {
                "extensions": {
                    MCP_TASKS_EXTENSION: {}
                }
            }
        }),
    ))
}

fn mcp_task_connector_error(
    id: Option<Value>,
    auth: Option<&AuthContext>,
    outcome: ConnectorCallOutcome,
) -> McpOutcome {
    if let Some(required_scope) = outcome.required_scope {
        let description = outcome
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("connector credential lacks the required scope")
            .to_string();
        return scope_forbidden(auth, Some(required_scope), description);
    }
    if outcome.http_status == 404
        || outcome.body.pointer("/error/code").and_then(Value::as_str) == Some("task_not_found")
    {
        return McpOutcome::BadRequest(rpc_error(id, -32602, "Invalid params: task not found"));
    }
    if outcome.protocol_error {
        let message = outcome
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Invalid task params")
            .to_string();
        return McpOutcome::BadRequest(rpc_error(id, -32602, message));
    }
    McpOutcome::BadRequest(rpc_error(
        id,
        -32603,
        "Internal error while resolving durable Connector task",
    ))
}

fn require_task_auth(auth: Option<&AuthContext>) -> Result<&AuthContext, McpOutcome> {
    let Some(auth) = auth else {
        return Err(scope_forbidden(
            None,
            Some(SCOPE_JOB_RUN),
            "connector credential is required for MCP task access",
        ));
    };
    if let Some(outcome) = require_mcp_scope(Some(auth), SCOPE_JOB_RUN) {
        return Err(outcome);
    }
    Ok(auth)
}

pub(super) async fn handle_request(
    method: &str,
    params: Value,
    id: Option<Value>,
    auth: Option<&AuthContext>,
    connector: &ConnectorRuntime,
) -> McpOutcome {
    if !request_supports_tasks(&params) {
        return missing_tasks_capability(id);
    }

    match method {
        "tasks/get" => {
            let params: McpTaskParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        format!("Invalid params: {error}"),
                    ));
                }
            };
            if !mcp_task_id_is_valid(&params.task_id) {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    "Invalid params: task not found",
                ));
            }
            let auth = match require_task_auth(auth) {
                Ok(auth) => auth,
                Err(outcome) => return outcome,
            };
            match connector
                .execution_task_result_for_auth(&params.task_id, auth)
                .await
            {
                Ok((_task, execution, outcome)) => {
                    let call_tool_result = execution
                        .is_terminal()
                        .then(|| connector_call_tool_result(outcome));
                    let result = mcp_detailed_task_result(&execution, call_tool_result);
                    McpOutcome::Ok(rpc_result(id, mcp_stateless_result(result, false)))
                }
                Err(outcome) => mcp_task_connector_error(id, Some(auth), outcome),
            }
        }
        "tasks/update" => {
            let params: McpTaskUpdateParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        format!("Invalid params: {error}"),
                    ));
                }
            };
            if !mcp_task_id_is_valid(&params.task_id) || !params.input_responses.is_object() {
                return McpOutcome::BadRequest(rpc_error(id, -32602, "Invalid params"));
            }
            let auth = match require_task_auth(auth) {
                Ok(auth) => auth,
                Err(outcome) => return outcome,
            };
            if let Err(outcome) = connector
                .execution_task_result_for_auth(&params.task_id, auth)
                .await
            {
                return mcp_task_connector_error(id, Some(auth), outcome);
            }
            // A3 never creates input_required Tasks. Per the Tasks extension,
            // responses for unknown/already-satisfied input requests are ignored.
            McpOutcome::Ok(rpc_result(id, mcp_stateless_result(json!({}), false)))
        }
        "tasks/cancel" => {
            let params: McpTaskParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return McpOutcome::BadRequest(rpc_error(
                        id,
                        -32602,
                        format!("Invalid params: {error}"),
                    ));
                }
            };
            if !mcp_task_id_is_valid(&params.task_id) {
                return McpOutcome::BadRequest(rpc_error(
                    id,
                    -32602,
                    "Invalid params: task not found",
                ));
            }
            let auth = match require_task_auth(auth) {
                Ok(auth) => auth,
                Err(outcome) => return outcome,
            };
            if let Err(outcome) = connector
                .cancel_execution_task_for_auth(&params.task_id, auth)
                .await
            {
                return mcp_task_connector_error(id, Some(auth), outcome);
            }
            McpOutcome::Ok(rpc_result(id, mcp_stateless_result(json!({}), false)))
        }
        _ => unreachable!("validated MCP Tasks method: {method}"),
    }
}

/// Promote a successful task-polling Connector tools/call outcome into an MCP Task.
/// Returns `None` only when the Connector result is an ordinary non-blocking result
/// with no durable execution identity, so the caller can render its normal tools/call response.
pub(super) async fn promote_connector_tool_call(
    id: &Option<Value>,
    outcome: &ConnectorCallOutcome,
    auth: Option<&AuthContext>,
    connector: &ConnectorRuntime,
) -> Option<McpOutcome> {
    let Some(execution_id) = outcome
        .body
        .pointer("/data/execution/execution_id")
        .and_then(Value::as_str)
    else {
        return (outcome.body["blocking"].as_bool() == Some(true)).then(|| {
            McpOutcome::BadRequest(rpc_error(
                id.clone(),
                -32603,
                "active Connector execution did not expose durable execution identity",
            ))
        });
    };

    let Some(auth) = auth else {
        return Some(scope_forbidden(
            None,
            Some(SCOPE_JOB_RUN),
            "connector credential is required for MCP task access",
        ));
    };

    Some(
        match connector.materialize_execution_task_for_auth(execution_id, auth) {
            Ok(execution) if execution.mcp_task_is_materialized() => {
                if execution.is_active() && !execution.terminal_continuation_is_armed() {
                    return Some(McpOutcome::BadRequest(rpc_error(
                        id.clone(),
                        -32603,
                        "active Connector execution is not durably armed for terminal polling",
                    )));
                }
                if execution.is_terminal() && !execution.mcp_task_result_is_finalized() {
                    return Some(McpOutcome::BadRequest(rpc_error(
                        id.clone(),
                        -32603,
                        "materialized MCP task became terminal before its durable result was finalized",
                    )));
                }
                McpOutcome::Ok(rpc_result(
                    id.clone(),
                    mcp_stateless_result(mcp_create_task_result(&execution), false),
                ))
            }
            Ok(execution) if execution.is_terminal() => {
                let ordinary = match connector
                    .ordinary_execution_result_for_auth(execution_id, auth)
                    .await
                {
                    Ok(ordinary) => ordinary,
                    Err(task_outcome) => {
                        return Some(mcp_task_connector_error(
                            id.clone(),
                            Some(auth),
                            task_outcome,
                        ));
                    }
                };
                let result = connector_call_tool_result(ordinary);
                McpOutcome::Ok(rpc_result(id.clone(), mcp_stateless_result(result, false)))
            }
            Ok(_) => McpOutcome::BadRequest(rpc_error(
                id.clone(),
                -32603,
                "active Connector execution was not durably materialized for MCP task polling",
            )),
            Err(task_outcome) => mcp_task_connector_error(id.clone(), Some(auth), task_outcome),
        },
    )
}
