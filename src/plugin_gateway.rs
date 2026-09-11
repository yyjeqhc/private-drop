//! Model-facing adapter for Runner-owned native Tool Plugins.
//!
//! Plugin processes and their executable configuration never cross the Runner
//! boundary.  This module resolves only caller-visible logical identities,
//! stores bounded describe observations, and dispatches the closed typed Plugin
//! gateway with opaque exact Runner/provider/schema bindings.

pub(crate) use webcodex_core::plugin::*;

use crate::auth::{AuthContext, SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE, SCOPE_PLUGIN_MANAGE};
use crate::tool_runtime::sessions::SessionTransport;
use crate::tool_runtime::specialized::{
    SpecializedGovernanceDenial, SpecializedOperationPolicy, SpecializedSource,
};
use crate::tool_runtime::{PluginToolCall, ToolResult, ToolRuntime};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const PLUGIN_TOOL_NAME: &str = "plugin_tool";
const MAX_PLUGIN_BINDINGS: usize = 512;
const GATEWAY_WAIT_TIMEOUT: Duration = Duration::from_secs(125);
pub(crate) const MAX_PLUGIN_CATALOG_CONTEXT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
struct PluginBinding {
    client_id: String,
    runner_instance_id: String,
    provider_id: String,
    provider_instance_id: String,
    tool_name: String,
    schema: PluginSchemaObservation,
}

#[derive(Default)]
struct BindingStore {
    values: HashMap<String, PluginBinding>,
    order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct PluginGatewayRuntime {
    bindings: Mutex<BindingStore>,
}

impl PluginGatewayRuntime {
    fn remember(&self, value: PluginBinding) -> String {
        let mut store = self
            .bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = loop {
            let candidate = format!("wc_pbind_{}", uuid::Uuid::new_v4().simple());
            if !store.values.contains_key(&candidate) {
                break candidate;
            }
        };
        store.order.push_back(binding.clone());
        store.values.insert(binding.clone(), value);
        while store.values.len() > MAX_PLUGIN_BINDINGS {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            store.values.remove(&oldest);
        }
        binding
    }

    fn binding(&self, binding: &str) -> Option<PluginBinding> {
        self.bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values
            .get(binding)
            .cloned()
    }

    fn forget(&self, binding: &str) {
        let mut store = self
            .bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.values.remove(binding);
        store.order.retain(|candidate| candidate != binding);
    }

    #[cfg(test)]
    pub(crate) fn binding_count(&self) -> usize {
        self.bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values
            .len()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPluginRunner {
    pub(crate) client_id: String,
    pub(crate) runner_instance_id: String,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct GatewayError {
    code: String,
    message: String,
    recovery: Option<&'static str>,
    dispatch_state: Option<PluginDispatchState>,
}

impl GatewayError {
    fn local(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            recovery: None,
            dispatch_state: Some(PluginDispatchState::NotStarted),
        }
    }

    fn recovery(mut self, recovery: &'static str) -> Self {
        self.recovery = Some(recovery);
        self
    }
}

#[derive(Debug, Clone)]
enum GatewaySuccess {
    Metadata(Value),
    ToolResult(PluginToolResult),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginOperation {
    List,
    Check,
    Reload,
    Describe,
    Call,
}

impl PluginOperation {
    fn parse(action: &str) -> Option<Self> {
        match action {
            "list" => Some(Self::List),
            "check" => Some(Self::Check),
            "reload" => Some(Self::Reload),
            "describe" => Some(Self::Describe),
            "call" => Some(Self::Call),
            _ => None,
        }
    }

    pub(crate) fn policy(self) -> SpecializedOperationPolicy {
        match self {
            Self::List => SpecializedOperationPolicy::read(
                SpecializedSource::Plugin,
                "list",
                SCOPE_PLUGIN_INSPECT,
            ),
            Self::Describe => SpecializedOperationPolicy::read(
                SpecializedSource::Plugin,
                "describe",
                SCOPE_PLUGIN_INSPECT,
            ),
            Self::Call => SpecializedOperationPolicy::local_execution(
                SpecializedSource::Plugin,
                "call",
                SCOPE_PLUGIN_INVOKE,
            ),
            Self::Check => SpecializedOperationPolicy::management(
                SpecializedSource::Plugin,
                "check",
                SCOPE_PLUGIN_MANAGE,
                false,
                true,
            ),
            Self::Reload => SpecializedOperationPolicy::management(
                SpecializedSource::Plugin,
                "reload",
                SCOPE_PLUGIN_MANAGE,
                true,
                true,
            ),
        }
    }
}

pub(crate) fn invoke_authorized(auth: Option<&AuthContext>) -> bool {
    auth.is_some_and(|auth| auth.has_scope(SCOPE_PLUGIN_INVOKE))
}

/// Bounded model/ledger projection. Arbitrary Plugin arguments and opaque
/// bindings are deliberately represented only by presence bits.
pub(crate) fn audit_arguments(arguments: &Value) -> Value {
    let object = arguments.as_object();
    let action = object
        .and_then(|o| o.get("action"))
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 16
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        });
    let runner = object
        .and_then(|o| o.get("runner"))
        .and_then(Value::as_str)
        .filter(|value| bounded_runner_id(value));
    let plugin = object
        .and_then(|o| o.get("plugin"))
        .and_then(Value::as_str)
        .filter(|value| validate_provider_id(value).is_ok());
    let tool = object
        .and_then(|o| o.get("tool"))
        .and_then(Value::as_str)
        .filter(|value| validate_tool_name(value).is_ok());
    json!({
        "action": action,
        "runner": runner,
        "plugin": plugin,
        "tool": tool,
        "binding_present": object.is_some_and(|o| o.get("binding").is_some()),
        "arguments_present": object.is_some_and(|o| o.get("arguments").is_some()),
    })
}

fn bounded_runner_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Add authoritative bounded provider identity when an opaque call binding can
/// be resolved without executing the Plugin. The binding value, provider
/// instance identity, schema observation, and arbitrary arguments stay out of
/// the projection.
async fn audit_request_with_identity(
    runtime: &ToolRuntime,
    request: &PluginToolCall,
    auth: Option<&AuthContext>,
) -> Value {
    let arguments = serde_json::to_value(request).unwrap_or_else(|_| json!({}));
    let audit = audit_arguments(&arguments);
    if !invoke_authorized(auth) {
        return audit;
    }
    let Some(binding) = arguments
        .as_object()
        .filter(|object| object.get("action").and_then(Value::as_str) == Some("call"))
        .and_then(|object| object.get("binding"))
        .and_then(Value::as_str)
        .and_then(|binding| runtime.plugin_gateway.binding(binding))
    else {
        return audit;
    };
    let Ok(runner) = resolve_runner(runtime, &binding.client_id, auth).await else {
        return audit;
    };
    if runner.runner_instance_id != binding.runner_instance_id {
        return audit;
    }
    audit_arguments_with_resolved_binding(audit, &binding)
}

fn audit_arguments_with_resolved_binding(mut audit: Value, binding: &PluginBinding) -> Value {
    audit["runner"] = Value::String(binding.client_id.clone());
    audit["plugin"] = Value::String(binding.provider_id.clone());
    audit["tool"] = Value::String(binding.tool_name.clone());
    audit["binding_resolved"] = Value::Bool(true);
    audit
}

#[derive(Debug, Clone)]
pub(crate) struct PluginInvocationResult {
    operation: PluginOperation,
    result: Result<GatewaySuccess, GatewayError>,
}

impl PluginInvocationResult {
    pub(crate) fn policy(&self) -> SpecializedOperationPolicy {
        self.operation.policy()
    }

    pub(crate) fn success(&self) -> bool {
        match &self.result {
            Ok(GatewaySuccess::Metadata(_)) => true,
            Ok(GatewaySuccess::ToolResult(result)) => !result.is_error,
            Err(_) => false,
        }
    }

    pub(crate) fn dispatch_certainty(&self) -> &'static str {
        match &self.result {
            Err(error) => dispatch_state_name(
                error
                    .dispatch_state
                    .unwrap_or(PluginDispatchState::NotStarted),
            ),
            Ok(_) => "completed",
        }
    }

    pub(crate) fn failure_kind(&self) -> Option<&str> {
        match &self.result {
            Ok(GatewaySuccess::ToolResult(result)) if result.is_error => Some("plugin_tool_error"),
            Err(error) => Some(error.code.as_str()),
            _ => None,
        }
    }

    pub(crate) fn to_mcp_result(&self) -> Value {
        render_gateway_result(self.result.clone())
    }

    pub(crate) fn to_tool_result(&self) -> ToolResult {
        match &self.result {
            Ok(GatewaySuccess::Metadata(value)) => ToolResult::ok(value.clone()),
            Ok(GatewaySuccess::ToolResult(result)) => {
                let mut output = serde_json::to_value(result).unwrap_or_else(|_| json!({}));
                if let Some(object) = output.as_object_mut() {
                    object.insert(
                        "dispatch_certainty".to_string(),
                        Value::String("completed".to_string()),
                    );
                    if result.is_error {
                        object.insert(
                            "failure_kind".to_string(),
                            Value::String("plugin_tool_error".to_string()),
                        );
                    }
                }
                if result.is_error {
                    ToolResult::err_with_output("Plugin tool reported an error", output)
                } else {
                    ToolResult::ok(output)
                }
            }
            Err(error) => gateway_error_tool_result(error),
        }
    }
}

/// Canonical action-aware Plugin invocation shared by MCP and the generic Tool
/// Runtime. This is the only owner of Plugin scope/session/permission lifecycle.
pub(crate) async fn invoke(
    runtime: &ToolRuntime,
    request: PluginToolCall,
    recording_session_id: Option<&str>,
    auth: Option<&AuthContext>,
    transport: SessionTransport,
) -> Result<PluginInvocationResult, SpecializedGovernanceDenial> {
    let operation = PluginOperation::parse(&request.action)
        .expect("PluginToolCall parser admits only the closed action vocabulary");
    let policy = operation.policy();
    let audit = audit_request_with_identity(runtime, &request, auth).await;
    let permit = runtime
        .govern_specialized_invocation(
            PLUGIN_TOOL_NAME,
            policy,
            transport,
            recording_session_id,
            auth,
            &audit,
        )
        .await?;

    let result = execute_business(runtime, operation, request, auth).await;
    let invocation = PluginInvocationResult { operation, result };
    runtime.finish_specialized_invocation(
        permit,
        invocation.success(),
        invocation.dispatch_certainty(),
        invocation.failure_kind(),
    );
    Ok(invocation)
}

async fn execute_business(
    runtime: &ToolRuntime,
    operation: PluginOperation,
    request: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<GatewaySuccess, GatewayError> {
    let result = match operation {
        PluginOperation::List => list(runtime, request, auth)
            .await
            .map(GatewaySuccess::Metadata),
        PluginOperation::Check => check(runtime, request, auth)
            .await
            .map(GatewaySuccess::Metadata),
        PluginOperation::Reload => reload(runtime, request, auth)
            .await
            .map(GatewaySuccess::Metadata),
        PluginOperation::Describe => describe(runtime, request, auth)
            .await
            .map(GatewaySuccess::Metadata),
        PluginOperation::Call => call_plugin(runtime, request, auth)
            .await
            .map(GatewaySuccess::ToolResult),
    };
    result
}

async fn list(
    runtime: &ToolRuntime,
    args: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.tool.is_some() || args.binding.is_some() || args.arguments.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=list accepts only optional runner and plugin",
        ));
    }
    if args.plugin.is_some() && args.runner.is_none() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=list requires runner when plugin is provided",
        ));
    }
    if let Some(runner) = args.runner.as_deref() {
        let runner = resolve_runner(runtime, runner, auth).await?;
        if let Some(plugin) = args.plugin.as_deref() {
            let plugin = required_provider(Some(plugin))?;
            let (provider, tools) =
                observe_effective_provider_tools(runtime, &runner, plugin, auth).await?;
            let mut value = sanitize_provider_view(provider);
            value["runner"] = Value::String(runner.client_id.clone());
            value["toolCount"] = Value::from(tools.len());
            value["tools"] = Value::Array(
                tools
                    .into_iter()
                    .map(sanitize_tool_summary)
                    .collect::<Vec<_>>(),
            );
            return Ok(value);
        }
        let response =
            execute_exact(runtime, &runner, PluginGatewayRequest::ProvidersList, auth).await?;
        let providers = response_providers(response)?;
        return Ok(json!({
            "runner": runner.client_id,
            "plugins": providers
                .into_iter()
                .map(sanitize_provider_view)
                .collect::<Vec<_>>()
        }));
    }

    let runners = visible_plugin_runners(runtime, auth)
        .await
        .into_iter()
        .map(|runner| {
            let mut value = json!({"runner": runner.client_id});
            if let Some(name) = runner.display_name {
                value["name"] = Value::String(name);
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(json!({"runners": runners}))
}

async fn check(
    runtime: &ToolRuntime,
    args: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.tool.is_some() || args.binding.is_some() || args.arguments.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=check accepts only runner and plugin",
        ));
    }
    let runner_id = required_runner(args.runner.as_deref())?;
    let plugin_id = required_provider(args.plugin.as_deref())?;
    let runner = resolve_runner(runtime, runner_id, auth).await?;
    let response = execute_exact(
        runtime,
        &runner,
        PluginGatewayRequest::Check {
            provider_id: plugin_id.to_string(),
        },
        auth,
    )
    .await?;
    if let Some(error) = response.error {
        return Err(response_error(response.dispatch_state, error));
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::Checked { report }) => {
            Ok(sanitize_check_report(&runner.client_id, report))
        }
        _ => Err(GatewayError::local(
            "invalid_plugin_response",
            "Runner returned an unexpected Plugin check response",
        )),
    }
}

async fn reload(
    runtime: &ToolRuntime,
    args: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.plugin.is_some()
        || args.tool.is_some()
        || args.binding.is_some()
        || args.arguments.is_some()
    {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=reload accepts only runner",
        ));
    }
    let runner_id = required_runner(args.runner.as_deref())?;
    let runner = resolve_runner(runtime, runner_id, auth).await?;
    let response = execute_exact(runtime, &runner, PluginGatewayRequest::Reload, auth).await?;
    if let Some(error) = response.error {
        return Err(response_error(response.dispatch_state, error));
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::Reloaded {
            providers,
            failures,
        }) => Ok(json!({
            "runner": runner.client_id,
            "plugins": providers
                .into_iter()
                .map(sanitize_provider_view)
                .collect::<Vec<_>>(),
            "failures": failures
        })),
        _ => Err(GatewayError::local(
            "invalid_plugin_response",
            "Runner returned an unexpected Plugin reload response",
        )),
    }
}

async fn describe(
    runtime: &ToolRuntime,
    args: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.binding.is_some() || args.arguments.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=describe accepts only runner, plugin, and tool",
        ));
    }
    let runner_id = required_runner(args.runner.as_deref())?;
    let plugin_id = required_provider(args.plugin.as_deref())?;
    let tool_name = required_tool(args.tool.as_deref())?;
    let runner = resolve_runner(runtime, runner_id, auth).await?;
    let (provider, tools) =
        observe_effective_provider_tools(runtime, &runner, plugin_id, auth).await?;
    let tool = tools
        .into_iter()
        .find(|candidate| candidate.name == tool_name)
        .ok_or_else(|| {
            GatewayError::local(
                "tool_not_found",
                "the requested tool is not present on the current effective Plugin provider",
            )
        })?;
    let binding = runtime.plugin_gateway.remember(PluginBinding {
        client_id: runner.client_id.clone(),
        runner_instance_id: runner.runner_instance_id.clone(),
        provider_id: provider.provider_id.clone(),
        provider_instance_id: provider.provider_instance_id.clone(),
        tool_name: tool.name.clone(),
        schema: tool.schema_observation(),
    });
    Ok(json!({
        "runner": runner.client_id,
        "plugin": provider.provider_id,
        "pluginName": provider.name,
        "tool": tool,
        "binding": binding
    }))
}

async fn observe_effective_provider_tools(
    runtime: &ToolRuntime,
    runner: &ResolvedPluginRunner,
    plugin_id: &str,
    auth: Option<&AuthContext>,
) -> Result<(PluginProviderView, Vec<PluginTool>), GatewayError> {
    let providers_response =
        execute_exact(runtime, runner, PluginGatewayRequest::ProvidersList, auth).await?;
    let providers = match response_providers(providers_response) {
        Ok(value) => value,
        Err(mut error) if matches!(error.code.as_str(), "stale_runner" | "runner_unavailable") => {
            error.code = "plugin_replaced".to_string();
            error.recovery =
                Some("Re-list the Plugin. WebCodex did not retarget or replay the operation.");
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let provider = providers
        .into_iter()
        .find(|provider| provider.provider_id == plugin_id)
        .ok_or_else(|| {
            GatewayError::local(
                "plugin_unavailable",
                "the requested Plugin is not currently available on the exact Runner",
            )
        })?;

    let response = execute_exact(
        runtime,
        runner,
        PluginGatewayRequest::ToolsList {
            provider_id: provider.provider_id.clone(),
            provider_instance_id: provider.provider_instance_id.clone(),
        },
        auth,
    )
    .await?;
    let tools = match response_tools(response) {
        Ok(tools) => tools,
        Err(mut error)
            if matches!(
                error.code.as_str(),
                "stale_runner"
                    | "runner_unavailable"
                    | "stale_plugin_provider"
                    | "plugin_provider_unavailable"
            ) =>
        {
            error.code = "plugin_replaced".to_string();
            error.recovery =
                Some("Re-list the Plugin. WebCodex did not retarget or replay the operation.");
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    Ok((provider, tools))
}

async fn call_plugin(
    runtime: &ToolRuntime,
    args: PluginToolCall,
    auth: Option<&AuthContext>,
) -> Result<PluginToolResult, GatewayError> {
    if args.runner.is_some() || args.plugin.is_some() || args.tool.is_some() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=call accepts only binding and arguments",
        ));
    }
    let binding_id = required_binding(args.binding.as_deref())?.to_string();
    let arguments = args.arguments.ok_or_else(|| {
        GatewayError::local("invalid_arguments", "action=call requires arguments")
    })?;
    if !arguments.is_object() {
        return Err(GatewayError::local(
            "invalid_arguments",
            "action=call arguments must be a JSON object",
        ));
    }
    validate_json_value(&arguments, PLUGIN_MAX_ARGUMENT_BYTES, "tool arguments").map_err(|_| {
        GatewayError::local("invalid_arguments", "tool arguments exceed Plugin bounds")
    })?;
    let observed = runtime
        .plugin_gateway
        .binding(&binding_id)
        .ok_or_else(describe_required_error)?;

    // A binding identifies the observation; it does not grant authority. Resolve
    // the logical Runner against the current credential again, then require the
    // exact Runner instance that produced the describe observation.
    let runner = resolve_runner(runtime, &observed.client_id, auth)
        .await
        .map_err(|_| describe_required_error())?;
    if runner.runner_instance_id != observed.runner_instance_id {
        runtime.plugin_gateway.forget(&binding_id);
        return Err(GatewayError::local(
            "plugin_replaced",
            "the exact Runner instance described by this binding is no longer current",
        )
        .recovery("Re-describe this Plugin tool. WebCodex did not retarget or replay the call."));
    }

    // Deliberately do not re-list/re-resolve the provider here. The exact
    // provider instance, tool name, and schema from this binding are the only
    // legal dispatch target.
    let response = match execute_exact(
        runtime,
        &runner,
        PluginGatewayRequest::ToolsCall {
            provider_id: observed.provider_id.clone(),
            provider_instance_id: observed.provider_instance_id.clone(),
            name: observed.tool_name.clone(),
            arguments,
            expected_schema: observed.schema,
        },
        auth,
    )
    .await
    {
        Ok(response) => response,
        Err(mut error) => {
            if error.code == "plugin_replaced" {
                runtime.plugin_gateway.forget(&binding_id);
                error.recovery = Some(
                    "Re-describe this Plugin tool. WebCodex did not retarget or replay the call.",
                );
            }
            return Err(error);
        }
    };
    let state = response.dispatch_state;
    if let Some(error) = response.error {
        if matches!(
            error.code.as_str(),
            "plugin_schema_changed"
                | "plugin_tool_unavailable"
                | "stale_plugin_provider"
                | "plugin_provider_unavailable"
        ) {
            runtime.plugin_gateway.forget(&binding_id);
        }
        let mut error = response_error(state, error);
        if matches!(
            error.code.as_str(),
            "plugin_schema_changed" | "plugin_tool_unavailable"
        ) {
            error.recovery = Some(
                "Re-describe this Plugin tool before calling again; WebCodex did not retarget or replay the call.",
            );
        }
        if error.code == "stale_plugin_provider" || error.code == "plugin_provider_unavailable" {
            error.code = "plugin_replaced".to_string();
            error.recovery = Some(
                "The exact Plugin provider instance changed or retired. Re-describe it; WebCodex did not retarget or replay the call.",
            );
        }
        return Err(error);
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::ToolResult { result }) => Ok(result),
        _ => Err(GatewayError::local(
            "invalid_plugin_result",
            "Runner returned an unexpected Plugin tool result",
        )),
    }
}

pub(crate) async fn visible_plugin_runners(
    runtime: &ToolRuntime,
    auth: Option<&AuthContext>,
) -> Vec<ResolvedPluginRunner> {
    let access = crate::runner_http::runner_access_from_auth(auth);
    let mut runners = Vec::new();
    for runner in runtime
        .runner_registry
        .list_runners_for_auth(access.as_ref())
        .await
    {
        if !runner.connected
            || !runner.capabilities.native_tool_plugins
            || runtime
                .runner_registry
                .assert_runner_access(access.as_ref(), &runner.client_id)
                .await
                .is_err()
        {
            continue;
        }
        runners.push(ResolvedPluginRunner {
            client_id: runner.client_id,
            runner_instance_id: runner.runner_instance_id,
            display_name: runner.display_name,
        });
    }
    runners
}

pub(crate) async fn resolve_runner(
    runtime: &ToolRuntime,
    runner_id: &str,
    auth: Option<&AuthContext>,
) -> Result<ResolvedPluginRunner, GatewayError> {
    if runner_id.trim().is_empty()
        || runner_id.len() > 128
        || runner_id.chars().any(char::is_control)
    {
        return Err(GatewayError::local(
            "invalid_runner",
            "runner is not a valid exact Runner client id",
        ));
    }
    visible_plugin_runners(runtime, auth)
        .await
        .into_iter()
        .find(|runner| runner.client_id == runner_id)
        .ok_or_else(|| {
            GatewayError::local(
                "runner_unavailable",
                "the exact Runner is not currently available to this credential",
            )
        })
}

pub(crate) async fn execute_exact(
    runtime: &ToolRuntime,
    runner: &ResolvedPluginRunner,
    request: PluginGatewayRequest,
    auth: Option<&AuthContext>,
) -> Result<PluginGatewayResponse, GatewayError> {
    let access = crate::runner_http::runner_access_from_auth(auth);
    let (request_id, receiver) = runtime
        .runner_registry
        .enqueue_plugin_gateway(
            &runner.client_id,
            &runner.runner_instance_id,
            request,
            access.as_ref(),
            PLUGIN_TOOL_NAME.to_string(),
        )
        .await
        .map_err(|message| {
            let (code, public_message) = if message.contains("stale")
                || message.contains("exact Runner")
                || message.contains("offline")
            {
                (
                    "plugin_replaced",
                    "the exact Runner or Plugin provider changed or became unavailable before dispatch",
                )
            } else {
                (
                    "plugin_gateway_unavailable",
                    "the Plugin gateway request could not be queued",
                )
            };
            GatewayError::local(code, public_message).recovery(
                "Re-list or re-describe the Plugin. WebCodex did not retarget or replay this operation.",
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
                PluginDispatchState::NotStarted
            } else {
                PluginDispatchState::OutcomeUnknown
            };
            Err(GatewayError {
                code: "plugin_gateway_timeout".to_string(),
                message: if state == PluginDispatchState::NotStarted {
                    "Plugin gateway request timed out before Runner dispatch".to_string()
                } else {
                    "Plugin gateway request timed out after dispatch may have begun; do not retry automatically"
                        .to_string()
                },
                recovery: None,
                dispatch_state: Some(state),
            })
        }
    }
}

fn response_providers(
    response: PluginGatewayResponse,
) -> Result<Vec<PluginProviderView>, GatewayError> {
    if let Some(error) = response.error {
        return Err(response_error(response.dispatch_state, error));
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::Providers { providers }) => Ok(providers),
        _ => Err(GatewayError::local(
            "invalid_plugin_response",
            "Runner returned an unexpected Plugin provider response",
        )),
    }
}

fn response_tools(response: PluginGatewayResponse) -> Result<Vec<PluginTool>, GatewayError> {
    if let Some(error) = response.error {
        return Err(response_error(response.dispatch_state, error));
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::Tools { tools }) => Ok(tools),
        _ => Err(GatewayError::local(
            "invalid_plugin_response",
            "Runner returned an unexpected Plugin tools/list response",
        )),
    }
}

fn response_project_catalog(
    response: PluginGatewayResponse,
) -> Result<ProjectPluginCatalog, GatewayError> {
    if let Some(error) = response.error {
        return Err(response_error(response.dispatch_state, error));
    }
    match response.payload {
        Some(PluginGatewayResponsePayload::ProjectCatalog { catalog }) => Ok(catalog),
        _ => Err(GatewayError::local(
            "invalid_plugin_response",
            "Runner returned an unexpected project Plugin catalog response",
        )),
    }
}

pub(crate) fn project_plugin_catalog_projection(
    catalog: &ProjectPluginCatalog,
    max_bytes: usize,
) -> Value {
    fn value(catalog: &ProjectPluginCatalog, entries: &[ProjectPluginCatalogEntry]) -> Value {
        let truncated = entries.len() < catalog.total_count;
        json!({
            "catalog_revision": catalog.catalog_revision,
            "total_count": catalog.total_count,
            "returned_count": entries.len(),
            "truncated": truncated,
            "entries": entries,
            "discovery_hint": truncated.then_some(
                "Use explicit plugin_tool list and describe for broader or current schema discovery."
            ),
        })
    }

    let mut entries = Vec::new();
    for entry in &catalog.entries {
        let mut candidate = entries.clone();
        candidate.push(entry.clone());
        if serde_json::to_vec(&value(catalog, &candidate))
            .map(|bytes| bytes.len() <= max_bytes)
            .unwrap_or(false)
        {
            entries.push(entry.clone());
        } else {
            break;
        }
    }
    value(catalog, &entries)
}

fn project_catalog_reason(error: &GatewayError) -> &'static str {
    match error.code.as_str() {
        "project_target_unavailable" => "project_target_unavailable",
        _ => "plugin_runtime_unavailable",
    }
}

impl ToolRuntime {
    pub(crate) async fn plugin_project_catalog_context_projection(
        &self,
        project: &crate::tool_runtime::ResolvedProject,
        auth: Option<&AuthContext>,
    ) -> Result<Value, &'static str> {
        let project_id = crate::tool_runtime::runner_local_project_id(&project.resolved_id)
            .ok_or("project_target_unavailable")?;
        let runner = resolve_runner(self, &project.config.client_id, auth)
            .await
            .map_err(|error| project_catalog_reason(&error))?;
        let response = execute_exact(
            self,
            &runner,
            PluginGatewayRequest::ProjectCatalog {
                project_id: project_id.to_string(),
            },
            auth,
        )
        .await
        .map_err(|error| project_catalog_reason(&error))?;
        let catalog =
            response_project_catalog(response).map_err(|error| project_catalog_reason(&error))?;
        Ok(project_plugin_catalog_projection(
            &catalog,
            MAX_PLUGIN_CATALOG_CONTEXT_BYTES,
        ))
    }
}

fn response_error(state: PluginDispatchState, error: PluginGatewayError) -> GatewayError {
    GatewayError {
        code: error.code,
        message: error.message,
        recovery: None,
        dispatch_state: Some(state),
    }
}

fn sanitize_check_report(runner: &str, report: PluginCheckReport) -> Value {
    let mut value = json!({
        "runner": runner,
        "plugin": report.provider_id,
        "ready": report.ready,
        "phase": check_phase_name(report.phase),
        "toolCount": report.tool_count,
        "tools": report
            .tools
            .into_iter()
            .map(|tool| {
                let mut summary = json!({"name": tool.name});
                if let Some(title) = tool.title {
                    summary["title"] = Value::String(title);
                }
                summary
            })
            .collect::<Vec<_>>()
    });
    if let Some(code) = report.code {
        value["code"] = Value::String(code);
    }
    if let Some(detail) = report.detail {
        value["detail"] = Value::String(detail);
    }
    if let Some(diagnostic) = report.diagnostic {
        let mut summary = json!({"code": diagnostic.code});
        if let Some(tool) = diagnostic.tool {
            summary["tool"] = Value::String(tool);
        }
        if let Some(field) = diagnostic.field {
            summary["field"] = Value::String(field);
        }
        value["diagnostic"] = summary;
    }
    value
}

fn check_phase_name(phase: PluginCheckPhase) -> &'static str {
    match phase {
        PluginCheckPhase::Config => "config",
        PluginCheckPhase::Environment => "environment",
        PluginCheckPhase::Executable => "executable",
        PluginCheckPhase::Spawn => "spawn",
        PluginCheckPhase::Stdio => "stdio",
        PluginCheckPhase::Initialize => "initialize",
        PluginCheckPhase::ToolsList => "tools_list",
        PluginCheckPhase::Validation => "validation",
        PluginCheckPhase::Ready => "ready",
    }
}

fn sanitize_tool_summary(tool: PluginTool) -> Value {
    let mut summary = json!({"name": tool.name});
    if let Some(title) = tool.title {
        summary["title"] = Value::String(title);
    }
    summary
}

fn sanitize_provider_view(provider: PluginProviderView) -> Value {
    json!({
        "plugin": provider.provider_id,
        "name": provider.name,
        "status": provider.status,
        "errorCode": provider.error_code
    })
}

fn required_runner(value: Option<&str>) -> Result<&str, GatewayError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GatewayError::local("invalid_arguments", "action requires runner"))
}

fn required_provider(value: Option<&str>) -> Result<&str, GatewayError> {
    let value =
        value.ok_or_else(|| GatewayError::local("invalid_arguments", "action requires plugin"))?;
    validate_provider_id(value)
        .map_err(|_| GatewayError::local("invalid_arguments", "plugin id is invalid"))?;
    Ok(value)
}

fn required_tool(value: Option<&str>) -> Result<&str, GatewayError> {
    let value =
        value.ok_or_else(|| GatewayError::local("invalid_arguments", "action requires tool"))?;
    validate_tool_name(value)
        .map_err(|_| GatewayError::local("invalid_arguments", "tool name is invalid"))?;
    Ok(value)
}

fn required_binding(value: Option<&str>) -> Result<&str, GatewayError> {
    let value = value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| GatewayError::local("invalid_arguments", "action=call requires binding"))?;
    let Some(random) = value.strip_prefix("wc_pbind_") else {
        return Err(GatewayError::local(
            "invalid_arguments",
            "binding is not a valid opaque Plugin binding",
        ));
    };
    if random.len() != 32
        || !random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(GatewayError::local(
            "invalid_arguments",
            "binding is not a valid opaque Plugin binding",
        ));
    }
    Ok(value)
}

fn describe_required_error() -> GatewayError {
    GatewayError::local(
        "describe_required",
        "this Plugin binding is unavailable to the current credential or is no longer retained",
    )
    .recovery(
        "Call plugin_tool with action=describe for the intended runner, plugin, and tool, then call with the returned binding. WebCodex did not retarget or replay the call.",
    )
}

fn render_gateway_result(result: Result<GatewaySuccess, GatewayError>) -> Value {
    match result {
        Ok(GatewaySuccess::Metadata(value)) => gateway_success_result(value),
        Ok(GatewaySuccess::ToolResult(result)) => {
            serde_json::to_value(result).unwrap_or_else(|_| {
                gateway_error_result(GatewayError::local(
                    "invalid_plugin_result",
                    "Plugin result could not be encoded",
                ))
            })
        }
        Err(error) => gateway_error_result(error),
    }
}

fn gateway_error_tool_result(error: &GatewayError) -> ToolResult {
    let state = error
        .dispatch_state
        .unwrap_or(PluginDispatchState::NotStarted);
    let mut output = json!({
        "error": {
            "code": error.code,
            "message": error.message,
        },
        "failure_kind": error.code,
        "dispatch_certainty": dispatch_state_name(state),
    });
    if let Some(recovery) = error.recovery {
        output["recovery"] = Value::String(recovery.to_string());
    }
    ToolResult::err_with_output(error.message.clone(), output)
}

fn gateway_success_result(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": "Plugin metadata available in structuredContent."}],
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
        "error": {"code": error.code, "message": error.message}
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

fn dispatch_state_name(state: PluginDispatchState) -> &'static str {
    match state {
        PluginDispatchState::NotStarted => "not_started",
        PluginDispatchState::OutcomeUnknown => "outcome_unknown",
        PluginDispatchState::Completed => "completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_binding(provider: &str, tool: &str) -> PluginBinding {
        PluginBinding {
            client_id: "runner-a".to_string(),
            runner_instance_id: "runner-instance-a".to_string(),
            provider_id: provider.to_string(),
            provider_instance_id: format!("{provider}-instance"),
            tool_name: tool.to_string(),
            schema: PluginSchemaObservation {
                input_schema: json!({"type":"object"}),
                output_schema: None,
                annotations: None,
            },
        }
    }

    #[test]
    fn webcodex_generated_gateway_results_keep_canonical_data_only_in_structured_content() {
        let metadata = json!({
            "runner": "runner-a",
            "plugins": [{"plugin": "repo-tools", "status": "ready"}]
        });
        let rendered = render_gateway_result(Ok(GatewaySuccess::Metadata(metadata.clone())));
        assert_eq!(rendered["structuredContent"], metadata);
        assert_eq!(rendered["isError"], false);
        assert_eq!(
            rendered["content"][0]["text"],
            "Plugin metadata available in structuredContent."
        );
        assert!(!rendered["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("repo-tools"));

        let message = "é".repeat(GATEWAY_ERROR_FALLBACK_BYTES);
        let rendered = gateway_error_result(GatewayError {
            code: "plugin_replaced".to_string(),
            message: message.clone(),
            recovery: Some("Re-list the Plugin before retrying."),
            dispatch_state: Some(PluginDispatchState::OutcomeUnknown),
        });
        assert_eq!(rendered["isError"], true);
        assert_eq!(
            rendered["structuredContent"]["error"]["code"],
            "plugin_replaced"
        );
        assert_eq!(rendered["structuredContent"]["error"]["message"], message);
        assert_eq!(
            rendered["structuredContent"]["dispatchState"],
            "outcome_unknown"
        );
        assert_eq!(
            rendered["structuredContent"]["recovery"],
            "Re-list the Plugin before retrying."
        );
        let fallback = rendered["content"][0]["text"].as_str().unwrap();
        assert!(fallback.len() <= GATEWAY_ERROR_FALLBACK_BYTES);
        assert!(!fallback.contains("plugin_replaced"));
        assert!(!fallback.contains("Re-list the Plugin"));
    }

    #[test]
    fn provider_tool_results_are_passed_through_without_gateway_projection() {
        let cases = [
            PluginToolResult {
                content: vec![PluginContent::Text {
                    text: "text-only".to_string(),
                }],
                structured_content: None,
                is_error: false,
            },
            PluginToolResult {
                content: vec![],
                structured_content: Some(json!({"kind": "structured-only"})),
                is_error: false,
            },
            PluginToolResult {
                content: vec![PluginContent::Text {
                    text: "dual-text".to_string(),
                }],
                structured_content: Some(json!({"kind": "dual"})),
                is_error: false,
            },
            PluginToolResult {
                content: vec![PluginContent::Text {
                    text: "provider-error".to_string(),
                }],
                structured_content: Some(json!({"code": "PROVIDER_ERROR"})),
                is_error: true,
            },
        ];
        for result in cases {
            let expected = serde_json::to_value(&result).unwrap();
            let rendered = render_gateway_result(Ok(GatewaySuccess::ToolResult(result)));
            assert_eq!(rendered, expected);
        }
    }

    #[test]
    fn plugin_bindings_are_independent_and_evicted_fifo() {
        let runtime = PluginGatewayRuntime::default();
        let first = runtime.remember(test_binding("provider-a", "tool-a"));
        let second = runtime.remember(test_binding("provider-b", "tool-b"));
        assert_ne!(first, second);
        assert!(first.starts_with("wc_pbind_"));
        assert!(second.starts_with("wc_pbind_"));
        assert_eq!(runtime.binding(&first).unwrap().tool_name, "tool-a");
        assert_eq!(runtime.binding(&second).unwrap().tool_name, "tool-b");

        runtime.forget(&first);
        assert!(runtime.binding(&first).is_none());
        assert_eq!(
            runtime.binding(&second).unwrap().provider_id,
            "provider-b",
            "forgetting one exact binding must not disturb another provider/tool binding"
        );

        let oldest = runtime.remember(test_binding("oldest", "tool"));
        for index in 0..MAX_PLUGIN_BINDINGS {
            runtime.remember(test_binding(&format!("provider-{index}"), "tool"));
        }
        assert!(
            runtime.binding(&oldest).is_none(),
            "binding store must remain bounded with deterministic oldest eviction"
        );
    }

    #[test]
    fn canonical_plugin_tool_catalog_hides_runtime_identity_and_provider_defined_output_schema() {
        let spec = webcodex_tool_contracts::registered_tool_specs()
            .into_iter()
            .find(|spec| spec.name == PLUGIN_TOOL_NAME)
            .expect("plugin_tool must be a canonical registered ToolSpec");
        let spec = serde_json::to_value(spec).unwrap();
        let encoded = serde_json::to_string(&spec).unwrap();
        assert_eq!(spec["name"], PLUGIN_TOOL_NAME);
        assert!(spec["outputSchema"].is_object());
        assert!(spec["inputSchema"]["properties"]["binding"].is_object());
        assert!(spec["inputSchema"]["properties"]["plugin"].is_object());
        assert!(spec["inputSchema"]["properties"]["runner"].is_object());
        assert!(spec["inputSchema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action == "check"));
        assert_eq!(
            spec["inputSchema"]["properties"]["binding"]["pattern"],
            "^wc_pbind_[0-9a-f]{32}$"
        );
        assert!(spec["description"]
            .as_str()
            .unwrap()
            .contains("call accepts only binding + arguments"));
        assert!(!encoded.contains("provider_instance_id"));
        assert!(!encoded.contains("runner_instance_id"));
        assert!(!encoded.contains("revision"));
        assert!(!encoded.contains("command"));
        assert!(!encoded.contains("cwd"));
        assert!(!encoded.contains("\"env\""));
    }

    #[test]
    fn plugin_audit_projection_never_contains_binding_or_arbitrary_arguments() {
        let binding = "wc_pbind_private_binding_marker";
        let secret = "PLUGIN_PRIVATE_ARGUMENT_MARKER";
        let audit = audit_arguments(&json!({
            "action": "call",
            "binding": binding,
            "arguments": {
                "token": secret,
                "nested": {"credential": "also-private"}
            }
        }));
        let encoded = serde_json::to_string(&audit).unwrap();
        assert!(!encoded.contains(binding));
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("also-private"));
        assert_eq!(audit["binding_present"], true);
        assert_eq!(audit["arguments_present"], true);
    }

    #[test]
    fn plugin_audit_projection_drops_unbounded_or_unrecognized_identity_fields() {
        let secret = "PRIVATE\nPLUGIN\nFIELD";
        let audit = audit_arguments(&json!({
            "action": secret,
            "runner": secret,
            "plugin": secret,
            "tool": secret,
            "arguments": {"token": "never-project-this"}
        }));
        let encoded = serde_json::to_string(&audit).unwrap();
        assert!(!encoded.contains("PRIVATE"));
        assert!(!encoded.contains("never-project-this"));
        assert!(audit["action"].is_null());
        assert!(audit["runner"].is_null());
        assert!(audit["plugin"].is_null());
        assert!(audit["tool"].is_null());
        assert_eq!(audit["arguments_present"], true);
    }

    #[test]
    fn plugin_audit_projection_resolves_bounded_binding_identity_without_opaque_values() {
        let binding = test_binding("provider-a", "tool-a");
        let opaque = "wc_pbind_private_binding_marker";
        let secret = "PLUGIN_PRIVATE_ARGUMENT_MARKER";
        let audit = audit_arguments_with_resolved_binding(
            audit_arguments(&json!({
                "action": "call",
                "binding": opaque,
                "arguments": {"token": secret}
            })),
            &binding,
        );
        let encoded = serde_json::to_string(&audit).unwrap();
        assert_eq!(audit["runner"], "runner-a");
        assert_eq!(audit["plugin"], "provider-a");
        assert_eq!(audit["tool"], "tool-a");
        assert_eq!(audit["binding_resolved"], true);
        assert!(!encoded.contains(&opaque));
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("provider-a-instance"));
    }

    #[test]
    fn plugin_operation_policy_distinguishes_inspect_execution_and_management() {
        use crate::tool_runtime::specialized::SpecializedEffect;

        assert_eq!(
            PluginOperation::List.policy().effect,
            SpecializedEffect::Read
        );
        assert_eq!(
            PluginOperation::Describe.policy().effect,
            SpecializedEffect::Read
        );
        assert_eq!(
            PluginOperation::Call.policy().effect,
            SpecializedEffect::LocalExecution
        );
        assert_eq!(
            PluginOperation::Check.policy().effect,
            SpecializedEffect::Management
        );
        assert_eq!(
            PluginOperation::Reload.policy().effect,
            SpecializedEffect::Management
        );
        assert_eq!(
            PluginOperation::Check.policy().required_scope,
            SCOPE_PLUGIN_MANAGE
        );
        assert!(!PluginOperation::Check.policy().write_like);
        assert!(PluginOperation::Check.policy().shell_like);
        assert!(PluginOperation::Reload.policy().write_like);
        assert!(PluginOperation::Reload.policy().shell_like);
        assert_eq!(
            PluginOperation::Call.policy().required_scope,
            SCOPE_PLUGIN_INVOKE
        );
    }
}
