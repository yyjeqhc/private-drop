//! Runtime dispatch adapters for job tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;

impl ToolRuntime {
    pub(crate) async fn dispatch_job_tool(
        &self,
        call: ToolCall,
        auth: Option<&AuthContext>,
        ssh_resource: Option<&str>,
    ) -> ToolResult {
        match call {
            ToolCall::RunJob {
                project,
                command,
                session_id,
                timeout_secs,
                cwd,
                purpose,
                shell,
            } => {
                self.run_job_for_auth_with_contract_with_ssh_resource(
                    project,
                    command,
                    session_id,
                    timeout_secs,
                    cwd,
                    Vec::new(),
                    auth,
                    purpose,
                    shell,
                    ssh_resource,
                )
                .await
            }
            ToolCall::StopJob {
                project,
                job_id,
                session_id,
                confirm,
            } => {
                self.stop_job_model_facing(project, job_id, session_id, confirm, auth)
                    .await
            }
            ToolCall::ObserveJobs {
                items,
                tail_lines,
                wait_secs,
            } => {
                self.observe_jobs_for_auth(items, tail_lines, wait_secs, auth)
                    .await
            }
            ToolCall::ListJobs {
                limit,
                status,
                project,
                session_id,
            } => {
                self.list_jobs_for_auth_with_filters(limit, status, project, session_id, auth)
                    .await
            }
            ToolCall::JobTail {
                job_id,
                tail_lines,
                after_observation_token,
                wait_secs,
            } => {
                self.job_tail_for_auth(job_id, tail_lines, auth, after_observation_token, wait_secs)
                    .await
            }
            _ => unreachable!("non-job tool routed to job dispatcher"),
        }
    }
}
