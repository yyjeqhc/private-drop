use super::helpers::{project_relative_runner_cwd, resolve_runner_cwd};
use super::{ExecutionPurpose, ExecutionShell, ToolResult, ToolRuntime};
use crate::projects::ProjectConfig;
use crate::runner_protocol::{
    PersistentShellRequest, PersistentShellResult, ShellJobContext, RAW_SHELL_COMMAND_MAX_BYTES,
};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

const SERVER_MAX_PERSISTENT_SHELLS: usize = 64;
const SERVER_MAX_TERMINAL_SHELLS: usize = 128;

#[derive(Debug, Clone)]
struct SessionShellRecord {
    shell_id: String,
    workflow_session_id: String,
    runtime_project_id: String,
    executor: String,
    client_id: Option<String>,
    /// Named SSH resource this shell is bound to, if any. `None` for local and
    /// plain Runner shells. Bound at open and never re-derived from Session
    /// context, so a later context change cannot redirect an open shell.
    resource: Option<String>,
    shell: String,
    profile: Option<String>,
    initial_cwd: String,
    initial_cwd_frozen: bool,
    cwd: String,
    created_at: i64,
    last_activity_at: i64,
    state: String,
    busy: Arc<AtomicBool>,
    exit_code: Option<i32>,
    close_reason: Option<String>,
}

impl SessionShellRecord {
    fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "opening" | "running")
    }

    /// Build the safe `ShellJobContext` that routes this shell to its bound
    /// Runner resource. Carries only the named resource + ids; never host,
    /// ControlPath, credentials, or command text.
    fn job_context(&self) -> Option<ShellJobContext> {
        self.resource.as_ref().map(|resource| ShellJobContext {
            runtime_project_id: Some(self.runtime_project_id.clone()),
            workflow_session_id: Some(self.workflow_session_id.clone()),
            ssh_resource: Some(resource.clone()),
            project_cwd: None,
            cwd: None,
            purpose: None,
            shell: None,
            command_preview: String::new(),
            validation_steps: Vec::new(),
            validation: None,
            structured_execution: None,
        })
    }
}

#[derive(Debug, Default)]
struct SessionShellRegistryState {
    records: HashMap<String, SessionShellRecord>,
    terminal_order: VecDeque<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionShellRegistry {
    state: Arc<Mutex<SessionShellRegistryState>>,
}

impl Default for SessionShellRegistry {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionShellRegistryState::default())),
        }
    }
}

impl SessionShellRegistry {
    async fn reserve_open(
        &self,
        workflow_session_id: &str,
        runtime_project_id: &str,
        executor: &str,
        client_id: Option<String>,
        resource: Option<String>,
        shell: &str,
        initial_cwd: &str,
    ) -> Result<String, String> {
        let mut state = self.state.lock().await;
        if state
            .records
            .values()
            .any(|record| record.workflow_session_id == workflow_session_id && record.is_active())
        {
            return Err(
                "persistent_shell_already_open: Workflow Session already has an active persistent shell"
                    .to_string(),
            );
        }
        let active = state
            .records
            .values()
            .filter(|record| record.is_active())
            .count();
        if active >= SERVER_MAX_PERSISTENT_SHELLS {
            return Err(format!(
                "persistent_shell_limit_reached: Server limit is {SERVER_MAX_PERSISTENT_SHELLS}"
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let shell_id = format!("wc_shell_{}", Uuid::new_v4().simple());
        state.records.insert(
            shell_id.clone(),
            SessionShellRecord {
                shell_id: shell_id.clone(),
                workflow_session_id: workflow_session_id.to_string(),
                runtime_project_id: runtime_project_id.to_string(),
                executor: executor.to_string(),
                client_id,
                resource,
                shell: shell.to_string(),
                profile: None,
                initial_cwd: initial_cwd.to_string(),
                initial_cwd_frozen: false,
                cwd: initial_cwd.to_string(),
                created_at: now,
                last_activity_at: now,
                state: "opening".to_string(),
                busy: Arc::new(AtomicBool::new(false)),
                exit_code: None,
                close_reason: None,
            },
        );
        Ok(shell_id)
    }

    async fn record_for_operation(
        &self,
        workflow_session_id: &str,
        runtime_project_id: &str,
        shell_id: &str,
    ) -> Result<SessionShellRecord, String> {
        let state = self.state.lock().await;
        state
            .records
            .get(shell_id)
            .filter(|record| {
                record.workflow_session_id == workflow_session_id
                    && record.runtime_project_id == runtime_project_id
            })
            .cloned()
            .ok_or_else(|| {
                "persistent_shell_not_found: shell does not belong to this Session and project"
                    .to_string()
            })
    }

    async fn begin_exec(
        &self,
        workflow_session_id: &str,
        runtime_project_id: &str,
        shell_id: &str,
    ) -> Result<SessionShellRecord, String> {
        let mut state = self.state.lock().await;
        let record = state
            .records
            .get_mut(shell_id)
            .filter(|record| {
                record.workflow_session_id == workflow_session_id
                    && record.runtime_project_id == runtime_project_id
            })
            .ok_or_else(|| {
                "persistent_shell_not_found: shell does not belong to this Session and project"
                    .to_string()
            })?;
        if record.state == "opening" {
            return Err("shell_busy: persistent shell is still opening".to_string());
        }
        if !record.is_active() {
            return Err(format!(
                "persistent_shell_stale: persistent shell is {}",
                record.state
            ));
        }
        if record
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("shell_busy: persistent shell is executing a command".to_string());
        }
        Ok(record.clone())
    }

    async fn apply_result(&self, result: &PersistentShellResult) {
        let mut state = self.state.lock().await;
        let Some(record) = state.records.get_mut(&result.shell_id) else {
            return;
        };
        if record.workflow_session_id != result.workflow_session_id
            || record.runtime_project_id != result.runtime_project_id
        {
            return;
        }
        let next_state = if result.shell_state == "unknown" && result.error_code.is_some() {
            "lost".to_string()
        } else {
            result.shell_state.clone()
        };
        // Results can arrive out of order across concurrent exec/close
        // requests. A terminal shell id is never reusable, so no late result
        // may change the authoritative terminal state in the Server registry.
        if !record.is_active() {
            record.busy.store(false, Ordering::SeqCst);
            return;
        }
        record.state = next_state;
        record.busy.store(result.busy, Ordering::SeqCst);
        record.exit_code = result.exit_code;
        if let Some(cwd) = &result.cwd {
            record.cwd = cwd.clone();
        }
        if !record.initial_cwd_frozen {
            if let Some(initial_cwd) = result.initial_cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
                record.initial_cwd = initial_cwd.to_string();
                record.initial_cwd_frozen = true;
            }
        }
        if let Some(shell) = &result.shell {
            record.shell = shell.clone();
        }
        if result.profile.is_some() {
            record.profile = result.profile.clone();
        }
        if let Some(created_at) = result.created_at {
            record.created_at = created_at;
        }
        if let Some(last_activity_at) = result.last_activity_at {
            record.last_activity_at = last_activity_at;
        } else {
            record.last_activity_at = chrono::Utc::now().timestamp();
        }
        record.close_reason = result.close_reason.clone();
        if !record.is_active() {
            let shell_id = record.shell_id.clone();
            if !state.terminal_order.iter().any(|id| id == &shell_id) {
                state.terminal_order.push_back(shell_id);
            }
            prune_terminal_records(&mut state);
        } else {
            state
                .terminal_order
                .retain(|shell_id| shell_id != &result.shell_id);
        }
    }

    async fn mark_lost(&self, shell_id: &str, reason: &str) {
        let mut state = self.state.lock().await;
        if let Some(record) = state.records.get_mut(shell_id) {
            record.state = "lost".to_string();
            record.busy.store(false, Ordering::SeqCst);
            record.close_reason = Some(reason.to_string());
            record.last_activity_at = chrono::Utc::now().timestamp();
            if !state.terminal_order.iter().any(|id| id == shell_id) {
                state.terminal_order.push_back(shell_id.to_string());
            }
            prune_terminal_records(&mut state);
        }
    }

    async fn active_for_session(&self, session_id: &str) -> Vec<SessionShellRecord> {
        self.state
            .lock()
            .await
            .records
            .values()
            .filter(|record| record.workflow_session_id == session_id && record.is_active())
            .cloned()
            .collect()
    }
}

struct SessionShellExecGuard {
    busy: Arc<AtomicBool>,
}

impl Drop for SessionShellExecGuard {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

fn prune_terminal_records(state: &mut SessionShellRegistryState) {
    while state.terminal_order.len() > SERVER_MAX_TERMINAL_SHELLS {
        let Some(shell_id) = state.terminal_order.pop_front() else {
            break;
        };
        if state
            .records
            .get(&shell_id)
            .is_some_and(|record| !record.is_active())
        {
            state.records.remove(&shell_id);
        }
    }
}

impl ToolRuntime {
    pub(crate) async fn dispatch_session_shell_tool(
        &self,
        call: super::ToolCall,
        ssh_resource: Option<&str>,
    ) -> ToolResult {
        let session_id = call
            .session_id()
            .expect("persistent shell tools require an explicit Session");
        if self
            .sessions
            .lifecycle_state(session_id)
            .is_some_and(|lifecycle| !lifecycle.allows_mutation())
        {
            return shell_tool_error(
                "persistent_shell_session_inactive",
                "Workflow Session is not active",
                None,
            );
        }
        match call {
            super::ToolCall::OpenSessionShell {
                project,
                session_id,
                cwd,
                shell,
            } => {
                self.open_session_shell(project, session_id, cwd, shell, ssh_resource)
                    .await
            }
            super::ToolCall::SessionShellExec {
                project,
                session_id,
                shell_id,
                command,
                timeout_secs,
                purpose,
            } => {
                self.session_shell_exec(
                    project,
                    session_id,
                    shell_id,
                    command,
                    timeout_secs,
                    purpose,
                )
                .await
            }
            super::ToolCall::SessionShellStatus {
                project,
                session_id,
                shell_id,
            } => {
                self.session_shell_status(project, session_id, shell_id)
                    .await
            }
            super::ToolCall::CloseSessionShell {
                project,
                session_id,
                shell_id,
            } => {
                self.close_session_shell(project, session_id, shell_id)
                    .await
            }
            _ => unreachable!("non-persistent-shell tool routed to persistent-shell dispatcher"),
        }
    }

    async fn open_session_shell(
        &self,
        project: String,
        session_id: String,
        cwd: Option<String>,
        shell: Option<ExecutionShell>,
        ssh_resource: Option<&str>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let runtime_project_id = resolved.resolved_id;
        let project_config = resolved.config;
        self.reconcile_active_shell_before_open(&session_id, &runtime_project_id)
            .await;
        let shell_name = shell.map(|shell| shell.as_str()).unwrap_or("sh");
        // An SSH persistent shell still runs through the owning Runner (the
        // Runner opens the remote shell on its host).
        if let Some(resource) = ssh_resource {
            let client_id = project_config.client_id.clone();
            // Remote cwd: explicit open cwd > Session default_cwd (already
            // validated remote-path-shaped) > Runner resource default_cwd /
            // remote login default (filled by the Runner). It is NOT constrained
            // to the local project root.
            let effective_cwd = match resolve_remote_cwd(cwd.as_deref()) {
                Ok(cwd) => cwd,
                Err(error) => return shell_tool_error("persistent_shell_cwd_invalid", error, None),
            };
            let shell_id = match self
                .session_shells
                .reserve_open(
                    &session_id,
                    &runtime_project_id,
                    "ssh",
                    Some(client_id.clone()),
                    Some(resource.to_string()),
                    shell_name,
                    effective_cwd.as_deref().unwrap_or(""),
                )
                .await
            {
                Ok(shell_id) => shell_id,
                Err(error) => return shell_tool_error_from_message(error, None),
            };
            let job_context = ShellJobContext {
                runtime_project_id: Some(runtime_project_id.clone()),
                workflow_session_id: Some(session_id.clone()),
                ssh_resource: Some(resource.to_string()),
                project_cwd: None,
                cwd: effective_cwd.clone(),
                purpose: None,
                shell: None,
                command_preview: String::new(),
                validation_steps: Vec::new(),
                validation: None,
                structured_execution: None,
            };
            let request = PersistentShellRequest {
                action: "open".to_string(),
                shell_id: shell_id.clone(),
                workflow_session_id: session_id.clone(),
                runtime_project_id: runtime_project_id.clone(),
                cwd: effective_cwd,
                shell: shell.map(|shell| shell.as_str().to_string()),
                command: None,
                timeout_secs: None,
                purpose: None,
            };
            let result = match self
                .run_runner_persistent_shell(&client_id, request, Some(job_context), 35)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    if let Ok(record) = self
                        .session_shells
                        .record_for_operation(&session_id, &runtime_project_id, &shell_id)
                        .await
                    {
                        self.close_or_mark_lost(&record, "runner_open_result_lost", &error)
                            .await;
                    }
                    return shell_tool_lost_error_from_message(error, Some(&shell_id));
                }
            };
            self.session_shells.apply_result(&result).await;
            if let Some(error) = self
                .close_opened_shell_if_session_inactive(&session_id, &runtime_project_id, &shell_id)
                .await
            {
                return error;
            }
            return persistent_result_to_tool(
                result,
                &project_config,
                &runtime_project_id,
                &session_id,
                "open",
                "ssh",
                ssh_resource,
                None,
            );
        }
        let client_id = project_config.client_id.clone();
        let effective_cwd = match resolve_runner_cwd(&project_config, cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => return shell_tool_error("persistent_shell_cwd_invalid", error, None),
        };
        let shell_id = match self
            .session_shells
            .reserve_open(
                &session_id,
                &runtime_project_id,
                "agent",
                Some(client_id.clone()),
                None,
                shell_name,
                &effective_cwd,
            )
            .await
        {
            Ok(shell_id) => shell_id,
            Err(error) => return shell_tool_error_from_message(error, None),
        };
        let request = PersistentShellRequest {
            action: "open".to_string(),
            shell_id: shell_id.clone(),
            workflow_session_id: session_id.clone(),
            runtime_project_id: runtime_project_id.clone(),
            cwd: Some(effective_cwd),
            shell: shell.map(|shell| shell.as_str().to_string()),
            command: None,
            timeout_secs: None,
            purpose: None,
        };
        let result = match self
            .run_runner_persistent_shell(&client_id, request, None, 35)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                if let Ok(record) = self
                    .session_shells
                    .record_for_operation(&session_id, &runtime_project_id, &shell_id)
                    .await
                {
                    self.close_or_mark_lost(&record, "runner_open_result_lost", &error)
                        .await;
                }
                return shell_tool_lost_error_from_message(error, Some(&shell_id));
            }
        };
        self.session_shells.apply_result(&result).await;
        if let Some(error) = self
            .close_opened_shell_if_session_inactive(&session_id, &runtime_project_id, &shell_id)
            .await
        {
            return error;
        }
        persistent_result_to_tool(
            result,
            &project_config,
            &runtime_project_id,
            &session_id,
            "open",
            "agent",
            None,
            None,
        )
    }

    async fn session_shell_exec(
        &self,
        project: String,
        session_id: String,
        shell_id: String,
        command: String,
        timeout_secs: Option<u64>,
        purpose: Option<ExecutionPurpose>,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let runtime_project_id = resolved.resolved_id;
        let project_config = resolved.config;
        if command.len() > RAW_SHELL_COMMAND_MAX_BYTES {
            return shell_tool_error(
                "persistent_shell_invalid_command",
                format!("command exceeds the {RAW_SHELL_COMMAND_MAX_BYTES}-byte Server limit"),
                Some(&shell_id),
            );
        }
        let record = match self
            .session_shells
            .begin_exec(&session_id, &runtime_project_id, &shell_id)
            .await
        {
            Ok(record) => record,
            Err(error) => return shell_tool_error_from_message(error, Some(&shell_id)),
        };
        let _exec_guard = SessionShellExecGuard {
            busy: Arc::clone(&record.busy),
        };
        let requested_timeout_secs = timeout_secs.unwrap_or(60);
        if requested_timeout_secs == 0 {
            return shell_tool_error(
                "persistent_shell_invalid_timeout",
                "timeout_secs must be at least 1",
                Some(&shell_id),
            );
        }
        let timeout_secs = requested_timeout_secs.min(3_600);
        let client_id = record.client_id.as_deref().unwrap_or_default();
        let request = PersistentShellRequest {
            action: "exec".to_string(),
            shell_id: shell_id.clone(),
            workflow_session_id: session_id.clone(),
            runtime_project_id: runtime_project_id.clone(),
            cwd: None,
            shell: None,
            command: Some(command),
            timeout_secs: Some(timeout_secs),
            purpose: purpose.map(|purpose| purpose.as_str().to_string()),
        };
        let result = self
            .run_runner_persistent_shell(client_id, request, record.job_context(), timeout_secs + 5)
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.close_or_mark_lost(&record, "runner_exec_result_lost", &error)
                    .await;
                return shell_tool_lost_error_from_message(error, Some(&shell_id));
            }
        };
        self.session_shells.apply_result(&result).await;
        persistent_result_to_tool(
            result,
            &project_config,
            &runtime_project_id,
            &session_id,
            "exec",
            record.executor.as_str(),
            record.resource.as_deref(),
            purpose,
        )
    }

    async fn session_shell_status(
        &self,
        project: String,
        session_id: String,
        shell_id: String,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let runtime_project_id = resolved.resolved_id;
        let project_config = resolved.config;
        let record = match self
            .session_shells
            .record_for_operation(&session_id, &runtime_project_id, &shell_id)
            .await
        {
            Ok(record) => record,
            Err(error) => return shell_tool_error_from_message(error, Some(&shell_id)),
        };
        if !record.is_active() {
            return persistent_record_to_tool(record, &project_config, "status");
        }
        let result = self
            .run_runner_persistent_shell(
                record.client_id.as_deref().unwrap_or_default(),
                PersistentShellRequest {
                    action: "status".to_string(),
                    shell_id: shell_id.clone(),
                    workflow_session_id: session_id.clone(),
                    runtime_project_id: runtime_project_id.clone(),
                    cwd: None,
                    shell: None,
                    command: None,
                    timeout_secs: None,
                    purpose: None,
                },
                record.job_context(),
                5,
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.close_or_mark_lost(&record, "runner_status_result_lost", &error)
                    .await;
                return shell_tool_lost_error_from_message(error, Some(&shell_id));
            }
        };
        self.session_shells.apply_result(&result).await;
        persistent_result_to_tool(
            result,
            &project_config,
            &runtime_project_id,
            &session_id,
            "status",
            record.executor.as_str(),
            record.resource.as_deref(),
            None,
        )
    }

    async fn close_session_shell(
        &self,
        project: String,
        session_id: String,
        shell_id: String,
    ) -> ToolResult {
        let resolved = match self.resolve_project_input(&project).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_tool_result(),
        };
        let runtime_project_id = resolved.resolved_id;
        let project_config = resolved.config;
        let record = match self
            .session_shells
            .record_for_operation(&session_id, &runtime_project_id, &shell_id)
            .await
        {
            Ok(record) => record,
            Err(error) => return shell_tool_error_from_message(error, Some(&shell_id)),
        };
        if !record.is_active() {
            return persistent_record_to_tool(record, &project_config, "close");
        }
        let result = self.close_record(&record, "explicit_close").await;
        match result {
            Ok(result) => {
                self.session_shells.apply_result(&result).await;
                persistent_result_to_tool(
                    result,
                    &project_config,
                    &runtime_project_id,
                    &session_id,
                    "close",
                    record.executor.as_str(),
                    record.resource.as_deref(),
                    None,
                )
            }
            Err(error) => {
                self.session_shells.mark_lost(&shell_id, &error).await;
                shell_tool_lost_error_from_message(error, Some(&shell_id))
            }
        }
    }

    async fn close_record(
        &self,
        record: &SessionShellRecord,
        reason: &str,
    ) -> Result<PersistentShellResult, String> {
        self.run_runner_persistent_shell(
            record.client_id.as_deref().unwrap_or_default(),
            PersistentShellRequest {
                action: "close".to_string(),
                shell_id: record.shell_id.clone(),
                workflow_session_id: record.workflow_session_id.clone(),
                runtime_project_id: record.runtime_project_id.clone(),
                cwd: None,
                shell: None,
                command: None,
                timeout_secs: None,
                purpose: Some(reason.to_string()),
            },
            record.job_context(),
            10,
        )
        .await
    }

    async fn reconcile_active_shell_before_open(&self, session_id: &str, runtime_project_id: &str) {
        let records = self.session_shells.active_for_session(session_id).await;
        for record in records
            .into_iter()
            .filter(|record| record.runtime_project_id == runtime_project_id)
        {
            let result = self
                .run_runner_persistent_shell(
                    record.client_id.as_deref().unwrap_or_default(),
                    PersistentShellRequest {
                        action: "status".to_string(),
                        shell_id: record.shell_id.clone(),
                        workflow_session_id: record.workflow_session_id.clone(),
                        runtime_project_id: record.runtime_project_id.clone(),
                        cwd: None,
                        shell: None,
                        command: None,
                        timeout_secs: None,
                        purpose: None,
                    },
                    record.job_context(),
                    5,
                )
                .await;
            match result {
                Ok(result) => self.session_shells.apply_result(&result).await,
                Err(error) => {
                    self.session_shells
                        .mark_lost(&record.shell_id, &error)
                        .await
                }
            }
        }
    }

    async fn close_opened_shell_if_session_inactive(
        &self,
        session_id: &str,
        runtime_project_id: &str,
        shell_id: &str,
    ) -> Option<ToolResult> {
        if self
            .sessions
            .lifecycle_state(session_id)
            .is_some_and(|lifecycle| lifecycle.allows_mutation())
        {
            return None;
        }
        if let Ok(record) = self
            .session_shells
            .record_for_operation(session_id, runtime_project_id, shell_id)
            .await
        {
            match self
                .close_record(&record, "workflow_session_closed_during_open")
                .await
            {
                Ok(result) => {
                    self.session_shells.apply_result(&result).await;
                    self.record_session_close_shell_result(session_id, &result);
                }
                Err(error) => {
                    self.session_shells.mark_lost(shell_id, &error).await;
                    self.sessions
                        .record_session_close_persistent_shell_evidence(
                            session_id,
                            shell_id,
                            "lost",
                            "lost",
                            Some(shell_error_code(&error)),
                            false,
                        );
                }
            }
        }
        Some(shell_tool_error(
            "persistent_shell_session_inactive",
            "Workflow Session closed while the persistent shell was opening; the shell was released",
            Some(shell_id),
        ))
    }

    async fn close_or_mark_lost(
        &self,
        record: &SessionShellRecord,
        reason: &str,
        original_error: &str,
    ) {
        match self.close_record(record, reason).await {
            Ok(result) => {
                let close_failed = result.error_code.is_some();
                self.session_shells.apply_result(&result).await;
                if close_failed {
                    self.session_shells
                        .mark_lost(&record.shell_id, original_error)
                        .await;
                }
            }
            Err(_) => {
                self.session_shells
                    .mark_lost(&record.shell_id, original_error)
                    .await
            }
        }
    }

    pub(crate) async fn close_persistent_shells_for_session(&self, session_id: &str) -> usize {
        let records = self.session_shells.active_for_session(session_id).await;
        let mut closed = 0usize;
        for record in records {
            match self.close_record(&record, "workflow_session_closed").await {
                Ok(result) => {
                    self.session_shells.apply_result(&result).await;
                    self.record_session_close_shell_result(session_id, &result);
                    if result.error_code.is_none() {
                        closed = closed.saturating_add(1);
                    }
                }
                Err(error) => {
                    self.session_shells
                        .mark_lost(&record.shell_id, &error)
                        .await;
                    self.sessions
                        .record_session_close_persistent_shell_evidence(
                            session_id,
                            &record.shell_id,
                            "lost",
                            "lost",
                            Some(shell_error_code(&error)),
                            false,
                        );
                }
            }
        }
        closed
    }

    fn record_session_close_shell_result(&self, session_id: &str, result: &PersistentShellResult) {
        let (shell_state, execution_state) = normalized_result_states(result);
        self.sessions
            .record_session_close_persistent_shell_evidence(
                session_id,
                &result.shell_id,
                shell_state,
                execution_state,
                result.error_code.as_deref(),
                result.already_closed,
            );
    }

    async fn run_runner_persistent_shell(
        &self,
        client_id: &str,
        request: PersistentShellRequest,
        job_context: Option<ShellJobContext>,
        wait_secs: u64,
    ) -> Result<PersistentShellResult, String> {
        let (request_id, receiver) = self
            .runner_registry
            .enqueue_persistent_shell(
                client_id.to_string(),
                request,
                job_context,
                "tool_runtime".to_string(),
            )
            .await?;
        match tokio::time::timeout(Duration::from_secs(wait_secs), receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => {
                self.runner_registry.cancel_request(&request_id).await;
                Err("persistent_shell_result_lost: Runner result waiter was dropped".to_string())
            }
            Err(_) => {
                let dispatched = self.runner_registry.cancel_request(&request_id).await;
                Err(if dispatched {
                    "persistent_shell_runner_unavailable: result timed out after dispatch; shell state is lost"
                        .to_string()
                } else {
                    "persistent_shell_runner_unavailable: Runner did not accept the request"
                        .to_string()
                })
            }
        }
    }
}

/// Resolve the remote cwd for an SSH persistent shell. Unlike the project
/// paths it is NOT constrained to the project root: it is a remote path that
/// only needs the existing bounded remote-path shape validation. `None` lets
/// the Runner fall back to the resource default_cwd / remote login default.
fn resolve_remote_cwd(cwd: Option<&str>) -> Result<Option<String>, String> {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return Ok(None);
    };
    if cwd.len() > 4096 || cwd.chars().any(char::is_control) {
        return Err("cwd must be a bounded remote path without control characters".to_string());
    }
    Ok(Some(cwd.to_string()))
}

fn persistent_record_to_tool(
    record: SessionShellRecord,
    project: &ProjectConfig,
    action: &str,
) -> ToolResult {
    let result = PersistentShellResult {
        shell_id: record.shell_id,
        workflow_session_id: record.workflow_session_id.clone(),
        runtime_project_id: record.runtime_project_id.clone(),
        shell_state: record.state,
        execution_state: action.to_string(),
        command_started: false,
        command_completed: false,
        exit_code: record.exit_code,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        duration_ms: 0,
        cwd: Some(record.cwd),
        initial_cwd: Some(record.initial_cwd),
        shell: Some(record.shell),
        profile: record.profile,
        created_at: Some(record.created_at),
        last_activity_at: Some(record.last_activity_at),
        busy: record.busy.load(Ordering::SeqCst),
        already_closed: action == "close",
        close_reason: record.close_reason,
        error_code: None,
        error: None,
    };
    persistent_result_to_tool(
        result,
        project,
        &record.runtime_project_id,
        &record.workflow_session_id,
        action,
        record.executor.as_str(),
        record.resource.as_deref(),
        None,
    )
}

fn persistent_result_to_tool(
    mut result: PersistentShellResult,
    project: &ProjectConfig,
    runtime_project_id: &str,
    session_id: &str,
    action: &str,
    executor: &str,
    resource: Option<&str>,
    purpose: Option<ExecutionPurpose>,
) -> ToolResult {
    normalize_persistent_result_state(&mut result);
    let cwd = relative_cwd(project, result.cwd.as_deref());
    let initial_cwd = relative_cwd(project, result.initial_cwd.as_deref());
    let command_ok = result.command_completed && result.exit_code == Some(0);
    let output = json!({
        "shell_id": result.shell_id,
        "project": runtime_project_id,
        "session_id": session_id,
        "executor": executor,
        "resource": resource,
        "shell": result.shell,
        "profile": result.profile,
        "initial_cwd": initial_cwd,
        "cwd": cwd,
        "created_at": result.created_at,
        "last_activity_at": result.last_activity_at,
        "shell_state": result.shell_state,
        "execution_state": result.execution_state,
        "command_started": result.command_started,
        "command_completed": result.command_completed,
        "command_ok": command_ok,
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "stdout_truncated": result.stdout_truncated,
        "stderr_truncated": result.stderr_truncated,
        "duration_ms": result.duration_ms,
        "busy": result.busy,
        "already_closed": result.already_closed,
        "close_reason": result.close_reason,
        "purpose": purpose.map(|purpose| purpose.as_str()),
        "error_code": result.error_code,
        "tool_failure": result.error_code.is_some(),
    });
    if let Some(error) = result.error {
        return ToolResult::err_with_output(error, output);
    }
    if action == "exec" && !command_ok {
        return ToolResult::err_with_output(
            format!(
                "persistent shell command exited with {}",
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown status".to_string())
            ),
            output,
        );
    }
    ToolResult::ok(output)
}

fn relative_cwd(project: &ProjectConfig, cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    project_relative_runner_cwd(project, cwd).ok()
}

fn shell_tool_error_from_message(message: String, shell_id: Option<&str>) -> ToolResult {
    let (code, detail) = split_shell_error(&message);
    shell_tool_error(code, detail, shell_id)
}

fn shell_tool_lost_error_from_message(message: String, shell_id: Option<&str>) -> ToolResult {
    let (code, detail) = split_shell_error(&message);
    shell_tool_error_with_state(code, detail, shell_id, "lost", "lost")
}

fn split_shell_error(message: &str) -> (&str, &str) {
    let Some((code, detail)) = message.split_once(':') else {
        return ("persistent_shell_runtime_error", message);
    };
    let code = code.trim();
    if code.is_empty()
        || code.len() > 80
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        ("persistent_shell_runtime_error", message)
    } else {
        (code, detail.trim())
    }
}

fn shell_tool_error(code: &str, message: impl Into<String>, shell_id: Option<&str>) -> ToolResult {
    let shell_state = if code == "shell_reset_required" {
        "poisoned"
    } else {
        "unknown"
    };
    shell_tool_error_with_state(code, message, shell_id, shell_state, "rejected")
}

fn shell_tool_error_with_state(
    code: &str,
    message: impl Into<String>,
    shell_id: Option<&str>,
    shell_state: &str,
    execution_state: &str,
) -> ToolResult {
    let message = message.into();
    ToolResult::err_with_output(
        format!("{code}: {message}"),
        json!({
            "error_code": code,
            "shell_id": shell_id,
            "command_started": false,
            "command_completed": false,
            "shell_state": shell_state,
            "execution_state": execution_state,
            "tool_failure": true,
        }),
    )
}

fn normalize_persistent_result_state(result: &mut PersistentShellResult) {
    if result.shell_state == "unknown" && result.error_code.is_some() {
        result.shell_state = "lost".to_string();
        result.execution_state = "lost".to_string();
    }
}

fn normalized_result_states(result: &PersistentShellResult) -> (&str, &str) {
    if result.shell_state == "unknown" && result.error_code.is_some() {
        ("lost", "lost")
    } else {
        (&result.shell_state, &result.execution_state)
    }
}

fn shell_error_code(message: &str) -> &str {
    split_shell_error(message).0
}
