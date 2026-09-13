//! Runtime dispatch adapter for handoff tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;

impl ToolRuntime {
    pub(crate) async fn dispatch_handoff_tool(
        &self,
        call: ToolCall,
        auth: Option<&AuthContext>,
        context_continuity_capable: bool,
        recording_session_id: Option<&str>,
    ) -> ToolResult {
        match call {
            ToolCall::SessionHandoffSummary {
                session_id,
                project,
                include_workspace,
                include_checkpoints,
                include_validation,
                summary_only,
                limit,
            } => {
                // The explicit business target never rebaselines an unrelated recorder.
                let can_recover = context_continuity_capable
                    && recording_session_id.is_none_or(|id| id == session_id)
                    && !summary_only
                    && limit.is_none_or(|limit| {
                        limit == 0 || limit >= super::handoff::DEFAULT_HANDOFF_LIMIT
                    })
                    && include_workspace.unwrap_or(true)
                    && include_checkpoints.unwrap_or(true)
                    && include_validation.unwrap_or(true);
                // Recovery should reconstruct the same bounded current state as
                // the former automatic handoff. For a project-scoped Session,
                // derive its already-authorized canonical Project when the caller
                // omitted `project`; partial handoffs keep their existing lighter
                // semantics and cannot establish a baseline.
                let project = if can_recover
                    && project
                        .as_deref()
                        .map(str::trim)
                        .is_none_or(|project| project.is_empty())
                {
                    self.sessions.session_project(&session_id).flatten()
                } else {
                    project
                };
                let observed_revision = can_recover
                    .then(|| self.sessions.context_revision(&session_id))
                    .flatten();
                let mut result = self
                    .session_handoff_summary(
                        session_id.clone(),
                        project,
                        include_workspace,
                        include_checkpoints,
                        include_validation,
                        summary_only,
                        limit,
                        auth,
                    )
                    .await;
                if context_continuity_capable
                    && recording_session_id.is_none_or(|id| id == session_id)
                    && result.success
                {
                    super::session_context::establish_handoff_context_baseline(
                        &mut result,
                        &session_id,
                        observed_revision,
                        self.sessions.context_revision(&session_id),
                    );
                }
                result
            }
            _ => unreachable!("non-handoff tool routed to handoff dispatcher"),
        }
    }
}
