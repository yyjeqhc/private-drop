//! Model-facing adapter for Runner-local managed SSH resources.
//!
//! Raw SSH destinations are accepted only by `register` and travel only in the
//! transient exact-Runner request. Bindings and responses contain no target or
//! authentication material.

use crate::auth::{AuthContext, AuthKind, SCOPE_SSH_LOCAL};
use crate::tool_runtime::sessions::SessionTransport;
use crate::tool_runtime::specialized::{
    SpecializedGovernanceDenial, SpecializedOperationPolicy, SpecializedSource,
};
use crate::tool_runtime::{SshResourceToolCall, ToolResult, ToolRuntime};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use webcodex_core::ssh_resource::{
    validate_response_for_request, SshResourceRequest, SshResourceResponse,
};

pub(crate) const SSH_RESOURCE_TOOL_NAME: &str = "ssh_resource";
const MAX_BINDINGS: usize = 256;
const RUNNER_WAIT_TIMEOUT: Duration = Duration::from_secs(65);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerIdentity {
    kind: AuthKind,
    user_id: Option<String>,
    username: Option<String>,
    api_key_id: Option<String>,
    role: Option<String>,
    is_bootstrap: bool,
    token_kind: Option<String>,
    allowed_client_id: Option<String>,
    shared_key_hash: Option<String>,
    project_grant_id: Option<String>,
}

impl CallerIdentity {
    fn from_auth(auth: &AuthContext) -> Self {
        Self {
            kind: auth.kind,
            user_id: auth.user_id.clone(),
            username: auth.username.clone(),
            api_key_id: auth.api_key_id.clone(),
            role: auth.role.clone(),
            is_bootstrap: auth.is_bootstrap,
            token_kind: auth.token_kind.clone(),
            allowed_client_id: auth.allowed_client_id.clone(),
            shared_key_hash: auth.shared_key_hash.clone(),
            project_grant_id: auth.project_grant_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Binding {
    caller: CallerIdentity,
    client_id: String,
    runner_instance_id: String,
    revision: u64,
}

#[derive(Default)]
struct BindingStore {
    values: HashMap<String, Binding>,
    order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct SshResourceGatewayRuntime {
    bindings: Mutex<BindingStore>,
}

impl SshResourceGatewayRuntime {
    fn remember(&self, value: Binding) -> String {
        let mut store = self
            .bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let binding = loop {
            let candidate = format!("wc_sbind_{}", uuid::Uuid::new_v4().simple());
            if !store.values.contains_key(&candidate) {
                break candidate;
            }
        };
        store.order.push_back(binding.clone());
        store.values.insert(binding.clone(), value);
        while store.values.len() > MAX_BINDINGS {
            let Some(oldest) = store.order.pop_front() else {
                break;
            };
            store.values.remove(&oldest);
        }
        binding
    }

    fn binding(&self, binding: &str) -> Option<Binding> {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshResourceOperation {
    List,
    Register,
    Remove,
}

impl SshResourceOperation {
    fn parse(action: &str) -> Option<Self> {
        match action {
            "list" => Some(Self::List),
            "register" => Some(Self::Register),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }

    pub(crate) fn policy(self) -> SpecializedOperationPolicy {
        match self {
            Self::List => SpecializedOperationPolicy::read(
                SpecializedSource::SshResource,
                "list",
                SCOPE_SSH_LOCAL,
            ),
            Self::Register => SpecializedOperationPolicy::management(
                SpecializedSource::SshResource,
                "register",
                SCOPE_SSH_LOCAL,
                true,
                false,
            ),
            Self::Remove => SpecializedOperationPolicy::management(
                SpecializedSource::SshResource,
                "remove",
                SCOPE_SSH_LOCAL,
                true,
                false,
            ),
        }
    }
}

pub(crate) fn operation_policy(
    arguments: &Value,
) -> Result<SpecializedOperationPolicy, &'static str> {
    let action = arguments
        .get("action")
        .and_then(Value::as_str)
        .ok_or("ssh_resource action is required")?;
    SshResourceOperation::parse(action)
        .map(SshResourceOperation::policy)
        .ok_or("ssh_resource action must be one of list, register, or remove")
}

#[derive(Debug, Clone)]
struct ResolvedRunner {
    client_id: String,
    runner_instance_id: String,
}

#[derive(Debug, Clone)]
struct GatewayError {
    code: &'static str,
    message: &'static str,
    recovery: Option<&'static str>,
    dispatch_state: &'static str,
}

impl GatewayError {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            recovery: None,
            dispatch_state: "not_started",
        }
    }

    fn recovery(mut self, recovery: &'static str) -> Self {
        self.recovery = Some(recovery);
        self
    }

    fn completed(mut self) -> Self {
        self.dispatch_state = "completed";
        self
    }

    fn outcome_unknown(mut self) -> Self {
        self.dispatch_state = "outcome_unknown";
        self
    }
}

pub(crate) fn authorized(auth: Option<&AuthContext>) -> bool {
    auth.is_some_and(|auth| auth.has_scope(SCOPE_SSH_LOCAL))
}

pub(crate) fn mcp_output_schema() -> Value {
    let resource_name = || {
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": webcodex_core::ssh_resource::SSH_RESOURCE_NAME_MAX_BYTES
        })
    };
    let inventory_entry = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "name": resource_name(),
            "source": {"type": "string", "enum": ["static", "managed"]},
            "active": {"type": "boolean"},
            "pending_restart": {"type": "boolean"}
        },
        "required": ["name", "source", "active", "pending_restart"]
    });
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "runner": {"type": "string", "minLength": 1, "maxLength": 128},
                    "binding": {"type": "string", "pattern": "^wc_sbind_[0-9a-f]{32}$"},
                    "resources": {
                        "type": "array",
                        "maxItems": webcodex_core::ssh_resource::MANAGED_SSH_RESOURCE_MAX_COUNT,
                        "items": inventory_entry
                    }
                },
                "required": ["runner", "binding", "resources"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "resource": resource_name(),
                    "persisted": {"type": "boolean"},
                    "active": {"type": "boolean"},
                    "restart_required": {"type": "boolean"}
                },
                "required": ["resource", "persisted", "active", "restart_required"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "error": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "code": {"type": "string", "minLength": 1},
                            "message": {"type": "string", "minLength": 1, "maxLength": 512}
                        },
                        "required": ["code", "message"]
                    },
                    "dispatchState": {
                        "type": "string",
                        "enum": ["not_started", "completed", "outcome_unknown"]
                    },
                    "recovery": {"type": "string", "minLength": 1}
                },
                "required": ["error", "dispatchState"]
            }
        ]
    })
}

pub(crate) fn tool_spec(compact: bool) -> Value {
    let definition = webcodex_tool_contracts::lookup_tool_definition(SSH_RESOURCE_TOOL_NAME)
        .expect("ssh_resource ToolDefinition");
    let model_spec = definition.model_spec.expect("ssh_resource model spec");
    let mut input_schema = (model_spec.input_schema)();
    if let Some(properties) = input_schema["properties"].as_object_mut() {
        properties.insert(
            "recording_session_id".to_string(),
            json!({
                "type": "string",
                "pattern": "^wc_sess_[A-Za-z0-9_]+$",
                "description": "Optional explicit Workflow Session used for authority, read-only/guard, permission, and audit governance. It is never inferred from MCP transport identity."
            }),
        );
    }
    let mut value = json!({
        "name": SSH_RESOURCE_TOOL_NAME,
        "description": model_spec.description,
        "inputSchema": input_schema,
        "annotations": webcodex_tool_contracts::tool_annotations(SSH_RESOURCE_TOOL_NAME)
    });
    if !compact {
        value["outputSchema"] = mcp_output_schema();
    }
    value
}

/// Body-free audit projection for the MCP lifecycle. The raw target is never
/// recorded in ordinary tool telemetry or Session evidence.
pub(crate) fn audit_arguments(arguments: &Value) -> Value {
    let object = arguments.as_object();
    json!({
        "action": object.and_then(|o| o.get("action")).and_then(Value::as_str),
        "runner": object.and_then(|o| o.get("runner")).and_then(Value::as_str),
        "resource_name": object.and_then(|o| o.get("name")).and_then(Value::as_str),
        "binding_present": object.is_some_and(|o| o.get("binding").is_some()),
        "target_present": object.is_some_and(|o| o.get("target").is_some()),
        "default_cwd_present": object.is_some_and(|o| o.get("default_cwd").is_some()),
    })
}

#[derive(Debug, Clone)]
pub(crate) struct SshResourceInvocationResult {
    operation: SshResourceOperation,
    result: Result<Value, GatewayError>,
}

impl SshResourceInvocationResult {
    pub(crate) fn policy(&self) -> SpecializedOperationPolicy {
        self.operation.policy()
    }

    pub(crate) fn success(&self) -> bool {
        self.result.is_ok()
    }

    pub(crate) fn dispatch_certainty(&self) -> &'static str {
        self.result
            .as_ref()
            .err()
            .map(|error| error.dispatch_state)
            .unwrap_or("completed")
    }

    pub(crate) fn failure_kind(&self) -> Option<&str> {
        self.result.as_ref().err().map(|error| error.code)
    }

    pub(crate) fn to_mcp_result(&self) -> Value {
        match &self.result {
            Ok(value) => success_result(value.clone()),
            Err(error) => error_result(error.clone()),
        }
    }

    pub(crate) fn to_tool_result(&self) -> ToolResult {
        match &self.result {
            Ok(value) => ToolResult::ok(value.clone()),
            Err(error) => {
                let mut output = json!({
                    "error_kind": error.code,
                    "dispatch_state": error.dispatch_state,
                });
                if let Some(recovery) = error.recovery {
                    output["recovery"] = Value::String(recovery.to_string());
                }
                ToolResult::err_with_output(error.message, output)
            }
        }
    }
}

pub(crate) async fn invoke(
    runtime: &ToolRuntime,
    request: SshResourceToolCall,
    recording_session_id: Option<&str>,
    auth: Option<&AuthContext>,
    transport: SessionTransport,
) -> Result<SshResourceInvocationResult, SpecializedGovernanceDenial> {
    let Some(operation) = SshResourceOperation::parse(&request.action) else {
        return Err(SpecializedGovernanceDenial::Tool(
            ToolResult::err_with_output(
                "action must be one of list, register, or remove",
                json!({
                    "error_kind": "ssh_resource_invalid",
                    "dispatch_state": "not_started",
                }),
            ),
        ));
    };
    let policy = operation.policy();
    let audit = audit_arguments(&serde_json::to_value(&request).unwrap_or_else(|_| json!({})));
    let permit = runtime
        .govern_specialized_invocation(
            SSH_RESOURCE_TOOL_NAME,
            policy,
            transport,
            recording_session_id,
            auth,
            &audit,
        )
        .await?;
    let result = execute_business(runtime, operation, request, auth).await;
    let invocation = SshResourceInvocationResult { operation, result };
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
    operation: SshResourceOperation,
    request: SshResourceToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    match operation {
        SshResourceOperation::List => list(runtime, request, auth).await,
        SshResourceOperation::Register => register(runtime, request, auth).await,
        SshResourceOperation::Remove => remove(runtime, request, auth).await,
    }
}

pub(crate) async fn call(
    runtime: &ToolRuntime,
    arguments: Value,
    auth: Option<&AuthContext>,
) -> Value {
    let parsed: SshResourceToolCall = match serde_json::from_value(arguments) {
        Ok(parsed) => parsed,
        Err(_) => {
            return error_result(GatewayError::new(
                "ssh_resource_invalid",
                "ssh_resource arguments are invalid",
            ))
        }
    };
    let Some(operation) = SshResourceOperation::parse(&parsed.action) else {
        return error_result(GatewayError::new(
            "ssh_resource_invalid",
            "action must be one of list, register, or remove",
        ));
    };
    if !authorized(auth) {
        return error_result(GatewayError::new(
            "insufficient_scope",
            "Runner-local SSH resource access requires the ssh:local scope",
        ));
    }
    match execute_business(runtime, operation, parsed, auth).await {
        Ok(value) => success_result(value),
        Err(error) => error_result(error),
    }
}

async fn list(
    runtime: &ToolRuntime,
    args: SshResourceToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.binding.is_some()
        || args.name.is_some()
        || args.target.is_some()
        || args.default_cwd.is_some()
    {
        return Err(GatewayError::new(
            "ssh_resource_invalid",
            "action=list accepts only runner",
        ));
    }
    let runner_id = args.runner.as_deref().ok_or_else(|| {
        GatewayError::new(
            "ssh_resource_invalid",
            "action=list requires an exact runner",
        )
    })?;
    let runner = resolve_runner(runtime, runner_id, auth).await?;
    let response = execute_exact(runtime, &runner, SshResourceRequest::List, auth).await?;
    let SshResourceResponse::List {
        revision,
        resources,
    } = response
    else {
        return response_to_error(response);
    };
    let caller = CallerIdentity::from_auth(auth.expect("authorized checked above"));
    let binding = runtime.ssh_resource_gateway.remember(Binding {
        caller,
        client_id: runner.client_id.clone(),
        runner_instance_id: runner.runner_instance_id,
        revision,
    });
    Ok(json!({
        "runner": runner.client_id,
        "binding": binding,
        "resources": resources
    }))
}

async fn register(
    runtime: &ToolRuntime,
    args: SshResourceToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.runner.is_some() {
        return Err(GatewayError::new(
            "ssh_resource_invalid",
            "action=register accepts binding, name, target, and optional default_cwd",
        ));
    }
    let name = required(&args.name, "name")?.to_string();
    let target = required(&args.target, "target")?.to_string();
    let (binding_id, observed, runner) = resolve_binding(runtime, args.binding, auth).await?;
    let response = match execute_exact(
        runtime,
        &runner,
        SshResourceRequest::Register {
            expected_revision: observed.revision,
            name,
            target,
            default_cwd: args.default_cwd,
        },
        auth,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            if matches!(
                error.code,
                "runner_replaced" | "ssh_resource_outcome_unknown"
            ) {
                runtime.ssh_resource_gateway.forget(&binding_id);
            }
            return Err(error);
        }
    };
    render_mutation_response(runtime, &binding_id, response)
}

async fn remove(
    runtime: &ToolRuntime,
    args: SshResourceToolCall,
    auth: Option<&AuthContext>,
) -> Result<Value, GatewayError> {
    if args.runner.is_some() || args.target.is_some() || args.default_cwd.is_some() {
        return Err(GatewayError::new(
            "ssh_resource_invalid",
            "action=remove accepts only binding and name",
        ));
    }
    let name = required(&args.name, "name")?.to_string();
    let (binding_id, observed, runner) = resolve_binding(runtime, args.binding, auth).await?;
    let response = match execute_exact(
        runtime,
        &runner,
        SshResourceRequest::Remove {
            expected_revision: observed.revision,
            name,
        },
        auth,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            if matches!(
                error.code,
                "runner_replaced" | "ssh_resource_outcome_unknown"
            ) {
                runtime.ssh_resource_gateway.forget(&binding_id);
            }
            return Err(error);
        }
    };
    render_mutation_response(runtime, &binding_id, response)
}

fn render_mutation_response(
    runtime: &ToolRuntime,
    binding_id: &str,
    response: SshResourceResponse,
) -> Result<Value, GatewayError> {
    match response {
        SshResourceResponse::Register {
            resource,
            persisted,
            active,
            restart_required,
            ..
        }
        | SshResourceResponse::Remove {
            resource,
            persisted,
            active,
            restart_required,
            ..
        } => {
            runtime.ssh_resource_gateway.forget(binding_id);
            Ok(json!({
                "resource": resource,
                "persisted": persisted,
                "active": active,
                "restart_required": restart_required
            }))
        }
        SshResourceResponse::Error { ref code, .. }
            if matches!(
                code.as_str(),
                "ssh_resource_registry_stale" | "ssh_resource_outcome_unknown"
            ) =>
        {
            runtime.ssh_resource_gateway.forget(binding_id);
            response_to_error(response)
        }
        other => response_to_error(other),
    }
}

async fn resolve_binding(
    runtime: &ToolRuntime,
    binding: Option<String>,
    auth: Option<&AuthContext>,
) -> Result<(String, Binding, ResolvedRunner), GatewayError> {
    let binding_id = binding.ok_or_else(binding_required_error)?;
    if !binding_id.starts_with("wc_sbind_") || binding_id.len() != 41 {
        return Err(binding_required_error());
    }
    let observed = runtime
        .ssh_resource_gateway
        .binding(&binding_id)
        .ok_or_else(binding_required_error)?;
    let caller = CallerIdentity::from_auth(auth.expect("authorized checked above"));
    if observed.caller != caller {
        return Err(binding_required_error());
    }
    let runner = resolve_runner(runtime, &observed.client_id, auth)
        .await
        .map_err(|_| binding_required_error())?;
    if runner.runner_instance_id != observed.runner_instance_id {
        runtime.ssh_resource_gateway.forget(&binding_id);
        return Err(GatewayError::new(
            "runner_replaced",
            "the exact Runner instance observed by this binding is no longer current",
        )
        .recovery("List SSH resources again. WebCodex did not retarget or replay the mutation."));
    }
    Ok((binding_id, observed, runner))
}

async fn resolve_runner(
    runtime: &ToolRuntime,
    runner_id: &str,
    auth: Option<&AuthContext>,
) -> Result<ResolvedRunner, GatewayError> {
    if runner_id.trim().is_empty()
        || runner_id.len() > 128
        || runner_id.chars().any(char::is_control)
    {
        return Err(GatewayError::new(
            "ssh_resource_invalid",
            "runner is not a valid exact Runner client id",
        ));
    }
    let access = crate::runner_http::runner_access_from_auth(auth);
    runtime
        .runner_registry
        .list_runners_for_auth(access.as_ref())
        .await
        .into_iter()
        .find(|runner| {
            runner.client_id == runner_id
                && runner.connected
                && runner.capabilities.managed_ssh_resources
        })
        .map(|runner| ResolvedRunner {
            client_id: runner.client_id,
            runner_instance_id: runner.runner_instance_id,
        })
        .ok_or_else(|| {
            GatewayError::new(
                "ssh_resource_registry_unavailable",
                "the exact Runner is not currently available with managed SSH resource support",
            )
        })
}

async fn execute_exact(
    runtime: &ToolRuntime,
    runner: &ResolvedRunner,
    request: SshResourceRequest,
    auth: Option<&AuthContext>,
) -> Result<SshResourceResponse, GatewayError> {
    let expected_request = request.clone();
    let access = crate::runner_http::runner_access_from_auth(auth);
    let (request_id, receiver) = runtime
        .runner_registry
        .enqueue_ssh_resource(
            &runner.client_id,
            &runner.runner_instance_id,
            request,
            access.as_ref(),
            SSH_RESOURCE_TOOL_NAME.to_string(),
        )
        .await
        .map_err(|message| {
            if message.starts_with("runner_replaced")
                || message.contains("exact Runner")
                || message.contains("offline")
            {
                GatewayError::new(
                    "runner_replaced",
                    "the exact Runner changed or became unavailable before dispatch",
                )
                .recovery(
                    "List SSH resources again. WebCodex did not retarget or replay the operation.",
                )
            } else {
                GatewayError::new(
                    "ssh_resource_registry_unavailable",
                    "the managed SSH resource request could not be queued",
                )
            }
        })?;
    let response = match tokio::time::timeout(RUNNER_WAIT_TIMEOUT, receiver).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) | Err(_) => {
            let dispatched = runtime
                .runner_registry
                .cancel_request_dispatch_state(&request_id)
                .await;
            return Err(if dispatched == Some(false) {
                GatewayError::new(
                    "ssh_resource_registry_unavailable",
                    "managed SSH resource request timed out before Runner dispatch",
                )
            } else {
                GatewayError::new(
                    "ssh_resource_outcome_unknown",
                    "managed SSH resource request may have reached the Runner; list resources before any retry",
                )
                .outcome_unknown()
                .recovery("List SSH resources again before attempting another mutation.")
            });
        }
    };
    if !response.success || response.exit_code != Some(0) {
        return Err(
            if response
                .error
                .as_deref()
                .is_some_and(|message| message.starts_with("runner_replaced"))
            {
                GatewayError::new(
                    "runner_replaced",
                    "the exact Runner changed before managed SSH resource dispatch",
                )
            } else if response.request_dispatched == Some(false) {
                GatewayError::new(
                    "ssh_resource_registry_unavailable",
                    "managed SSH resource request failed before Runner dispatch",
                )
            } else {
                invalid_runner_response_error(&expected_request)
            },
        );
    }
    let stdout = response
        .stdout
        .as_deref()
        .ok_or_else(|| invalid_runner_response_error(&expected_request))?;
    let parsed = serde_json::from_str::<SshResourceResponse>(stdout)
        .map_err(|_| invalid_runner_response_error(&expected_request))?;
    validate_response_for_request(&expected_request, &parsed)
        .map_err(|_| invalid_runner_response_error(&expected_request))?;
    Ok(parsed)
}

fn invalid_runner_response_error(request: &SshResourceRequest) -> GatewayError {
    if matches!(
        request,
        SshResourceRequest::Register { .. } | SshResourceRequest::Remove { .. }
    ) {
        GatewayError::new(
            "ssh_resource_outcome_unknown",
            "managed SSH resource request reached the Runner but no valid correlated response was returned",
        )
        .outcome_unknown()
        .recovery("List SSH resources again before attempting another mutation.")
    } else {
        GatewayError::new(
            "ssh_resource_registry_unavailable",
            "Runner returned an invalid managed SSH resource response",
        )
        .completed()
    }
}

fn response_to_error<T>(response: SshResourceResponse) -> Result<T, GatewayError> {
    match response {
        SshResourceResponse::Error { code, .. } => {
            let (code, message, recovery) = match code.as_str() {
                "ssh_resource_not_found" => (
                    "ssh_resource_not_found",
                    "managed SSH resource was not found",
                    None,
                ),
                "ssh_resource_static_conflict" => (
                    "ssh_resource_static_conflict",
                    "resource name is reserved by static Runner configuration",
                    None,
                ),
                "ssh_resource_static_read_only" => (
                    "ssh_resource_static_read_only",
                    "static Runner SSH resources are read-only",
                    None,
                ),
                "ssh_resource_name_conflict" => (
                    "ssh_resource_name_conflict",
                    "managed resource name already has different configuration; remove it before registering a new target",
                    None,
                ),
                "ssh_resource_registry_stale" => (
                    "ssh_resource_registry_stale",
                    "managed SSH resource registry changed since this binding was listed",
                    Some("List SSH resources again before retrying the mutation."),
                ),
                "ssh_resource_outcome_unknown" => (
                    "ssh_resource_outcome_unknown",
                    "managed SSH resource mutation may have changed durable state",
                    Some("List SSH resources again before attempting another mutation."),
                ),
                "ssh_resource_invalid" => (
                    "ssh_resource_invalid",
                    "managed SSH resource request or registry is invalid",
                    None,
                ),
                _ => (
                    "ssh_resource_registry_unavailable",
                    "managed SSH resource registry is unavailable",
                    None,
                ),
            };
            let mut error = GatewayError::new(code, message);
            error.recovery = recovery;
            error.dispatch_state = if code == "ssh_resource_outcome_unknown" {
                "outcome_unknown"
            } else {
                "completed"
            };
            Err(error)
        }
        _ => Err(GatewayError::new(
            "ssh_resource_registry_unavailable",
            "Runner returned an unexpected managed SSH resource response",
        )
        .completed()),
    }
}

fn required<'a>(value: &'a Option<String>, field: &'static str) -> Result<&'a str, GatewayError> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GatewayError::new(
                "ssh_resource_invalid",
                if field == "target" {
                    "action=register requires target"
                } else {
                    "this action requires resource name"
                },
            )
        })
}

fn binding_required_error() -> GatewayError {
    GatewayError::new(
        "ssh_resource_binding_required",
        "a current opaque binding from ssh_resource action=list is required",
    )
    .recovery("List SSH resources on the intended exact Runner, then retry with its binding.")
}

fn success_result(value: Value) -> Value {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false
    })
}

fn error_result(error: GatewayError) -> Value {
    let mut structured = json!({
        "error": {"code": error.code, "message": error.message},
        "dispatchState": error.dispatch_state,
    });
    if let Some(recovery) = error.recovery {
        structured["recovery"] = Value::String(recovery.to_string());
    }
    let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_invalid_action_fails_closed_without_panicking() {
        let runtime = ToolRuntime::new_for_tests();
        let request = SshResourceToolCall {
            action: "probe".to_string(),
            runner: None,
            binding: None,
            name: None,
            target: None,
            default_cwd: None,
        };
        let result = invoke(&runtime, request, None, None, SessionTransport::Mcp).await;
        let Err(SpecializedGovernanceDenial::Tool(result)) = result else {
            panic!("invalid direct SSH resource action must fail as a bounded tool error");
        };
        assert!(!result.success);
        assert_eq!(result.output["error_kind"], "ssh_resource_invalid");
        assert_eq!(result.output["dispatch_state"], "not_started");
    }

    #[test]
    fn audit_projection_never_contains_target_or_default_cwd() {
        let target = "17724@w10";
        let cwd = "C:/private/work";
        let audit = audit_arguments(&json!({
            "action": "register",
            "binding": "wc_sbind_0123456789abcdef0123456789abcdef",
            "name": "w10",
            "target": target,
            "default_cwd": cwd
        }));
        let serialized = serde_json::to_string(&audit).unwrap();
        assert!(!serialized.contains(target));
        assert!(!serialized.contains(cwd));
        assert_eq!(audit["resource_name"], "w10");
        assert_eq!(audit["target_present"], true);
        assert_eq!(audit["default_cwd_present"], true);
    }

    #[test]
    fn mcp_output_schema_accepts_action_specific_structured_results() {
        let schema = mcp_output_schema();
        for value in [
            json!({
                "runner": "runner-a",
                "binding": "wc_sbind_0123456789abcdef0123456789abcdef",
                "resources": [{
                    "name": "spe",
                    "source": "managed",
                    "active": true,
                    "pending_restart": false
                }]
            }),
            json!({
                "resource": "spe",
                "persisted": true,
                "active": false,
                "restart_required": true
            }),
            json!({
                "error": {
                    "code": "ssh_resource_registry_stale",
                    "message": "managed SSH resource registry changed"
                },
                "dispatchState": "completed",
                "recovery": "List SSH resources again before retrying."
            }),
        ] {
            crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&value, &schema)
                .unwrap_or_else(|error| {
                    panic!("ssh_resource MCP schema rejected {value}: {error}")
                });
        }
    }

    #[test]
    fn operation_policy_keeps_list_read_like_and_mutations_management_like() {
        use crate::tool_runtime::specialized::SpecializedEffect;

        assert_eq!(
            SshResourceOperation::List.policy().effect,
            SpecializedEffect::Read
        );
        assert_eq!(
            SshResourceOperation::Register.policy().effect,
            SpecializedEffect::Management
        );
        assert_eq!(
            SshResourceOperation::Remove.policy().effect,
            SpecializedEffect::Management
        );
        assert!(SshResourceOperation::Register.policy().write_like);
        assert!(!SshResourceOperation::Register.policy().shell_like);
        assert!(SshResourceOperation::Remove.policy().write_like);
        assert!(!SshResourceOperation::Remove.policy().shell_like);
    }
}
