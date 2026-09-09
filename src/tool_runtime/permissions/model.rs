//! Authority decision types and wire-stable constants.
//!
//! Wire shape of [`PermissionDecision`] is preserved for ledger / handoff
//! compatibility. Typed enums ([`AuthorityMode`], [`PermissionOutcome`]) are
//! the internal model; string fields on the decision remain the serialized form.

/// Bounded recent permission rows in session handoff summaries.
pub(crate) const DEFAULT_PERMISSION_RECENT_LIMIT: usize = 20;

/// Environment variable for the canonical authority mode.
pub(crate) const AUTHORITY_MODE_ENV: &str = "WEBCODEX_AUTHORITY_MODE";

/// Legacy operator configuration; unambiguous values migrate to authority modes.
pub(crate) const LEGACY_PERMISSION_MODE_ENV: &str = "WEBCODEX_PERMISSION_MODE";

/// Canonical authority mode (soft policy; never overrides hard safety).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorityMode {
    /// Trusted agent: auto-authorize permission-bearing tools after hard
    /// safety (scopes, project boundary, session guards, path policy).
    TrustedAgent,
    /// Restricted: consequential tools require human authorization; runtime
    /// surface denies them (connector surface routes through one-time
    /// approvals).
    Restricted,
}

impl AuthorityMode {
    pub(crate) const DEFAULT: Self = Self::TrustedAgent;

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TrustedAgent => "trusted_agent",
            Self::Restricted => "restricted",
        }
    }

    /// Parse a mode name (case-sensitive, trimmed by caller).
    pub(crate) fn parse(raw: &str) -> Result<Self, AuthorityModeParseError> {
        match raw {
            "trusted_agent" | "dev_auto_approve" => Ok(Self::TrustedAgent),
            "restricted" | "require_approval" => Ok(Self::Restricted),
            other => Err(AuthorityModeParseError {
                value: other.to_string(),
            }),
        }
    }

    pub(crate) fn human_approval_required(self) -> bool {
        matches!(self, Self::Restricted)
    }

    pub(crate) fn auto_authorize(self) -> bool {
        matches!(self, Self::TrustedAgent)
    }
}

impl Default for AuthorityMode {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Failed to parse a configured authority mode string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorityModeParseError {
    pub(crate) value: String,
}

impl std::fmt::Display for AuthorityModeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid {AUTHORITY_MODE_ENV} value {:?}; expected one of: \
             trusted_agent, restricted",
            self.value
        )
    }
}

impl std::error::Error for AuthorityModeParseError {}

pub(crate) use webcodex_core::workflow_session_contract::{PermissionDecision, PermissionOutcome};

pub(crate) fn new_permission_decision(
    policy: impl Into<String>,
    outcome: PermissionOutcome,
    reason: impl Into<String>,
    risk: impl Into<String>,
    tool_name: impl Into<String>,
    project: Option<&str>,
) -> PermissionDecision {
    PermissionDecision {
        required: true,
        policy: policy.into(),
        request_id: format!("wc_perm_{}", uuid::Uuid::new_v4().simple()),
        status: outcome.as_str().to_string(),
        reason: reason.into(),
        risk: risk.into(),
        tool_name: tool_name.into(),
        project: project.map(str::to_string),
    }
}
