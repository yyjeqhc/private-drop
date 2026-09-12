//! Runtime handlers for explicit Workflow Session tool calls.

use super::session_context::{
    session_authority_denied_result, session_lifecycle_denied_result, session_message_error_result,
    session_project_mismatch_result, unknown_session_result,
    workflow_session_authority_fingerprint, SessionProjectMismatch,
};
use super::tool_inputs::SessionMode;
use super::{sessions, RecoveryKind, ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;
use serde_json::json;
use sha2::{Digest, Sha256};

impl ToolRuntime {
    pub(crate) async fn dispatch_session_tool(
        &self,
        call: ToolCall,
        auth: Option<&AuthContext>,
        transport: sessions::SessionTransport,
    ) -> ToolResult {
        match call {
            ToolCall::StartSession {
                project,
                title,
                mode,
                deny_write_tools,
                deny_shell_tools,
                execution_context,
            } => {
                self.start_session_tool(
                    project,
                    title,
                    mode,
                    deny_write_tools,
                    deny_shell_tools,
                    execution_context,
                    auth,
                )
                .await
            }
            ToolCall::SessionSummary { session_id, limit } => {
                self.session_summary_tool(session_id, limit, auth).await
            }
            ToolCall::UpdateSessionContext {
                project,
                session_id,
                execution_context,
            } => {
                self.update_session_context_tool(
                    project,
                    session_id,
                    execution_context,
                    auth,
                    transport,
                )
                .await
            }
            ToolCall::CloseSession { session_id } => {
                self.close_session_tool(session_id, auth).await
            }
            ToolCall::ValidationSummary {
                project,
                session_id,
                limit,
            } => {
                self.validation_summary_tool(project, session_id, limit, auth)
                    .await
            }
            ToolCall::PostSessionMessage {
                session_id,
                kind,
                message,
                tags,
                reply_to,
                priority,
                requires_ack,
            } => {
                self.post_session_message_tool(
                    session_id,
                    kind,
                    message,
                    tags,
                    reply_to,
                    priority,
                    requires_ack,
                    auth,
                )
                .await
            }
            ToolCall::ListSessionMessages {
                session_id,
                kind,
                status,
                message_id,
                reply_to,
                limit,
            } => {
                self.list_session_messages_tool(
                    session_id, kind, status, message_id, reply_to, limit, auth,
                )
                .await
            }
            ToolCall::GetSessionAssignment {
                session_id,
                message_id,
            } => {
                self.get_session_assignment_tool(session_id, message_id, auth)
                    .await
            }
            ToolCall::ObserveSessionMessages {
                session_id,
                after_observation_token,
                wait_secs,
                limit,
            } => {
                self.observe_session_messages_tool(
                    session_id,
                    after_observation_token,
                    wait_secs,
                    limit,
                    auth,
                )
                .await
            }
            ToolCall::ResolveSessionMessage {
                session_id,
                message_id,
                resolution,
            } => {
                self.resolve_session_message_tool(session_id, message_id, resolution, auth)
                    .await
            }
            ToolCall::CompleteSessionMessage {
                session_id,
                message_id,
                answer,
                completion_key,
                tags,
                expected_assignment_fence,
                priority,
                trusted_recording_session_id,
            } => {
                self.complete_session_message_tool(
                    session_id,
                    message_id,
                    answer,
                    completion_key,
                    expected_assignment_fence,
                    tags,
                    priority,
                    trusted_recording_session_id,
                    auth,
                )
                .await
            }
            ToolCall::SessionDiscussionSummary { session_id, limit } => {
                self.session_discussion_summary_tool(session_id, limit, auth)
                    .await
            }
            _ => unreachable!("non-session tool routed to session dispatcher"),
        }
    }

    pub(crate) async fn start_session_tool(
        &self,
        project: Option<String>,
        title: Option<String>,
        mode: SessionMode,
        deny_write_tools: bool,
        deny_shell_tools: bool,
        execution_context: Option<sessions::SessionExecutionContext>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let execution_context = match execution_context
            .map(sessions::SessionExecutionContext::validated)
            .transpose()
        {
            Ok(context) => context,
            Err(error) => return invalid_execution_context_result(error),
        };
        let resolved = match project {
            Some(project_input) => match self
                .resolve_project_input_for_auth(&project_input, auth)
                .await
            {
                Ok(resolved) => Some(resolved),
                Err(err) => return err.into_tool_result(),
            },
            None => None,
        };
        // Best-effort load of project-local instruction files (AGENTS.md,
        // CLAUDE.md, ...). Any read failure is swallowed and never fails
        // start_session. `null` when no project was provided.
        let project_instructions = match &resolved {
            Some(resolved) => Some(self.load_project_instructions(&resolved.config).await),
            None => None,
        };
        if resolved.is_none() && auth.is_some_and(AuthContext::is_open_anonymous) {
            return ToolResult::err_with_output(
                "session_owner_identity_unavailable",
                json!({
                    "error_kind": "session_owner_identity_unavailable",
                    "tool_name": "start_session",
                    "state_changed": false,
                }),
            );
        }
        let owner_authority_fingerprint = match workflow_session_authority_fingerprint(auth) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                return ToolResult::err_with_output(
                    "session_authority_identity_unavailable",
                    json!({
                        "error_kind": "session_authority_identity_unavailable",
                        "tool_name": "start_session",
                        "state_changed": false,
                    }),
                );
            }
        };
        let options = sessions::SessionCreateOptions::new(
            resolved
                .as_ref()
                .map(|resolved| resolved.resolved_id.clone()),
            title,
            mode,
            sessions::SessionGuards::effective(
                mode,
                sessions::SessionGuards {
                    deny_write_tools,
                    deny_shell_tools,
                },
            ),
        )
        .with_owner_authority_fingerprint(Some(owner_authority_fingerprint))
        .with_project_instructions(project_instructions.clone())
        .with_execution_context(execution_context.unwrap_or_default());
        let summary = match self.sessions.start_session_with_options(options) {
            Ok(summary) => summary,
            Err(error) => {
                return invalid_execution_context_result(error);
            }
        };
        ToolResult::ok(json!({
            "success": true,
            "session_id": summary.session_id,
            "project": summary.project,
            "project_input": resolved.as_ref().map(|resolved| resolved.input.clone()),
            "resolved_project": resolved.as_ref().map(|resolved| resolved.resolved_id.clone()),
            "title": summary.title,
            "mode": summary.mode,
            "guards": summary.guards,
            "execution_context": summary.execution_context,
            "lifecycle": summary.lifecycle,
            "created_at": summary.created_at,
            "project_instructions": project_instructions,
        }))
    }

    pub(crate) async fn update_session_context_tool(
        &self,
        project: String,
        session_id: String,
        execution_context: sessions::SessionExecutionContext,
        auth: Option<&AuthContext>,
        transport: sessions::SessionTransport,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "update_session_context", auth)
            .await
        {
            return result;
        }
        let resolved = match self.resolve_project_input_for_auth(&project, auth).await {
            Ok(resolved) => resolved,
            Err(err) => return err.into_tool_result(),
        };
        let Some(summary) = self.sessions.summary(&session_id, None) else {
            return unknown_session_result(&session_id);
        };
        if summary.project.as_deref() != Some(resolved.resolved_id.as_str()) {
            let mismatch = SessionProjectMismatch {
                session_project: summary.project.unwrap_or_else(|| "<unscoped>".to_string()),
                request_project: resolved.resolved_id,
            };
            return session_project_mismatch_result(
                &session_id,
                "update_session_context",
                &mismatch,
            );
        }
        if !summary.lifecycle.allows_mutation() {
            return session_lifecycle_denied_result(
                &session_id,
                "update_session_context",
                sessions::SessionLifecycleDenial {
                    lifecycle: summary.lifecycle,
                },
            );
        }
        match self
            .sessions
            .update_execution_context(&session_id, execution_context, transport)
        {
            Ok(outcome) => ToolResult::ok(json!({
                "success": true,
                "session_id": outcome.summary.session_id,
                "project": outcome.summary.project,
                "title": outcome.summary.title,
                "mode": outcome.summary.mode,
                "guards": outcome.summary.guards,
                "lifecycle": outcome.summary.lifecycle,
                "execution_context": outcome.summary.execution_context,
                "previous_execution_context": outcome.previous_execution_context,
                "changed": outcome.changed,
                "created_at": outcome.summary.created_at,
                "updated_at": outcome.summary.updated_at,
            })),
            Err(sessions::SessionExecutionContextUpdateError::UnknownSession) => {
                unknown_session_result(&session_id)
            }
            Err(sessions::SessionExecutionContextUpdateError::SessionNotActive { lifecycle }) => {
                session_lifecycle_denied_result(
                    &session_id,
                    "update_session_context",
                    sessions::SessionLifecycleDenial { lifecycle },
                )
            }
            Err(sessions::SessionExecutionContextUpdateError::SessionHasNoProject) => {
                ToolResult::err_with_output(
                    "session_execution_context_requires_project",
                    json!({
                        "error_kind": "session_execution_context_requires_project",
                        "failure_kind": "invalid_session_state",
                        "session_id": session_id,
                        "state_changed": false,
                    }),
                )
            }
            Err(sessions::SessionExecutionContextUpdateError::InvalidExecutionContext(error)) => {
                invalid_execution_context_result(error)
            }
        }
    }

    pub(crate) async fn session_summary_tool(
        &self,
        session_id: String,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "session_summary", auth)
            .await
        {
            return result;
        }
        match self.sessions.summary(&session_id, limit) {
            Some(summary) => ToolResult::ok(
                serde_json::to_value(summary)
                    .unwrap_or_else(|_| json!({"session_id": session_id, "events": []})),
            ),
            None => unknown_session_result(&session_id),
        }
    }

    pub(crate) async fn close_session_tool(
        &self,
        session_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "close_session", auth)
            .await
        {
            return result;
        }
        match self.sessions.close_session(&session_id) {
            Ok(outcome) => {
                // Commit the lifecycle transition first so an in-flight open
                // cannot escape cleanup by reserving after the initial scan.
                let persistent_shells_closed =
                    self.close_persistent_shells_for_session(&session_id).await;
                ToolResult::ok(json!({
                    "success": true,
                    "session_id": outcome.summary.session_id,
                    "lifecycle": outcome.summary.lifecycle,
                    "already_closed": outcome.already_closed,
                    "persistent_shells_closed": persistent_shells_closed,
                    "updated_at": outcome.summary.updated_at,
                }))
            }
            Err(sessions::SessionCloseError::UnknownSession) => unknown_session_result(&session_id),
        }
    }

    pub(crate) async fn authorize_session_target(
        &self,
        session_id: &str,
        tool_name: &str,
        auth: Option<&AuthContext>,
    ) -> Result<Option<super::project_resolution::ResolvedProject>, ToolResult> {
        let Some((project, owner_authority_fingerprint)) =
            self.sessions.session_target_authority(session_id)
        else {
            return Err(unknown_session_result(session_id));
        };
        if let Some(project) = project {
            // Project authorization and immutable creation-time Session authority
            // are independent checks. Local/dev has no credential project check,
            // but still has the same canonical authority fence as every caller.
            let resolved = if let Some(auth) = auth {
                let resolved = match self
                    .resolve_project_input_for_auth(&project, Some(auth))
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(err) => return Err(err.into_tool_result()),
                };
                if resolved.resolved_id != project {
                    return Err(session_project_mismatch_result(
                        session_id,
                        tool_name,
                        &SessionProjectMismatch {
                            session_project: project,
                            request_project: resolved.resolved_id,
                        },
                    ));
                }
                Some(resolved)
            } else {
                None
            };
            let caller_fingerprint = workflow_session_authority_fingerprint(auth)
                .map_err(|_| session_authority_denied_result(session_id, tool_name))?;
            #[cfg(test)]
            let synthetic_test_fixture = owner_authority_fingerprint
                == sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT;
            #[cfg(not(test))]
            let synthetic_test_fixture = false;
            if !synthetic_test_fixture && owner_authority_fingerprint != caller_fingerprint {
                return Err(session_authority_denied_result(session_id, tool_name));
            }
            return Ok(resolved);
        }

        #[cfg(test)]
        if owner_authority_fingerprint == sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT
        {
            return Ok(None);
        }

        if auth.is_some_and(AuthContext::is_open_anonymous) {
            return Err(session_authority_denied_result(session_id, tool_name));
        }
        let caller_fingerprint = workflow_session_authority_fingerprint(auth)
            .map_err(|_| session_authority_denied_result(session_id, tool_name))?;
        if owner_authority_fingerprint != caller_fingerprint {
            return Err(session_authority_denied_result(session_id, tool_name));
        }
        Ok(None)
    }

    fn trusted_collaboration_author_session(
        &self,
        trusted_recording_session_id: Option<String>,
    ) -> Option<String> {
        // The kernel injects this private field only after explicitly authorizing
        // the recording Session. No auth/window inference is permitted here.
        trusted_recording_session_id
            .filter(|recording_session_id| self.sessions.contains_session(recording_session_id))
    }

    fn completion_key_fingerprint(
        completion_key: String,
    ) -> Result<String, sessions::SessionMessageError> {
        let completion_key = completion_key.trim();
        if completion_key.is_empty()
            || completion_key.chars().count() > sessions::MAX_MESSAGE_COMPLETION_KEY_CHARS
        {
            return Err(sessions::SessionMessageError::InvalidInput(
                "completion_key must contain 1..=128 characters".to_string(),
            ));
        }
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex.session-message-completion.v1\0");
        hasher.update(completion_key.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub(crate) async fn post_session_message_tool(
        &self,
        session_id: String,
        kind: sessions::SessionMessageKind,
        message: String,
        tags: Vec<String>,
        reply_to: Option<String>,
        priority: sessions::SessionMessagePriority,
        requires_ack: bool,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "post_session_message", auth)
            .await
        {
            return result;
        }
        match self.sessions.post_message_with_ack(
            sessions::PostSessionMessageInput {
                session_id: session_id.clone(),
                kind,
                message,
                tags,
                reply_to,
                priority,
            },
            requires_ack,
        ) {
            Ok(message) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "message_id": message.message_id,
                "message": message,
            })),
            Err(err) => session_message_error_result(&session_id, None, err),
        }
    }

    pub(crate) async fn list_session_messages_tool(
        &self,
        session_id: String,
        kind: Option<sessions::SessionMessageKind>,
        status: Option<sessions::SessionMessageStatus>,
        message_id: Option<String>,
        reply_to: Option<String>,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "list_session_messages", auth)
            .await
        {
            return result;
        }
        match self.sessions.list_messages(
            &session_id,
            sessions::ListSessionMessagesFilter {
                kind,
                status,
                message_id,
                reply_to,
                limit,
            },
        ) {
            Ok(messages) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "messages": messages,
            })),
            Err(err) => session_message_error_result(&session_id, None, err),
        }
    }

    pub(crate) async fn get_session_assignment_tool(
        &self,
        session_id: String,
        message_id: String,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "get_session_assignment", auth)
            .await
        {
            return result;
        }
        match self.sessions.get_assignment(&session_id, &message_id) {
            Ok(snapshot) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "message_id": snapshot.todo.message_id,
                "todo": snapshot.todo,
                "direct_replies": snapshot.direct_replies,
                "assignment_fence": snapshot.assignment_fence,
            })),
            Err(err) => session_message_error_result(&session_id, Some(&message_id), err),
        }
    }

    pub(crate) async fn observe_session_messages_tool(
        &self,
        session_id: String,
        after_observation_token: Option<String>,
        wait_secs: Option<u64>,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "observe_session_messages", auth)
            .await
        {
            return result;
        }
        if wait_secs.is_some() && after_observation_token.is_none() {
            return invalid_session_message_observation_request(
                &session_id,
                "wait_secs requires after_observation_token",
            );
        }
        if wait_secs == Some(0) {
            return invalid_session_message_observation_request(
                &session_id,
                "wait_secs must be at least 1",
            );
        }
        if limit == Some(0) {
            return invalid_session_message_observation_request(
                &session_id,
                "limit must be at least 1",
            );
        }
        let wait_secs = wait_secs.map(|wait_secs| wait_secs.min(60));
        let limit = limit.map(|limit| limit.min(sessions::MAX_MESSAGE_LIST_LIMIT));
        match self
            .sessions
            .observe_messages(
                &session_id,
                after_observation_token.as_deref(),
                wait_secs,
                limit,
            )
            .await
        {
            Ok(observation) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "messages": observation.messages,
                "observation_token": observation.observation_token,
                "changed": observation.changed,
                "wait_outcome": observation.wait_outcome,
                "waited_ms": observation.waited_ms,
                "history_lost": observation.history_lost,
                "has_more": observation.has_more,
            })),
            Err(err) => session_message_observation_error_result(&session_id, err),
        }
    }

    pub(crate) async fn resolve_session_message_tool(
        &self,
        session_id: String,
        message_id: String,
        resolution: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "resolve_session_message", auth)
            .await
        {
            return result;
        }
        match self
            .sessions
            .resolve_message(&session_id, &message_id, resolution)
        {
            Ok(message) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "message_id": message.message_id,
                "message": message,
            })),
            Err(err) => session_message_error_result(&session_id, Some(&message_id), err),
        }
    }

    pub(crate) async fn complete_session_message_tool(
        &self,
        session_id: String,
        message_id: String,
        answer: String,
        completion_key: String,
        expected_assignment_fence: String,
        tags: Vec<String>,
        priority: sessions::SessionMessagePriority,
        trusted_recording_session_id: Option<String>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "complete_session_message", auth)
            .await
        {
            return result;
        }
        let completion_id = match Self::completion_key_fingerprint(completion_key) {
            Ok(completion_id) => completion_id,
            Err(err) => return session_message_error_result(&session_id, Some(&message_id), err),
        };
        let author_session_id =
            self.trusted_collaboration_author_session(trusted_recording_session_id);
        match self
            .sessions
            .complete_message(sessions::CompleteSessionMessageInput {
                session_id: session_id.clone(),
                message_id: message_id.clone(),
                answer,
                tags,
                priority,
                completion_id: completion_id.clone(),
                author_session_id,
                expected_assignment_fence,
            }) {
            Ok(outcome) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "message_id": outcome.todo.message_id,
                "answer_message_id": outcome.answer.message_id,
                "completion_id": completion_id,
                "replayed": outcome.replayed,
                "todo": outcome.todo,
                "answer": outcome.answer,
            })),
            Err(err) => session_message_error_result(&session_id, Some(&message_id), err),
        }
    }

    pub(crate) async fn session_discussion_summary_tool(
        &self,
        session_id: String,
        limit: Option<usize>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        if let Err(result) = self
            .authorize_session_target(&session_id, "session_discussion_summary", auth)
            .await
        {
            return result;
        }
        match self.sessions.discussion_summary(&session_id, limit) {
            Ok(summary) => ToolResult::ok(json!({
                "success": true,
                "session_id": session_id,
                "counts": summary.counts,
                "open_guidance": summary.open_guidance,
                "open_questions": summary.open_questions,
                "open_risks": summary.open_risks,
                "open_todos": summary.open_todos,
                "high_priority_open_todos": summary.high_priority_open_todos,
                "recent_answers": summary.recent_answers,
                "recent_completions": summary.recent_completions,
                "recent_progress": summary.recent_progress,
                "recent_decisions": summary.recent_decisions,
            })),
            Err(err) => session_message_error_result(&session_id, None, err),
        }
    }
}

fn invalid_session_message_observation_request(session_id: &str, message: &str) -> ToolResult {
    ToolResult::err_with_output(
        message,
        json!({
            "error_kind": "invalid_session_message_observation_request",
            "failure_kind": "invalid_arguments",
            "session_id": session_id,
            "state_changed": false,
        }),
    )
    .with_recovery(RecoveryKind::FixInput, None)
}

fn session_message_observation_error_result(
    session_id: &str,
    error: sessions::SessionMessageObservationError,
) -> ToolResult {
    match error {
        sessions::SessionMessageObservationError::UnknownSession => {
            unknown_session_result(session_id)
        }
        sessions::SessionMessageObservationError::MalformedToken
        | sessions::SessionMessageObservationError::OversizedToken
        | sessions::SessionMessageObservationError::WrongSession
        | sessions::SessionMessageObservationError::FutureRevision => ToolResult::err_with_output(
            "invalid_session_message_observation_token",
            json!({
                "error_kind": "invalid_session_message_observation_token",
                "failure_kind": "invalid_arguments",
                "session_id": session_id,
                "state_changed": false,
            }),
        )
        .with_recovery(RecoveryKind::FixInput, None),
        sessions::SessionMessageObservationError::InvalidObservationState => {
            ToolResult::err_with_output(
                "invalid_message_observation_state",
                json!({
                    "error_kind": "invalid_message_observation_state",
                    "session_id": session_id,
                    "state_changed": false,
                }),
            )
            .with_recovery(RecoveryKind::NoAction, None)
        }
    }
}

fn invalid_execution_context_result(error: String) -> ToolResult {
    ToolResult::err_with_output(
        error,
        json!({
            "error_kind": "invalid_execution_context",
            "failure_kind": "invalid_arguments",
            "field": "execution_context",
            "state_changed": false,
        }),
    )
}
