//! Built-in MCP gateway for Runner-owned local stdio providers.
//!
//! Public model-facing behavior is layered here; bounded Server↔Runner wire
//! types stay in `webcodex-core`. Exact Runner/provider identities never leave
//! this module's internal dispatch path.

pub(crate) use webcodex_core::mcp_gateway::*;

use crate::auth::{AuthContext, SCOPE_MCP_LOCAL};
use crate::tool_runtime::ToolRuntime;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const MCP_TOOL_NAME: &str = "mcp_tool";
const MAX_SCHEMA_OBSERVATIONS: usize = 512;
const GATEWAY_WAIT_TIMEOUT: Duration = Duration::from_secs(125);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ObservationKey {
    client_id: String,
    provider_id: String,
    provider_instance_id: String,
    tool_name: String,
}

#[derive(Default)]
struct ObservationStore {
    values: HashMap<ObservationKey, McpGatewaySchemaObservation>,
    order: VecDeque<ObservationKey>,
}

#[derive(Default)]
pub(crate) struct McpGatewayRuntime {
    observations: Mutex<ObservationStore>,
}

impl McpGatewayRuntime {
    fn remember(&self, key: ObservationKey, value: McpGatewaySchemaObservation) {
        let mut store = self
            .observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !store.values.contains_key(&key) {
            store.order.push_back(key.clone());
        }
        store.values.insert(key, value);
        while store.values.len() > MAX_SCHEMA_OBSERVATIONS {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            store.values.remove(&oldest);
        }
    }

    fn observed(&self, key: &ObservationKey) -> Option<McpGatewaySchemaObservation> {
        self.observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values
            .get(key)
            .cloned()
    }

    fn forget(&self, key: &ObservationKey) {
        let mut store = self
            .observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.values.remove(key);
        store.order.retain(|candidate| candidate != key);
    }
}

#[derive(Debug, Clone)]
struct ResolvedProvider {
    client_id: String,
    runner_instance_id: String,
    provider_id: String,
    provider_instance_id: String,
    name: String,
}

#[derive(Debug)]
struct GatewayError {
    code: String,
    message: String,
    recovery: Option<&'static str>,
    dispatch_state: Option<McpGatewayDispatchState>,
}

impl GatewayError {
    fn local(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            recovery: None,
            dispatch_state: Some(McpGatewayDispatchState::NotStarted),
        }
    }

    fn recovery(mut self, recovery: &'static str) -> Self {
        self.recovery = Some(recovery);
        self
    }
}

#[derive(Debug)]
enum GatewaySuccess {
    Metadata(Value),
    UpstreamToolResult(McpGatewayToolResult),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpToolArguments {
    action: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

pub(crate) fn authorized(auth: Option<&AuthContext>) -> bool {
    auth.is_some_and(|auth| auth.has_scope(SCOPE_MCP_LOCAL))
}

pub(crate) fn tool_spec() -> Value {
    json!({
        "name": MCP_TOOL_NAME,
        "description": "Access explicitly authorized Runner-owned local MCP servers through WebCodex's built-in gateway. No-argument action=list reports registration routing resolvability, not provider process health; action=list with server and action=describe interact with the provider. Use action=describe before action=call, and re-describe when WebCodex reports a schema change. Provider process identities and schema revision tokens are intentionally hidden.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "describe", "call"]
                },
                "server": {
                    "type": "string",
                    "description": "Logical MCP server/provider id returned by action=list."
                },
                "tool": {
                    "type": "string",
                    "description": "Logical upstream tool name."
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments for action=call, matching the most recent action=describe schema."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": false
        }
    })
}

pub(crate) async fn call(
    runtime: &ToolRuntime,
    arguments: Value,
    auth: Option<&AuthContext>,
) -> Value {
    if !authorized(auth) {
        return gateway_error_result(GatewayError::local(
            "insufficient_scope",
            "local MCP access requires the mcp:local scope",
        ));
    }
    let parsed: McpToolArguments = match serde_json::from_value(arguments) {
        Ok(parsed) => parsed,
        Err(_) => {
            return gateway_error_result(GatewayError::local(
                "invalid_arguments",
                "mcp_tool arguments are invalid",
            ))
        }
    };

    let result = match parsed.action.as_str() {
        "list" => list(runtime, parsed, auth)
            .await
            .map(GatewaySuccess::Metadata),
        "describe" => describe(runtime, parsed, auth)
            .await
            .map(GatewaySuccess::Metadata),
        "call" => call_upstream(runtime, parsed, auth)
            .await
            .map(GatewaySuccess::UpstreamToolResult),
        _ => Err(GatewayError::local(
            "invalid_action",
            "action must be one of list, describe, or call",
        )),
    };
    render_gateway_result(result)
}

async fn list(
    runtime: &ToolRuntime,
    args: McpToolArguments,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.tool.is_some() || args.arguments.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=list does not accept tool or arguments",
        ));
    }
    let candidates = visible_provider_candidates(runtime, auth).await;
    if let Some(server) = args.server.as_deref() {
        validate_provider_id(server).map_err(|_| {
            GatewayError::local("invalid_server", "server is not a valid logical MCP id")
        })?;
        let provider = resolve_provider(&candidates, server)?;
        let response = execute_exact(
            runtime,
            &provider,
            McpGatewayRequest::ToolsList {
                provider_id: provider.provider_id.clone(),
                provider_instance_id: provider.provider_instance_id.clone(),
            },
            auth,
        )
        .await?;
        let tools = response_tools(response)?;
        let compact = tools
            .into_iter()
            .map(|tool| {
                let mut value = json!({"name": tool.name});
                if let Some(title) = tool.title {
                    value["title"] = Value::String(title);
                }
                if let Some(description) = tool.description {
                    value["description"] = Value::String(description);
                }
                value
            })
            .collect::<Vec<_>>();
        return Ok(json!({
            "server": provider.provider_id,
            "name": provider.name,
            "tools": compact
        }));
    }

    Ok(registration_routing_summary(&candidates))
}

fn registration_routing_summary(candidates: &BTreeMap<String, Vec<ResolvedProvider>>) -> Value {
    let mut servers = Vec::with_capacity(candidates.len());
    for (provider_id, entries) in candidates {
        let first = &entries[0];
        servers.push(json!({
            "server": provider_id,
            "name": first.name,
            "resolvable": entries.len() == 1,
            "status": if entries.len() == 1 { "resolvable" } else { "ambiguous" }
        }));
    }
    json!({"servers": servers})
}

async fn describe(
    runtime: &ToolRuntime,
    args: McpToolArguments,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.arguments.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=describe does not accept arguments",
        ));
    }
    let server = required_id(args.server.as_deref(), "server")?;
    let tool_name = required_tool(args.tool.as_deref())?;
    let candidates = visible_provider_candidates(runtime, auth).await;
    let provider = resolve_provider(&candidates, server)?;
    let response = execute_exact(
        runtime,
        &provider,
        McpGatewayRequest::ToolsList {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
        },
        auth,
    )
    .await?;
    let tools = response_tools(response)?;
    let Some(tool) = tools
        .into_iter()
        .find(|candidate| candidate.name == tool_name)
    else {
        return Err(GatewayError::local(
            "tool_not_found",
            "the requested tool is not present on the current MCP server",
        ));
    };
    runtime.mcp_gateway.remember(
        ObservationKey {
            client_id: provider.client_id.clone(),
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            tool_name: tool.name.clone(),
        },
        tool.schema_observation(),
    );
    Ok(json!({
        "server": provider.provider_id,
        "serverName": provider.name,
        "tool": tool
    }))
}

async fn call_upstream(
    runtime: &ToolRuntime,
    args: McpToolArguments,
    auth: Option<&AuthContext>,
) -> Result<McpGatewayToolResult, GatewayError> {
    let server = required_id(args.server.as_deref(), "server")?;
    let tool_name = required_tool(args.tool.as_deref())?;
    let arguments = args.arguments.ok_or_else(|| {
        GatewayError::local("invalid_arguments", "action=call requires arguments")
    })?;
    if !arguments.is_object() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=call arguments must be a JSON object",
        ));
    }
    validate_json_value(&arguments, MCP_GATEWAY_MAX_ARGUMENT_BYTES, "tool arguments").map_err(
        |_| GatewayError::local("invalid_arguments", "tool arguments exceed gateway bounds"),
    )?;
    let candidates = visible_provider_candidates(runtime, auth).await;
    let provider = resolve_provider(&candidates, server)?;
    let observation_key = ObservationKey {
        client_id: provider.client_id.clone(),
        provider_id: provider.provider_id.clone(),
        provider_instance_id: provider.provider_instance_id.clone(),
        tool_name: tool_name.to_string(),
    };
    let expected_schema = runtime
        .mcp_gateway
        .observed(&observation_key)
        .ok_or_else(|| {
            GatewayError::local(
                "describe_required",
                "describe this tool before calling it so WebCodex can bind the current schema",
            )
            .recovery("Call mcp_tool with action=describe for this server and tool, then retry with the described schema.")
        })?;

    let response = execute_exact(
        runtime,
        &provider,
        McpGatewayRequest::ToolsCall {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
            name: tool_name.to_string(),
            arguments,
            expected_schema,
        },
        auth,
    )
    .await?;

    if let Some(error) = response.error.as_ref() {
        if matches!(
            error.code.as_str(),
            "provider_schema_changed" | "provider_tool_missing"
        ) {
            runtime.mcp_gateway.forget(&observation_key);
        }
    }
    let dispatch_state = response.dispatch_state;
    if let Some(error) = response.error {
        let mut gateway_error = GatewayError {
            code: error.code.clone(),
            message: error.message,
            recovery: None,
            dispatch_state: Some(dispatch_state),
        };
        if matches!(
            error.code.as_str(),
            "provider_schema_changed" | "provider_tool_missing"
        ) {
            gateway_error.recovery = Some(
                "Call mcp_tool with action=describe for this server and tool before calling again.",
            );
        }
        if error.code == "stale_provider" {
            gateway_error.code = "provider_replaced".to_string();
            gateway_error.recovery = Some(
                "The exact provider instance changed. Re-list or re-describe; WebCodex did not retarget or replay the call.",
            );
        }
        return Err(gateway_error);
    }
    match response.payload {
        Some(McpGatewayResponsePayload::ToolResult { result }) => Ok(result),
        _ => Err(GatewayError::local(
            "invalid_provider_result",
            "provider returned an unexpected response shape",
        )),
    }
}

async fn visible_provider_candidates(
    runtime: &ToolRuntime,
    auth: Option<&AuthContext>,
) -> BTreeMap<String, Vec<ResolvedProvider>> {
    let mut providers: BTreeMap<String, Vec<ResolvedProvider>> = BTreeMap::new();
    let access = crate::runner_http::runner_access_from_auth(auth);
    for client in runtime
        .runner_registry
        .list_runners_for_auth(access.as_ref())
        .await
    {
        if !client.connected
            || runtime
                .runner_registry
                .assert_runner_access(access.as_ref(), &client.client_id)
                .await
                .is_err()
        {
            // `list_runners_for_auth` applies lightweight auth-group visibility.
            // Managed-user clients also require the existing exact owner check;
            // do it here before projecting even sanitized provider metadata.
            continue;
        }
        let Some(inventory) = client
            .policy
            .as_ref()
            .and_then(|policy| policy.mcp_gateway_providers.as_ref())
        else {
            continue;
        };
        for provider in inventory {
            providers
                .entry(provider.provider_id.clone())
                .or_default()
                .push(ResolvedProvider {
                    client_id: client.client_id.clone(),
                    runner_instance_id: client.runner_instance_id.clone(),
                    provider_id: provider.provider_id.clone(),
                    provider_instance_id: provider.provider_instance_id.clone(),
                    name: provider.name.clone(),
                });
        }
    }
    providers
}

fn resolve_provider(
    candidates: &BTreeMap<String, Vec<ResolvedProvider>>,
    provider_id: &str,
) -> Result<ResolvedProvider, GatewayError> {
    let Some(matches) = candidates.get(provider_id) else {
        return Err(GatewayError::local(
            "server_unavailable",
            "the requested MCP server is not currently available to this credential",
        ));
    };
    if matches.len() != 1 {
        return Err(GatewayError::local(
            "server_ambiguous",
            "multiple visible Runners advertise the same logical MCP server id; choose unique provider ids",
        ));
    }
    Ok(matches[0].clone())
}

async fn execute_exact(
    runtime: &ToolRuntime,
    provider: &ResolvedProvider,
    request: McpGatewayRequest,
    auth: Option<&AuthContext>,
) -> Result<McpGatewayResponse, GatewayError> {
    let access = crate::runner_http::runner_access_from_auth(auth);
    let (request_id, receiver) = runtime
        .runner_registry
        .enqueue_mcp_gateway(
            &provider.client_id,
            &provider.runner_instance_id,
            request,
            access.as_ref(),
            "mcp_tool".to_string(),
        )
        .await
        .map_err(|message| {
            let (code, public_message) = if message.contains("stale")
                || message.contains("exact Runner")
            {
                (
                    "provider_replaced",
                    "the exact MCP provider instance changed or became unavailable before dispatch",
                )
            } else {
                (
                    "gateway_unavailable",
                    "the MCP provider request could not be queued",
                )
            };
            GatewayError::local(code, public_message).recovery(
                "Re-list or re-describe the MCP server. WebCodex did not retarget or replay this operation.",
            )
        })?;

    match tokio::time::timeout(GATEWAY_WAIT_TIMEOUT, receiver).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) | Err(_) => {
            let dispatched = runtime
                .runner_registry
                .cancel_request_dispatch_state(&request_id)
                .await;
            let state = if dispatched == Some(false) {
                McpGatewayDispatchState::NotStarted
            } else {
                McpGatewayDispatchState::OutcomeUnknown
            };
            Err(GatewayError {
                code: "gateway_timeout".to_string(),
                message: if state == McpGatewayDispatchState::NotStarted {
                    "gateway request timed out before Runner dispatch".to_string()
                } else {
                    "gateway request timed out after dispatch may have begun; do not retry automatically"
                        .to_string()
                },
                recovery: None,
                dispatch_state: Some(state),
            })
        }
    }
}

fn response_tools(response: McpGatewayResponse) -> Result<Vec<McpGatewayTool>, GatewayError> {
    if let Some(error) = response.error {
        let code = if error.code == "stale_provider" {
            "provider_replaced".to_string()
        } else {
            error.code
        };
        return Err(GatewayError {
            code,
            message: error.message,
            recovery: Some(
                "Re-list the MCP server; WebCodex did not retarget or replay the stale operation.",
            ),
            dispatch_state: Some(response.dispatch_state),
        });
    }
    match response.payload {
        Some(McpGatewayResponsePayload::Tools { tools }) => Ok(tools),
        _ => Err(GatewayError::local(
            "invalid_provider_tools",
            "provider returned an unexpected tools/list response",
        )),
    }
}

fn required_id<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str, GatewayError> {
    let value = value.ok_or_else(|| {
        GatewayError::local("invalid_arguments", format!("action requires {field}"))
    })?;
    validate_provider_id(value).map_err(|_| {
        GatewayError::local(
            "invalid_arguments",
            format!("{field} is not a valid logical MCP id"),
        )
    })?;
    Ok(value)
}

fn required_tool(value: Option<&str>) -> Result<&str, GatewayError> {
    let value =
        value.ok_or_else(|| GatewayError::local("invalid_arguments", "action requires tool"))?;
    validate_tool_name(value)
        .map_err(|_| GatewayError::local("invalid_arguments", "tool name is invalid"))?;
    Ok(value)
}

fn render_gateway_result(result: Result<GatewaySuccess, GatewayError>) -> Value {
    match result {
        Ok(GatewaySuccess::Metadata(value)) => gateway_success_result(value),
        Ok(GatewaySuccess::UpstreamToolResult(result)) => serde_json::to_value(result)
            .unwrap_or_else(|_| {
                gateway_error_result(GatewayError::local(
                    "invalid_provider_result",
                    "provider result could not be encoded",
                ))
            }),
        Err(error) => gateway_error_result(error),
    }
}

fn gateway_success_result(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": "Local MCP metadata available in structuredContent."}],
        "structuredContent": value,
        "isError": false
    })
}

const GATEWAY_ERROR_FALLBACK_BYTES: usize = 1024;

fn bounded_gateway_error_fallback(message: &str) -> String {
    if message.len() <= GATEWAY_ERROR_FALLBACK_BYTES {
        return message.to_string();
    }
    let mut end = GATEWAY_ERROR_FALLBACK_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

fn gateway_error_result(error: GatewayError) -> Value {
    let text = bounded_gateway_error_fallback(&error.message);
    let dispatch_state = error.dispatch_state.map(dispatch_state_name);
    let mut structured = json!({
        "error": {
            "code": error.code,
            "message": error.message
        }
    });
    if let Some(state) = dispatch_state {
        structured["dispatchState"] = Value::String(state.to_string());
    }
    if let Some(recovery) = error.recovery {
        structured["recovery"] = Value::String(recovery.to_string());
    }
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": true
    })
}

fn dispatch_state_name(state: McpGatewayDispatchState) -> &'static str {
    match state {
        McpGatewayDispatchState::NotStarted => "not_started",
        McpGatewayDispatchState::OutcomeUnknown => "outcome_unknown",
        McpGatewayDispatchState::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcodex_generated_gateway_results_keep_canonical_data_only_in_structured_content() {
        let metadata = json!({
            "server": "repo-tools",
            "tools": [{"name": "search_symbol"}]
        });
        let rendered = render_gateway_result(Ok(GatewaySuccess::Metadata(metadata.clone())));
        assert_eq!(rendered["structuredContent"], metadata);
        assert_eq!(rendered["isError"], false);
        assert_eq!(
            rendered["content"][0]["text"],
            "Local MCP metadata available in structuredContent."
        );
        assert!(!rendered["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("search_symbol"));

        let message = "é".repeat(GATEWAY_ERROR_FALLBACK_BYTES);
        let rendered = gateway_error_result(GatewayError {
            code: "provider_schema_changed".to_string(),
            message: message.clone(),
            recovery: Some("Describe the provider tool again before retrying."),
            dispatch_state: Some(McpGatewayDispatchState::OutcomeUnknown),
        });
        assert_eq!(rendered["isError"], true);
        assert_eq!(
            rendered["structuredContent"]["error"]["code"],
            "provider_schema_changed"
        );
        assert_eq!(rendered["structuredContent"]["error"]["message"], message);
        assert_eq!(
            rendered["structuredContent"]["dispatchState"],
            "outcome_unknown"
        );
        assert_eq!(
            rendered["structuredContent"]["recovery"],
            "Describe the provider tool again before retrying."
        );
        let fallback = rendered["content"][0]["text"].as_str().unwrap();
        assert!(fallback.len() <= GATEWAY_ERROR_FALLBACK_BYTES);
        assert!(!fallback.contains("provider_schema_changed"));
        assert!(!fallback.contains("Describe the provider tool"));
    }

    #[test]
    fn upstream_tool_results_are_passed_through_without_gateway_projection() {
        let cases = [
            McpGatewayToolResult {
                content: vec![McpGatewayContent::Text {
                    text: "text-only".to_string(),
                }],
                structured_content: None,
                is_error: false,
            },
            McpGatewayToolResult {
                content: vec![],
                structured_content: Some(json!({"kind": "structured-only"})),
                is_error: false,
            },
            McpGatewayToolResult {
                content: vec![McpGatewayContent::Text {
                    text: "dual-text".to_string(),
                }],
                structured_content: Some(json!({"kind": "dual"})),
                is_error: false,
            },
            McpGatewayToolResult {
                content: vec![McpGatewayContent::Text {
                    text: "provider-error".to_string(),
                }],
                structured_content: Some(json!({"code": "PROVIDER_ERROR"})),
                is_error: true,
            },
        ];
        for result in cases {
            let expected = serde_json::to_value(&result).unwrap();
            let rendered = render_gateway_result(Ok(GatewaySuccess::UpstreamToolResult(result)));
            assert_eq!(rendered, expected);
        }
    }

    #[test]
    fn fixed_tool_catalog_contract_has_no_runtime_identity_or_revision() {
        let spec = tool_spec();
        let encoded = serde_json::to_string(&spec).unwrap();
        assert!(encoded.contains("mcp_tool"));
        assert!(
            spec.get("outputSchema").is_none(),
            "mcp_tool preserves provider-defined structuredContent, so a fixed outputSchema would be misleading"
        );
        assert!(!encoded.contains("provider_instance_id"));
        assert!(!encoded.contains("agent_instance_id"));
        let properties = spec["inputSchema"]["properties"].as_object().unwrap();
        assert!(!properties.contains_key("revision"));
        assert!(!properties.contains_key("revisionToken"));
        assert!(!properties.contains_key("provider_instance_id"));
        assert!(!properties.contains_key("agent_instance_id"));
    }

    #[test]
    fn no_arg_list_reports_routing_resolvability_not_health() {
        let provider = |client_id: &str, instance_id: &str| ResolvedProvider {
            client_id: client_id.to_string(),
            runner_instance_id: format!("{client_id}-agent"),
            provider_id: "server".to_string(),
            provider_instance_id: instance_id.to_string(),
            name: "Server".to_string(),
        };
        let candidates = BTreeMap::from([
            (
                "ambiguous".to_string(),
                vec![
                    provider("runner-a", "instance-a"),
                    provider("runner-b", "instance-b"),
                ],
            ),
            (
                "unique".to_string(),
                vec![ResolvedProvider {
                    provider_id: "unique".to_string(),
                    ..provider("runner-c", "instance-c")
                }],
            ),
        ]);
        let summary = registration_routing_summary(&candidates);
        let servers = summary["servers"].as_array().unwrap();
        let unique = servers
            .iter()
            .find(|server| server["server"] == "unique")
            .unwrap();
        assert_eq!(unique["resolvable"], true);
        assert_eq!(unique["status"], "resolvable");
        let ambiguous = servers
            .iter()
            .find(|server| server["server"] == "ambiguous")
            .unwrap();
        assert_eq!(ambiguous["resolvable"], false);
        assert_eq!(ambiguous["status"], "ambiguous");
        assert!(!serde_json::to_string(&summary)
            .unwrap()
            .contains("available"));
    }

    #[test]
    fn local_mcp_authority_is_explicit() {
        let mut legacy = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
        legacy.scopes = crate::auth::DIRECT_SHARED_KEY_MODEL_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect();
        assert!(!authorized(Some(&legacy)));

        legacy.scopes.push(crate::auth::SCOPE_MCP_LOCAL.to_string());
        assert!(authorized(Some(&legacy)));

        let mut bootstrap = crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap);
        bootstrap.is_bootstrap = true;
        assert!(authorized(Some(&bootstrap)));
    }

    #[test]
    fn upstream_tool_result_keeps_host_level_error_semantics() {
        let provider_result = McpGatewayToolResult {
            content: vec![McpGatewayContent::Text {
                text: "upstream rejected the request".to_string(),
            }],
            structured_content: Some(json!({"code": "UPSTREAM_REJECTED"})),
            is_error: true,
        };

        let result = render_gateway_result(Ok(GatewaySuccess::UpstreamToolResult(provider_result)));
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "upstream rejected the request"
        );
        assert_eq!(result["structuredContent"]["code"], "UPSTREAM_REJECTED");
        assert!(result["structuredContent"].get("isError").is_none());
    }

    #[test]
    fn observations_are_bound_to_exact_provider_instance() {
        let runtime = McpGatewayRuntime::default();
        let observed = McpGatewaySchemaObservation {
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
        };
        let original = ObservationKey {
            client_id: "runner".to_string(),
            provider_id: "provider".to_string(),
            provider_instance_id: "instance-a".to_string(),
            tool_name: "echo".to_string(),
        };
        runtime.remember(original.clone(), observed.clone());
        assert_eq!(runtime.observed(&original), Some(observed));

        let replacement = ObservationKey {
            provider_instance_id: "instance-b".to_string(),
            ..original
        };
        assert!(runtime.observed(&replacement).is_none());
    }

    #[test]
    fn observations_are_bounded_and_internal() {
        let runtime = McpGatewayRuntime::default();
        for index in 0..=MAX_SCHEMA_OBSERVATIONS {
            runtime.remember(
                ObservationKey {
                    client_id: "runner".to_string(),
                    provider_id: "provider".to_string(),
                    provider_instance_id: "provider-instance".to_string(),
                    tool_name: format!("tool-{index}"),
                },
                McpGatewaySchemaObservation {
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                    annotations: None,
                },
            );
        }
        let store = runtime.observations.lock().unwrap();
        assert_eq!(store.values.len(), MAX_SCHEMA_OBSERVATIONS);
    }
}
