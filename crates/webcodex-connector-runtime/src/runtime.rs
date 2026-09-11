//! Durable, transport-neutral Connector task and execution runtime.

use crate::context::ConnectorContext;
use crate::execution;
use crate::projections::{
    approval_gate_outcome, bounded_goal, check_request_hash, checks_stale_outcome,
    command_action_hash, command_request_hash, connector_window_binding, context_refresh_payload,
    durable_task_review_projection, edit_operation_hash, host_review_projection, invalid_input,
    kernel_failure_may_have_applied, model_next_action, navigation_payload, paginate_search_output,
    parse_input, parse_search_cursor, project_brief, project_brief_from_fingerprint,
    search_cursor_signature, short_oid, store_error_outcome, validate_operation_id, validate_path,
    validate_task_id, validation_projection, validation_recipe_error, KernelFailure,
    DEFAULT_TASK_LIST_LIMIT, MAX_TASK_LIST_LIMIT,
};
use crate::surface;
use crate::wire_models::{
    sanitize_value, ChecksRunInput, CodeImpactInput, CodeNavigateInput, CodeNavigateOperation,
    CommandsRunInput, EditsApplyInput, FilesListInput, FilesReadInput, FilesSearchInput,
    SearchResultMode, TaskCancelInput, TaskFinishInput, TaskListInput, TaskResumeInput,
    TaskReviewInput, TaskStartInput,
};
use crate::workspace::{LocalResultDecision, PreparedWorkspace, WorkspaceManager};
use crate::{
    ConnectorCallContext, ConnectorCallOutcome, ConnectorJobHostError, ConnectorPermission,
    ConnectorProjectRegistration, ConnectorRecipeId, ConnectorSemanticCheck, ConnectorToolFailure,
    ConnectorToolRequest, ConnectorTransport, ConnectorWindowId,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use webcodex_core::lsp_bridge::{
    redact_absolute_paths, MAX_DOCUMENT_DIAGNOSTICS_LIMIT, MAX_DOCUMENT_SYMBOLS_LIMIT,
    MAX_FIND_REFERENCES_LIMIT, MAX_GOTO_DEFINITION_LIMIT, MAX_WORKSPACE_SYMBOLS_LIMIT,
};
use webcodex_core::runner_protocol::{
    RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON, RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV,
};
use webcodex_runner_registry::{command_preview, RunnerRegistry};
use webcodex_store::{
    ConnectorApprovalGate, ConnectorBinding, ConnectorEditOperationGate, ConnectorExecution,
    ConnectorExecutionReservation, ConnectorExecutionState, ConnectorResultDecisionStatus,
    ConnectorRunState, ConnectorTaskContinuation, ConnectorTaskMode, ConnectorTaskResult,
    ConnectorTaskSnapshot, ConnectorTaskState, ConnectorTaskStoreError,
    ConnectorWorkspaceTransition, Database, NewConnectorResult, NewConnectorTask,
};
use webcodex_validation::{resolve_validation_recipe, RecipeId, SemanticCheck};
use webcodex_workspace::project_context::{
    capture_project_context, compare_project_context, ContextRefreshSummary,
    ProjectContextFingerprint,
};

const MAX_EVENT_COUNT: usize = 50;
const MAX_GUIDANCE_PER_RESPONSE: usize = 16;
const MAX_REVIEW_APPLIED_PATHS: usize = 200;
const COMMAND_APPROVAL_TTL_SECS: i64 = 60 * 60;
const CONNECTOR_PATCH_PREVIEW_BYTES: usize = 128 * 1024;
const CONNECTOR_SEARCH_WINDOW: usize = crate::projections::CONNECTOR_SEARCH_WINDOW;

fn validation_recipe_id(recipe: ConnectorRecipeId) -> RecipeId {
    match recipe {
        ConnectorRecipeId::Rust => RecipeId::Rust,
        ConnectorRecipeId::Node => RecipeId::Node,
        ConnectorRecipeId::Python => RecipeId::Python,
        ConnectorRecipeId::Go => RecipeId::Go,
    }
}

fn validation_semantic_check(check: ConnectorSemanticCheck) -> SemanticCheck {
    match check {
        ConnectorSemanticCheck::Format => SemanticCheck::Format,
        ConnectorSemanticCheck::Check => SemanticCheck::Check,
        ConnectorSemanticCheck::Test => SemanticCheck::Test,
    }
}
#[cfg(test)]
type FinishTestHook = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

pub struct ConnectorRuntime {
    runner_registry: Arc<RunnerRegistry>,
    pub(crate) db: Arc<Database>,
    context: ConnectorContext,
    workspace: crate::workspace::WorkspaceManager,
    executions: execution::ExecutionService,
    workspace_ops: tokio::sync::Mutex<()>,
    task_locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    context_locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    #[cfg(test)]
    finish_after_fingerprint: StdMutex<Option<FinishTestHook>>,
    #[cfg(test)]
    mutation_before_task_lock: StdMutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl ConnectorRuntime {
    pub fn new(
        runner_registry: Arc<RunnerRegistry>,
        db: Arc<Database>,
        context: ConnectorContext,
    ) -> Result<Self, String> {
        context.validate()?;
        let workspace = WorkspaceManager::new(&context)?;
        WorkspaceManager::recover_result_decisions(
            &db,
            &context.project_id,
            Path::new(&context.executor_root),
            chrono::Utc::now().timestamp(),
        )
        .map_err(|error| format!("failed to recover local result decision: {error}"))?;
        let executions = execution::ExecutionService::new(
            runner_registry.clone(),
            db.clone(),
            workspace.clone(),
        );
        let (runs_recovered, executions_recovered) = executions
            .reconcile_startup(&context.project_id, chrono::Utc::now().timestamp())
            .map_err(|error| format!("failed to recover connector runs: {error}"))?;
        if runs_recovered > 0 || executions_recovered > 0 {
            tracing::warn!(
                project_id = %context.project_id,
                runs = runs_recovered,
                executions = executions_recovered,
                "Recovered unfinished connector executions as interrupted"
            );
        }
        let preserved = db
            .connector_preserved_workspaces(&context.project_id)
            .map_err(|error| format!("failed to inspect connector workspaces: {error}"))?;
        for warning in workspace.recover(&context, &preserved) {
            tracing::warn!(project_id = %context.project_id, warning = %warning, "Connector workspace recovery was incomplete");
        }
        Ok(Self {
            runner_registry,
            db,
            context,
            workspace,
            executions,
            workspace_ops: tokio::sync::Mutex::new(()),
            task_locks: StdMutex::new(HashMap::new()),
            context_locks: StdMutex::new(HashMap::new()),
            #[cfg(test)]
            finish_after_fingerprint: StdMutex::new(None),
            #[cfg(test)]
            mutation_before_task_lock: StdMutex::new(None),
        })
    }

    pub fn context(&self) -> &ConnectorContext {
        &self.context
    }

    fn project_access_allowed(&self, auth: &ConnectorCallContext) -> bool {
        auth.access
            .project_access_allowed(&self.context.project_grant_id)
    }

    /// Build the readiness projection and record the probe outcome as the
    /// connector endpoint observation (real activity, not config inference).

    pub async fn host_review(
        &self,
        auth: &ConnectorCallContext,
        input: TaskReviewInput,
    ) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(&input.task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        let mut outcome = self
            .task_review(
                json!(input),
                &task.owner_subject_id,
                auth,
                ConnectorTransport::Api,
                false,
            )
            .await;
        if outcome.ok {
            // Read-only guidance read-state for the console timeline: the
            // watermark the model has claimed, and the newest still-pending
            // guidance. This never advances the watermark — opening the host
            // review page must not consume guidance the model has yet to read.
            let read_state = self
                .db
                .connector_guidance_read_state(&input.task_id, &self.context.project_id)
                .unwrap_or(None);
            outcome.body = host_review_projection(&outcome.body, read_state);
        }
        outcome
    }

    pub async fn host_cancel(
        &self,
        auth: &ConnectorCallContext,
        input: TaskCancelInput,
    ) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(&input.task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        self.task_cancel(json!(input), &task.owner_subject_id, auth)
            .await
    }

    pub fn host_decide(
        &self,
        task_id: &str,
        result_id: Option<&str>,
        decision: LocalResultDecision,
        reason: Option<&str>,
        now: i64,
    ) -> Result<ConnectorTaskResult, ConnectorTaskStoreError> {
        WorkspaceManager::decide_connector_result_local(
            &self.db,
            &self.context.project_id,
            task_id,
            result_id,
            Path::new(&self.context.executor_root),
            decision,
            "local_console",
            reason,
            now,
        )
    }

    fn execution_task_not_found() -> ConnectorCallOutcome {
        store_error_outcome(ConnectorTaskStoreError::NotFound, None)
    }

    fn execution_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(ConnectorTaskSnapshot, ConnectorExecution), ConnectorCallOutcome> {
        if !auth.access.allows(ConnectorPermission::JobRun) {
            return Err(ConnectorCallOutcome::scope_denied(
                ConnectorPermission::JobRun,
            ));
        }
        if !self.project_access_allowed(auth) {
            return Err(Self::execution_task_not_found());
        }
        let subject_id = auth.access.principal.as_str();
        self.db
            .connector_execution_for_subject(execution_id, &self.context.project_id, subject_id)
            .map_err(|error| match error {
                ConnectorTaskStoreError::NotFound => Self::execution_task_not_found(),
                other => store_error_outcome(other, None),
            })
    }

    fn execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(ConnectorTaskSnapshot, ConnectorExecution), ConnectorCallOutcome> {
        let (task, execution) = self.execution_for_auth(execution_id, auth)?;
        if !execution.mcp_task_is_materialized() {
            return Err(Self::execution_task_not_found());
        }
        Ok((task, execution))
    }

    pub async fn ordinary_execution_result_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<ConnectorCallOutcome, ConnectorCallOutcome> {
        let (mut task, execution) = self.execution_for_auth(execution_id, auth)?;
        task.run_id = execution.run_id.clone();
        let runner_access = auth.access.runner_access.clone();
        let projection = self
            .executions
            .projection(&execution, &runner_access, true)
            .await;
        let mut data = json!({ "execution": projection });
        self.attach_pending_guidance(&task, &mut data);
        Ok(ConnectorCallOutcome::success_blocking_at(
            &task,
            task.event_cursor,
            data,
            execution.blocks_finish(),
        ))
    }

    pub fn materialize_execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<ConnectorExecution, ConnectorCallOutcome> {
        if !auth.access.allows(ConnectorPermission::JobRun) {
            return Err(ConnectorCallOutcome::scope_denied(
                ConnectorPermission::JobRun,
            ));
        }
        if !self.project_access_allowed(auth) {
            return Err(Self::execution_task_not_found());
        }
        let subject_id = auth.access.principal.as_str();
        self.db
            .materialize_connector_execution_mcp_task_for_subject(
                execution_id,
                &self.context.project_id,
                subject_id,
                chrono::Utc::now().timestamp(),
            )
            .map_err(|error| match error {
                ConnectorTaskStoreError::NotFound => Self::execution_task_not_found(),
                other => store_error_outcome(other, None),
            })
    }

    pub async fn execution_task_result_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<
        (
            ConnectorTaskSnapshot,
            ConnectorExecution,
            ConnectorCallOutcome,
        ),
        ConnectorCallOutcome,
    > {
        let (mut task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() && !execution.mcp_task_result_is_finalized() {
            return Err(store_error_outcome(
                ConnectorTaskStoreError::InvalidState(
                    "materialized MCP task is terminal before its durable result was finalized"
                        .to_string(),
                ),
                Some(&task),
            ));
        }
        // A Connector task may have been resumed into a later run. MCP task
        // identity is the exact durable execution, so never project a newer
        // run id onto an older execution handle.
        task.run_id = execution.run_id.clone();
        let execution_event_cursor = self
            .db
            .connector_execution_event_cursor(&execution)
            .map_err(|error| store_error_outcome(error, Some(&task)))?;
        let projection = self.executions.durable_task_projection(&execution);
        let outcome = ConnectorCallOutcome::success_blocking_at(
            &task,
            execution_event_cursor,
            json!({ "execution": projection }),
            execution.blocks_finish(),
        );
        Ok((task, execution, outcome))
    }

    pub async fn cancel_execution_task_for_auth(
        &self,
        execution_id: &str,
        auth: &ConnectorCallContext,
    ) -> Result<(), ConnectorCallOutcome> {
        let (task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() {
            return Ok(());
        }
        let task_lock = self.task_lock(&task.task_id);
        let _task_guard = task_lock.lock().await;
        let (task, execution) = self.execution_task_for_auth(execution_id, auth)?;
        if execution.is_terminal() {
            return Ok(());
        }
        if task.run_id != execution.run_id {
            return Err(Self::execution_task_not_found());
        }
        let current = self
            .db
            .latest_connector_execution(
                &task.task_id,
                &self.context.project_id,
                &task.owner_subject_id,
                None,
            )
            .map_err(|error| store_error_outcome(error, Some(&task)))?;
        if current
            .as_ref()
            .is_none_or(|current| current.execution_id != execution.execution_id)
        {
            return Err(Self::execution_task_not_found());
        }
        let host = auth.host.clone();
        let runner_access = auth.access.runner_access.clone();
        self.executions
            .cancel_task(task.clone(), None, host, runner_access)
            .await
            .map(|_| ())
            .map_err(|error| store_error_outcome(error, Some(&task)))
    }

    pub async fn call_for_window(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
    ) -> ConnectorCallOutcome {
        self.call_for_window_inner(capability, arguments, auth, transport, window, false)
            .await
    }

    pub async fn call_for_window_with_task_polling(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
    ) -> ConnectorCallOutcome {
        self.call_for_window_inner(capability, arguments, auth, transport, window, true)
            .await
    }

    async fn call_for_window_inner(
        &self,
        capability: &str,
        arguments: Value,
        auth: Option<&ConnectorCallContext>,
        transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        if surface::capability_spec(capability).is_none() {
            return ConnectorCallOutcome::error(
                400,
                "unknown_capability",
                format!(
                    "'{capability}' is not available in the project connector; use one of: {}",
                    surface::CAPABILITY_NAMES.join(", ")
                ),
                false,
                false,
                Some("Call task_start first, then use the returned task_id."),
                None,
                true,
            );
        }

        let Some(auth) = auth else {
            return ConnectorCallOutcome::error(
                401,
                "authentication_required",
                "connector capabilities require an authenticated identity",
                false,
                true,
                Some("Configure Bearer authentication in the connector client."),
                None,
                false,
            );
        };
        let access = &auth.access;
        if !access.project_access_allowed(&self.context.project_grant_id) {
            return ConnectorCallOutcome::error(
                403,
                "project_credential_rejected",
                "the authenticated credential is not authorized for this project",
                false,
                true,
                Some("Use the credential generated by setup for this project."),
                None,
                false,
            );
        }
        let required_permission = ConnectorPermission::for_capability(capability);
        if !access.allows(required_permission) {
            return ConnectorCallOutcome::scope_denied(required_permission);
        }
        let subject_id = access.principal.as_str().to_string();

        let now = chrono::Utc::now().timestamp();
        if let Err(error) = self.db.ensure_connector_binding(ConnectorBinding {
            project_id: &self.context.project_id,
            project_name: &self.context.project_name,
            workspace_id: &self.context.workspace_id,
            executor_ref: &self.context.executor_project,
            subject_id: &subject_id,
            profile: &self.context.profile,
            now,
        }) {
            return store_error_outcome(error, None);
        }

        // Read operations coordinate with lifecycle transitions, while every
        // mutation/reservation method owns its narrower task-lock boundary.
        let task_lock = if matches!(
            capability,
            "files_read" | "files_search" | "code_navigate" | "code_impact"
        ) {
            arguments
                .get("task_id")
                .and_then(Value::as_str)
                .map(|task_id| self.task_lock(task_id))
        } else {
            None
        };
        let _task_guard = match task_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let outcome = match capability {
            "task_start" => {
                self.task_start(arguments, &subject_id, auth, transport, window, now)
                    .await
            }
            "task_list" => self.task_list(arguments, &subject_id).await,
            "task_resume" => self.task_resume(arguments, &subject_id, window, now).await,
            "files_list" => {
                self.files_list(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "files_read" => {
                self.files_read(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "files_search" => {
                self.files_search(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "code_navigate" => {
                self.code_navigate(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "code_impact" => {
                self.code_impact(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "edits_apply" => {
                self.edits_apply(arguments, &subject_id, auth, transport, now)
                    .await
            }
            "checks_run" => {
                self.checks_run(
                    arguments,
                    &subject_id,
                    auth,
                    transport,
                    now,
                    defer_execution_guidance,
                )
                .await
            }
            "commands_run" => {
                self.commands_run(
                    arguments,
                    &subject_id,
                    auth,
                    transport,
                    now,
                    defer_execution_guidance,
                )
                .await
            }
            "task_review" => {
                self.task_review(arguments, &subject_id, auth, transport, true)
                    .await
            }
            "task_cancel" => self.task_cancel(arguments, &subject_id, auth).await,
            "task_finish" => {
                self.task_finish(arguments, &subject_id, auth, transport, now)
                    .await
            }
            _ => unreachable!("capability registry checked before dispatch"),
        };
        outcome
    }

    fn task_lock(&self, task_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.task_locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(task_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(task_id.to_string(), Arc::downgrade(&lock));
        lock
    }

    fn context_lock(&self, subject_id: &str, window_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = format!("{subject_id}:{window_key}");
        let mut locks = self.context_locks.lock().unwrap();
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn workspace_fingerprint(
        &self,
        task: &ConnectorTaskSnapshot,
        capability: &'static str,
    ) -> Result<String, ConnectorCallOutcome> {
        let manager = self.workspace.clone();
        let task_for_fingerprint = task.clone();
        match tokio::task::spawn_blocking(move || {
            manager.action_precondition(&task_for_fingerprint)
        })
        .await
        {
            Ok(Ok(fingerprint)) => Ok(fingerprint),
            Ok(Err(message)) => Err(ConnectorCallOutcome::error_for_task(
                409,
                "workspace_fingerprint_failed",
                self.sanitize_task_string(task, &message),
                false,
                true,
                Some("Resolve the Git workspace issue, then retry the operation."),
                task,
                Value::Null,
            )),
            Err(error) => {
                tracing::error!(error = %error, capability, "connector workspace fingerprint task failed");
                Err(ConnectorCallOutcome::error_for_task(
                    500,
                    "workspace_fingerprint_failed",
                    "connector could not fingerprint the current workspace",
                    false,
                    true,
                    Some("Inspect server logs before retrying the operation."),
                    task,
                    Value::Null,
                ))
            }
        }
    }

    async fn task_start(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        window: Option<&ConnectorWindowId>,
        now: i64,
    ) -> ConnectorCallOutcome {
        if arguments.get("mode").and_then(Value::as_str) == Some("inspect") {
            return ConnectorCallOutcome::error(
                400,
                "inspect_mode_retired",
                "inspect mode was retired before v0.4 and is no longer executable",
                false,
                true,
                Some("Use read_only for analysis, or normal for writable work, command execution, and validation."),
                None,
                true,
            );
        }
        let input: TaskStartInput = match parse_input("task_start", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let goal = input.goal.trim();
        if goal.is_empty() || goal.len() > 4000 {
            return invalid_input("task_start", "goal must be 1..=4000 bytes");
        }
        let mode = input.mode.as_str();
        if mode == "normal" && !auth.access.allows(ConnectorPermission::ProjectWrite) {
            return ConnectorCallOutcome::scope_denied(ConnectorPermission::ProjectWrite);
        }

        let normalized_target =
            match webcodex_workspace::project_overview::normalize_project_overview_path(
                input.target_path.as_deref().unwrap_or(""),
            ) {
                Ok(path) => path,
                Err(message) => return invalid_input("task_start", message),
            };
        let fingerprint = match self
            .capture_connector_context(normalized_target.clone())
            .await
        {
            Ok(fingerprint) => fingerprint,
            Err(outcome) => return outcome,
        };

        // Serialize get-or-create for one stable window/project. Without this,
        // two simultaneous first turns could both observe no mapping and
        // create duplicate durable tasks.
        let context_lock = window.map(|window| self.context_lock(subject_id, window.key()));
        let _context_guard = match context_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let project_identity = format!(
            "{}:{}",
            self.context.project_id, fingerprint.project_root_sha256
        );
        let existing_context = if let Some(window) = window {
            match self.db.connector_window_context(
                window.key(),
                &self.context.project_id,
                subject_id,
                &fingerprint.project_root_sha256,
            ) {
                Ok(context) => context,
                Err(error) => return store_error_outcome(error, None),
            }
        } else {
            None
        };

        if let Some(existing_context) = existing_context.as_ref() {
            let existing = match self.db.connector_task(
                &existing_context.task_id,
                &self.context.project_id,
                subject_id,
            ) {
                Ok(task) => Some(task),
                Err(ConnectorTaskStoreError::NotFound) => None,
                Err(error) => return store_error_outcome(error, None),
            };
            if let Some(task) = existing {
                if task.mode == ConnectorTaskMode::InspectLegacy {
                    return Self::retired_inspect_task_outcome(&task);
                }
                if let Some(outcome) = Self::invalid_mode_transition_outcome(&task, mode) {
                    return outcome;
                }
                let refresh =
                    compare_project_context(Some(&existing_context.fingerprint), &fingerprint);
                if task.task_status == ConnectorTaskState::Active
                    && task.run_status == ConnectorRunState::Running
                {
                    return self
                        .continue_window_task(
                            task,
                            goal,
                            mode,
                            auth,
                            window.expect("existing window context has a window"),
                            &fingerprint,
                            &refresh,
                            now,
                        )
                        .await;
                }
                if task.run_status == ConnectorRunState::Interrupted
                    && task.task_status == ConnectorTaskState::NeedsAttention
                {
                    let window = window.expect("existing window context has a window");
                    let cursor = match self.db.append_interrupted_connector_instruction_and_bind(
                        &task.task_id,
                        &self.context.project_id,
                        subject_id,
                        goal,
                        mode,
                        now,
                        connector_window_binding(&window.clone(), &fingerprint, now),
                    ) {
                        Ok(cursor) => cursor,
                        Err(error) => return store_error_outcome(error, Some(&task)),
                    };
                    let navigation = self.db.activate_window_project(
                        subject_id,
                        window.key(),
                        &project_identity,
                    );
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "task_interrupted",
                        "the previous project context was recovered, but its execution was interrupted and cannot be resumed by a chat request",
                        false,
                        true,
                        Some("Review the task, then resume or reject it from the WebCodex host."),
                        &task,
                        cursor,
                        json!({
                            "continuation": "recovered",
                            "instruction_appended": true,
                            "context": context_refresh_payload(&refresh),
                            "project_switch": navigation_payload(Some(&navigation), true),
                            "local_command": format!("webcodex task resume {}", task.task_id)
                        }),
                    );
                }
                // A reviewed/closed task remains durable history. The mapping
                // may advance to a new task without deleting the old row.
            }
        }

        let task_id = format!("wc_task_{}", uuid::Uuid::new_v4().simple());
        let run_id = format!("wc_run_{}", uuid::Uuid::new_v4().simple());
        let non_writable = mode != "normal";
        let prepared = match self
            .prepare_connector_workspace(&task_id, &run_id, non_writable, auth)
            .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let new_task = NewConnectorTask {
            task_id: &task_id,
            run_id: &run_id,
            project_id: &self.context.project_id,
            workspace_id: &self.context.workspace_id,
            subject_id,
            goal,
            mode,
            target_executor_ref: &self.context.executor_project,
            execution_executor_ref: &prepared.execution_executor_ref,
            target_root: &self.context.executor_root,
            execution_root: &prepared.execution_root,
            baseline_commit: prepared.baseline_commit.as_deref(),
            baseline_tree: prepared.baseline_tree.as_deref(),
            isolated: prepared.isolated,
            now,
        };
        let stored = match window {
            Some(window) => {
                let window = window.clone();
                self.db.start_connector_task_and_bind(
                    new_task,
                    connector_window_binding(&window, &fingerprint, now),
                )
            }
            None => self.db.start_connector_task(new_task),
        };
        let task = match stored {
            Ok(task) => task,
            Err(error) => {
                if let Some(cleanup) = self
                    .workspace
                    .discard_prepared(&self.context.executor_root, &prepared)
                {
                    tracing::warn!(cleanup = %cleanup, "failed to fully clean unpersisted workspace");
                }
                return store_error_outcome(error, None);
            }
        };
        let navigation = window.map(|window| {
            self.db
                .activate_window_project(subject_id, window.key(), &project_identity)
        });
        let brief = project_brief(
            &task,
            prepared.project_overview.as_ref(),
            prepared.git_dirty,
            prepared.git_conflict_count,
        );
        ConnectorCallOutcome::success(
            &task,
            json!({
                "project": {
                    "id": self.context.project_id,
                    "name": self.context.project_name
                },
                "goal": goal,
                "mode": mode,
                "status": task.task_status,
                "continuation": "created",
                "instruction_appended": true,
                "history": {
                    "preserved": true,
                    "event_cursor_before": 0,
                    "event_cursor_after": task.event_cursor
                },
                "context": context_refresh_payload(&compare_project_context(None, &fingerprint)),
                "project_switch": navigation_payload(navigation.as_ref(), false),
                "brief": brief,
                "next": "Use the brief to choose the first targeted read; edit with returned sha256 guards, validate, review, and finish."
            }),
        )
    }

    async fn capture_connector_context(
        &self,
        target_path: String,
    ) -> Result<ProjectContextFingerprint, ConnectorCallOutcome> {
        let root = self.context.executor_root.clone();
        match tokio::task::spawn_blocking(move || {
            capture_project_context(Path::new(&root), Some(&target_path))
        })
        .await
        {
            Ok(Ok(fingerprint)) => Ok(fingerprint),
            Ok(Err(message)) => Err(ConnectorCallOutcome::error(
                409,
                "project_context_unavailable",
                self.sanitize_executor_string(&message),
                false,
                true,
                Some("Resolve the repository path or Git state, then retry the instruction."),
                None,
                false,
            )),
            Err(error) => {
                tracing::error!(error = %error, "connector context fingerprint task failed");
                Err(ConnectorCallOutcome::error(
                    500,
                    "project_context_unavailable",
                    "connector could not fingerprint the project context",
                    false,
                    true,
                    Some("Inspect server logs before retrying the instruction."),
                    None,
                    false,
                ))
            }
        }
    }

    async fn prepare_connector_workspace(
        &self,
        task_id: &str,
        run_id: &str,
        non_writable: bool,
        auth: &ConnectorCallContext,
    ) -> Result<PreparedWorkspace, ConnectorCallOutcome> {
        let _workspace_guard = if non_writable {
            None
        } else {
            Some(self.workspace_ops.lock().await)
        };
        let manager = self.workspace.clone();
        let context = self.context.clone();
        let task_for_prepare = task_id.to_string();
        let run_for_prepare = run_id.to_string();
        let prepared = match tokio::task::spawn_blocking(move || {
            manager.prepare(&context, &task_for_prepare, &run_for_prepare, non_writable)
        })
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                let guidance = if error.reason_code == "writable_slot_occupied" {
                    "Finish, resume, or reject the task occupying the writable slot."
                } else {
                    "Resolve the reported Git/private-state issue; normal mode never falls back to the target checkout."
                };
                return Err(ConnectorCallOutcome::error_with_data(
                    409,
                    "workspace_preparation_failed",
                    error.message,
                    false,
                    true,
                    Some(guidance),
                    json!({
                        "stage": error.stage,
                        "reason_code": error.reason_code,
                    }),
                    None,
                    false,
                ));
            }
            Err(error) => {
                tracing::error!(error = %error, "connector workspace preparation task failed");
                return Err(ConnectorCallOutcome::error(
                    500,
                    "workspace_preparation_failed",
                    "connector could not prepare the isolated execution workspace",
                    false,
                    true,
                    Some("Inspect server logs, then retry the instruction."),
                    None,
                    false,
                ));
            }
        };
        if prepared.isolated {
            let host = auth.host.clone();
            let registration = host
                .register_isolated_project(ConnectorProjectRegistration {
                    client_id: prepared.agent_client_id.clone(),
                    project_id: prepared.agent_project_id.clone(),
                    name: format!("WebCodex {}", prepared.agent_project_id),
                    path: prepared.execution_root.clone(),
                    description: Some("WebCodex managed isolated task worktree".to_string()),
                })
                .await;
            if let Err(registration_error) = registration {
                let cleanup = self
                    .workspace
                    .discard_prepared(&self.context.executor_root, &prepared);
                let registration_message = match registration_error {
                    ConnectorJobHostError::Rejected(message)
                    | ConnectorJobHostError::OutcomeUnknown(message) => message,
                    ConnectorJobHostError::Adapter(message) => Some(message),
                };
                if let Some(error) = registration_message.as_deref() {
                    tracing::warn!(
                        error = %self.sanitize_executor_string(error),
                        "temporary Runner project registration failed"
                    );
                }
                if let Some(cleanup) = cleanup {
                    tracing::warn!(cleanup = %cleanup, "failed to fully clean rejected workspace preparation");
                }
                return Err(ConnectorCallOutcome::error_with_data(
                    409,
                    "workspace_preparation_failed",
                    "the isolated writable workspace could not be registered with the Runner",
                    false,
                    true,
                    Some("Resolve the Runner project-registration policy, then retry; the target checkout was not used as a writable fallback."),
                    json!({
                        "stage": "runner_project_registration",
                        "reason_code": "runner_project_registration_failed",
                    }),
                    None,
                    false,
                ));
            }
        }
        Ok(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    async fn continue_window_task(
        &self,
        task: ConnectorTaskSnapshot,
        instruction: &str,
        mode: &str,
        auth: &ConnectorCallContext,
        window: &ConnectorWindowId,
        fingerprint: &ProjectContextFingerprint,
        refresh: &ContextRefreshSummary,
        now: i64,
    ) -> ConnectorCallOutcome {
        let event_cursor_before = task.event_cursor;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&task);
        }
        if let Some(outcome) = Self::invalid_mode_transition_outcome(&task, mode) {
            return outcome;
        }
        let prepared = if mode == "normal" && !task.isolated {
            match self
                .prepare_connector_workspace(&task.task_id, &task.run_id, false, auth)
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(outcome) => return outcome,
            }
        } else {
            None
        };
        let workspace = prepared
            .as_ref()
            .map(|prepared| ConnectorWorkspaceTransition {
                target_executor_ref: &self.context.executor_project,
                execution_executor_ref: &prepared.execution_executor_ref,
                target_root: &self.context.executor_root,
                execution_root: &prepared.execution_root,
                baseline_commit: prepared.baseline_commit.as_deref().unwrap_or_default(),
                baseline_tree: prepared.baseline_tree.as_deref().unwrap_or_default(),
            });
        let (continued, cursor, previous_mode) = match self.db.continue_connector_task_and_bind(
            ConnectorTaskContinuation {
                task_id: &task.task_id,
                project_id: &self.context.project_id,
                subject_id: &task.owner_subject_id,
                instruction,
                mode,
                workspace,
                now,
            },
            connector_window_binding(&window.clone(), fingerprint, now),
        ) {
            Ok(continued) => continued,
            Err(error) => {
                if let Some(prepared) = prepared.as_ref() {
                    if let Some(cleanup) = self
                        .workspace
                        .discard_prepared(&self.context.executor_root, prepared)
                    {
                        tracing::warn!(cleanup = %cleanup, "failed to fully clean rejected workspace upgrade");
                    }
                }
                return store_error_outcome(error, Some(&task));
            }
        };
        let navigation = self.db.activate_window_project(
            &continued.owner_subject_id,
            window.key(),
            &format!(
                "{}:{}",
                self.context.project_id, fingerprint.project_root_sha256
            ),
        );
        let brief = match prepared.as_ref() {
            Some(prepared) => project_brief(
                &continued,
                prepared.project_overview.as_ref(),
                prepared.git_dirty,
                prepared.git_conflict_count,
            ),
            None => project_brief_from_fingerprint(&continued, fingerprint),
        };
        ConnectorCallOutcome::success_at(
            &continued,
            cursor,
            json!({
                "project": {
                    "id": self.context.project_id,
                    "name": self.context.project_name
                },
                "goal": continued.goal,
                "instruction": instruction,
                "mode": continued.mode,
                "status": continued.task_status,
                "continuation": "continued",
                "instruction_appended": true,
                "history": {
                    "preserved": true,
                    "event_cursor_before": event_cursor_before,
                    "event_cursor_after": cursor
                },
                "capability": {
                    "changed": previous_mode != continued.mode,
                    "previous_mode": previous_mode,
                    "mode": continued.mode,
                    "write_scope_verified": mode == "normal",
                    "workspace_upgraded": prepared.is_some()
                },
                "context": context_refresh_payload(refresh),
                "project_switch": navigation_payload(Some(&navigation), true),
                "brief": brief,
                "next": "Continue from the preserved history; read only context reported as refreshed before editing or validating."
            }),
        )
    }

    fn persist_window_context(
        &self,
        window: &ConnectorWindowId,
        subject_id: &str,
        task_id: &str,
        fingerprint: &ProjectContextFingerprint,
        now: i64,
    ) -> Result<(), ConnectorTaskStoreError> {
        self.db.bind_connector_window_context(
            window.key(),
            window.source(),
            &self.context.project_id,
            subject_id,
            &fingerprint.project_root_sha256,
            task_id,
            &fingerprint.target_directory,
            fingerprint,
            now,
        )
    }

    async fn files_read(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesReadInput = match parse_input("files_read", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.files.is_empty() || input.files.len() > 8 {
            return invalid_input("files_read", "files must contain 1..=8 entries");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };

        let mut results = Vec::with_capacity(input.files.len());
        for file in &input.files {
            if let Err(message) = validate_path(&file.path) {
                return invalid_input("files_read", message);
            }
            if file.limit.is_some_and(|limit| !(1..=500).contains(&limit)) {
                return invalid_input("files_read", "file limit must be 1..=500");
            }
            let args = json!({
                "project": task.execution_executor_ref,
                "items": [{
                    "path": file.path,
                    "start_line": file.start_line,
                    "limit": file.limit.unwrap_or(200)
                }],
                "with_line_numbers": file.with_line_numbers.unwrap_or(true),
                "max_result_bytes": 512 * 1024
            });
            match self
                .invoke_kernel("read_files", args, &task, auth, transport)
                .await
                .and_then(|output| Self::single_batch_item_output("read_files", output))
            {
                Ok(mut output) => {
                    output["path"] = json!(file.path);
                    results.push(output);
                }
                Err(error) => {
                    let cursor = self.record_event(
                        &task,
                        "files_read",
                        json!({ "ok": false, "requested": input.files.len(), "completed": results.len() }),
                        now,
                    );
                    return self.kernel_error_outcome(
                        error,
                        &task,
                        cursor,
                        json!({ "files": results }),
                    );
                }
            }
        }
        let cursor = match self.record_event(
            &task,
            "files_read",
            json!({ "ok": true, "file_count": results.len() }),
            now,
        ) {
            Ok(cursor) => cursor,
            Err(outcome) => return outcome,
        };
        ConnectorCallOutcome::success_at(&task, cursor, json!({ "files": results }))
    }

    /// Discovery: what does this project contain?
    ///
    /// Read-only and available in `read_only` tasks, which have no shell — so
    /// for those this is the only way to learn the project's shape instead of
    /// guessing paths for `files_read`.
    async fn files_list(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesListInput = match parse_input("files_list", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Some(path) = input.path.as_deref() {
            if let Err(message) = validate_path(path) {
                return invalid_input("files_list", message);
            }
        }
        if input.globs.len() > 20 {
            return invalid_input("files_list", "globs are limited to 20 entries");
        }
        if input
            .globs
            .iter()
            .any(|glob| glob.is_empty() || glob.len() > 256)
        {
            return invalid_input("files_list", "each glob must be 1..=256 bytes");
        }
        if input
            .limit
            .is_some_and(|limit| !(1..=1000).contains(&limit))
        {
            return invalid_input("files_list", "limit must be 1..=1000");
        }
        if input.depth.is_some_and(|depth| !(1..=16).contains(&depth)) {
            return invalid_input("files_list", "depth must be 1..=16");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let args = json!({
            "project": task.execution_executor_ref,
            "path": input.path,
            "globs": input.globs,
            "depth": input.depth,
            "limit": input.limit.unwrap_or(200),
            "offset": input.offset.unwrap_or(0),
        });
        match self
            .invoke_kernel("list_project_tracked_files", args, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "files_list",
                    json!({ "ok": true, "returned": output.get("returned").cloned() }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(&task, "files_list", json!({ "ok": false }), now);
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    async fn files_search(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: FilesSearchInput = match parse_input("files_search", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.pattern.trim().is_empty() || input.pattern.len() > 500 {
            return invalid_input("files_search", "pattern must be 1..=500 bytes");
        }
        if let Some(path) = input.path.as_deref() {
            if let Err(message) = validate_path(path) {
                return invalid_input("files_search", message);
            }
        }
        if input.limit.is_some_and(|limit| !(1..=100).contains(&limit)) {
            return invalid_input("files_search", "limit must be 1..=100");
        }
        if input.context_before.unwrap_or(0) > 5 || input.context_after.unwrap_or(0) > 5 {
            return invalid_input("files_search", "search context must be 0..=5 lines");
        }
        if input.include_globs.len() > 20 || input.exclude_globs.len() > 20 {
            return invalid_input(
                "files_search",
                "include/exclude globs are limited to 20 each",
            );
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let page_limit = input.limit.unwrap_or(50);
        let signature = search_cursor_signature(&input, page_limit);
        let offset = match input.cursor.as_deref() {
            Some(cursor) => match parse_search_cursor(cursor, &signature) {
                Ok(offset) if offset < CONNECTOR_SEARCH_WINDOW => offset,
                _ => {
                    return invalid_input(
                        "files_search",
                        "cursor is invalid, belongs to a different query, or exceeds the bounded search window",
                    )
                }
            },
            None => 0,
        };
        let fetch_limit = offset
            .saturating_add(page_limit)
            .min(CONNECTOR_SEARCH_WINDOW);
        let args = json!({
            "project": task.execution_executor_ref,
            "queries": [{
                "pattern": input.pattern,
                "path": input.path,
                "limit": fetch_limit,
                "context_before": input.context_before.unwrap_or(0),
                "context_after": input.context_after.unwrap_or(0),
                "include_globs": input.include_globs,
                "exclude_globs": input.exclude_globs,
                "result_mode": input.result_mode.unwrap_or(SearchResultMode::Matches),
                "timeout_secs": 20
            }],
            "max_result_bytes": 512 * 1024
        });
        match self
            .invoke_kernel("search_project_texts", args, &task, auth, transport)
            .await
            .and_then(|output| Self::single_batch_item_output("search_project_texts", output))
        {
            Ok(output) => {
                let output = paginate_search_output(
                    output,
                    input.result_mode.unwrap_or(SearchResultMode::Matches),
                    offset,
                    page_limit,
                    &signature,
                );
                let cursor = match self.record_event(
                    &task,
                    "files_search",
                    json!({ "ok": true, "offset": offset, "limit": page_limit }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(&task, "files_search", json!({ "ok": false }), now);
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    async fn code_navigate(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        // Preserve field presence before serde maps explicit JSON nulls to None.
        // Operation-specific fields are strict even when an irrelevant field is
        // supplied as null rather than omitted.
        let supplied_fields = arguments.as_object().map(|object| {
            [
                ("path", object.contains_key("path")),
                ("query", object.contains_key("query")),
                ("line", object.contains_key("line")),
                ("column", object.contains_key("column")),
                (
                    "include_declaration",
                    object.contains_key("include_declaration"),
                ),
                ("limit", object.contains_key("limit")),
            ]
        });
        let input: CodeNavigateInput = match parse_input("code_navigate", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let allowed_fields: &[&str] = match input.operation {
            CodeNavigateOperation::Status => &[],
            CodeNavigateOperation::DocumentSymbols => &["path", "limit"],
            CodeNavigateOperation::WorkspaceSymbols => &["query", "limit"],
            CodeNavigateOperation::Definition => &["path", "line", "column", "limit"],
            CodeNavigateOperation::References => {
                &["path", "line", "column", "include_declaration", "limit"]
            }
            CodeNavigateOperation::Diagnostics => &["path", "limit"],
            CodeNavigateOperation::Hover => &["path", "line", "column"],
        };
        if let Some((field, _)) = supplied_fields
            .into_iter()
            .flatten()
            .find(|(field, present)| *present && !allowed_fields.contains(field))
        {
            return invalid_input(
                "code_navigate",
                format!(
                    "{field} is not valid for operation {}",
                    input.operation.as_str()
                ),
            );
        }
        let operation = input.operation.as_str();
        let (tool_name, mut args) = match code_navigation_tool_call(&input) {
            Ok(call) => call,
            Err(message) => return invalid_input("code_navigate", message),
        };
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        args["project"] = json!(task.execution_executor_ref);
        match self
            .invoke_kernel(tool_name, args, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "code_navigate",
                    json!({ "ok": true, "operation": operation }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(
                    &task,
                    "code_navigate",
                    json!({ "ok": false, "operation": operation }),
                    now,
                );
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    async fn code_impact(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: CodeImpactInput = match parse_input("code_impact", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_path(&input.path) {
            return invalid_input("code_impact", message);
        }
        if redact_absolute_paths(&input.path) != input.path {
            return invalid_input("code_impact", "path must be project-relative");
        }
        if input.line < 1 || input.column < 1 {
            return invalid_input("code_impact", "line and column must be >= 1");
        }
        if !(1..=2).contains(&input.depth) {
            return invalid_input("code_impact", "depth must be 1..=2");
        }
        if !(1..=100).contains(&input.limit) {
            return invalid_input("code_impact", "limit must be 1..=100");
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let arguments = json!({
            "project": task.execution_executor_ref,
            "path": input.path,
            "line": input.line,
            "column": input.column,
            "direction": input.direction,
            "depth": input.depth,
            "limit": input.limit,
        });
        match self
            .invoke_kernel("call_hierarchy", arguments, &task, auth, transport)
            .await
        {
            Ok(output) => {
                let cursor = match self.record_event(
                    &task,
                    "code_impact",
                    json!({
                        "ok": true,
                        "direction": input.direction,
                        "depth": input.depth,
                    }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let cursor = self.record_event(
                    &task,
                    "code_impact",
                    json!({
                        "ok": false,
                        "direction": input.direction,
                        "depth": input.depth,
                    }),
                    now,
                );
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    async fn edits_apply(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: EditsApplyInput = match parse_input("edits_apply", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("edits_apply", message);
        }
        if input.changes.is_empty() || input.changes.len() > 16 {
            return invalid_input("edits_apply", "changes must contain 1..=16 entries");
        }
        for change in &input.changes {
            if let Err(message) = validate_path(&change.path) {
                return invalid_input("edits_apply", message);
            }
            if let Some(to_path) = change.to_path.as_deref() {
                if let Err(message) = validate_path(to_path) {
                    return invalid_input("edits_apply", message);
                }
            }
        }
        let change_bytes = serde_json::to_vec(&input.changes)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if change_bytes > 1024 * 1024 {
            return invalid_input("edits_apply", "serialized changes exceed 1 MiB");
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let _task_guard = task_lock.lock().await;
        let task = match self.active_writable_task(&input.task_id, subject_id, "edits_apply", now) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let request_sha256 =
            edit_operation_hash(&task, &input.changes, input.dry_run.unwrap_or(false));
        match self.db.begin_connector_edit_operation(
            &task.task_id,
            &self.context.project_id,
            &task.owner_subject_id,
            &input.operation_id,
            &request_sha256,
            now,
        ) {
            Ok(ConnectorEditOperationGate::Started) => {}
            Ok(ConnectorEditOperationGate::Replay(mut output)) => {
                output["operation_id"] = json!(input.operation_id);
                output["idempotent_replay"] = json!(true);
                let cursor = match self.record_event(
                    &task,
                    "edits_apply",
                    json!({ "ok": true, "replay": true, "operation_id": input.operation_id }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                return ConnectorCallOutcome::success_at(&task, cursor, output);
            }
            Ok(ConnectorEditOperationGate::Pending) => {
                let cursor = self.record_event(
                    &task,
                    "edits_apply",
                    json!({ "ok": false, "operation_pending": true, "operation_id": input.operation_id }),
                    now,
                );
                return ConnectorCallOutcome::error_for_task_at(
                    409,
                    "edit_operation_uncertain",
                    "this operation did not reach a durable result; it will not be replayed automatically",
                    false,
                    true,
                    Some("Inspect task_review and the affected files, then use a new operation_id with fresh hashes only if another edit is needed."),
                    &task,
                    match cursor { Ok(cursor) => cursor, Err(outcome) => return outcome },
                    json!({ "operation_id": input.operation_id }),
                );
            }
            Ok(ConnectorEditOperationGate::Conflict) => {
                return ConnectorCallOutcome::error_for_task(
                    409,
                    "operation_id_conflict",
                    "operation_id was already used with different changes or preconditions",
                    false,
                    false,
                    Some("Use a new operation_id for a logically different edit batch."),
                    &task,
                    json!({ "operation_id": input.operation_id }),
                )
            }
            Err(error) => return store_error_outcome(error, Some(&task)),
        }
        let args = json!({
            "project": task.execution_executor_ref,
            "changes": input.changes,
            "dry_run": input.dry_run.unwrap_or(false)
        });
        match self
            .invoke_kernel("apply_text_edits", args, &task, auth, transport)
            .await
        {
            Ok(mut output) => {
                output["operation_id"] = json!(input.operation_id);
                output["idempotent_replay"] = json!(false);
                if let Err(error) = self.db.complete_connector_edit_operation(
                    &task.task_id,
                    &self.context.project_id,
                    &task.owner_subject_id,
                    &input.operation_id,
                    &request_sha256,
                    &output,
                    now,
                ) {
                    return store_error_outcome(error, Some(&task));
                }
                // Paths are part of the durable event so review surfaces can
                // show what changed without a workspace scan (bounded by the
                // 16-change schema limit).
                let mut changed_paths: Vec<&str> = Vec::new();
                for change in &input.changes {
                    for path in [Some(change.path.as_str()), change.to_path.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        if !changed_paths.contains(&path) {
                            changed_paths.push(path);
                        }
                    }
                }
                let cursor = match self.record_event(
                    &task,
                    "edits_apply",
                    json!({
                        "ok": true,
                        "dry_run": input.dry_run.unwrap_or(false),
                        "operation_id": input.operation_id,
                        "change_count": input.changes.len(),
                        "changed_paths": changed_paths
                    }),
                    now,
                ) {
                    Ok(cursor) => cursor,
                    Err(outcome) => return outcome,
                };
                self.attach_pending_guidance(&task, &mut output);
                ConnectorCallOutcome::success_at(&task, cursor, output)
            }
            Err(error) => {
                let uncertain = kernel_failure_may_have_applied(&error);
                if !uncertain {
                    if let Err(store_error) = self.db.fail_connector_edit_operation(
                        &task.task_id,
                        &input.operation_id,
                        &request_sha256,
                        now,
                    ) {
                        return store_error_outcome(store_error, Some(&task));
                    }
                }
                let cursor = self.record_event(
                    &task,
                    "edits_apply",
                    json!({
                        "ok": false,
                        "dry_run": input.dry_run.unwrap_or(false),
                        "operation_id": input.operation_id,
                        "operation_uncertain": uncertain
                    }),
                    now,
                );
                if uncertain {
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "edit_operation_uncertain",
                        "the edit did not reach a confirmed completed or fully rolled-back state; automatic replay is disabled",
                        false,
                        true,
                        Some("Inspect task_review and affected files before issuing any new edit operation."),
                        &task,
                        match cursor { Ok(cursor) => cursor, Err(outcome) => return outcome },
                        json!({ "operation_id": input.operation_id }),
                    );
                }
                self.kernel_error_outcome(error, &task, cursor, Value::Null)
            }
        }
    }

    async fn checks_run(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: ChecksRunInput = match parse_input("checks_run", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("checks_run", message);
        }
        if input.checks.is_empty() || input.checks.len() > 3 {
            return invalid_input("checks_run", "checks must contain 1..=3 entries");
        }
        let unique = input.checks.iter().copied().collect::<HashSet<_>>();
        if unique.len() != input.checks.len() {
            return invalid_input("checks_run", "checks must not contain duplicates");
        }
        if input
            .timeout_secs
            .is_some_and(|value| !(1..=120).contains(&value))
        {
            return invalid_input("checks_run", "timeout_secs must be 1..=120");
        }
        if let Some(cwd) = input.cwd.as_deref() {
            if let Err(message) = validate_path(cwd) {
                return invalid_input("checks_run", message);
            }
        }
        if input
            .test_filter
            .as_deref()
            .is_some_and(|filter| filter.len() > 500)
        {
            return invalid_input("checks_run", "test_filter must be at most 500 bytes");
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let task = match self.active_executable_task(&input.task_id, subject_id, "checks_run", now)
        {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let host = auth.host.clone();
        let recipe = input.recipe.map(validation_recipe_id);
        let checks = input
            .checks
            .iter()
            .copied()
            .map(validation_semantic_check)
            .collect::<Vec<_>>();
        let resolved = match resolve_validation_recipe(
            Path::new(&task.execution_root),
            input.cwd.as_deref(),
            recipe,
            &checks,
            input.test_filter.as_deref(),
        ) {
            Ok(resolved) => resolved,
            Err(error) => return validation_recipe_error(&task, error),
        };
        let client_id = task
            .execution_executor_ref
            .strip_prefix("agent:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(client_id, _)| client_id);
        let requires_go_test_json = resolved.steps.iter().any(|step| {
            step.name == "test" && step.program == "go" && step.args == ["test", "-json", "./..."]
        });
        let mut validation_steps = resolved.steps.clone();
        // Steer cargo at the shared cache outside the slot: reset uses
        // `git clean -ffdx`, which would otherwise wipe target/ and force a
        // cold build on every task.
        let shared_cargo_target = std::path::Path::new(&self.context.runs_root)
            .parent()
            .map(|state| state.join("cache/cargo-target"));
        if let Some(shared_cargo_target) = shared_cargo_target {
            for step in &mut validation_steps {
                if step.program == "cargo" {
                    step.env.push((
                        "CARGO_TARGET_DIR".to_string(),
                        shared_cargo_target.to_string_lossy().to_string(),
                    ));
                }
            }
        }
        let recipe_identity = resolved.durable_identity();
        let timeout_secs = input.timeout_secs.unwrap_or(120);
        let request_sha256 = check_request_hash(
            &task,
            &recipe_identity,
            input.cwd.as_deref(),
            resolved.test_filter.as_deref(),
            timeout_secs,
        );
        let existing = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            Some(&input.operation_id),
        ) {
            Ok(Some(execution)) if execution.request_sha256 != request_sha256 => {
                return store_error_outcome(
                    ConnectorTaskStoreError::OperationIdConflict(input.operation_id),
                    Some(&task),
                )
            }
            Ok(execution) => execution.map(ConnectorExecutionReservation::Existing),
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let plan = input
            .checks
            .iter()
            .map(|check| check.as_str().to_string())
            .collect::<Vec<_>>();
        let reservation = match existing {
            Some(existing) => existing,
            None => {
                let access = Some(auth.access.runner_access.clone());
                let supports_structured_validation = match client_id {
                    Some(client_id) => self
                        .runner_registry
                        .runner_supports_for_auth(
                            client_id,
                            RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV,
                            access.as_ref(),
                        )
                        .await
                        .unwrap_or(false),
                    None => false,
                };
                if !supports_structured_validation {
                    return ConnectorCallOutcome::error_for_task(
                        409,
                        "structured_validation_unavailable",
                        "the selected local Runner does not support structured validation jobs",
                        false,
                        true,
                        Some("Upgrade and reconnect the WebCodex Runner, then retry checks_run."),
                        &task,
                        json!({
                            "required_capability":
                                RUNNER_CAPABILITY_STRUCTURED_VALIDATION_ARGV
                        }),
                    );
                }
                if requires_go_test_json {
                    let supported = match client_id {
                        Some(client_id) => self
                            .runner_registry
                            .runner_supports_for_auth(
                                client_id,
                                RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON,
                                access.as_ref(),
                            )
                            .await
                            .unwrap_or(false),
                        None => false,
                    };
                    if !supported {
                        return ConnectorCallOutcome::error_for_task(
                            409,
                            "structured_go_test_json_unavailable",
                            "the selected local Runner does not support machine-readable Go test validation",
                            false,
                            true,
                            Some("Upgrade and reconnect the WebCodex Runner, then retry checks_run."),
                            &task,
                            json!({
                                "required_capability":
                                    RUNNER_CAPABILITY_STRUCTURED_GO_TEST_JSON
                            }),
                        );
                    }
                }
                let check_workspace_sha256 =
                    match self.workspace_fingerprint(&task, "checks_run").await {
                        Ok(fingerprint) => fingerprint,
                        Err(outcome) => return outcome,
                    };
                match self.executions.reserve(
                    &task,
                    "check",
                    &input.operation_id,
                    &request_sha256,
                    &plan,
                    Some(&recipe_identity),
                    Some(&check_workspace_sha256),
                    timeout_secs,
                    now,
                ) {
                    Ok(reservation) => reservation,
                    Err(error) => return store_error_outcome(error, Some(&task)),
                }
            }
        };
        let execution_cwd = Path::new(&task.execution_root)
            .join(&resolved.recipe_root_relative)
            .to_string_lossy()
            .into_owned();
        drop(task_guard);
        self.execution_outcome(
            self.executions
                .execute(
                    reservation,
                    task.clone(),
                    "structured validation".to_string(),
                    Some(execution_cwd),
                    timeout_secs,
                    host,
                    auth.access.runner_access.clone(),
                    validation_steps,
                )
                .await,
            &task,
            auth,
            defer_execution_guidance,
        )
        .await
    }

    async fn commands_run(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: CommandsRunInput = match parse_input("commands_run", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if let Err(message) = validate_operation_id(&input.operation_id) {
            return invalid_input("commands_run", message);
        }
        if input.command.trim().is_empty() || input.command.len() > 32768 {
            return invalid_input("commands_run", "command must be 1..=32768 bytes");
        }
        if input
            .timeout_secs
            .is_some_and(|value| !(1..=120).contains(&value))
        {
            return invalid_input("commands_run", "timeout_secs must be 1..=120");
        }
        if let Some(cwd) = input.cwd.as_deref() {
            if let Err(message) = validate_path(cwd) {
                return invalid_input("commands_run", message);
            }
        }
        #[cfg(test)]
        if let Some(entered) = self.mutation_before_task_lock.lock().unwrap().clone() {
            entered.add_permits(1);
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let task =
            match self.active_executable_task(&input.task_id, subject_id, "commands_run", now) {
                Ok(task) => task,
                Err(outcome) => return outcome,
            };
        let timeout_secs = input.timeout_secs.unwrap_or(120);
        let request_sha256 =
            command_request_hash(&task, &input.command, input.cwd.as_deref(), timeout_secs);
        let existing = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            Some(&input.operation_id),
        ) {
            Ok(Some(execution)) if execution.request_sha256 != request_sha256 => {
                return store_error_outcome(
                    ConnectorTaskStoreError::OperationIdConflict(input.operation_id),
                    Some(&task),
                )
            }
            Ok(execution) => execution.map(ConnectorExecutionReservation::Existing),
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let reservation = match existing {
            Some(existing) => existing,
            None => {
                let manager = self.workspace.clone();
                let task_for_precondition = task.clone();
                let precondition = match tokio::task::spawn_blocking(move || {
                    manager.action_precondition(&task_for_precondition)
                })
                .await
                {
                    Ok(Ok(precondition)) => precondition,
                    Ok(Err(message)) => {
                        let cursor = self.record_event(
                            &task,
                            "commands_run",
                            json!({ "ok": false, "stage": "approval_precondition" }),
                            now,
                        );
                        let cursor = cursor.unwrap_or(task.event_cursor);
                        return ConnectorCallOutcome::error_for_task_at(
                            409,
                            "approval_precondition_failed",
                            self.sanitize_task_string(&task, &message),
                            false,
                            true,
                            Some("Resolve the Git workspace issue, then retry."),
                            &task,
                            cursor,
                            Value::Null,
                        );
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "connector approval precondition task failed");
                        return ConnectorCallOutcome::error_for_task(
                            500,
                            "approval_precondition_failed",
                            "connector could not capture the command precondition",
                            false,
                            true,
                            Some("Inspect server logs before retrying the command request."),
                            &task,
                            Value::Null,
                        );
                    }
                };
                let action_hash = command_action_hash(&request_sha256, &precondition);
                // The human decides on this summary: it must show what runs.
                // The preview is first-line/120-char bounded, never the full
                // command body.
                let action_summary = format!(
                    "raw project command ({} bytes{}, workspace {}): {}",
                    input.command.len(),
                    input
                        .cwd
                        .as_deref()
                        .map(|cwd| format!(", cwd {cwd}"))
                        .unwrap_or_default(),
                    short_oid(&precondition),
                    command_preview(&input.command)
                );
                let authority = auth.execution_authority.clone();
                if authority.auto_authorize {
                    // Trusted agent authority: no human approval interruption
                    // and no pending approval record. The auto-authorization is
                    // still a durable audit fact on the task event stream.
                    let _ = self.record_event(
                        &task,
                        "authority_auto_authorized",
                        json!({
                            "action_kind": "commands_run",
                            "action_hash": action_hash,
                            "action_summary": action_summary,
                            "authority_mode": authority.mode,
                            "authority_source": authority.source,
                            "resolved_rule": authority.resolved_rule,
                            "risk": "shell",
                            "principal": subject_id,
                            "project": self.context.project_id,
                        }),
                        now,
                    );
                } else {
                    let gate = match self.db.request_or_consume_connector_approval(
                        &task.task_id,
                        &self.context.project_id,
                        subject_id,
                        "commands_run",
                        &action_hash,
                        &action_summary,
                        now,
                        now + COMMAND_APPROVAL_TTL_SECS,
                    ) {
                        Ok(gate) => gate,
                        Err(error) => return store_error_outcome(error, Some(&task)),
                    };
                    if !matches!(&gate, ConnectorApprovalGate::Authorized(_)) {
                        let current = self.task(&task.task_id, subject_id).unwrap_or(task);
                        return approval_gate_outcome(gate, &current);
                    }
                }
                match self.executions.reserve(
                    &task,
                    "command",
                    &input.operation_id,
                    &request_sha256,
                    &[],
                    None,
                    None,
                    timeout_secs,
                    chrono::Utc::now().timestamp(),
                ) {
                    Ok(reservation) => reservation,
                    Err(error) => return store_error_outcome(error, Some(&task)),
                }
            }
        };
        drop(task_guard);
        self.execution_outcome(
            self.executions
                .execute(
                    reservation,
                    task.clone(),
                    input.command,
                    input.cwd,
                    timeout_secs,
                    auth.host.clone(),
                    auth.access.runner_access.clone(),
                    Vec::new(),
                )
                .await,
            &task,
            auth,
            defer_execution_guidance,
        )
        .await
    }

    async fn execution_outcome(
        &self,
        result: Result<webcodex_store::ConnectorExecution, ConnectorTaskStoreError>,
        task: &ConnectorTaskSnapshot,
        auth: &ConnectorCallContext,
        defer_execution_guidance: bool,
    ) -> ConnectorCallOutcome {
        let current = self
            .task(&task.task_id, &task.owner_subject_id)
            .unwrap_or_else(|_| task.clone());
        match result {
            Ok(execution) => {
                let runner_access = auth.access.runner_access.clone();
                let projection = self
                    .executions
                    .projection(&execution, &runner_access, true)
                    .await;
                let mut data = json!({ "execution": projection });
                if !defer_execution_guidance {
                    self.attach_pending_guidance(&current, &mut data);
                }
                ConnectorCallOutcome::success_blocking_at(
                    &current,
                    current.event_cursor,
                    data,
                    execution.blocks_finish(),
                )
            }
            Err(error) => store_error_outcome(error, Some(&current)),
        }
    }

    async fn task_review(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
        deliver_guidance: bool,
    ) -> ConnectorCallOutcome {
        let input: TaskReviewInput = match parse_input("task_review", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.after_cursor.is_some_and(|cursor| cursor < 0) {
            return invalid_input("task_review", "after_cursor must be non-negative");
        }
        if input.wait_ms.is_some_and(|wait| wait > 15_000) {
            return invalid_input("task_review", "wait_ms must be 0..=15000");
        }
        if input
            .max_events
            .is_some_and(|count| count == 0 || count > MAX_EVENT_COUNT)
        {
            return invalid_input("task_review", "max_events must be 1..=50");
        }
        let initial_task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let review = match self
            .executions
            .wait_for_review(initial_task, input.after_cursor, input.wait_ms.unwrap_or(0))
            .await
        {
            Ok(review) => review,
            Err(error) => return store_error_outcome(error, None),
        };
        let task = review.task;
        let result =
            match self
                .db
                .connector_task_result(&task.task_id, &self.context.project_id, subject_id)
            {
                Ok(result) => result,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
        let changes = if let Some(result) = result.as_ref() {
            let diff_preview = if input.include_diff.unwrap_or(false) {
                match WorkspaceManager::patch_preview(result, CONNECTOR_PATCH_PREVIEW_BYTES) {
                    Ok(preview) => preview,
                    Err(message) => {
                        return ConnectorCallOutcome::error_for_task(
                            409,
                            "result_artifact_unavailable",
                            self.sanitize_task_string(&task, &message),
                            false,
                            true,
                            Some("Inspect the local task state before accepting this result."),
                            &task,
                            Value::Null,
                        )
                    }
                }
            } else {
                None
            };
            self.sanitize_task_value(
                &task,
                json!({
                    "source": "stable_task_result",
                    "patch_sha256": result.patch_sha256,
                    "patch_bytes": result.patch_bytes,
                    "changed_paths": result.changed_paths,
                    "warnings": result.warnings,
                    "diff_preview": diff_preview
                }),
            )
        } else if task.task_status == ConnectorTaskState::Cancelled {
            json!({
                "source": "cancelled_task",
                "changed_paths": [],
                "diff_preview": null
            })
        } else if review
            .execution
            .as_ref()
            .is_some_and(webcodex_store::ConnectorExecution::is_active)
        {
            // The diff stays deferred while a command runs — a synchronous
            // workspace scan here would stall the review long-poll behind the
            // executor. The paths this task has applied are already durable
            // facts in its event log, so surface those instead of going dark.
            // Queried straight from the applied-edit events rather than
            // filtered out of the recent timeline, so a path applied early in a
            // long task is still reported.
            let applied = match self.db.connector_task_applied_paths(
                &task.task_id,
                &task.project_id,
                &task.owner_subject_id,
                MAX_REVIEW_APPLIED_PATHS,
            ) {
                Ok(applied) => applied,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
            json!({
                "source": "live_workspace_deferred",
                "reason": "execution_active",
                "changed_paths": applied.paths,
                "changed_paths_source": "applied_edits",
                "changed_paths_complete": applied.complete,
                "changed_paths_total": applied.total,
                "diff_preview": null
            })
        } else {
            match self
                .invoke_kernel(
                    "show_changes",
                    json!({
                        "project": task.execution_executor_ref,
                        "include_diff": input.include_diff.unwrap_or(false),
                        "max_hunks": 20,
                        "max_hunk_lines": 80,
                        "session_event_limit": 0
                    }),
                    &task,
                    auth,
                    transport,
                )
                .await
            {
                Ok(output) => output,
                Err(_) => {
                    // Never blind the reviewer because the workspace scan
                    // failed (e.g. the slot is wedged after a terminal
                    // failure): degrade to the durable applied-path record
                    // instead of failing the whole review.
                    // Same durable source the active-execution branch uses, so
                    // a path applied early in a long task is still reported and
                    // a bounded list never claims to be the whole set. A
                    // lookup that also fails reports nothing rather than
                    // implying the task changed nothing.
                    let applied = self.db.connector_task_applied_paths(
                        &task.task_id,
                        &task.project_id,
                        &task.owner_subject_id,
                        MAX_REVIEW_APPLIED_PATHS,
                    );
                    let (paths, total, complete) = match applied {
                        Ok(applied) => (applied.paths, applied.total, applied.complete),
                        Err(_) => (Vec::new(), 0, false),
                    };
                    json!({
                        "source": "workspace_scan_failed",
                        "changed_paths": paths,
                        "changed_paths_source": "applied_edits",
                        "changed_paths_complete": complete,
                        "changed_paths_total": total,
                        "diff_preview": null
                    })
                }
            }
        };
        let events = match self.db.connector_task_events(
            &task.task_id,
            &task.project_id,
            &task.owner_subject_id,
            MAX_EVENT_COUNT,
        ) {
            Ok(events) => events,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let max_events = input.max_events.unwrap_or(MAX_EVENT_COUNT);
        let mut events = events
            .into_iter()
            .filter(|event| {
                input
                    .after_cursor
                    .is_none_or(|cursor| event.sequence > cursor)
            })
            .collect::<Vec<_>>();
        events.drain(..events.len().saturating_sub(max_events));
        let execution = match review.execution.as_ref() {
            Some(execution) => {
                let runner_access = auth.access.runner_access.clone();
                Some(
                    self.executions
                        .projection(
                            execution,
                            &runner_access,
                            input.include_output_tail.unwrap_or(false),
                        )
                        .await,
                )
            }
            None => None,
        };
        let blocking = review
            .execution
            .as_ref()
            .is_some_and(|execution| execution.blocks_finish());
        let next_action = execution
            .as_ref()
            .and_then(|value| value["next_action"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if task.task_status == ConnectorTaskState::Cancelled {
                    "start_a_new_task"
                } else if task.run_status == ConnectorRunState::Interrupted {
                    "resume_or_reject_on_the_host"
                } else {
                    "continue_or_finish"
                }
                .to_string()
            });
        let mut data = durable_task_review_projection(&task, result.as_ref());
        data["changes"] = changes;
        data["active_execution"] = execution
            .as_ref()
            .filter(|_| {
                review
                    .execution
                    .as_ref()
                    .is_some_and(webcodex_store::ConnectorExecution::is_active)
            })
            .cloned()
            .unwrap_or(Value::Null);
        data["recent_execution"] = execution.unwrap_or(Value::Null);
        data["recent_events"] = json!(events);
        data["heartbeat"] = json!(review.heartbeat);
        data["next_action"] = json!(next_action);
        if deliver_guidance {
            self.attach_pending_guidance(&task, &mut data);
        }
        ConnectorCallOutcome::success_blocking_at(&task, task.event_cursor, data, blocking)
    }

    /// Recovery/diagnostic listing for durable tasks this credential may
    /// continue, most actionable first. Ordinary same-window continuation is
    /// resolved by task_start without duplicating the durable context.
    async fn task_list(&self, arguments: Value, subject_id: &str) -> ConnectorCallOutcome {
        let input: TaskListInput = match parse_input("task_list", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let limit = input.limit.unwrap_or(DEFAULT_TASK_LIST_LIMIT);
        if !(1..=MAX_TASK_LIST_LIMIT).contains(&limit) {
            return invalid_input("task_list", "limit must be 1..=20");
        }
        let tasks =
            match self
                .db
                .connector_tasks_for_subject(&self.context.project_id, subject_id, limit)
            {
                Ok(tasks) => tasks,
                Err(error) => return store_error_outcome(error, None),
            };
        let items: Vec<Value> = tasks
            .iter()
            .map(|task| {
                json!({
                    "task_id": task.task_id,
                    "goal": bounded_goal(&task.goal),
                    "task_status": task.task_status,
                    "updated_at": task.updated_at,
                    "execution_status": task.execution_status,
                    "validation_status": task.validation_status,
                    "next_action": model_next_action(task.task_status, &task.next_action),
                })
            })
            .collect();
        ConnectorCallOutcome::success_project(json!({
            "tasks": items,
            "count": items.len(),
            "note": "Ordinary work starts or continues with task_start. Use task_resume only to recover a specific durable task after window identity was lost."
        }))
    }

    /// Explicit recovery for a task whose automatic window binding is no
    /// longer available. Claims pending human guidance exactly like
    /// task_review does.
    async fn task_resume(
        &self,
        arguments: Value,
        subject_id: &str,
        window: Option<&ConnectorWindowId>,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: TaskResumeInput = match parse_input("task_resume", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        let task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&task);
        }
        let context_lock = window.map(|window| self.context_lock(subject_id, window.key()));
        let _context_guard = match context_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        let result =
            match self
                .db
                .connector_task_result(&task.task_id, &self.context.project_id, subject_id)
            {
                Ok(result) => result,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
        let applied = match self.db.connector_task_applied_paths(
            &task.task_id,
            &task.project_id,
            &task.owner_subject_id,
            MAX_REVIEW_APPLIED_PATHS,
        ) {
            Ok(applied) => applied,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let execution = match self.db.latest_connector_execution(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            None,
        ) {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let execution_value =
            execution.map(|execution| execution::execution_projection(&execution, now, None));
        // A local decision outranks stale execution advice: an accepted or
        // rejected result decides the story, whatever the last run said.
        let decision_action = result.as_ref().and_then(|result| match result.decision_status {
            ConnectorResultDecisionStatus::Accepted => {
                Some("the result was accepted locally; start the next piece of work with task_start")
            }
            ConnectorResultDecisionStatus::Rejected => Some(
                "the result was rejected; apply the guidance and start a corrected task with task_start",
            ),
            ConnectorResultDecisionStatus::Pending => None,
        });
        let next_action = decision_action
            .map(str::to_string)
            .or_else(|| {
                execution_value
                    .as_ref()
                    .and_then(|value| value["next_action"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                if result.is_some() {
                    "task_review, then ask the project owner to accept or reject locally"
                } else if task.task_status == ConnectorTaskState::Cancelled {
                    "start_a_new_task"
                } else if task.run_status == ConnectorRunState::Interrupted {
                    "ask the project owner to resume or reject this task on the host"
                } else {
                    "continue with files_read/edits_apply, then task_review"
                }
                .to_string()
            });
        let result_value = result
            .as_ref()
            .map(|result| {
                json!({
                    "result_id": result.result_id,
                    "summary": result.summary,
                    "changed_paths": result.changed_paths,
                    "patch_bytes": result.patch_bytes,
                    "decision_status": result.decision_status,
                })
            })
            .unwrap_or(Value::Null);
        let continuity = if let Some(window) = window {
            let previous = match self.db.connector_window_context_for_task(
                &task.task_id,
                &self.context.project_id,
                subject_id,
            ) {
                Ok(previous) => previous,
                Err(error) => return store_error_outcome(error, Some(&task)),
            };
            let target_path = previous
                .as_ref()
                .map(|context| context.target_path.clone())
                .unwrap_or_default();
            let fingerprint = match self.capture_connector_context(target_path).await {
                Ok(fingerprint) => fingerprint,
                Err(outcome) => return outcome,
            };
            if previous.as_ref().is_some_and(|context| {
                context.fingerprint.project_root_sha256 != fingerprint.project_root_sha256
            }) {
                return ConnectorCallOutcome::error_for_task(
                    409,
                    "project_context_mismatch",
                    "the durable task repository identity no longer matches the configured path",
                    false,
                    true,
                    Some("Recover the task only after restoring its original repository identity."),
                    &task,
                    json!({ "window_rebound": false }),
                );
            }
            let refresh = compare_project_context(
                previous.as_ref().map(|context| &context.fingerprint),
                &fingerprint,
            );
            if let Err(error) =
                self.persist_window_context(window, subject_id, &task.task_id, &fingerprint, now)
            {
                return store_error_outcome(error, Some(&task));
            }
            let navigation = self.db.activate_window_project(
                subject_id,
                window.key(),
                &format!(
                    "{}:{}",
                    self.context.project_id, fingerprint.project_root_sha256
                ),
            );
            json!({
                "window_rebound": true,
                "context": context_refresh_payload(&refresh),
                "project_switch": navigation_payload(Some(&navigation), true)
            })
        } else {
            json!({
                "window_rebound": false,
                "recovery_boundary": "no stable transport window identity was available"
            })
        };
        let mut data = json!({
            "goal": task.goal,
            "mode": task.mode,
            "task_status": task.task_status,
            "run_status": task.run_status,
            "isolated": task.isolated,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "result": result_value,
            "applied_paths": applied.paths,
            "applied_paths_total": applied.total,
            "applied_paths_complete": applied.complete,
            "recent_execution": execution_value.unwrap_or(Value::Null),
            "next_action": next_action,
            "continuity": continuity,
            "resume_note": "This window is now the task's continuation when a stable transport identity was available. Trust this bootstrap over assumptions from earlier windows, and apply any guidance below before acting."
        });
        self.attach_pending_guidance(&task, &mut data);
        // Timeline visibility for the console; terminal tasks skip the
        // running-only event guard on purpose, and a failed advisory event
        // must not fail the bootstrap.
        let cursor = if task.task_status == ConnectorTaskState::Active
            && task.run_status == ConnectorRunState::Running
        {
            match self.record_event(
                &task,
                "task_resume",
                json!({ "window_rebound": window.is_some() }),
                now,
            ) {
                Ok(cursor) => cursor,
                Err(_) => task.event_cursor,
            }
        } else {
            task.event_cursor
        };
        ConnectorCallOutcome::success_at(&task, cursor, self.sanitize_task_value(&task, data))
    }

    async fn task_cancel(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
    ) -> ConnectorCallOutcome {
        let input: TaskCancelInput = match parse_input("task_cancel", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input
            .reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty() || reason.len() > 500)
        {
            return invalid_input("task_cancel", "reason must be 1..=500 bytes when provided");
        }
        let task_lock = self.task_lock(&input.task_id);
        let _task_guard = task_lock.lock().await;
        let task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let host = auth.host.clone();
        let runner_access = auth.access.runner_access.clone();
        let execution = match self
            .executions
            .cancel_task(
                task.clone(),
                input.reason.as_deref(),
                host,
                runner_access.clone(),
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        let current = self.task(&task.task_id, subject_id).unwrap_or(task);
        let projection = match execution.as_ref() {
            Some(execution) => Some(
                self.executions
                    .projection(execution, &runner_access, true)
                    .await,
            ),
            None => None,
        };
        let blocking = execution
            .as_ref()
            .is_some_and(|execution| execution.blocks_finish());
        ConnectorCallOutcome::success_blocking_at(
            &current,
            current.event_cursor,
            json!({
                "status": current.task_status,
                "run_status": current.run_status,
                "execution": projection,
                "cancellation": if blocking { "requested" } else { "terminal" },
                "next_action": if blocking {
                    "wait_with_task_review"
                } else {
                    "start_a_new_task_if_more_work_is_needed"
                }
            }),
            blocking,
        )
    }

    async fn task_finish(
        &self,
        arguments: Value,
        subject_id: &str,
        auth: &ConnectorCallContext,
        _transport: ConnectorTransport,
        now: i64,
    ) -> ConnectorCallOutcome {
        let input: TaskFinishInput = match parse_input("task_finish", arguments) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        if input.summary.trim().is_empty() || input.summary.len() > 4000 {
            return invalid_input("task_finish", "summary must be 1..=4000 bytes");
        }
        let task_lock = self.task_lock(&input.task_id);
        let task_guard = task_lock.lock().await;
        let visible_task = match self.task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        if visible_task.mode == ConnectorTaskMode::InspectLegacy {
            return Self::retired_inspect_task_outcome(&visible_task);
        }
        if let Some(outcome) = Self::invalid_task_workspace_outcome(&visible_task) {
            return outcome;
        }
        let blocker = match self.db.connector_finish_blocker(&input.task_id) {
            Ok(blocker) => blocker,
            Err(error) => return store_error_outcome(error, Some(&visible_task)),
        };
        if let Some(execution) = blocker {
            let runner_access = auth.access.runner_access.clone();
            let projection = self
                .executions
                .projection(&execution, &runner_access, true)
                .await;
            return ConnectorCallOutcome::error_for_task(
                409,
                "execution_not_terminal",
                "task_finish is blocked until the active execution reaches a known terminal state",
                true,
                execution.state == ConnectorExecutionState::Unknown,
                Some(if execution.state == ConnectorExecutionState::Unknown {
                    "Inspect the executor state on the host before finishing this task."
                } else {
                    "Use task_review to wait for completion or task_cancel to stop the execution."
                }),
                &visible_task,
                json!({ "execution": projection }),
            );
        }
        let task = match self.active_task(&input.task_id, subject_id) {
            Ok(task) => task,
            Err(outcome) => return outcome,
        };
        let _workspace_guard = if task.isolated {
            Some(self.workspace_ops.lock().await)
        } else {
            None
        };
        let check_execution = match self.db.latest_connector_execution_by_kind(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            "check",
        ) {
            Ok(execution) => execution,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        if task.isolated && check_execution.is_none() {
            return ConnectorCallOutcome::error_for_task(
                409,
                "checks_required",
                "an isolated writable coding result must run structured checks before task_finish",
                false,
                true,
                Some("Call checks_run with a new operation_id, then retry task_finish."),
                &task,
                json!({}),
            );
        }
        if let Some(check) = check_execution
            .as_ref()
            .filter(|check| check.state == ConnectorExecutionState::Succeeded)
        {
            let Some(validated) = check.validated_workspace_sha256.as_deref() else {
                return checks_stale_outcome(
                    &task,
                    check,
                    "the latest successful check has no trusted workspace provenance",
                );
            };
            let current = match self.workspace_fingerprint(&task, "task_finish").await {
                Ok(current) => current,
                Err(outcome) => return outcome,
            };
            if current != validated {
                return checks_stale_outcome(
                    &task,
                    check,
                    "the workspace changed after the latest successful check",
                );
            }
            #[cfg(test)]
            let finish_hook = { self.finish_after_fingerprint.lock().unwrap().clone() };
            #[cfg(test)]
            if let Some((reached, resume)) = finish_hook {
                reached.notify_one();
                resume.notified().await;
            }
        }
        let manager = self.workspace.clone();
        let task_for_capture = task.clone();
        let captured =
            match tokio::task::spawn_blocking(move || manager.capture_result(&task_for_capture))
                .await
            {
                Ok(Ok(captured)) => captured,
                Ok(Err(message)) => {
                    let cursor = self.record_event(
                        &task,
                        "task_finish",
                        json!({ "ok": false, "stage": "capture_result" }),
                        now,
                    );
                    let cursor = cursor.unwrap_or(task.event_cursor);
                    return ConnectorCallOutcome::error_for_task_at(
                        409,
                        "result_capture_failed",
                        self.sanitize_task_string(&task, &message),
                        false,
                        true,
                        Some("Resolve the reported workspace issue, then retry task_finish."),
                        &task,
                        cursor,
                        Value::Null,
                    );
                }
                Err(error) => {
                    tracing::error!(error = %error, "connector result capture task failed");
                    return ConnectorCallOutcome::error_for_task(
                        500,
                        "result_capture_failed",
                        "connector could not capture a stable task result",
                        false,
                        true,
                        Some("Inspect server logs before retrying task_finish."),
                        &task,
                        Value::Null,
                    );
                }
            };
        let validation = validation_projection(check_execution.as_ref());
        let result_id = format!("wc_result_{}", uuid::Uuid::new_v4().simple());
        let mut cursor = match self.db.finish_connector_task(
            &task.task_id,
            &self.context.project_id,
            subject_id,
            NewConnectorResult {
                result_id: &result_id,
                summary: input.summary.trim(),
                patch_artifact: captured.patch_artifact.as_deref(),
                patch_sha256: captured.patch_sha256.as_deref(),
                patch_bytes: captured.patch_bytes,
                changed_paths: &captured.changed_paths,
                validation: &validation,
                warnings: &captured.warnings,
            },
            now,
        ) {
            Ok(cursor) => cursor,
            Err(error) => return store_error_outcome(error, Some(&task)),
        };
        drop(task_guard);
        let cleanup_warning = if task.isolated {
            let manager = self.workspace.clone();
            let task_for_release = task.clone();
            match tokio::task::spawn_blocking(move || {
                manager.release_task_workspace(&task_for_release)
            })
            .await
            {
                Ok(warning) => warning,
                Err(error) => {
                    tracing::error!(error = %error, "connector workspace release task failed");
                    Some("connector could not release the reusable execution workspace".to_string())
                }
            }
        } else {
            None
        }
        .map(|warning| self.sanitize_task_string(&task, &warning));
        if task.isolated {
            match self.db.record_connector_workspace_release(
                &task.task_id,
                &self.context.project_id,
                subject_id,
                cleanup_warning.is_none(),
                cleanup_warning.as_deref(),
                now,
            ) {
                Ok(release_cursor) => cursor = release_cursor,
                Err(error) => {
                    tracing::warn!(error = %error, task_id = %task.task_id, "Could not record connector workspace release");
                }
            }
        }
        let workspace_released = !task.isolated || cleanup_warning.is_none();
        ConnectorCallOutcome::success_at(
            &task,
            cursor,
            json!({
                "status": "ready_for_review",
                "run_status": "completed",
                "summary": input.summary.trim(),
                "result": {
                    "result_id": result_id,
                    "patch_sha256": captured.patch_sha256,
                    "patch_bytes": captured.patch_bytes,
                    "changed_paths": captured.changed_paths,
                    "validation": validation,
                    "warnings": captured.warnings,
                    "decision_status": "pending",
                    "cleanup_warning": cleanup_warning.clone()
                },
                "workspace": {
                    "strategy": if task.isolated { "reusable_slot" } else { "target_checkout" },
                    "released": workspace_released
                },
                "human_action": format!(
                    "Run 'webcodex task show {}', then accept or reject the result locally.",
                    task.task_id
                )
            }),
        )
    }

    fn task(
        &self,
        task_id: &str,
        subject_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        validate_task_id(task_id).map_err(|message| invalid_input("task", message))?;
        let task = self
            .db
            .connector_task(task_id, &self.context.project_id, subject_id)
            .map_err(|error| store_error_outcome(error, None))?;
        match (
            Path::new(&task.target_root).canonicalize(),
            Path::new(&self.context.executor_root).canonicalize(),
        ) {
            (Ok(recorded), Ok(configured)) if recorded == configured => Ok(task),
            (Ok(_), Ok(_)) => Err(ConnectorCallOutcome::error_for_task(
                409,
                "project_context_mismatch",
                "the durable task belongs to a different repository path",
                false,
                true,
                Some("Use the Connector configured for the task's original repository."),
                &task,
                json!({ "window_rebound": false }),
            )),
            _ => Err(ConnectorCallOutcome::error_for_task(
                409,
                "project_context_unavailable",
                "the durable task repository identity cannot be verified",
                false,
                true,
                Some("Restore the configured repository path before continuing this task."),
                &task,
                json!({ "window_rebound": false }),
            )),
        }
    }

    fn retired_inspect_task_outcome(task: &ConnectorTaskSnapshot) -> ConnectorCallOutcome {
        ConnectorCallOutcome::error_for_task(
            409,
            "inspect_mode_retired",
            "this pre-0.4 inspect task can no longer execute",
            false,
            true,
            Some("Reject or clean up this legacy task, then start a new read_only task for analysis or a new normal task for writable work."),
            task,
            Value::Null,
        )
    }

    fn invalid_mode_transition_outcome(
        task: &ConnectorTaskSnapshot,
        requested_mode: &str,
    ) -> Option<ConnectorCallOutcome> {
        (task.mode == ConnectorTaskMode::Normal && requested_mode == "read_only").then(|| {
            ConnectorCallOutcome::error_for_task(
                409,
                "mode_transition_invalid",
                "a writable normal task cannot transition to read_only",
                false,
                true,
                Some("Finish or reject the current writable task, then start a new read_only task for analysis."),
                task,
                json!({
                    "previous_mode": task.mode,
                    "requested_mode": requested_mode,
                }),
            )
        })
    }

    fn invalid_task_workspace_outcome(
        task: &ConnectorTaskSnapshot,
    ) -> Option<ConnectorCallOutcome> {
        let message = match task.mode {
            ConnectorTaskMode::Normal
                if !task.isolated
                    || task.execution_root == task.target_root
                    || task.baseline_commit.as_deref().is_none_or(str::is_empty)
                    || task.baseline_tree.as_deref().is_none_or(str::is_empty) =>
            {
                "normal task has an invalid isolated writable-workspace state"
            }
            ConnectorTaskMode::ReadOnly
                if task.isolated || task.execution_root != task.target_root =>
            {
                "read_only task has an invalid workspace state"
            }
            ConnectorTaskMode::Normal
            | ConnectorTaskMode::ReadOnly
            | ConnectorTaskMode::InspectLegacy => return None,
        };
        Some(ConnectorCallOutcome::error_for_task(
            409,
            "task_state_invalid",
            message,
            false,
            true,
            Some("Reject or clean up this inconsistent task, then start a new normal or read_only task."),
            task,
            json!({
                "mode": task.mode,
                "isolated": task.isolated,
            }),
        ))
    }

    fn active_task(
        &self,
        task_id: &str,
        subject_id: &str,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.task(task_id, subject_id)?;
        if task.mode != ConnectorTaskMode::InspectLegacy {
            if let Some(outcome) = Self::invalid_task_workspace_outcome(&task) {
                return Err(outcome);
            }
        }
        if task.run_status == ConnectorRunState::Interrupted {
            return Err(ConnectorCallOutcome::error_for_task(
                409,
                "task_interrupted",
                "this task was interrupted when the local connector runtime stopped",
                false,
                true,
                Some("Review the task, then resume it from the WebCodex host before continuing."),
                &task,
                json!({
                    "local_command": format!("webcodex task resume {}", task.task_id)
                }),
            ));
        }
        if task.task_status != ConnectorTaskState::Active
            || task.run_status != ConnectorRunState::Running
        {
            return Err(ConnectorCallOutcome::error_for_task(
                409,
                "task_not_active",
                "this task is already ready for review; start a new task for additional work",
                false,
                true,
                Some("Call task_start with the next requested outcome."),
                &task,
                Value::Null,
            ));
        }
        Ok(task)
    }

    fn active_writable_task(
        &self,
        task_id: &str,
        subject_id: &str,
        capability: &str,
        now: i64,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.active_task(task_id, subject_id)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(Self::retired_inspect_task_outcome(&task));
        }
        if task.mode == ConnectorTaskMode::ReadOnly {
            let cursor = self.record_event(
                &task,
                capability,
                json!({ "ok": false, "denied": "read_only" }),
                now,
            );
            return Err(ConnectorCallOutcome::error_for_task_at(
                403,
                "read_only_task",
                format!("{capability} is unavailable because this task is read_only"),
                false,
                true,
                Some("Start a normal task only after the user authorizes changes or execution."),
                &task,
                cursor.unwrap_or(task.event_cursor),
                Value::Null,
            ));
        }
        Ok(task)
    }

    fn active_executable_task(
        &self,
        task_id: &str,
        subject_id: &str,
        capability: &str,
        now: i64,
    ) -> Result<ConnectorTaskSnapshot, ConnectorCallOutcome> {
        let task = self.active_task(task_id, subject_id)?;
        if task.mode == ConnectorTaskMode::InspectLegacy {
            return Err(Self::retired_inspect_task_outcome(&task));
        }
        if task.mode == ConnectorTaskMode::ReadOnly {
            let cursor = self.record_event(
                &task,
                capability,
                json!({ "ok": false, "denied": "read_only" }),
                now,
            );
            return Err(ConnectorCallOutcome::error_for_task_at(
                403,
                "read_only_task",
                format!("{capability} is unavailable because this task is read_only"),
                false,
                true,
                Some("Start a normal task only after the user authorizes command execution."),
                &task,
                cursor.unwrap_or(task.event_cursor),
                Value::Null,
            ));
        }
        Ok(task)
    }

    /// Attach human guidance to a model-facing capability response.
    /// The task watermark provides an atomic, single-consumer claim across
    /// concurrent server responses. It does not provide an end-to-end delivery
    /// acknowledgement if the response is lost after the transaction commits.
    fn attach_pending_guidance(&self, task: &ConnectorTaskSnapshot, data: &mut Value) {
        // One transaction claims the guidance and advances the watermark, so a
        // second capability response running concurrently cannot claim the same
        // message, and guidance older than the timeline window is still found.
        let claimed = match self.db.claim_pending_connector_guidance(
            &task.task_id,
            &self.context.project_id,
            &task.owner_subject_id,
            MAX_GUIDANCE_PER_RESPONSE,
        ) {
            Ok(claimed) => claimed,
            Err(error) => {
                // A claim that failed delivered nothing and advanced nothing;
                // say so rather than letting the message look consumed.
                tracing::warn!(
                    task_id = %task.task_id,
                    error = %error,
                    "guidance claim failed; guidance stays pending",
                );
                return;
            }
        };
        if claimed.is_empty() {
            return;
        }
        let pending: Vec<Value> = claimed
            .iter()
            .map(|event| {
                json!({
                    "sequence": event.sequence,
                    "message": event.payload["message"],
                    "created_at": event.created_at,
                })
            })
            .collect();
        data["guidance"] = json!(pending);
        data["guidance_note"] =
            json!("Human guidance from the project owner — adjust course before continuing.");
    }

    /// Host-side entry: record a human guidance message on a task. Delivered
    /// to the model inside its next capability response for this task.
    pub fn host_guide(&self, task_id: &str, message: &str) -> ConnectorCallOutcome {
        let task = match self
            .db
            .local_connector_task(task_id, &self.context.project_id)
        {
            Ok(task) => task,
            Err(error) => return store_error_outcome(error, None),
        };
        let now = chrono::Utc::now().timestamp();
        match self.record_event(
            &task,
            "human_guidance",
            json!({ "message": message, "source": "host" }),
            now,
        ) {
            Ok(cursor) => ConnectorCallOutcome::success_at(
                &task,
                cursor,
                json!({ "recorded": true, "sequence": cursor }),
            ),
            Err(outcome) => outcome,
        }
    }

    fn record_event(
        &self,
        task: &ConnectorTaskSnapshot,
        capability: &str,
        payload: Value,
        now: i64,
    ) -> Result<i64, ConnectorCallOutcome> {
        self.db
            .append_connector_task_event(
                &task.task_id,
                &self.context.project_id,
                &task.owner_subject_id,
                capability,
                &payload,
                now,
            )
            .map_err(|error| store_error_outcome(error, Some(task)))
    }

    fn single_batch_item_output(tool_name: &str, output: Value) -> Result<Value, KernelFailure> {
        let Some(items) = output.get("items").and_then(Value::as_array) else {
            return Err(KernelFailure::Adapter(format!(
                "{tool_name} returned a malformed batch result without items"
            )));
        };
        if items.len() != 1 {
            return Err(KernelFailure::Adapter(format!(
                "{tool_name} returned {} items for a one-item connector request",
                items.len()
            )));
        }
        let item = &items[0];
        match item.get("success").and_then(Value::as_bool) {
            Some(true) => item.get("output").cloned().ok_or_else(|| {
                KernelFailure::Adapter(format!(
                    "{tool_name} returned a successful item without output"
                ))
            }),
            Some(false) => Err(KernelFailure::Tool {
                error: item
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                output: item.get("output").cloned().unwrap_or(Value::Null),
            }),
            None => Err(KernelFailure::Adapter(format!(
                "{tool_name} returned an item without a boolean success field"
            ))),
        }
    }

    async fn invoke_kernel(
        &self,
        tool_name: &str,
        arguments: Value,
        task: &ConnectorTaskSnapshot,
        auth: &ConnectorCallContext,
        transport: ConnectorTransport,
    ) -> Result<Value, KernelFailure> {
        let host = auth.host.clone();
        match host
            .invoke_tool(ConnectorToolRequest {
                tool_name: tool_name.to_string(),
                arguments,
                transport: transport.into(),
            })
            .await
        {
            Ok(output) => Ok(self.sanitize_task_value(task, output)),
            Err(ConnectorToolFailure::Permission { required, message }) => {
                Err(KernelFailure::Scope {
                    required_permission: required,
                    message,
                })
            }
            Err(ConnectorToolFailure::InvalidArguments(message))
            | Err(ConnectorToolFailure::Adapter(message)) => Err(KernelFailure::Adapter(message)),
            Err(ConnectorToolFailure::Tool { error, output }) => {
                Err(KernelFailure::Tool { error, output })
            }
        }
    }

    fn kernel_error_outcome(
        &self,
        error: KernelFailure,
        task: &ConnectorTaskSnapshot,
        cursor: Result<i64, ConnectorCallOutcome>,
        partial_data: Value,
    ) -> ConnectorCallOutcome {
        let cursor = match cursor {
            Ok(cursor) => cursor,
            Err(outcome) => return outcome,
        };
        match error {
            KernelFailure::Scope {
                required_permission,
                message,
            } => ConnectorCallOutcome::error_for_task_at_with_scope(
                403,
                "insufficient_scope",
                message,
                false,
                true,
                Some(
                    "Grant the required connector scope and retry only after checking task_review.",
                ),
                task,
                cursor,
                partial_data,
                required_permission,
            ),
            KernelFailure::Adapter(message) => ConnectorCallOutcome::error_for_task_at(
                500,
                "connector_adapter_error",
                format!(
                    "connector could not translate the capability: {}",
                    self.sanitize_task_string(task, &message)
                ),
                false,
                true,
                Some("Inspect server logs; do not retry a consequential call automatically."),
                task,
                cursor,
                partial_data,
            ),
            KernelFailure::Tool { error, output } => {
                let message = error
                    .as_deref()
                    .map(|message| self.sanitize_task_string(task, message))
                    .unwrap_or_else(|| "executor rejected the capability".to_string());
                let output = self.sanitize_task_value(task, output);
                ConnectorCallOutcome::error_for_task_at(
                    400,
                    "capability_failed",
                    message,
                    false,
                    false,
                    Some("Use the returned diagnostics, inspect if needed, then retry with a corrected call."),
                    task,
                    cursor,
                    json!({ "partial": partial_data, "executor": output }),
                )
            }
        }
    }

    fn sanitize_executor_string(&self, value: &str) -> String {
        value
            .replace(&self.context.executor_project, &self.context.project_id)
            .replace(&self.context.executor_root, ".")
            .replace(&self.context.runs_root, "<managed-runs>")
            .replace(&self.context.results_root, "<managed-results>")
            .replace(
                &self.context.project_registry_dir,
                "<managed-project-registry>",
            )
    }

    fn sanitize_task_value(&self, task: &ConnectorTaskSnapshot, mut value: Value) -> Value {
        sanitize_value(
            &mut value,
            &task.execution_executor_ref,
            &self.context.project_id,
            &task.execution_root,
        );
        if let Some(client_id) = executor_client_id(&task.execution_executor_ref) {
            replace_string_material(&mut value, client_id, "<agent>");
        }
        value
    }

    fn sanitize_task_string(&self, task: &ConnectorTaskSnapshot, value: &str) -> String {
        let value = value
            .replace(&task.execution_executor_ref, &self.context.project_id)
            .replace(&task.execution_root, ".")
            .replace(&self.context.runs_root, "<managed-runs>")
            .replace(&self.context.results_root, "<managed-results>")
            .replace(
                &self.context.project_registry_dir,
                "<managed-project-registry>",
            );
        match executor_client_id(&task.execution_executor_ref) {
            Some(client_id) => value.replace(client_id, "<agent>"),
            None => value,
        }
    }
}

fn executor_client_id(executor_ref: &str) -> Option<&str> {
    let (client_id, project_id) = executor_ref.strip_prefix("agent:")?.split_once(':')?;
    (!client_id.is_empty() && !project_id.is_empty()).then_some(client_id)
}

fn replace_string_material(value: &mut Value, needle: &str, replacement: &str) {
    match value {
        Value::String(string) => {
            if string.contains(needle) {
                *string = string.replace(needle, replacement);
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_string_material(item, needle, replacement);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                replace_string_material(item, needle, replacement);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn code_navigation_tool_call(input: &CodeNavigateInput) -> Result<(&'static str, Value), String> {
    let irrelevant = |fields: &[(&str, bool)]| {
        fields
            .iter()
            .find_map(|(name, present)| present.then_some(*name))
            .map(|name| {
                format!(
                    "{name} is not valid for operation {}",
                    input.operation.as_str()
                )
            })
    };
    let require_path = || -> Result<&str, String> {
        let path = input.path.as_deref().ok_or_else(|| {
            format!(
                "path is required for operation {}",
                input.operation.as_str()
            )
        })?;
        validate_path(path).map_err(str::to_string)?;
        if redact_absolute_paths(path) != path {
            return Err("path must be project-relative".to_string());
        }
        Ok(path)
    };
    let require_position = || -> Result<(usize, usize), String> {
        let line = input.line.ok_or_else(|| {
            format!(
                "line is required for operation {}",
                input.operation.as_str()
            )
        })?;
        let column = input.column.ok_or_else(|| {
            format!(
                "column is required for operation {}",
                input.operation.as_str()
            )
        })?;
        if line < 1 || column < 1 {
            return Err("line and column must be >= 1".to_string());
        }
        Ok((line, column))
    };
    let validate_limit = |maximum: usize| -> Result<(), String> {
        if input
            .limit
            .is_some_and(|limit| !(1..=maximum).contains(&limit))
        {
            return Err(format!(
                "limit for operation {} must be 1..={maximum}",
                input.operation.as_str()
            ));
        }
        Ok(())
    };

    match input.operation {
        CodeNavigateOperation::Status => {
            if let Some(message) = irrelevant(&[
                ("path", input.path.is_some()),
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
                ("limit", input.limit.is_some()),
            ]) {
                return Err(message);
            }
            Ok(("lsp_status", json!({})))
        }
        CodeNavigateOperation::DocumentSymbols => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            validate_limit(MAX_DOCUMENT_SYMBOLS_LIMIT)?;
            Ok((
                "document_symbols",
                json!({ "path": path, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::WorkspaceSymbols => {
            if let Some(message) = irrelevant(&[
                ("path", input.path.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let query = input.query.as_deref().unwrap_or_default().trim();
            if query.is_empty() || query.chars().count() > 200 {
                return Err(
                    "query for operation workspace_symbols must contain 1..=200 non-whitespace characters"
                        .to_string(),
                );
            }
            if redact_absolute_paths(query) != query {
                return Err(
                    "query for operation workspace_symbols must not contain absolute path material"
                        .to_string(),
                );
            }
            validate_limit(MAX_WORKSPACE_SYMBOLS_LIMIT)?;
            Ok((
                "workspace_symbols",
                json!({ "query": query, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::Definition => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            validate_limit(MAX_GOTO_DEFINITION_LIMIT)?;
            Ok((
                "goto_definition",
                json!({
                    "path": path,
                    "line": line,
                    "column": column,
                    "limit": input.limit
                }),
            ))
        }
        CodeNavigateOperation::References => {
            if let Some(message) = irrelevant(&[("query", input.query.is_some())]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            validate_limit(MAX_FIND_REFERENCES_LIMIT)?;
            Ok((
                "find_references",
                json!({
                    "path": path,
                    "line": line,
                    "column": column,
                    "include_declaration": input.include_declaration.unwrap_or(true),
                    "limit": input.limit
                }),
            ))
        }
        CodeNavigateOperation::Diagnostics => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("line", input.line.is_some()),
                ("column", input.column.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            validate_limit(MAX_DOCUMENT_DIAGNOSTICS_LIMIT)?;
            Ok((
                "document_diagnostics",
                json!({ "path": path, "limit": input.limit }),
            ))
        }
        CodeNavigateOperation::Hover => {
            if let Some(message) = irrelevant(&[
                ("query", input.query.is_some()),
                ("include_declaration", input.include_declaration.is_some()),
                ("limit", input.limit.is_some()),
            ]) {
                return Err(message);
            }
            let path = require_path()?;
            let (line, column) = require_position()?;
            Ok((
                "hover",
                json!({ "path": path, "line": line, "column": column }),
            ))
        }
    }
}
