//! Runtime dispatch adapters for discovery and observability tool calls.

use super::kernel::ToolProtocolCapabilities;
use super::runtime_info::ListRunnersOptions;
use super::tool_inputs::ListToolsOptions;
use super::{ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;

impl ToolRuntime {
    pub(crate) async fn dispatch_discovery_tool(
        &self,
        call: ToolCall,
        auth: Option<&AuthContext>,
        protocol_capabilities: ToolProtocolCapabilities,
    ) -> ToolResult {
        match call {
            ToolCall::ListTools {
                category,
                features,
                summary_only,
                limit,
            } => ToolResult::ok(self.list_tools_payload(ListToolsOptions {
                category,
                features,
                summary_only,
                limit,
            })),
            ToolCall::ListRunners {
                client_id,
                client_ids,
                include_projects,
                summary_only,
            } => {
                self.list_runners_with_options(
                    auth,
                    ListRunnersOptions {
                        client_id,
                        client_ids,
                        include_projects,
                        summary_only,
                    },
                )
                .await
            }
            ToolCall::RuntimeStatus {
                compact,
                summary_only,
                client_id,
            } => {
                self.runtime_status_with_options(auth, compact, summary_only, client_id)
                    .await
            }
            ToolCall::ReadToolTrace {
                trace_ref,
                offset,
                limit,
                payload_index,
            } => match tokio::task::spawn_blocking({
                let trace_ref = trace_ref.clone();
                move || {
                    crate::tool_request_trace::read_full_trace(
                        &trace_ref,
                        offset,
                        limit,
                        payload_index,
                    )
                }
            })
            .await
            .map_err(|_| crate::tool_request_trace::TraceReadError {
                kind: "trace_store_unavailable",
                message: "trace diagnostic worker failed".to_string(),
            })
            .and_then(|result| result)
            {
                Ok(output) => ToolResult::ok(output),
                Err(error) => ToolResult::err_with_output(
                    error.message.clone(),
                    serde_json::json!({
                        "error_kind": error.kind,
                        "message": error.message,
                        "trace_ref": trace_ref,
                        "state_changed": false,
                    }),
                ),
            },
            ToolCall::ToolManifest {
                tool_name,
                category,
                intent,
                include_recommended_flows,
                include_risk_summary,
            } => {
                self.tool_manifest(
                    tool_name,
                    category,
                    intent,
                    include_recommended_flows,
                    include_risk_summary,
                    protocol_capabilities,
                )
                .await
            }
            _ => unreachable!("non-discovery tool routed to discovery dispatcher"),
        }
    }
}
