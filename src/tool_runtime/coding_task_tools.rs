//! Runtime dispatch adapters for coding-task workflow tool calls.

use super::{sessions, window_activity::ToolCallCorrelation, ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;

impl ToolRuntime {
    pub(crate) async fn dispatch_coding_task_tool(
        &self,
        call: ToolCall,
        auth: Option<&AuthContext>,
        transport: sessions::SessionTransport,
        trusted_recording_session_id: Option<&str>,
        trusted_recording_session_project: Option<&str>,
        correlation: &mut ToolCallCorrelation,
    ) -> ToolResult {
        match call {
            ToolCall::WorkOnProject {
                project,
                client_id,
                path,
                mode,
                base_ref,
                instruction,
                include_project_instructions,
                include_workflow_guidance,
                session_id,
            } => {
                self.work_on_project(
                    project,
                    client_id,
                    path,
                    mode,
                    base_ref,
                    instruction,
                    session_id,
                    include_project_instructions,
                    include_workflow_guidance,
                    auth,
                    trusted_recording_session_id,
                    trusted_recording_session_project,
                    transport,
                    correlation,
                )
                .await
            }
            ToolCall::FinishCodingTask {
                project,
                session_id,
                summary_only,
                include_diff,
                include_workspace,
                include_hygiene,
                include_handoff,
                include_validation_summary,
            } => {
                self.finish_coding_task(
                    project,
                    session_id,
                    summary_only,
                    include_diff,
                    include_workspace,
                    include_hygiene,
                    include_handoff,
                    include_validation_summary,
                    auth,
                )
                .await
            }
            _ => unreachable!("non-coding-task tool routed to coding-task dispatcher"),
        }
    }
}
