use super::{RecoveryKind, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use crate::db::{
    GoalDetail, GoalLifecycle, GoalPatch, GoalStoreError, NewGoal, MAX_GOAL_LIST_LIMIT,
};
use serde::Serialize;
use serde_json::{json, to_value};

const DEFAULT_GOAL_LIST_LIMIT: usize = 50;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct GoalPlanProjection {
    pub version: u8,
    pub goal_id: String,
    pub title: String,
    pub objective: String,
    pub lifecycle: GoalLifecycle,
    pub revision: i64,
    pub updated_at_unix_ms: i64,
    pub terminal_at_unix_ms: Option<i64>,
    pub agent_task_count: i64,
    pub workflow_session_count: i64,
}

fn goal_plan_projection(goal: GoalDetail) -> GoalPlanProjection {
    GoalPlanProjection {
        version: 1,
        goal_id: goal.summary.goal_id,
        title: goal.summary.title,
        objective: goal.objective,
        lifecycle: goal.summary.lifecycle,
        revision: goal.summary.revision,
        updated_at_unix_ms: goal.summary.updated_at_unix_ms,
        terminal_at_unix_ms: goal.summary.terminal_at_unix_ms,
        agent_task_count: goal.summary.agent_task_count,
        workflow_session_count: goal.summary.workflow_session_count,
    }
}

fn goal_principal(
    auth: Option<&AuthContext>,
) -> Result<crate::db::CommunicationPrincipal, ToolResult> {
    super::communication::communication_principal(auth)
}

fn goal_store_unavailable() -> ToolResult {
    ToolResult::err_with_output(
        "Durable Goal storage is unavailable in this runtime",
        json!({
            "error_kind": "goal_store_unavailable",
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::UserAction, None)
}

fn goal_error(error: GoalStoreError, store_failure_recovery: RecoveryKind) -> ToolResult {
    let recovery = match error.code() {
        "goal_store_unavailable" => store_failure_recovery,
        "goal_not_found" | "goal_revision_changed" | "goal_terminal" => RecoveryKind::Reobserve,
        "goal_idempotency_conflict" => RecoveryKind::Reobserve,
        _ => RecoveryKind::FixInput,
    };
    ToolResult::err_with_output(
        error.message(),
        json!({
            "error_kind": error.code(),
            "message": error.message(),
            "current_revision": error.current_revision(),
            "state_changed": false,
        }),
    )
    .with_recovery(recovery, None)
}

fn target_authorization_error(error: crate::db::CommunicationStoreError) -> ToolResult {
    ToolResult::err_with_output(
        error.message(),
        json!({
            "error_kind": error.code(),
            "message": error.message(),
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::Reobserve, None)
}

fn serialized_goal_success<T: Serialize>(value: T) -> ToolResult {
    match to_value(value) {
        Ok(value) => ToolResult::ok(value),
        Err(error) => ToolResult::err_with_output(
            format!("Failed to serialize durable Goal result: {error}"),
            json!({
                "error_kind": "goal_result_serialization_failed",
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::NoAction, None),
    }
}

impl ToolRuntime {
    pub(crate) fn create_goal(
        &self,
        auth: Option<&AuthContext>,
        title: String,
        objective: String,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        match db.create_goal(
            &principal,
            NewGoal {
                title,
                objective,
                idempotency_key,
            },
        ) {
            Ok(result) => serialized_goal_success(result),
            Err(error) => goal_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn get_goal(&self, auth: Option<&AuthContext>, goal_id: String) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        match db.read_goal(&principal, &goal_id) {
            Ok(goal) => serialized_goal_success(json!({"goal": goal})),
            Err(error) => goal_error(error, RecoveryKind::Reobserve),
        }
    }

    fn exact_goal_plan(&self, auth: Option<&AuthContext>, goal_id: String) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        match db.read_goal(&principal, &goal_id) {
            Ok(goal) => serialized_goal_success(json!({
                "goal_plan": goal_plan_projection(goal),
            })),
            Err(error) => goal_error(error, RecoveryKind::Reobserve),
        }
    }

    pub(crate) fn present_goal_plan(
        &self,
        auth: Option<&AuthContext>,
        goal_id: String,
    ) -> ToolResult {
        self.exact_goal_plan(auth, goal_id)
    }

    pub(crate) fn goal_plan_state(
        &self,
        auth: Option<&AuthContext>,
        goal_id: String,
    ) -> ToolResult {
        self.exact_goal_plan(auth, goal_id)
    }

    pub(crate) fn list_goals(
        &self,
        auth: Option<&AuthContext>,
        lifecycle: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let lifecycle = match lifecycle
            .as_deref()
            .map(GoalLifecycle::from_input)
            .transpose()
        {
            Ok(lifecycle) => lifecycle,
            Err(error) => return goal_error(error, RecoveryKind::FixInput),
        };
        let limit = limit.unwrap_or(DEFAULT_GOAL_LIST_LIMIT);
        if limit == 0 || limit > MAX_GOAL_LIST_LIMIT {
            return goal_error(
                GoalStoreError::new(
                    "invalid_goal_list_limit",
                    format!("limit must be within 1..={MAX_GOAL_LIST_LIMIT}"),
                ),
                RecoveryKind::FixInput,
            );
        }
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        match db.list_goals(&principal, lifecycle, offset.unwrap_or(0), limit) {
            Ok(page) => serialized_goal_success(page),
            Err(error) => goal_error(error, RecoveryKind::Reobserve),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_goal(
        &self,
        auth: Option<&AuthContext>,
        goal_id: String,
        expected_revision: i64,
        title: Option<String>,
        objective: Option<String>,
        lifecycle: Option<String>,
        terminal_reason: Option<String>,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let lifecycle = match lifecycle
            .as_deref()
            .map(GoalLifecycle::from_input)
            .transpose()
        {
            Ok(lifecycle) => lifecycle,
            Err(error) => return goal_error(error, RecoveryKind::FixInput),
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        match db.update_goal(
            &principal,
            &goal_id,
            expected_revision,
            GoalPatch {
                title,
                objective,
                lifecycle,
                terminal_reason,
            },
            &idempotency_key,
        ) {
            Ok(result) => serialized_goal_success(result),
            Err(error) => goal_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) fn associate_goal_agent_task(
        &self,
        auth: Option<&AuthContext>,
        goal_id: String,
        task_id: String,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        // Authorize the Goal first so a foreign Goal cannot be used to probe target ids.
        if let Err(error) = db.read_goal(&principal, &goal_id) {
            return goal_error(error, RecoveryKind::Reobserve);
        }
        // Re-authorize the AgentTask in its own domain. The subsequent Goal record stores
        // only the exact identity; this check is never converted into inherited authority.
        if let Err(error) = db.read_agent_task(&principal, &task_id) {
            return target_authorization_error(error);
        }
        match db.associate_goal_agent_task(&principal, &goal_id, &task_id, &idempotency_key) {
            Ok(result) => serialized_goal_success(result),
            Err(error) => goal_error(error, RecoveryKind::RetrySame),
        }
    }

    pub(crate) async fn associate_goal_workflow_session(
        &self,
        auth: Option<&AuthContext>,
        goal_id: String,
        session_id: String,
        idempotency_key: String,
    ) -> ToolResult {
        let principal = match goal_principal(auth) {
            Ok(principal) => principal,
            Err(result) => return result,
        };
        let Some(db) = self.communication_db.as_ref() else {
            return goal_store_unavailable();
        };
        // Check Goal ownership before looking at the target Session to preserve existence hiding.
        if let Err(error) = db.read_goal(&principal, &goal_id) {
            return goal_error(error, RecoveryKind::Reobserve);
        }
        // This is the existing authoritative Session fence: it verifies the immutable
        // creation-time authority fingerprint and independently re-authorizes a bound Project.
        if let Err(result) = self
            .authorize_session_target(&session_id, "associate_goal_workflow_session", auth)
            .await
        {
            return result;
        }
        match db.associate_goal_workflow_session(
            &principal,
            &goal_id,
            &session_id,
            &idempotency_key,
        ) {
            Ok(result) => serialized_goal_success(result),
            Err(error) => goal_error(error, RecoveryKind::RetrySame),
        }
    }
}
