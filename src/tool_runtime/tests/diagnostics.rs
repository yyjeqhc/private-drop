use super::super::kernel::{
    check_runtime_tool_scope, HostFileImportTrust, ToolCallContext, ToolCallErrorStatus,
    ToolCallRequest, ToolProtocolCapabilities, ToolTransport,
};
use super::super::ToolRuntime;
use serde_json::json;

fn admin_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        role: Some("admin".to_string()),
        scopes: vec![crate::auth::SCOPE_ADMIN.to_string()],
        is_bootstrap: true,
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap)
    }
}

fn runtime_reader_auth() -> crate::auth::AuthContext {
    crate::auth::AuthContext {
        scopes: vec![crate::auth::SCOPE_RUNTIME_READ.to_string()],
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token)
    }
}

fn context(auth: Option<&crate::auth::AuthContext>) -> ToolCallContext<'_> {
    ToolCallContext {
        transport: ToolTransport::Mcp,
        session_id: None,
        auth,
        window: None,
        record_oauth_scope_denials: false,
        host_file_import_trust: HostFileImportTrust::Untrusted,
    }
}

fn missing_job_request() -> ToolCallRequest {
    ToolCallRequest {
        tool_name: "job_tail".to_string(),
        arguments: json!({"job_id": "missing-trace-ref-job"}),
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn failed_tool_trace_ref_requires_full_mode_admin_and_operator_capability() {
    let mut env = crate::test_support::TestEnvGuard::new();
    env.set("WEBCODEX_TOOL_REQUEST_TRACE", "full");
    let runtime = ToolRuntime::new_for_tests();
    let admin = admin_auth();
    let ordinary = runtime_reader_auth();
    let capable = ToolProtocolCapabilities {
        trace_diagnostics: true,
        ..Default::default()
    };

    assert!(matches!(
        check_runtime_tool_scope(None, "read_tool_trace"),
        Err(ToolCallErrorStatus::InsufficientScope {
            required_scope: Some(crate::auth::SCOPE_ADMIN),
            ..
        })
    ));
    assert!(matches!(
        check_runtime_tool_scope(Some(&ordinary), "read_tool_trace"),
        Err(ToolCallErrorStatus::InsufficientScope {
            required_scope: Some(crate::auth::SCOPE_ADMIN),
            ..
        })
    ));
    assert!(check_runtime_tool_scope(Some(&admin), "read_tool_trace").is_ok());

    let admin_trace = crate::tool_request_trace::new_trace_id();
    let admin_result = crate::tool_request_trace::scope_active_trace(
        Some(admin_trace.clone()),
        runtime.call_tool_with_protocol_capabilities(
            missing_job_request(),
            context(Some(&admin)),
            capable,
        ),
    )
    .await
    .result
    .expect("missing Job should return a structured failed tool result");
    assert!(!admin_result.success);
    assert_eq!(admin_result.output["trace_ref"], admin_trace);

    let ordinary_trace = crate::tool_request_trace::new_trace_id();
    let ordinary_result = crate::tool_request_trace::scope_active_trace(
        Some(ordinary_trace),
        runtime.call_tool_with_protocol_capabilities(
            missing_job_request(),
            context(Some(&ordinary)),
            capable,
        ),
    )
    .await
    .result
    .expect("ordinary missing Job should remain a structured failed tool result");
    assert!(!ordinary_result.success);
    assert!(ordinary_result.output.get("trace_ref").is_none());

    let hidden_trace = crate::tool_request_trace::new_trace_id();
    let hidden_result = crate::tool_request_trace::scope_active_trace(
        Some(hidden_trace),
        runtime.call_tool_with_protocol_capabilities(
            missing_job_request(),
            context(Some(&admin)),
            ToolProtocolCapabilities::default(),
        ),
    )
    .await
    .result
    .expect("capability-hidden trace diagnostics must not change tool failure delivery");
    assert!(!hidden_result.success);
    assert!(hidden_result.output.get("trace_ref").is_none());

    env.remove("WEBCODEX_TOOL_REQUEST_TRACE");
    let disabled_trace = crate::tool_request_trace::new_trace_id();
    let disabled_result = crate::tool_request_trace::scope_active_trace(
        Some(disabled_trace),
        runtime.call_tool_with_protocol_capabilities(
            missing_job_request(),
            context(Some(&admin)),
            capable,
        ),
    )
    .await
    .result
    .expect("trace-disabled failure must remain structured");
    assert!(!disabled_result.success);
    assert!(disabled_result.output.get("trace_ref").is_none());
}

#[tokio::test]
async fn trace_reader_tool_name_is_fail_closed_outside_operator_protocol_capability() {
    let runtime = ToolRuntime::new_for_tests();
    let admin = admin_auth();
    let outcome = runtime
        .call_tool_with_protocol_capabilities(
            ToolCallRequest {
                tool_name: "read_tool_trace".to_string(),
                arguments: json!({"trace_ref": crate::tool_request_trace::new_trace_id()}),
            },
            context(Some(&admin)),
            ToolProtocolCapabilities::default(),
        )
        .await;
    assert!(matches!(
        outcome.error_status,
        Some(ToolCallErrorStatus::InvalidArguments { ref message })
            if message.contains("Stateless MCP 2026 operator surfaces")
    ));
    assert!(outcome.result.is_none());
}
