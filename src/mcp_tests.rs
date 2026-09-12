use super::*;
use crate::runner_protocol::{
    RunnerCapabilities, RunnerPollRequest, RunnerProjectSummary, RunnerRegisterRequest,
    RunnerResultRequest,
};
use base64::engine::general_purpose;
use sha2::{Digest, Sha256};

#[test]
fn mcp_gateway_tool_call_params_do_not_retain_outer_meta() {
    for meta in [
        json!({"progressToken": "legacy-progress", "custom": {"private": true}}),
        json!({
            "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {"extensions": {}},
            "io.modelcontextprotocol/clientInfo": {"name": "outer-host", "version": "2026"},
            "openai/session": "chat-session-opaque-value"
        }),
    ] {
        let parsed: McpToolCallParams = serde_json::from_value(json!({
            "name": crate::mcp_gateway::MCP_TOOL_NAME,
            "arguments": {"action": "list"},
            "_meta": meta
        }))
        .unwrap();
        let McpToolCallParams { name, arguments } = parsed;
        assert_eq!(name, crate::mcp_gateway::MCP_TOOL_NAME);
        assert_eq!(arguments, json!({"action": "list"}));
    }
}

fn test_runtime() -> ToolRuntime {
    test_runtime_with_surface(ModelSurface::LocalCoding)
}

fn test_runtime_with_exposure(runtime_exposure: RuntimeExposure) -> ToolRuntime {
    ToolRuntime::new_for_tests().with_runtime_exposure(runtime_exposure)
}

fn test_runtime_with_surface(model_surface: ModelSurface) -> ToolRuntime {
    test_runtime_with_exposure(RuntimeExposure::Runtime(model_surface))
}

fn test_runtime_with_surface_and_public_url(
    model_surface: ModelSurface,
    public_url: &str,
) -> ToolRuntime {
    let runtime_info = crate::tool_runtime::RuntimeInfo {
        configured_public_url: Some(public_url.to_string()),
        ..Default::default()
    };
    ToolRuntime::new(
        std::sync::Arc::new(crate::runner_http::RunnerRegistry::default()),
        std::sync::Arc::new(runtime_info),
    )
    .with_runtime_exposure(RuntimeExposure::Runtime(model_surface))
}

fn start_authorized_test_session(
    runtime: &ToolRuntime,
    auth: &crate::auth::AuthContext,
    mode: crate::tool_runtime::SessionMode,
) -> crate::tool_runtime::SessionSummary {
    let fingerprint = crate::tool_runtime::workflow_session_authority_fingerprint(Some(auth))
        .expect("test authority must have a stable identity");
    runtime
        .sessions
        .start_session_with_options(
            crate::tool_runtime::SessionCreateOptions::new(
                None,
                Some("specialized governance test".to_string()),
                mode,
                crate::tool_runtime::SessionGuards::default(),
            )
            .with_owner_authority_fingerprint(Some(fingerprint)),
        )
        .unwrap()
}

/// Run one synchronous operation with a temporary model-surface env value.
/// The previous value is restored while the shared env lock is still held,
/// including during unwinding. Async request tests receive an already-built
/// runtime so process-global env state never needs to span an await.
fn with_model_surface_env<T>(value: Option<&str>, operation: impl FnOnce() -> T) -> T {
    struct Restore {
        previous: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(previous) => {
                    std::env::set_var(crate::model_surface::MCP_MODEL_SURFACE_ENV, previous)
                }
                None => std::env::remove_var(crate::model_surface::MCP_MODEL_SURFACE_ENV),
            }
        }
    }

    let guard = crate::admin_cli::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os(crate::model_surface::MCP_MODEL_SURFACE_ENV);
    match value {
        Some(value) => std::env::set_var(crate::model_surface::MCP_MODEL_SURFACE_ENV, value),
        None => std::env::remove_var(crate::model_surface::MCP_MODEL_SURFACE_ENV),
    }
    let _restore = Restore {
        previous,
        _guard: guard,
    };
    operation()
}

fn test_runtime_from_model_surface_env(value: Option<&str>) -> ToolRuntime {
    with_model_surface_env(value, || {
        let runtime_exposure = crate::model_surface::resolve_runtime_exposure(None)
            .expect("test runtime exposure configuration");
        test_runtime_with_exposure(runtime_exposure)
    })
}

fn rpc(method: &str, id: Option<Value>, params: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: Some("2.0".to_string()),
        method: method.to_string(),
        params,
        id,
    }
}

fn mcp_2026_params(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("MCP params must be an object")
        .insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
    params
}

fn mcp_2026_tasks_params(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("MCP params must be an object")
        .insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {
                    "extensions": {
                        MCP_TASKS_EXTENSION: {}
                    }
                }
            }),
        );
    params
}

#[test]
fn mcp_2026_tasks_capability_is_request_scoped_and_shape_strict() {
    assert!(!request_supports_tasks(&mcp_2026_params(json!({}))));
    assert!(request_supports_tasks(&mcp_2026_tasks_params(json!({}))));

    let mut malformed = mcp_2026_params(json!({}));
    malformed["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({
        "extensions": {
            MCP_TASKS_EXTENSION: true
        }
    });
    assert!(!request_supports_tasks(&malformed));
}

fn mcp_2026_ui_params(mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("MCP params must be an object")
        .insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": MCP_STATELESS_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {
                    "extensions": {
                        MCP_UI_EXTENSION: {
                            "mimeTypes": [MCP_UI_RESOURCE_MIME_TYPE]
                        }
                    }
                }
            }),
        );
    params
}

fn mcp_export_api_auth(api_key_id: &str, username: &str) -> crate::auth::AuthContext {
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::ApiToken);
    auth.username = Some(username.to_string());
    auth.api_key_id = Some(api_key_id.to_string());
    auth.token_kind = Some("user".to_string());
    auth.scopes = vec![
        crate::auth::SCOPE_RUNTIME_READ.to_string(),
        crate::auth::SCOPE_PROJECT_READ.to_string(),
    ];
    auth
}

#[path = "mcp_tests/agent_continuation_app.rs"]
mod agent_continuation_app;
#[path = "mcp_tests/artifact_export.rs"]
mod artifact_export;
#[path = "mcp_tests/computer_app.rs"]
mod computer_app;
#[path = "mcp_tests/file_import.rs"]
mod file_import;
#[path = "mcp_tests/goal_plan_app.rs"]
mod goal_plan_app;
#[path = "mcp_tests/http_transport.rs"]
mod http_transport;
#[path = "mcp_tests/model_ergonomics.rs"]
mod model_ergonomics;
#[path = "mcp_tests/model_surface.rs"]
mod model_surface;
#[path = "mcp_tests/oauth_scope.rs"]
mod oauth_scope;
#[path = "mcp_tests/plugin_check.rs"]
mod plugin_check;
#[path = "mcp_tests/plugin_tools.rs"]
mod plugin_tools;
#[path = "mcp_tests/project_connector.rs"]
mod project_connector;
#[path = "mcp_tests/protocol.rs"]
mod protocol;
#[path = "mcp_tests/result_app.rs"]
mod result_app;
#[path = "mcp_tests/runtime_tools.rs"]
mod runtime_tools;
#[path = "mcp_tests/ssh_resource.rs"]
mod ssh_resource;
#[path = "mcp_tests/tools.rs"]
mod tools;

// =========================================================================
// HTTP integration tests — exercise the real Salvo router + AuthMiddleware.
// These do not start a real server; they build a Router, wrap it in a
// Service, and dispatch TestClient requests through it.
// =========================================================================

use crate::test_support::{seed_oauth_client, seed_user, test_config, test_config_oauth2, test_db};
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;

fn seed_oauth_access_token(
    db: &crate::Database,
    client: &crate::models::OAuthClientRecord,
    user: &crate::models::UserRecord,
    scopes: &str,
) -> String {
    let now = chrono::Utc::now().timestamp();
    let plaintext = crate::auth::generate_oauth_access_token();
    let record = crate::models::OAuthAccessTokenRecord {
        id: uuid::Uuid::new_v4().to_string(),
        token_hash: crate::auth::hash_token(&plaintext),
        client_id: client.client_id.clone(),
        subject_kind: "managed_user".to_string(),
        subject_id: user.id.clone(),
        user_id: Some(user.id.clone()),
        scopes: scopes.to_string(),
        resource: None,
        shared_key_hash: None,
        created_at: now,
        expires_at: now + 3600,
        revoked_at: None,
        last_used_at: None,
    };
    db.insert_oauth_access_token(&record).unwrap();
    plaintext
}

/// Build a minimal Router matching the production /mcp wiring: Config,
/// Database, and ToolRuntime are injected so AuthMiddleware and mcp_post
/// resolve state exactly as in `main.rs`.
fn build_test_router(
    config: Arc<crate::Config>,
    db: Arc<crate::Database>,
    runtime: Arc<ToolRuntime>,
) -> Router {
    Router::new()
        .hoop(affix_state::inject(config))
        .hoop(affix_state::inject(db))
        .hoop(affix_state::inject(runtime))
        .hoop(affix_state::inject(
            crate::connector_runtime::ConnectorRuntimeSlot::default(),
        ))
        .push(
            Router::with_path("mcp")
                .hoop(crate::AuthMiddleware)
                .get(mcp_info)
                .post(mcp_post),
        )
}

/// Effective HTTP status: the explicitly set status_code, or OK when the
/// handler only rendered a body (Salvo defaults Json bodies to 200).
fn effective_status(resp: &Response) -> StatusCode {
    resp.status_code.unwrap_or(StatusCode::OK)
}

async fn oauth_mcp_request(
    service: &Service,
    token: &str,
    method: &str,
    params: Value,
) -> (StatusCode, Value, Option<String>) {
    let stateless_2026 = request_protocol_version(&params) == Some(MCP_STATELESS_PROTOCOL_VERSION);
    let tool_name = (method == "tools/call")
        .then(|| {
            params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten();
    let mut request = TestClient::post("http://localhost/mcp").bearer_auth(token);
    if stateless_2026 {
        request = request
            .add_header(
                MCP_PROTOCOL_VERSION_HEADER,
                MCP_STATELESS_PROTOCOL_VERSION,
                true,
            )
            .add_header(MCP_METHOD_HEADER, method, true);
        if let Some(tool_name) = tool_name.as_deref() {
            request = request.add_header(MCP_NAME_HEADER, tool_name, true);
        }
    }
    let mut resp = request
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": method,
            "params": params,
        }))
        .send(service)
        .await;
    let status = effective_status(&resp);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.take_json::<Value>().await.unwrap();
    (status, body, challenge)
}
