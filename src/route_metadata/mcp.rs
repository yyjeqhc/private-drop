use super::RouteAuth::AuthMiddleware;
use super::{
    route, AuditClass::*, RouteId::*, RouteMethod::*, RouteOpenApiProjection::*, RouteSpec,
    RouteSurface::*,
};
use webcodex_core::authority::{
    OAuthBodyAwarePolicy, OAuthRouteScopePolicy::*, SCOPE_RUNTIME_READ,
};

pub(super) const ROUTES: &[RouteSpec] = &[
    route(
        McpGet,
        Get,
        "/mcp",
        Require(SCOPE_RUNTIME_READ),
        Mcp,
        Hidden,
        Other,
        AuthMiddleware,
    ),
    route(
        McpPost,
        Post,
        "/mcp",
        BodyAware(OAuthBodyAwarePolicy::McpToolCall),
        Mcp,
        Hidden,
        Other,
        AuthMiddleware,
    ),
];
