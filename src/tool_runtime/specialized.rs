//! Server-owned governance for canonical gateway tools whose exact action policy
//! cannot be represented by one static ToolDefinition contract.
//!
//! Plugin and managed SSH gateways keep their native execution protocols. This
//! layer owns only the authority facts that must be resolved before any effect:
//! exact scope, Workflow Session authority/guards, permission policy, and a
//! bounded ledger lifecycle.

use serde_json::{json, Value};

use super::permissions::{
    add_permission_to_result, permission_execution_denied_result, PermissionDecision,
};
use super::session_context::{session_guard_denied_result, session_lifecycle_denied_result};
use super::sessions::{
    SessionPathHint, SessionToolContract, SessionTransport, ToolCallRecorderMetadata, ToolCallStart,
};
use super::{ToolResult, ToolRuntime};
use crate::auth::AuthContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecializedSource {
    Plugin,
    SshResource,
}

impl SpecializedSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::SshResource => "ssh-resource",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecializedEffect {
    Read,
    LocalExecution,
    Management,
}

impl SpecializedEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::LocalExecution => "local_execution",
            Self::Management => "management",
        }
    }

    fn consequential(self) -> bool {
        !matches!(self, Self::Read)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpecializedOperationPolicy {
    pub(crate) source: SpecializedSource,
    pub(crate) operation: &'static str,
    pub(crate) required_scope: &'static str,
    pub(crate) effect: SpecializedEffect,
    pub(crate) risk: &'static str,
    /// Management operations which mutate durable/local state are write-like.
    pub(crate) write_like: bool,
    /// Operations that start or invoke local executable code are shell-like for
    /// Workflow Session guard purposes, irrespective of provider annotations.
    pub(crate) shell_like: bool,
}

impl SpecializedOperationPolicy {
    pub(crate) fn read(
        source: SpecializedSource,
        operation: &'static str,
        required_scope: &'static str,
    ) -> Self {
        Self {
            source,
            operation,
            required_scope,
            effect: SpecializedEffect::Read,
            risk: "specialized_read",
            write_like: false,
            shell_like: false,
        }
    }

    pub(crate) fn local_execution(
        source: SpecializedSource,
        operation: &'static str,
        required_scope: &'static str,
    ) -> Self {
        Self {
            source,
            operation,
            required_scope,
            effect: SpecializedEffect::LocalExecution,
            risk: "specialized_local_execution",
            write_like: false,
            shell_like: true,
        }
    }

    pub(crate) fn management(
        source: SpecializedSource,
        operation: &'static str,
        required_scope: &'static str,
        write_like: bool,
        shell_like: bool,
    ) -> Self {
        Self {
            source,
            operation,
            required_scope,
            effect: SpecializedEffect::Management,
            risk: "specialized_management",
            write_like,
            shell_like,
        }
    }

    fn session_contract(self) -> SessionToolContract {
        SessionToolContract {
            risk_class: self.risk,
            read_like: self.effect == SpecializedEffect::Read,
            write_like: self.write_like,
            shell_like: self.shell_like,
            git_like: false,
            change_summary_like: false,
            project_write: false,
            path_hint: SessionPathHint::None,
            accepts_context_ack: false,
            advances_context_checkpoint: false,
        }
    }

    pub(crate) fn audit_projection(self) -> Value {
        json!({
            "source": self.source.as_str(),
            "operation": self.operation,
            "effect": self.effect.as_str(),
            "risk": self.risk,
            "required_scope": self.required_scope,
            "permission_required": self.effect.consequential(),
        })
    }
}

#[derive(Debug)]
pub(crate) enum SpecializedGovernanceDenial {
    Scope {
        required_scope: &'static str,
        description: String,
    },
    Tool(ToolResult),
}

#[derive(Debug)]
pub(crate) struct SpecializedInvocationPermit {
    policy: SpecializedOperationPolicy,
    session_start: Option<ToolCallStart>,
    permission: Option<PermissionDecision>,
}

fn bounded_ledger_arguments(policy: SpecializedOperationPolicy, identity: &Value) -> Value {
    json!({
        "specialized": policy.audit_projection(),
        "identity": identity,
    })
}

fn denial_terminal_projection(policy: SpecializedOperationPolicy, kind: &str) -> Value {
    json!({
        "specialized": policy.audit_projection(),
        "dispatch_certainty": "not_started",
        "success": false,
        "failure_kind": kind,
    })
}

impl ToolRuntime {
    /// Resolve the shared authority boundary for one already-classified
    /// specialized operation. Transport is explicit provenance only; it never
    /// changes authority or infers a Workflow Session.
    pub(crate) async fn govern_specialized_invocation(
        &self,
        external_tool_name: &str,
        policy: SpecializedOperationPolicy,
        transport: SessionTransport,
        recording_session_id: Option<&str>,
        auth: Option<&AuthContext>,
        identity: &Value,
    ) -> Result<SpecializedInvocationPermit, SpecializedGovernanceDenial> {
        if !auth.is_some_and(|auth| auth.has_scope(policy.required_scope)) {
            return Err(SpecializedGovernanceDenial::Scope {
                required_scope: policy.required_scope,
                description: format!(
                    "{} operation '{}' requires the {} scope",
                    policy.source.as_str(),
                    policy.operation,
                    policy.required_scope
                ),
            });
        }

        let mut resolved_session_project = None;
        if let Some(session_id) = recording_session_id {
            match self
                .authorize_session_target(session_id, external_tool_name, auth)
                .await
            {
                Ok(resolved) => {
                    resolved_session_project = resolved.map(|project| project.resolved_id);
                }
                Err(mut result) => {
                    result.output["dispatch_certainty"] = Value::String("not_started".to_string());
                    return Err(SpecializedGovernanceDenial::Tool(result));
                }
            }
        }

        let contract = policy.session_contract();
        let recorder_metadata = ToolCallRecorderMetadata {
            recording_session_id: recording_session_id.map(str::to_string),
            recording_session_project: resolved_session_project.clone(),
            recording_session_authorized: recording_session_id.is_some(),
            ..Default::default()
        };
        let mut session_start = self.sessions.record_tool_call_started_with_metadata(
            recording_session_id,
            transport,
            external_tool_name,
            &bounded_ledger_arguments(policy, identity),
            resolved_session_project.clone(),
            recorder_metadata,
            contract,
        );

        if let Some(session_id) = recording_session_id {
            if let Some(denial) =
                self.sessions
                    .lifecycle_denial(session_id, external_tool_name, contract)
            {
                let mut result =
                    session_lifecycle_denied_result(session_id, external_tool_name, denial);
                result.output["dispatch_certainty"] = Value::String("not_started".to_string());
                self.sessions.record_model_facing_tool_call_finished(
                    session_start,
                    false,
                    &denial_terminal_projection(policy, "session_lifecycle_denied"),
                    result.error.as_deref(),
                    Some("session_lifecycle_denied"),
                );
                return Err(SpecializedGovernanceDenial::Tool(result));
            }
            if let Some(denial) = self.sessions.guard_denial(session_id, contract) {
                let mut result =
                    session_guard_denied_result(session_id, external_tool_name, denial);
                result.output["dispatch_certainty"] = Value::String("not_started".to_string());
                self.sessions.record_model_facing_tool_call_finished(
                    session_start,
                    false,
                    &denial_terminal_projection(policy, "session_guard_denied"),
                    result.error.as_deref(),
                    Some("session_guard_denied"),
                );
                return Err(SpecializedGovernanceDenial::Tool(result));
            }
        }

        let permission = if policy.effect.consequential() {
            let decision = self.permission_evaluator.evaluate_resolved_required(
                external_tool_name,
                resolved_session_project.as_deref(),
                policy.risk,
            );
            if let Some(start) = session_start.as_mut() {
                self.sessions
                    .record_permission_decision(start, decision.clone());
            }
            if decision.status == "denied" || decision.status == "pending" {
                let mut result = permission_execution_denied_result(&decision);
                add_permission_to_result(&mut result, &decision);
                result.output["dispatch_certainty"] = Value::String("not_started".to_string());
                self.sessions.record_model_facing_tool_call_finished(
                    session_start,
                    false,
                    &denial_terminal_projection(policy, "permission_denied"),
                    result.error.as_deref(),
                    Some("permission_denied"),
                );
                return Err(SpecializedGovernanceDenial::Tool(result));
            }
            Some(decision)
        } else {
            None
        };

        Ok(SpecializedInvocationPermit {
            policy,
            session_start,
            permission,
        })
    }

    /// Close the authoritative specialized ledger lifecycle with only bounded
    /// terminal facts. Provider output and raw arguments never enter Session
    /// evidence here.
    pub(crate) fn finish_specialized_invocation(
        &self,
        permit: SpecializedInvocationPermit,
        success: bool,
        dispatch_certainty: &str,
        failure_kind: Option<&str>,
    ) {
        let terminal = json!({
            "specialized": permit.policy.audit_projection(),
            "dispatch_certainty": dispatch_certainty,
            "success": success,
            "failure_kind": failure_kind,
            "permission_status": permit.permission.as_ref().map(|decision| decision.status.as_str()),
        });
        self.sessions.record_model_facing_tool_call_finished(
            permit.session_start,
            success,
            &terminal,
            (!success).then_some("specialized operation failed"),
            failure_kind,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, AuthKind, SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE};
    use crate::tool_runtime::permissions::{AuthorityMode, PermissionEvaluator};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn auth(owner: &str, scopes: &[&str]) -> AuthContext {
        let mut auth = AuthContext::new(AuthKind::ApiToken);
        auth.user_id = Some(format!("user-{owner}"));
        auth.username = Some(owner.to_string());
        auth.api_key_id = Some(format!("key-{owner}"));
        auth.scopes
            .extend(scopes.iter().map(|scope| (*scope).to_string()));
        auth
    }

    fn session(
        runtime: &ToolRuntime,
        owner: &AuthContext,
        mode: crate::tool_runtime::SessionMode,
    ) -> crate::tool_runtime::sessions::SessionSummary {
        let fingerprint = crate::tool_runtime::workflow_session_authority_fingerprint(Some(owner))
            .expect("test authority fingerprint");
        runtime
            .sessions
            .start_session_with_options(
                crate::tool_runtime::sessions::SessionCreateOptions::new(
                    None,
                    Some("specialized governance".to_string()),
                    mode,
                    crate::tool_runtime::sessions::SessionGuards::default(),
                )
                .with_owner_authority_fingerprint(Some(fingerprint)),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn specialized_read_only_session_allows_read_and_denies_local_execution() {
        let runtime = ToolRuntime::new_for_tests();
        let auth = auth("alice", &[SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE]);
        let session = session(&runtime, &auth, crate::tool_runtime::SessionMode::ReadOnly);

        let read = runtime
            .govern_specialized_invocation(
                "plugin_tool",
                SpecializedOperationPolicy::read(
                    SpecializedSource::Plugin,
                    "list",
                    SCOPE_PLUGIN_INSPECT,
                ),
                SessionTransport::Mcp,
                Some(&session.session_id),
                Some(&auth),
                &json!({"plugin": "repo-tools"}),
            )
            .await
            .expect("read-like Plugin inspection remains allowed");
        runtime.finish_specialized_invocation(read, true, "completed", None);

        let denied = runtime
            .govern_specialized_invocation(
                "plugin_tool",
                SpecializedOperationPolicy::local_execution(
                    SpecializedSource::Plugin,
                    "call",
                    SCOPE_PLUGIN_INVOKE,
                ),
                SessionTransport::Mcp,
                Some(&session.session_id),
                Some(&auth),
                &json!({"plugin": "repo-tools"}),
            )
            .await
            .expect_err("read-only Session must deny local Plugin execution");
        let SpecializedGovernanceDenial::Tool(result) = denied else {
            panic!("expected Session guard denial");
        };
        assert_eq!(result.output["error_kind"], "session_guard_denied");
        assert_eq!(result.output["dispatch_certainty"], "not_started");
    }

    #[tokio::test]
    async fn specialized_permission_evaluator_denies_effect_before_dispatch_but_skips_reads() {
        let counter = Arc::new(AtomicUsize::new(0));
        let runtime = ToolRuntime::new_for_tests().with_permission_evaluator(
            PermissionEvaluator::with_mode(AuthorityMode::Restricted)
                .with_eval_counter(counter.clone()),
        );
        let auth = auth("alice", &[SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE]);

        let read = runtime
            .govern_specialized_invocation(
                "plugin_tool",
                SpecializedOperationPolicy::read(
                    SpecializedSource::Plugin,
                    "list",
                    SCOPE_PLUGIN_INSPECT,
                ),
                SessionTransport::Mcp,
                None,
                Some(&auth),
                &json!({}),
            )
            .await
            .expect("read-like operation skips permission approval");
        runtime.finish_specialized_invocation(read, true, "completed", None);
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        let denied = runtime
            .govern_specialized_invocation(
                "plugin_tool",
                SpecializedOperationPolicy::local_execution(
                    SpecializedSource::Plugin,
                    "call",
                    SCOPE_PLUGIN_INVOKE,
                ),
                SessionTransport::Mcp,
                None,
                Some(&auth),
                &json!({}),
            )
            .await
            .expect_err("restricted authority must deny specialized local execution");
        let SpecializedGovernanceDenial::Tool(result) = denied else {
            panic!("expected permission denial");
        };
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(result.output["failure_kind"], "permission_denied");
        assert_eq!(result.output["dispatch_certainty"], "not_started");
    }

    #[tokio::test]
    async fn specialized_recording_session_authority_is_exact_and_fail_closed() {
        let runtime = ToolRuntime::new_for_tests();
        let owner = auth("alice", &[SCOPE_PLUGIN_INSPECT]);
        let foreign = auth("bob", &[SCOPE_PLUGIN_INSPECT]);
        let session = session(&runtime, &owner, crate::tool_runtime::SessionMode::Normal);

        let denied = runtime
            .govern_specialized_invocation(
                "plugin_tool",
                SpecializedOperationPolicy::read(
                    SpecializedSource::Plugin,
                    "list",
                    SCOPE_PLUGIN_INSPECT,
                ),
                SessionTransport::Mcp,
                Some(&session.session_id),
                Some(&foreign),
                &json!({}),
            )
            .await
            .expect_err("foreign authority must not attach to exact recording Session");
        let SpecializedGovernanceDenial::Tool(result) = denied else {
            panic!("expected exact Session authority denial");
        };
        assert_eq!(result.output["failure_kind"], "session_authority_denied");
        assert_eq!(result.output["dispatch_certainty"], "not_started");
    }
}
