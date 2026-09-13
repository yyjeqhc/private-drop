//! Scope definitions and validation for the WebCodex auth system.
//!
//! Scopes are string-based permissions carried by authenticated principals.
//! Bootstrap auth is treated as holding every scope; managed tokens, delegated
//! OAuth access tokens, and lightweight contexts carry explicit granted scopes.

use std::collections::HashSet;

use super::context::AuthContext;
use crate::tool_runtime::metadata::lookup_tool_metadata;
pub(crate) use webcodex_core::authority::ToolAuthorityPolicy as OAuthToolScopePolicy;

// ---------------------------------------------------------------------------
// Scope constants
// ---------------------------------------------------------------------------

#[allow(unused_imports)]
pub use webcodex_core::authority::{
    AGENT_SCOPES, COMMUNICATION_MANAGE_SCOPES, COMMUNICATION_READ_SCOPES, KNOWN_SCOPES,
    MEMORY_MANAGE_SCOPES, MEMORY_READ_SCOPES, SCOPE_ACCOUNT_MANAGE, SCOPE_ADMIN,
    SCOPE_AGENT_JOB_UPDATE, SCOPE_AGENT_POLL, SCOPE_AGENT_REGISTER, SCOPE_AGENT_RESULT,
    SCOPE_CODING_AGENT_RUN, SCOPE_COMMUNICATION_MANAGE, SCOPE_COMMUNICATION_READ,
    SCOPE_COMPUTER_CLIPBOARD_READ, SCOPE_COMPUTER_CLIPBOARD_WRITE, SCOPE_COMPUTER_CONTROL,
    SCOPE_COMPUTER_DISPLAY_READ, SCOPE_COMPUTER_LAUNCH, SCOPE_COMPUTER_POINTER_CONTROL,
    SCOPE_COMPUTER_READ, SCOPE_JOB_DETACH, SCOPE_JOB_RUN, SCOPE_MCP_LOCAL, SCOPE_MEMORY_MANAGE,
    SCOPE_MEMORY_READ, SCOPE_PLUGIN_INSPECT, SCOPE_PLUGIN_INVOKE, SCOPE_PLUGIN_MANAGE,
    SCOPE_PROJECT_READ, SCOPE_PROJECT_WRITE, SCOPE_RUNNER_MANAGE, SCOPE_RUNTIME_READ,
    SCOPE_SESSION_COLLABORATE, SCOPE_SSH_LOCAL,
};

/// True when `scope` is one of the Runner transport scopes.
pub(crate) fn is_agent_scope(scope: &str) -> bool {
    AGENT_SCOPES.contains(&scope)
}

// ---------------------------------------------------------------------------
// Scope validation
// ---------------------------------------------------------------------------

/// Validate and normalize a list of Runner transport scopes. Returns an error
/// if any scope is not an `agent:*` scope. Rejects duplicates and unknown
/// scopes.
pub(crate) fn validate_agent_scopes(scopes: &[String]) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(scopes.len());
    for raw in scopes {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if !is_agent_scope(s) {
            return Err(format!(
                "agent tokens may only carry agent:* scopes; got '{}'",
                s
            ));
        }
        if !seen.insert(s.to_string()) {
            continue;
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Validate and normalize a list of scopes. Returns the cleaned scope list.
/// Rejects duplicates and unknown scopes.
pub(crate) fn validate_scopes(scopes: &[String]) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(scopes.len());
    for raw in scopes {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if !KNOWN_SCOPES.contains(&s) {
            return Err(format!("unknown scope '{}'", s));
        }
        if !seen.insert(s.to_string()) {
            continue;
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Serialize a scope list into the space-separated storage form.
pub(crate) fn scopes_to_string(scopes: &[String]) -> String {
    scopes.join(" ")
}

// ---------------------------------------------------------------------------
// Scope authorization helpers (used by handlers and middleware)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Authenticated route/tool scope policy
// ---------------------------------------------------------------------------
// The OAuth-prefixed type/function names below are retained for compatibility
// with the existing policy registry. Enforcement is principal-neutral; OAuth
// remains special only for delegated-scope issuance and wire error framing.

#[allow(unused_imports)]
pub(crate) use webcodex_core::authority::{OAuthBodyAwarePolicy, OAuthRouteScopePolicy};

pub(crate) fn oauth_route_scope_policy_for_path_method(
    method: &str,
    path: &str,
) -> OAuthRouteScopePolicy {
    crate::route_metadata::lookup(method, path)
        .map(|spec| spec.scope_policy)
        .unwrap_or(OAuthRouteScopePolicy::Unknown)
}

pub(crate) fn oauth_scope_policy_for_runtime_tool(tool_name: &str) -> OAuthToolScopePolicy {
    lookup_tool_metadata(tool_name)
        .map(|metadata| metadata.authority)
        .unwrap_or(OAuthToolScopePolicy::Unknown)
}

#[cfg(test)]
pub(crate) fn required_oauth_scope_for_path_method(
    method: &str,
    path: &str,
) -> Option<&'static str> {
    match oauth_route_scope_policy_for_path_method(method, path) {
        OAuthRouteScopePolicy::Require(scope) => Some(scope),
        _ => None,
    }
}

pub(crate) fn enforce_route_scope(
    ctx: &AuthContext,
    method: &str,
    path: &str,
) -> Result<(), (Option<&'static str>, String)> {
    match oauth_route_scope_policy_for_path_method(method, path) {
        OAuthRouteScopePolicy::Public | OAuthRouteScopePolicy::BodyAware(_) => Ok(()),
        OAuthRouteScopePolicy::Require(scope) => {
            if ctx.has_scope(scope) {
                Ok(())
            } else {
                Err((Some(scope), format!("missing required scope: {}", scope)))
            }
        }
        OAuthRouteScopePolicy::BootstrapOnly => {
            if ctx.is_bootstrap() {
                Ok(())
            } else {
                Err((None, "route requires bootstrap authority".to_string()))
            }
        }
        OAuthRouteScopePolicy::FirstPartyOnly => {
            if matches!(
                ctx.kind,
                super::context::AuthKind::Bootstrap | super::context::AuthKind::ApiToken
            ) {
                Ok(())
            } else {
                Err((
                    None,
                    "route requires a first-party bootstrap or personal API token".to_string(),
                ))
            }
        }
        // Runner transport identity and exact agent:* scopes are enforced by the
        // dedicated surface gate and transport handlers. Do not reinterpret
        // those credentials as ordinary runtime principals here.
        OAuthRouteScopePolicy::AgentSurface => Ok(()),
        OAuthRouteScopePolicy::Unknown => {
            if ctx.is_bootstrap() {
                Ok(())
            } else {
                Err((
                    None,
                    "authenticated route has no declared scope policy".to_string(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_runtime::metadata::lookup_tool_metadata;
    use crate::tool_runtime::tool_definition::known_tool_names;

    #[test]
    fn validate_scopes_rejects_unknown() {
        assert!(validate_scopes(&["runtime:read".to_string()]).is_ok());
        assert!(validate_scopes(&["bogus:scope".to_string()]).is_err());
    }

    #[test]
    fn validate_scopes_rejects_duplicates() {
        let result =
            validate_scopes(&["runtime:read".to_string(), "runtime:read".to_string()]).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn validate_agent_scopes_rejects_non_agent() {
        assert!(validate_agent_scopes(&["agent:register".to_string()]).is_ok());
        assert!(validate_agent_scopes(
            &["agent:register".to_string(), "runtime:read".to_string(),]
        )
        .is_err());
        assert!(validate_agent_scopes(&["admin".to_string()]).is_err());
    }

    #[test]
    fn scopes_to_string_round_trips() {
        let scopes = vec!["runtime:read".to_string(), "project:read".to_string()];
        let s = scopes_to_string(&scopes);
        assert_eq!(s, "runtime:read project:read");
    }
    #[test]
    fn oauth_route_policy_public_endpoints() {
        for (method, path) in [
            ("GET", "/.well-known/oauth-protected-resource"),
            ("GET", "/.well-known/oauth-authorization-server"),
            ("POST", "/oauth/token"),
            ("POST", "/oauth/revoke"),
            ("POST", "/oauth/authorize/login"),
            ("POST", "/oauth/authorize/consent"),
            ("POST", "/oauth/authorize/bridge"),
        ] {
            assert_eq!(
                oauth_route_scope_policy_for_path_method(method, path),
                OAuthRouteScopePolicy::Public,
                "{method} {path}"
            );
            assert_eq!(required_oauth_scope_for_path_method(method, path), None);
        }
    }

    #[test]
    fn oauth_route_policy_authorize_is_first_party_only() {
        assert_eq!(
            oauth_route_scope_policy_for_path_method("GET", "/oauth/authorize"),
            OAuthRouteScopePolicy::FirstPartyOnly
        );
        assert_eq!(
            required_oauth_scope_for_path_method("GET", "/oauth/authorize"),
            None
        );
    }

    #[test]
    fn oauth_route_policy_oauth_client_management_is_first_party_only() {
        for path in [
            "/api/oauth/clients/create",
            "/api/oauth/clients/list",
            "/api/oauth/clients/update_scopes",
            "/api/oauth/clients/revoke",
        ] {
            assert_eq!(
                oauth_route_scope_policy_for_path_method("POST", path),
                OAuthRouteScopePolicy::FirstPartyOnly,
                "POST {path}"
            );
            assert_eq!(required_oauth_scope_for_path_method("POST", path), None);
        }
    }

    #[test]
    fn oauth_route_policy_agent_surfaces() {
        for (method, path) in [
            ("POST", "/api/pairing/enroll"),
            ("POST", "/api/shell/agent/register"),
            ("POST", "/api/shell/agent/offline"),
            ("POST", "/api/shell/agent/poll"),
            ("POST", "/api/shell/agent/result"),
            ("POST", "/api/shell/agent/persistent_shell_result"),
            ("POST", "/api/shell/agent/job_update"),
            ("GET", "/api/agents/ws"),
        ] {
            assert_eq!(
                oauth_route_scope_policy_for_path_method(method, path),
                OAuthRouteScopePolicy::AgentSurface,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn oauth_route_policy_simple_require_scopes() {
        for (method, path, scope) in [
            ("GET", "/mcp", SCOPE_RUNTIME_READ),
            ("POST", "/api/runtime/status", SCOPE_RUNTIME_READ),
            ("POST", "/api/runtime-console/overview", SCOPE_RUNTIME_READ),
            ("POST", "/api/runtime-console/runner", SCOPE_RUNTIME_READ),
            (
                "POST",
                "/api/runtime-console/workflow-session-messages",
                SCOPE_RUNTIME_READ,
            ),
            (
                "POST",
                "/api/runtime-console/workflow-session-observe",
                SCOPE_RUNTIME_READ,
            ),
            (
                "POST",
                "/api/runtime-console/workflow-session-post-message",
                SCOPE_SESSION_COLLABORATE,
            ),
            (
                "POST",
                "/api/runtime-console/workflow-session-withdraw-message",
                SCOPE_SESSION_COLLABORATE,
            ),
            (
                "POST",
                "/api/runtime-console/workflow-session-replace-message",
                SCOPE_SESSION_COLLABORATE,
            ),
            ("POST", "/api/tools/list", SCOPE_RUNTIME_READ),
            ("POST", "/api/connector/task/start", SCOPE_RUNTIME_READ),
            ("POST", "/api/connector/files/read", SCOPE_PROJECT_READ),
            ("POST", "/api/connector/code/navigate", SCOPE_PROJECT_READ),
            ("POST", "/api/connector/code/impact", SCOPE_PROJECT_READ),
            ("POST", "/api/connector/edits/apply", SCOPE_PROJECT_WRITE),
            ("POST", "/api/connector/checks/run", SCOPE_JOB_RUN),
            ("POST", "/api/connector/task/cancel", SCOPE_JOB_RUN),
            ("POST", "/api/connector/task/finish", SCOPE_PROJECT_WRITE),
            ("POST", "/api/projects/git_status", SCOPE_PROJECT_READ),
            ("POST", "/api/runtime-console/projects", SCOPE_PROJECT_READ),
            (
                "POST",
                "/api/runtime-console/workflow-sessions",
                SCOPE_PROJECT_READ,
            ),
            (
                "POST",
                "/api/runtime-console/workflow-session",
                SCOPE_PROJECT_READ,
            ),
            ("POST", "/api/projects/run_job", SCOPE_JOB_RUN),
            ("POST", "/api/users/me", SCOPE_ACCOUNT_MANAGE),
            ("POST", "/api/tokens/list", SCOPE_ACCOUNT_MANAGE),
            ("POST", "/api/audit/stats", SCOPE_ACCOUNT_MANAGE),
        ] {
            assert_eq!(
                oauth_route_scope_policy_for_path_method(method, path),
                OAuthRouteScopePolicy::Require(scope),
                "{method} {path}"
            );
            assert_eq!(
                required_oauth_scope_for_path_method(method, path),
                Some(scope),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn route_scope_enforcement_is_principal_neutral() {
        let mut pat = AuthContext::new(super::super::context::AuthKind::ApiToken);
        pat.scopes = vec![SCOPE_RUNTIME_READ.to_string()];
        let mut oauth = AuthContext::new(super::super::context::AuthKind::OAuth2Token);
        oauth.scopes = vec![SCOPE_RUNTIME_READ.to_string()];
        let shared = crate::auth::shared_key_context("scope-matrix");
        let bootstrap = crate::auth::bootstrap_context();

        for path in [
            "/api/runtime/status",
            "/api/runtime-console/overview",
            "/api/runtime-console/runner",
            "/api/runtime-console/workflow-session-messages",
            "/api/runtime-console/workflow-session-observe",
        ] {
            for (label, auth) in [("pat", &pat), ("oauth", &oauth), ("shared", &shared)] {
                assert!(
                    enforce_route_scope(auth, "POST", path).is_ok(),
                    "{label} should honor runtime:read on {path}"
                );
            }
        }
        for path in [
            "/api/runtime-console/workflow-session-post-message",
            "/api/runtime-console/workflow-session-withdraw-message",
            "/api/runtime-console/workflow-session-replace-message",
        ] {
            for (label, auth) in [("pat", &pat), ("oauth", &oauth)] {
                assert_eq!(
                    enforce_route_scope(auth, "POST", path),
                    Err((
                        Some(SCOPE_SESSION_COLLABORATE),
                        "missing required scope: session:collaborate".to_string()
                    )),
                    "{label} runtime:read must not mutate Session collaboration on {path}"
                );
            }
            assert!(
                enforce_route_scope(&shared, "POST", path).is_ok(),
                "direct shared key should retain Session collaboration on {path}"
            );
        }
        for (label, auth) in [("pat", &pat), ("oauth", &oauth)] {
            assert_eq!(
                enforce_route_scope(auth, "POST", "/api/projects/git_status"),
                Err((
                    Some(SCOPE_PROJECT_READ),
                    "missing required scope: project:read".to_string()
                )),
                "{label} must not bypass missing project:read"
            );
        }
        for (label, auth) in [("pat", &pat), ("oauth", &oauth)] {
            assert_eq!(
                enforce_route_scope(auth, "POST", "/api/runtime-console/projects"),
                Err((
                    Some(SCOPE_PROJECT_READ),
                    "missing required scope: project:read".to_string()
                )),
                "{label} must not use Runtime Console without project:read"
            );
        }
        assert!(
            enforce_route_scope(&shared, "POST", "/api/projects/git_status").is_ok(),
            "direct shared key should use its declared project:read scope"
        );
        assert!(
            enforce_route_scope(&shared, "POST", "/api/runtime-console/projects").is_ok(),
            "direct shared key should retain its existing project:read Runtime Console access"
        );
        assert!(
            enforce_route_scope(&pat, "POST", "/api/oauth/clients/list").is_ok(),
            "PAT remains an allowed first-party identity"
        );
        assert!(
            enforce_route_scope(&oauth, "POST", "/api/oauth/clients/list").is_err(),
            "OAuth delegation cannot become first-party client management"
        );
        assert!(
            enforce_route_scope(&shared, "POST", "/api/future/authenticated-route").is_err(),
            "ordinary principals must fail closed on undeclared routes"
        );
        assert!(
            enforce_route_scope(&bootstrap, "POST", "/api/future/authenticated-route").is_ok(),
            "bootstrap keeps explicit superuser compatibility"
        );
    }

    #[test]
    fn api_token_account_manage_routes_require_explicit_scope() {
        let mut pat = AuthContext::new(super::super::context::AuthKind::ApiToken);
        pat.scopes = vec![SCOPE_RUNTIME_READ.to_string()];
        let paths = [
            "/api/users/create",
            "/api/users/list",
            "/api/users/me",
            "/api/tokens/create",
            "/api/tokens/register_hash",
            "/api/tokens/list",
            "/api/tokens/revoke",
            "/api/agent-tokens/create",
            "/api/agent-tokens/register_hash",
            "/api/agent-tokens/list",
            "/api/agent-tokens/revoke",
            "/api/pairing/create",
            "/api/audit/sessions",
            "/api/audit/session",
            "/api/audit/stats",
        ];

        for path in paths {
            assert_eq!(
                enforce_route_scope(&pat, "POST", path),
                Err((
                    Some(SCOPE_ACCOUNT_MANAGE),
                    "missing required scope: account:manage".to_string()
                )),
                "PAT must carry account:manage on {path}"
            );
        }

        pat.scopes.push(SCOPE_ACCOUNT_MANAGE.to_string());
        for path in paths {
            assert!(
                enforce_route_scope(&pat, "POST", path).is_ok(),
                "explicit account:manage must admit {path}"
            );
        }
    }

    #[test]
    fn oauth_route_policy_body_aware_routes() {
        assert_eq!(
            oauth_route_scope_policy_for_path_method("POST", "/api/tools/call"),
            OAuthRouteScopePolicy::BodyAware(OAuthBodyAwarePolicy::RuntimeToolCall)
        );
        assert_eq!(
            oauth_route_scope_policy_for_path_method("POST", "/mcp"),
            OAuthRouteScopePolicy::BodyAware(OAuthBodyAwarePolicy::McpToolCall)
        );
    }

    #[test]
    fn oauth_route_policy_unknown_is_unknown() {
        assert_eq!(
            oauth_route_scope_policy_for_path_method("POST", "/api/future/authenticated-route"),
            OAuthRouteScopePolicy::Unknown
        );
        assert_eq!(
            oauth_route_scope_policy_for_path_method("POST", "/api/tools/list/extra"),
            OAuthRouteScopePolicy::Unknown
        );
    }

    #[test]
    fn oauth_route_policy_legacy_codex_routes_are_removed() {
        for path in [
            "/api/codex/run",
            "/api/codex/context",
            "/api/codex/projects",
            "/api/codex/context_batch",
            "/api/codex/apply_patch",
            "/api/codex/edit",
            "/api/codex/artifact",
            "/api/codex/git",
            "/api/codex/job",
            "/api/codex/report",
        ] {
            assert_eq!(
                oauth_route_scope_policy_for_path_method("POST", path),
                OAuthRouteScopePolicy::Unknown,
                "{path}"
            );
        }
    }

    #[test]
    fn oauth_route_policy_authenticated_route_audit() {
        for spec in crate::route_metadata::iter_routes()
            .filter(|spec| spec.auth == crate::route_metadata::RouteAuth::AuthMiddleware)
        {
            assert_ne!(
                oauth_route_scope_policy_for_path_method(
                    match spec.method {
                        crate::route_metadata::RouteMethod::Get => "GET",
                        crate::route_metadata::RouteMethod::Post => "POST",
                    },
                    spec.path,
                ),
                OAuthRouteScopePolicy::Unknown,
                "{:?} {}",
                spec.method,
                spec.path
            );
        }
    }

    #[test]
    fn oauth_scope_policy_runtime_tool_scopes() {
        for (tool, policy) in [
            (
                "list_tools",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "start_session",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "session_summary",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "update_session_context",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "close_session",
                OAuthToolScopePolicy::Require(SCOPE_SESSION_COLLABORATE),
            ),
            (
                "post_session_message",
                OAuthToolScopePolicy::Require(SCOPE_SESSION_COLLABORATE),
            ),
            (
                "list_session_messages",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "get_session_assignment",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "observe_session_messages",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "resolve_session_message",
                OAuthToolScopePolicy::Require(SCOPE_SESSION_COLLABORATE),
            ),
            (
                "complete_session_message",
                OAuthToolScopePolicy::Require(SCOPE_SESSION_COLLABORATE),
            ),
            (
                "session_discussion_summary",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "runtime_status",
                OAuthToolScopePolicy::Require(SCOPE_RUNTIME_READ),
            ),
            (
                "read_files",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ),
            ),
            (
                "show_changes",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ),
            ),
            (
                "document_diagnostics",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ),
            ),
            ("hover", OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ)),
            (
                "workspace_symbols",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ),
            ),
            #[cfg(feature = "workspace-checkpoints")]
            (
                "workspace_checkpoint_create",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ),
            ),
            #[cfg(feature = "workspace-checkpoints")]
            (
                "workspace_checkpoint_restore",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            ("git_log", OAuthToolScopePolicy::Require(SCOPE_PROJECT_READ)),
            (
                "write_project_file",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "artifact_upload_begin",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "artifact_upload_chunk",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "artifact_upload_finish",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "artifact_upload_abort",
                OAuthToolScopePolicy::Require(SCOPE_PROJECT_WRITE),
            ),
            (
                "computer_list_windows",
                OAuthToolScopePolicy::Require(SCOPE_COMPUTER_READ),
            ),
            (
                "computer_snapshot",
                OAuthToolScopePolicy::Require(SCOPE_COMPUTER_READ),
            ),
            (
                "computer_save_snapshot",
                OAuthToolScopePolicy::RequireAll(&[SCOPE_PROJECT_WRITE, SCOPE_COMPUTER_READ]),
            ),
            ("run_shell", OAuthToolScopePolicy::Require(SCOPE_JOB_RUN)),
            ("stop_job", OAuthToolScopePolicy::Require(SCOPE_JOB_RUN)),
            ("cargo_test", OAuthToolScopePolicy::Require(SCOPE_JOB_RUN)),
            (
                "coding_agent_start",
                OAuthToolScopePolicy::RequireAll(&[SCOPE_CODING_AGENT_RUN, SCOPE_PROJECT_WRITE]),
            ),
            (
                "start_agent_task_endpoint_continuation",
                OAuthToolScopePolicy::RequireAll(&[
                    SCOPE_COMMUNICATION_READ,
                    SCOPE_COMMUNICATION_MANAGE,
                ]),
            ),
            (
                "start_agent_task_coding_run",
                OAuthToolScopePolicy::RequireAll(&[
                    SCOPE_COMMUNICATION_READ,
                    SCOPE_COMMUNICATION_MANAGE,
                    SCOPE_CODING_AGENT_RUN,
                    SCOPE_PROJECT_WRITE,
                ]),
            ),
            (
                "reconcile_agent_task_coding_run",
                OAuthToolScopePolicy::RequireAll(&[
                    SCOPE_COMMUNICATION_READ,
                    SCOPE_COMMUNICATION_MANAGE,
                    SCOPE_CODING_AGENT_RUN,
                ]),
            ),
            (
                "coding_agent_observe",
                OAuthToolScopePolicy::Require(SCOPE_CODING_AGENT_RUN),
            ),
            (
                "coding_agent_cancel",
                OAuthToolScopePolicy::Require(SCOPE_CODING_AGENT_RUN),
            ),
        ] {
            assert_eq!(oauth_scope_policy_for_runtime_tool(tool), policy, "{tool}");
        }
    }

    #[test]
    fn oauth_route_policy_tool_scope_policy_matches_metadata_for_representative_tools() {
        for tool in [
            "list_tools",
            "start_session",
            "session_summary",
            "update_session_context",
            "close_session",
            "post_session_message",
            "list_session_messages",
            "observe_session_messages",
            "resolve_session_message",
            "complete_session_message",
            "session_discussion_summary",
            #[cfg(feature = "workspace-checkpoints")]
            "workspace_checkpoint_create",
            #[cfg(feature = "workspace-checkpoints")]
            "workspace_checkpoint_restore",
            "show_changes",
            "document_diagnostics",
            "hover",
            "workspace_symbols",
            "read_files",
            "write_project_file",
            "artifact_upload_begin",
            "artifact_upload_chunk",
            "artifact_upload_finish",
            "artifact_upload_abort",
            "apply_unified_diff",
            "computer_list_windows",
            "computer_find_elements",
            "computer_element_state",
            "computer_activate_window",
            "computer_snapshot",
            "run_shell",
            "cargo_test",
        ] {
            let metadata = lookup_tool_metadata(tool).unwrap();
            assert_eq!(
                oauth_scope_policy_for_runtime_tool(tool),
                metadata.authority,
                "{tool}"
            );
        }
    }

    #[test]
    fn runtime_tool_scope_policy_is_exactly_tool_definition_authority() {
        for tool in known_tool_names() {
            let metadata = lookup_tool_metadata(tool).unwrap();
            assert_eq!(
                oauth_scope_policy_for_runtime_tool(tool),
                metadata.authority,
                "{tool}"
            );
        }
    }

    #[test]
    fn runner_manage_scope_is_not_inherited_by_project_anonymous_or_plugin_authority() {
        assert!(
            crate::auth::shared_key_context("runner-manage-check").has_scope(SCOPE_RUNNER_MANAGE)
        );
        assert!(!crate::auth::open_anonymous_context().has_scope(SCOPE_RUNNER_MANAGE));
        assert!(
            !crate::auth::shared_key::project_credential_context("wc_pgrant_runnermanage")
                .has_scope(SCOPE_RUNNER_MANAGE)
        );
        assert_ne!(SCOPE_RUNNER_MANAGE, SCOPE_PLUGIN_MANAGE);
        assert!(KNOWN_SCOPES.contains(&SCOPE_RUNNER_MANAGE));
    }

    #[test]
    fn detached_scope_is_never_implicit_for_legacy_lightweight_authority() {
        for (label, auth) in [
            (
                "shared-key",
                crate::auth::shared_key_context("detached-scope-check"),
            ),
            ("open", crate::auth::open_anonymous_context()),
            (
                "project-credential",
                crate::auth::shared_key::project_credential_context("wc_pgrant_detachedscope"),
            ),
        ] {
            assert!(
                auth.has_scope(SCOPE_JOB_RUN),
                "{label} should retain job:run"
            );
            assert!(
                !auth.has_scope(SCOPE_JOB_DETACH),
                "{label} must not implicitly gain detached execution authority"
            );
        }
    }

    #[test]
    fn coding_agent_scope_is_never_implicit_for_legacy_lightweight_authority() {
        for (label, auth) in [
            (
                "shared-key",
                crate::auth::shared_key_context("coding-agent-scope-check"),
            ),
            ("open", crate::auth::open_anonymous_context()),
            (
                "project-credential",
                crate::auth::shared_key::project_credential_context("wc_pgrant_codingagentscope"),
            ),
        ] {
            assert!(
                !auth.has_scope(SCOPE_CODING_AGENT_RUN),
                "{label} must not implicitly gain delegated coding-agent authority"
            );
        }
        for base in [SCOPE_PROJECT_WRITE, SCOPE_JOB_RUN, SCOPE_MCP_LOCAL] {
            let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
            auth.scopes = vec![base.to_string()];
            assert!(!auth.has_scope(SCOPE_CODING_AGENT_RUN), "{base}");
        }
    }

    #[test]
    fn memory_scope_policy_and_lightweight_credential_classes_are_explicit() {
        assert_eq!(
            oauth_scope_policy_for_runtime_tool("memory_search"),
            OAuthToolScopePolicy::RequireAll(MEMORY_READ_SCOPES)
        );
        assert_eq!(
            oauth_scope_policy_for_runtime_tool("memory_read"),
            OAuthToolScopePolicy::RequireAll(MEMORY_READ_SCOPES)
        );
        assert_eq!(
            oauth_scope_policy_for_runtime_tool("memory_set"),
            OAuthToolScopePolicy::RequireAll(MEMORY_MANAGE_SCOPES)
        );
        assert_eq!(
            oauth_scope_policy_for_runtime_tool("memory_delete"),
            OAuthToolScopePolicy::RequireAll(MEMORY_MANAGE_SCOPES)
        );
        assert!(KNOWN_SCOPES.contains(&SCOPE_MEMORY_READ));
        assert!(KNOWN_SCOPES.contains(&SCOPE_MEMORY_MANAGE));

        let direct = crate::auth::shared_key_context("memory-scope-contract");
        assert!(direct.has_scope(SCOPE_MEMORY_READ));
        assert!(direct.has_scope(SCOPE_MEMORY_MANAGE));
        assert!(
            crate::auth::shared_key::DIRECT_SHARED_KEY_MODEL_SCOPES.contains(&SCOPE_MEMORY_READ)
        );
        assert!(
            crate::auth::shared_key::DIRECT_SHARED_KEY_MODEL_SCOPES.contains(&SCOPE_MEMORY_MANAGE)
        );

        for (label, auth) in [
            ("open", crate::auth::open_anonymous_context()),
            (
                "project-credential",
                crate::auth::shared_key::project_credential_context("wc_pgrant_memoryscope"),
            ),
        ] {
            assert!(!auth.has_scope(SCOPE_MEMORY_READ), "{label}");
            assert!(!auth.has_scope(SCOPE_MEMORY_MANAGE), "{label}");
        }
        assert!(
            !crate::auth::project_share::PROJECT_SHARE_OAUTH_SCOPES.contains(&SCOPE_MEMORY_READ)
        );
        assert!(
            !crate::auth::project_share::PROJECT_SHARE_OAUTH_SCOPES.contains(&SCOPE_MEMORY_MANAGE)
        );
    }

    #[test]
    fn oauth_scope_policy_unknown_tool_is_unknown() {
        assert_eq!(
            oauth_scope_policy_for_runtime_tool("definitely_not_a_tool"),
            OAuthToolScopePolicy::Unknown
        );
    }
}
