//! Canonical declarative metadata for mounted HTTP routes.
//!
//! Handler mounting stays explicit in the owning HTTP modules. This registry owns
//! the security- and surface-relevant metadata that previously drifted across
//! auth, OpenAPI, console, audit, and test-only route tables.

mod account;
mod connector;
mod consoles;
mod mcp;
mod oauth;
mod openapi;
mod operations;
mod runner_transport;
mod runtime;

pub(crate) use openapi::{OpenApiExampleSet, OpenApiOperationSpec};
use webcodex_core::authority::OAuthRouteScopePolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteMethod {
    Get,
    Post,
}

impl RouteMethod {
    pub(crate) fn matches(self, method: &str) -> bool {
        match self {
            Self::Get => method.trim().eq_ignore_ascii_case("GET"),
            Self::Post => method.trim().eq_ignore_ascii_case("POST"),
        }
    }

    pub(crate) const fn openapi_key(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteAuth {
    /// No bearer AuthMiddleware. The endpoint is intentionally public at the
    /// HTTP route layer (it may still validate protocol credentials in-body).
    Public,
    /// No bearer AuthMiddleware because the handler owns its current identity
    /// or short-lived authorization contract.
    HandlerManaged,
    /// Mounted behind the shared bearer AuthMiddleware.
    AuthMiddleware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteSurface {
    OAuth,
    Mcp,
    RuntimeApi,
    Connector,
    HostConsole,
    RuntimeConsole,
    Admin,
    Audit,
    AccountManagement,
    AccountControl,
    Pairing,
    RunnerTransport,
    /// Public browser/document delivery only. This surface carries no bearer
    /// authentication or token-admission semantics.
    PublicWeb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteOpenApiProjection {
    Hidden,
    /// Dedicated operation on the normal server `/openapi.json` GPT Actions surface.
    PublicAction(OpenApiOperationSpec),
    /// Project Connector capability identity; semantic ToolSpec data stays in the
    /// canonical Connector capability registry.
    ConnectorCapability(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AuditClass {
    Other,
    Edit,
    Context,
    Job,
    Command,
    Report,
    Artifact,
    Git,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RouteId {
    WellKnownProtectedResource,
    WellKnownAuthorizationServer,
    OAuthToken,
    OAuthRevoke,
    OAuthAuthorize,
    OAuthAuthorizeLogin,
    OAuthAuthorizeConsent,
    OAuthAuthorizeBridge,
    OAuthAuthorizeProject,
    PairingEnroll,
    McpGet,
    McpPost,
    ConnectorReadiness,
    ConnectorTaskStart,
    ConnectorTaskList,
    ConnectorTaskResume,
    ConnectorFilesList,
    ConnectorFilesRead,
    ConnectorFilesSearch,
    ConnectorCodeNavigate,
    ConnectorCodeImpact,
    ConnectorEditsApply,
    ConnectorChecksRun,
    ConnectorCommandsRun,
    ConnectorTaskReview,
    ConnectorTaskCancel,
    ConnectorTaskFinish,
    HostConsoleReadiness,
    HostConsoleTasks,
    HostConsoleActivity,
    HostConsoleWorkflowSessions,
    HostConsoleWorkflowSession,
    HostConsoleTaskReview,
    HostConsoleTaskCancel,
    HostConsoleTaskGuide,
    HostConsoleApprovals,
    HostConsoleApprovalDecide,
    HostConsoleDevices,
    HostConsoleResultAccept,
    HostConsoleResultReject,
    HostConsoleConnect,
    RuntimeConsoleOverview,
    RuntimeConsoleRunner,
    RuntimeConsoleWindows,
    RuntimeConsoleWindow,
    RuntimeConsoleProjects,
    RuntimeConsoleWorkflowSessions,
    RuntimeConsoleWorkflowSession,
    RuntimeConsoleWorkflowSessionMessages,
    RuntimeConsoleWorkflowSessionObserve,
    RuntimeConsoleWorkflowSessionPostMessage,
    RuntimeConsoleWorkflowSessionWithdrawMessage,
    RuntimeConsoleWorkflowSessionReplaceMessage,
    RuntimeConsoleCommunicationAgents,
    RuntimeConsoleCommunicationAgentCreate,
    RuntimeConsoleCommunicationAgentUpdate,
    RuntimeConsoleCommunicationEndpointAttach,
    RuntimeConsoleCommunicationEndpointRenew,
    RuntimeConsoleCommunicationEndpointDetach,
    RuntimeConsoleCommunicationConversations,
    RuntimeConsoleCommunicationConversationCreate,
    RuntimeConsoleCommunicationConversation,
    RuntimeConsoleCommunicationMessagePost,
    RuntimeConsoleCommunicationInbox,
    RuntimeConsoleCommunicationInboxConsume,
    AdminDashboard,
    AdminProjectsRegister,
    AdminProjectsCreate,
    AdminProjectsEnable,
    AdminProjectsDisable,
    AdminProjectsUnregister,
    ToolsList,
    ToolsCall,
    ArtifactsImport,
    JobsStatus,
    JobsLog,
    JobsStop,
    JobsList,
    JobsTail,
    ProjectsList,
    ProjectsRegister,
    ProjectsCreate,
    ProjectsUnregister,
    ProjectsReadFile,
    ProjectsGitStatus,
    ProjectsGitDiff,
    ProjectsGitDiffSummary,
    ProjectsListFiles,
    ProjectsSearchText,
    ProjectsApplyUnifiedDiff,
    ProjectsRunShell,
    ProjectsGitRestorePaths,
    ProjectsDiscardUntracked,
    ProjectsRunJob,
    RuntimeStatus,
    OAuthClientsCreate,
    OAuthClientsList,
    OAuthClientsUpdateScopes,
    OAuthClientsRevoke,
    OAuthSharedKeyClientProvision,
    UsersCreate,
    UsersList,
    UsersMe,
    TokensCreate,
    TokensRegisterHash,
    TokensList,
    TokensRevoke,
    AgentTokensCreate,
    AgentTokensRegisterHash,
    AgentTokensList,
    AgentTokensRevoke,
    PairingCreate,
    ShellRun,
    ShellFile,
    ShellJob,
    ShellJobsStatus,
    ShellJobsLog,
    ShellJobsStop,
    ShellJobsList,
    RunnerRegister,
    RunnerOffline,
    RunnerPoll,
    RunnerResult,
    RunnerPersistentShellResult,
    RunnerJobUpdate,
    RunnerWs,
    AuditSessions,
    AuditSession,
    AuditStats,
    OpenApiDocument,
    ConsoleWebRoot,
    ConsoleWebAppJs,
    ConsoleWebStylesCss,
    RuntimeWebRoot,
    RuntimeWebAppJs,
    RuntimeWebStylesCss,
    AdminWebRoot,
    AdminWebAppJs,
    AdminWebStylesCss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteSpec {
    pub(crate) id: RouteId,
    pub(crate) method: RouteMethod,
    pub(crate) path: &'static str,
    pub(crate) scope_policy: OAuthRouteScopePolicy,
    pub(crate) surface: RouteSurface,
    pub(crate) openapi_projection: RouteOpenApiProjection,
    pub(crate) audit_class: AuditClass,
    pub(crate) auth: RouteAuth,
}

const fn route(
    id: RouteId,
    method: RouteMethod,
    path: &'static str,
    scope_policy: OAuthRouteScopePolicy,
    surface: RouteSurface,
    openapi_projection: RouteOpenApiProjection,
    audit_class: AuditClass,
    auth: RouteAuth,
) -> RouteSpec {
    RouteSpec {
        id,
        method,
        path,
        scope_policy,
        surface,
        openapi_projection,
        audit_class,
        auth,
    }
}

#[cfg(test)]
use crate::auth::scopes::SCOPE_SESSION_COLLABORATE;
#[cfg(test)]
use AuditClass::*;
#[cfg(test)]
use OAuthRouteScopePolicy::*;
#[cfg(test)]
use RouteId::*;
#[cfg(test)]
use RouteOpenApiProjection::*;
#[cfg(test)]
use RouteSurface::*;

const ROUTE_GROUPS: &[&[RouteSpec]] = &[
    oauth::PUBLIC_ROUTES,
    account::ENROLLMENT_ROUTES,
    mcp::ROUTES,
    connector::ROUTES,
    consoles::ROUTES,
    operations::ADMIN_ROUTES,
    runtime::ROUTES,
    oauth::MANAGEMENT_ROUTES,
    account::ROUTES,
    runtime::SHELL_ROUTES,
    runner_transport::ROUTES,
    operations::AUDIT_ROUTES,
    operations::PUBLIC_WEB_ROUTES,
    consoles::PUBLIC_WEB_ROUTES,
];

pub(crate) fn iter_routes() -> impl Iterator<Item = &'static RouteSpec> + Clone {
    ROUTE_GROUPS.iter().flat_map(|routes| routes.iter())
}

pub(crate) fn spec(id: RouteId) -> &'static RouteSpec {
    iter_routes()
        .find(|spec| spec.id == id)
        .unwrap_or_else(|| panic!("RouteId {id:?} has no canonical RouteSpec"))
}

#[cfg(test)]
pub(crate) fn path(id: RouteId) -> &'static str {
    spec(id).path
}

/// Path relative to the production `/api` parent router.
pub(crate) fn api_path(id: RouteId) -> &'static str {
    spec(id)
        .path
        .strip_prefix("/api/")
        .unwrap_or_else(|| panic!("RouteId {id:?} is not an /api route"))
}

/// Path relative to the root Salvo router.
pub(crate) fn root_path(id: RouteId) -> &'static str {
    spec(id).path.trim_start_matches('/')
}

/// Return the path segment for one canonical direct child route.
///
/// The production browser routers stay nested; this helper proves that the
/// child RouteSpec actually belongs immediately under the canonical parent.
pub(crate) fn direct_child_path(parent: RouteId, child: RouteId) -> &'static str {
    let parent_path = spec(parent).path;
    let child_path = spec(child).path;
    child_path
        .strip_prefix(parent_path)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .filter(|suffix| !suffix.is_empty() && !suffix.contains('/'))
        .unwrap_or_else(|| {
            panic!(
                "RouteId {child:?} path {child_path:?} is not a direct child of {parent:?} path {parent_path:?}"
            )
        })
}

/// Return one root-relative path after proving two method-specific specs share
/// the same canonical path (used for one Salvo Router with GET + POST handlers).
pub(crate) fn shared_root_path(first: RouteId, second: RouteId) -> &'static str {
    let first_path = spec(first).path;
    assert_eq!(first_path, spec(second).path, "shared route path mismatch");
    first_path.trim_start_matches('/')
}

pub(crate) fn lookup(method: &str, path: &str) -> Option<&'static RouteSpec> {
    let path = normalize_path(path);
    iter_routes().find(|spec| spec.method.matches(method) && spec.path == path)
}

/// Exact path-only lookup for consumers whose historical contract was an
/// allowlist over `Request::uri().path()` or a persisted audit endpoint.
/// Method-aware scope lookup below intentionally keeps its older benign
/// normalization; path-only security gates must not gain new aliases here.
pub(crate) fn lookup_path(path: &str) -> Option<&'static RouteSpec> {
    iter_routes().find(|spec| spec.path == path)
}

pub(crate) fn path_has_surface(path: &str, surface: RouteSurface) -> bool {
    iter_routes().any(|spec| spec.path == path && spec.surface == surface)
}

pub(crate) fn audit_class_for_path(path: &str) -> Option<AuditClass> {
    lookup_path(path).map(|spec| spec.audit_class)
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    let path = path.split('?').next().unwrap_or(path);
    let path = if path.is_empty() { "/" } else { path };
    let with_slash = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if with_slash.len() > 1 {
        with_slash.trim_end_matches('/').to_string()
    } else {
        with_slash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn production_prefix(source: &'static str) -> &'static str {
        source.split("#[cfg(test)]").next().unwrap_or(source)
    }

    fn mounted_route_sources() -> [&'static str; 5] {
        [
            include_str!("lib.rs"),
            production_prefix(include_str!("connector_runtime/http.rs")),
            production_prefix(include_str!("host_console_http.rs")),
            production_prefix(include_str!("runtime_console_http.rs")),
            production_prefix(include_str!("admin_http.rs")),
        ]
    }

    fn exact_route_id_reference_count(source: &str, needle: &str) -> usize {
        source
            .match_indices(needle)
            .filter(|(index, _)| {
                source[*index + needle.len()..]
                    .chars()
                    .next()
                    .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
            })
            .count()
    }

    #[test]
    fn canonical_route_metadata_has_unique_method_path_pairs() {
        let mut seen = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for route_spec in iter_routes() {
            assert!(
                route_spec.path.starts_with('/'),
                "{:?}: {}",
                route_spec.id,
                route_spec.path
            );
            assert!(
                ids.insert(route_spec.id as usize),
                "duplicate RouteId: {:?}",
                route_spec.id
            );
            assert!(
                seen.insert((route_spec.method as u8, route_spec.path)),
                "duplicate canonical route metadata for {:?} {}",
                route_spec.method,
                route_spec.path
            );
            assert_eq!(spec(route_spec.id), route_spec);
            let method = match route_spec.method {
                RouteMethod::Get => "GET",
                RouteMethod::Post => "POST",
            };
            assert_eq!(lookup(method, route_spec.path), Some(route_spec));
        }
        assert_eq!(
            iter_routes().count(),
            AdminWebStylesCss as usize + 1,
            "canonical iteration must cover every RouteId exactly once",
        );
        assert_eq!(iter_routes().count(), 139, "canonical route closure");
        assert_eq!(lookup("GET", "/mcp").unwrap().id, McpGet);
        assert_eq!(lookup("POST", "/mcp").unwrap().id, McpPost);
    }

    #[test]
    fn openapi_projection_is_closed_unique_and_connector_bijective() {
        let mut public_operation_ids = BTreeSet::new();
        let mut connector_capabilities = BTreeSet::new();

        for route_spec in iter_routes() {
            match route_spec.openapi_projection {
                Hidden => {}
                PublicAction(operation) => {
                    assert_eq!(route_spec.method, RouteMethod::Post, "{:?}", route_spec.id);
                    assert_eq!(route_spec.surface, RuntimeApi, "{:?}", route_spec.id);
                    assert!(!operation.operation_id.is_empty(), "{:?}", route_spec.id);
                    assert!(!operation.request_schema.is_empty(), "{:?}", route_spec.id);
                    assert!(!operation.response_schema.is_empty(), "{:?}", route_spec.id);
                    assert!(
                        public_operation_ids.insert(operation.operation_id),
                        "duplicate public OpenAPI operationId: {}",
                        operation.operation_id
                    );
                }
                ConnectorCapability(name) => {
                    assert_eq!(route_spec.method, RouteMethod::Post, "{:?}", route_spec.id);
                    assert_eq!(route_spec.surface, Connector, "{:?}", route_spec.id);
                    assert!(!name.is_empty(), "{:?}", route_spec.id);
                    assert!(
                        connector_capabilities.insert(name),
                        "duplicate Connector capability route binding: {name}"
                    );
                }
            }
        }

        assert!(
            public_operation_ids.len() < 30,
            "GPT Actions operation budget exceeded: {}",
            public_operation_ids.len()
        );
        let canonical_connector_capabilities =
            webcodex_connector_runtime::surface::CAPABILITY_NAMES
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
        assert_eq!(
            connector_capabilities, canonical_connector_capabilities,
            "RouteSpec Connector bindings must be a bijection with the canonical capability registry"
        );
    }

    #[test]
    fn canonical_lookup_normalizes_only_benign_request_path_variants() {
        assert_eq!(
            lookup(" post ", "api/runtime/status/?ignored=1")
                .unwrap()
                .id,
            RuntimeStatus
        );
        assert!(lookup("GET", "/api/runtime/status").is_none());
        assert!(lookup("POST", "/api/future/authenticated-route").is_none());
        assert!(lookup("POST", "/api/runtime/status/extra").is_none());

        // Path-only surface/audit consumers replaced exact historical
        // allowlists and must not inherit the scope lookup's normalization.
        assert!(lookup_path("/api/runtime/status").is_some());
        assert!(lookup_path("/api/runtime/status/").is_none());
        assert!(path_has_surface("/api/agents/ws", RunnerTransport));
        assert!(!path_has_surface("/api/agents/ws/", RunnerTransport));
    }

    #[test]
    fn production_leaf_mounts_reference_each_canonical_spec_exactly_once() {
        let combined = mounted_route_sources().concat();
        let mut references = 0usize;
        for spec in iter_routes() {
            let needle = format!("RouteId::{:?}", spec.id);
            assert_eq!(
                exact_route_id_reference_count(&combined, &needle),
                1,
                "production leaf mount must reference {needle} exactly once"
            );
            references += 1;
        }
        assert_eq!(references, 139, "A2 production leaf RouteId closure");
    }

    #[test]
    fn production_leaf_mounts_do_not_use_literal_paths() {
        let combined = mounted_route_sources().concat();
        assert_eq!(
            combined.matches("Router::with_path(\"").count(),
            1,
            "only the structural /api parent may remain a literal mount"
        );
        assert_eq!(
            combined.matches("Router::with_path(\"api\")").count(),
            1,
            "the sole literal mount must be the non-leaf /api parent"
        );

        let lib = include_str!("lib.rs");
        assert_eq!(
            lib.matches(".hoop(AuthMiddleware)").count(),
            3,
            "a new AuthMiddleware root must be covered by the canonical route inventory invariant"
        );
    }

    #[test]
    fn public_web_routes_are_neutral_hidden_metadata() {
        let routes = iter_routes()
            .filter(|spec| spec.surface == PublicWeb)
            .collect::<Vec<_>>();
        assert_eq!(routes.len(), 10);
        for route in routes {
            assert_eq!(route.method, RouteMethod::Get, "{:?}", route.id);
            assert_eq!(
                route.scope_policy,
                OAuthRouteScopePolicy::Public,
                "{:?}",
                route.id
            );
            assert_eq!(route.auth, RouteAuth::Public, "{:?}", route.id);
            assert_eq!(route.openapi_projection, Hidden, "{:?}", route.id);
            assert_eq!(route.audit_class, Other, "{:?}", route.id);
        }
        assert_eq!(direct_child_path(ConsoleWebRoot, ConsoleWebAppJs), "app.js");
        assert_eq!(
            direct_child_path(RuntimeWebRoot, RuntimeWebStylesCss),
            "styles.css"
        );
        assert!(std::panic::catch_unwind(|| {
            direct_child_path(ConsoleWebRoot, RuntimeWebAppJs)
        })
        .is_err());
    }

    #[test]
    fn runtime_console_mutations_are_never_runtime_read_routes() {
        let mutations = [
            RuntimeConsoleWorkflowSessionPostMessage,
            RuntimeConsoleWorkflowSessionWithdrawMessage,
            RuntimeConsoleWorkflowSessionReplaceMessage,
        ];
        for id in mutations {
            assert_eq!(
                spec(id).scope_policy,
                Require(SCOPE_SESSION_COLLABORATE),
                "{id:?} must retain mutation-capable Session authority"
            );
        }
        for spec in iter_routes().filter(|spec| spec.surface == RuntimeConsole) {
            assert_eq!(spec.openapi_projection, Hidden, "{:?}", spec.id);
        }
    }

    #[test]
    fn communication_routes_use_independent_read_and_manage_authority() {
        let reads = [
            RuntimeConsoleCommunicationAgents,
            RuntimeConsoleCommunicationConversations,
            RuntimeConsoleCommunicationConversation,
            RuntimeConsoleCommunicationInbox,
        ];
        for id in reads {
            assert_eq!(
                spec(id).scope_policy,
                Require(crate::auth::scopes::SCOPE_COMMUNICATION_READ),
                "{id:?} must require communication read authority"
            );
        }
        let mutations = [
            RuntimeConsoleCommunicationAgentCreate,
            RuntimeConsoleCommunicationAgentUpdate,
            RuntimeConsoleCommunicationEndpointAttach,
            RuntimeConsoleCommunicationEndpointRenew,
            RuntimeConsoleCommunicationEndpointDetach,
            RuntimeConsoleCommunicationConversationCreate,
            RuntimeConsoleCommunicationMessagePost,
            RuntimeConsoleCommunicationInboxConsume,
        ];
        for id in mutations {
            assert_eq!(
                spec(id).scope_policy,
                Require(crate::auth::scopes::SCOPE_COMMUNICATION_MANAGE),
                "{id:?} must require communication manage authority"
            );
            assert_ne!(
                spec(id).scope_policy,
                Require(crate::auth::scopes::SCOPE_PROJECT_READ),
                "communication identity must not inherit Project authority"
            );
            assert_ne!(
                spec(id).scope_policy,
                Require(crate::auth::scopes::SCOPE_SESSION_COLLABORATE),
                "Conversation must remain distinct from Workflow Session collaboration"
            );
        }
    }

    #[test]
    fn path_only_metadata_is_unambiguous_per_canonical_path() {
        let mut metadata = BTreeMap::new();
        for spec in iter_routes() {
            let path_only = (spec.surface, spec.audit_class);
            match metadata.insert(spec.path, path_only) {
                Some(existing) => assert_eq!(existing, path_only, "{}", spec.path),
                None => {}
            }
        }
    }

    #[test]
    fn audit_class_preserves_existing_http_stats_semantics() {
        for (path, class) in [
            ("/api/projects/apply_unified_diff", Edit),
            ("/api/projects/read_file", Context),
            ("/api/projects/run_job", Job),
            ("/api/tools/call", Command),
            ("/api/runtime/status", Report),
            ("/api/artifacts/import", Artifact),
            ("/api/projects/git_diff", Git),
            ("/api/projects/run_shell", Shell),
        ] {
            assert_eq!(audit_class_for_path(path), Some(class), "{path}");
        }
        assert_eq!(
            audit_class_for_path("/api/connector/edits/apply"),
            Some(Other)
        );
        assert_eq!(
            audit_class_for_path("/api/runtime-console/projects"),
            Some(Other)
        );
    }
}
