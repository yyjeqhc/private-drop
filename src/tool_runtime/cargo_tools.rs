//! Runtime dispatch adapters for Cargo tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use serde_json::json;

impl ToolRuntime {
    pub(crate) async fn dispatch_cargo_tool(
        &self,
        call: ToolCall,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let tool_name = call.tool_name();
        let mut result = match call {
            ToolCall::CargoFmt {
                project,
                session_id,
                cwd,
                check,
                timeout_secs,
                sync_wait_secs,
            } => {
                self.cargo_fmt_with_context(
                    project,
                    cwd,
                    check,
                    timeout_secs,
                    sync_wait_secs,
                    session_id,
                    ssh_resource,
                    auth,
                )
                .await
            }
            ToolCall::CargoCheck {
                project,
                session_id,
                cwd,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                timeout_secs,
                sync_wait_secs,
            } => {
                self.cargo_check_with_context(
                    project,
                    cwd,
                    all_targets,
                    all_features,
                    no_default_features,
                    features,
                    package,
                    timeout_secs,
                    sync_wait_secs,
                    session_id,
                    ssh_resource,
                    auth,
                )
                .await
            }
            ToolCall::CargoTest {
                project,
                session_id,
                cwd,
                filter,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                no_run,
                require_tests,
                min_tests,
                timeout_secs,
                sync_wait_secs,
            } => {
                self.cargo_test_with_context(
                    project,
                    cwd,
                    filter,
                    all_targets,
                    all_features,
                    no_default_features,
                    features,
                    package,
                    no_run,
                    require_tests,
                    min_tests,
                    timeout_secs,
                    sync_wait_secs,
                    session_id,
                    ssh_resource,
                    auth,
                )
                .await
            }
            ToolCall::GoTest {
                project,
                session_id,
                cwd,
                packages,
                timeout_secs,
                sync_wait_secs,
            } => {
                self.go_test_with_context(
                    project,
                    cwd,
                    packages,
                    timeout_secs,
                    sync_wait_secs,
                    session_id,
                    ssh_resource,
                    auth,
                )
                .await
            }
            _ => unreachable!("non-cargo tool routed to cargo dispatcher"),
        };
        if !result.success
            && !result
                .output
                .get("command_started")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            let error = result
                .error
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let failure_kind = if error.contains("capabil")
                || error.contains("offline")
                || error.contains("not connected")
                || error.contains("unknown shell client")
            {
                "capability_unavailable"
            } else if error.contains("permission")
                || error.contains("scope")
                || error.contains("unauthorized")
            {
                "permission_denied"
            } else if error.contains("sandbox") {
                "sandbox_unavailable"
            } else if error.contains("cwd") || error.contains("working directory") {
                "cwd_invalid"
            } else if error.contains("project") && error.contains("unknown") {
                "project_not_found"
            } else {
                "invalid_arguments"
            };
            if !result.output.is_object() {
                result.output = json!({});
            }
            let output = result.output.as_object_mut().expect("output object");
            output
                .entry("execution_source".to_string())
                .or_insert_with(|| json!(tool_name));
            output
                .entry("command_started".to_string())
                .or_insert_with(|| json!(false));
            output
                .entry("command_completed".to_string())
                .or_insert_with(|| json!(false));
            output
                .entry("failure_kind".to_string())
                .or_insert_with(|| json!(failure_kind));
        }
        result
    }
}
