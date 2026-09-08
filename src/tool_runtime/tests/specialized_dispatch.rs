//! Behavioral coverage of the shared pre-static-policy dispatch boundary.

use super::super::kernel::{
    HostFileImportTrust, ToolCallContext, ToolCallErrorStatus, ToolCallRequest, ToolTransport,
};
use super::super::specialized::try_dispatch_specialized_gateway;
use super::super::ToolRuntime;
use super::support::auth_context;
use crate::auth::{
    SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE, SCOPE_PLUGIN_MANAGE, SCOPE_SSH_LOCAL,
};
use serde_json::json;

fn context() -> ToolCallContext<'static> {
    ToolCallContext {
        transport: ToolTransport::Api,
        session_id: None,
        auth: None,
        window: None,
        record_oauth_scope_denials: true,
        host_file_import_trust: HostFileImportTrust::Untrusted,
    }
}

#[tokio::test]
async fn specialized_dispatch_leaves_ordinary_and_unknown_requests_untouched() {
    let runtime = ToolRuntime::new_for_tests();
    for tool_name in ["list_projects", "read_project_file", "unknown_gateway"] {
        // Even malformed ordinary arguments belong to the generic lifecycle.
        let request = ToolCallRequest {
            tool_name: tool_name.to_string(),
            arguments: json!(null),
        };
        assert!(
            try_dispatch_specialized_gateway(&runtime, &request, context())
                .await
                .is_none()
        );
        assert_eq!(request.arguments, json!(null));
    }
}

#[tokio::test]
async fn specialized_dispatch_maps_each_gateway_action_scope_without_static_policy() {
    let runtime = ToolRuntime::new_for_tests();
    for (tool_name, arguments, scope) in [
        (
            "plugin_tool",
            json!({"action":"list"}),
            SCOPE_PLUGIN_INSPECT,
        ),
        (
            "plugin_tool",
            json!({"action":"describe", "runner":"runner", "plugin":"provider", "tool":"echo"}),
            SCOPE_PLUGIN_INSPECT,
        ),
        (
            "plugin_tool",
            json!({"action":"call", "binding":"wc_pbind_00000000000000000000000000000000", "arguments":{}}),
            SCOPE_PLUGIN_INVOKE,
        ),
        (
            "plugin_tool",
            json!({"action":"check", "runner":"runner", "plugin":"provider"}),
            SCOPE_PLUGIN_MANAGE,
        ),
        (
            "plugin_tool",
            json!({"action":"reload", "runner":"runner"}),
            SCOPE_PLUGIN_MANAGE,
        ),
        ("ssh_resource", json!({"action":"list"}), SCOPE_SSH_LOCAL),
        (
            "ssh_resource",
            json!({"action":"register"}),
            SCOPE_SSH_LOCAL,
        ),
        ("ssh_resource", json!({"action":"remove"}), SCOPE_SSH_LOCAL),
    ] {
        let request = ToolCallRequest {
            tool_name: tool_name.to_string(),
            arguments,
        };
        let outcome = try_dispatch_specialized_gateway(&runtime, &request, context())
            .await
            .unwrap();
        assert!(!outcome.success);
        assert!(outcome.result.is_none());
        assert!(
            matches!(outcome.error_status, Some(ToolCallErrorStatus::InsufficientScope {
            required_scope: Some(required), ..
        }) if required == scope),
            "{request:?}"
        );
    }
}

#[tokio::test]
async fn specialized_dispatch_parse_failures_never_fall_through() {
    let runtime = ToolRuntime::new_for_tests();
    for tool_name in ["plugin_tool", "ssh_resource"] {
        for arguments in [
            json!(null),
            json!({}),
            json!({"action":"unknown"}),
            json!({"action":"list", "binding":42}),
        ] {
            let request = ToolCallRequest {
                tool_name: tool_name.to_string(),
                arguments,
            };
            let outcome = try_dispatch_specialized_gateway(&runtime, &request, context())
                .await
                .unwrap();
            assert!(!outcome.success);
            assert!(outcome.result.is_none());
            assert!(
                matches!(
                    outcome.error_status,
                    Some(ToolCallErrorStatus::InvalidArguments { .. })
                ),
                "{request:?}"
            );
        }
    }
}

#[tokio::test]
async fn specialized_dispatch_preserves_recording_authority_denial_as_tool_result() {
    let runtime = ToolRuntime::new_for_tests();
    let mut auth = auth_context(Some("alice"), false);
    auth.scopes = vec![
        SCOPE_PLUGIN_INSPECT.to_string(),
        SCOPE_SSH_LOCAL.to_string(),
    ];
    let owner = auth_context(Some("bob"), false);
    let fingerprint = super::super::workflow_session_authority_fingerprint(Some(&owner)).unwrap();
    let session = runtime
        .sessions
        .start_session_with_options(
            super::super::SessionCreateOptions::new(
                None,
                None,
                super::super::SessionMode::Normal,
                super::super::SessionGuards::default(),
            )
            .with_owner_authority_fingerprint(Some(fingerprint)),
        )
        .unwrap();
    for (session_id, error_field, error_kind) in [
        ("wc_sess_missing", "error_kind", "unknown_session_id"),
        (
            session.session_id.as_str(),
            "failure_kind",
            "session_authority_denied",
        ),
    ] {
        for tool_name in ["plugin_tool", "ssh_resource"] {
            let request = ToolCallRequest {
                tool_name: tool_name.to_string(),
                arguments: json!({"action":"list"}),
            };
            let outcome = try_dispatch_specialized_gateway(
                &runtime,
                &request,
                ToolCallContext {
                    auth: Some(&auth),
                    session_id: Some(session_id),
                    ..context()
                },
            )
            .await
            .unwrap();
            assert!(!outcome.success);
            assert!(outcome.error_status.is_none());
            let result = outcome.result.unwrap();
            assert_eq!(result.output[error_field], error_kind);
            assert_eq!(result.output["dispatch_certainty"], "not_started");
        }
    }
}
