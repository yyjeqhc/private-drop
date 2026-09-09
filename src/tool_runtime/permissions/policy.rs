//! Authority mode policy: how each mode maps to a decision outcome.
//!
//! Hard safety rules (session guard, path policy, scopes, …) are **not**
//! implemented here and must never be weakened by mode choice.

use super::model::{
    new_permission_decision, AuthorityMode, AuthorityModeParseError, PermissionDecision,
    PermissionOutcome, AUTHORITY_MODE_ENV, LEGACY_PERMISSION_MODE_ENV,
};
use serde_json::{json, Value};

/// Resolved rule name recorded on auto-authorized decisions.
pub(crate) const TRUSTED_AGENT_AUTO_REASON: &str = "trusted_agent_authority";

/// Resolved rule name recorded on restricted-mode denials of the runtime
/// surface (the connector surface routes through one-time human approvals).
pub(crate) const RESTRICTED_DENY_REASON: &str = "restricted_requires_human_authorization";

/// Where the resolved authority mode came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthoritySource {
    /// No explicit configuration; product default for self-hosted deployments.
    Default,
    /// Explicit `WEBCODEX_AUTHORITY_MODE`.
    Env,
    /// An unambiguous legacy operator configuration was migrated.
    LegacyEnv,
    /// Unknown or conflicting legacy configuration remains invalid.
    LegacyEnvRejected,
}

impl AuthoritySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Env => concat!("env:", "WEBCODEX_AUTHORITY_MODE"),
            Self::LegacyEnv => "migrated_env:WEBCODEX_PERMISSION_MODE",
            Self::LegacyEnvRejected => concat!("rejected_legacy_env:", "WEBCODEX_PERMISSION_MODE"),
        }
    }
}

/// Effective configuration used by the evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectiveAuthorityConfig {
    /// A known mode (including the default when the env var is unset).
    Active {
        mode: AuthorityMode,
        source: AuthoritySource,
    },
    /// Configuration is invalid — refuse auto-authorization (fail closed).
    InvalidMode {
        value: String,
        source: AuthoritySource,
    },
}

impl EffectiveAuthorityConfig {
    /// Resolve the authority mode from the process environment.
    ///
    /// Unset or empty `WEBCODEX_AUTHORITY_MODE` → [`AuthorityMode::TrustedAgent`]
    /// with source `default`. Unknown non-empty value → invalid (fail closed).
    /// Unambiguous legacy values migrate; unknown or conflicting values fail closed.
    pub(crate) fn from_env() -> Self {
        let read = |name, source| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(Self::InvalidMode {
                value: "<non-utf8>".to_string(),
                source,
            }),
        };
        let current = match read(AUTHORITY_MODE_ENV, AuthoritySource::Env) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let legacy = match read(
            LEGACY_PERMISSION_MODE_ENV,
            AuthoritySource::LegacyEnvRejected,
        ) {
            Ok(value) => value,
            Err(error) => return error,
        };
        Self::from_values(current.as_deref(), legacy.as_deref())
    }

    fn from_values(current: Option<&str>, legacy: Option<&str>) -> Self {
        let Some(legacy) = legacy.filter(|value| !value.trim().is_empty()) else {
            return Self::from_raw(current);
        };
        let mode = match AuthorityMode::parse(legacy.trim()) {
            Ok(mode) => mode,
            Err(_) => {
                return Self::InvalidMode {
                    value: "unknown legacy permission mode".to_string(),
                    source: AuthoritySource::LegacyEnvRejected,
                }
            }
        };
        if current.is_some_and(|value| !value.trim().is_empty()) {
            let config = Self::from_raw(current);
            return match config {
                Self::Active { mode: selected, .. } if selected != mode => Self::InvalidMode {
                    value: "conflicting authority and legacy permission modes".to_string(),
                    source: AuthoritySource::LegacyEnvRejected,
                },
                other => other,
            };
        }
        Self::Active {
            mode,
            source: AuthoritySource::LegacyEnv,
        }
    }

    /// Resolve from an optional raw mode string (tests and explicit config).
    pub(crate) fn from_raw(raw: Option<&str>) -> Self {
        let source = match raw {
            Some(value) if !value.trim().is_empty() => AuthoritySource::Env,
            _ => AuthoritySource::Default,
        };
        match resolve_authority_mode(raw) {
            Ok(mode) => Self::Active { mode, source },
            Err(err) => Self::InvalidMode {
                value: err.value,
                source,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn with_mode(mode: AuthorityMode) -> Self {
        Self::Active {
            mode,
            source: AuthoritySource::Default,
        }
    }

    pub(crate) fn mode_name(&self) -> &str {
        match self {
            Self::Active { mode, .. } => mode.as_str(),
            Self::InvalidMode { .. } => "invalid",
        }
    }

    pub(crate) fn source(&self) -> AuthoritySource {
        match self {
            Self::Active { source, .. } | Self::InvalidMode { source, .. } => *source,
        }
    }

    pub(crate) fn human_approval_required(&self) -> bool {
        match self {
            Self::Active { mode, .. } => mode.human_approval_required(),
            // Fail closed: do not advertise frictionless auto-authorization.
            Self::InvalidMode { .. } => true,
        }
    }

    pub(crate) fn auto_authorize(&self) -> bool {
        match self {
            Self::Active { mode, .. } => mode.auto_authorize(),
            Self::InvalidMode { .. } => false,
        }
    }
}

/// Resolve a mode from optional raw config.
///
/// - `None` / empty / whitespace → default `trusted_agent`
/// - known mode name → `Ok`
/// - anything else → `Err` with the invalid value
pub(crate) fn resolve_authority_mode(
    raw: Option<&str>,
) -> Result<AuthorityMode, AuthorityModeParseError> {
    match raw {
        None => Ok(AuthorityMode::DEFAULT),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(AuthorityMode::DEFAULT)
            } else {
                AuthorityMode::parse(trimmed)
            }
        }
    }
}

/// Apply mode policy for a permission-bearing tool call.
///
/// Caller must already have determined that the tool requires permission.
pub(crate) fn decide_for_required_tool(
    config: &EffectiveAuthorityConfig,
    tool_name: &str,
    project: Option<&str>,
    risk: &str,
) -> PermissionDecision {
    match config {
        EffectiveAuthorityConfig::Active {
            mode: AuthorityMode::TrustedAgent,
            ..
        } => new_permission_decision(
            AuthorityMode::TrustedAgent.as_str(),
            PermissionOutcome::AutoApproved,
            TRUSTED_AGENT_AUTO_REASON,
            risk,
            tool_name,
            project,
        ),
        EffectiveAuthorityConfig::Active {
            mode: AuthorityMode::Restricted,
            ..
        } => new_permission_decision(
            AuthorityMode::Restricted.as_str(),
            PermissionOutcome::Denied,
            RESTRICTED_DENY_REASON,
            risk,
            tool_name,
            project,
        ),
        EffectiveAuthorityConfig::InvalidMode { value, .. } => new_permission_decision(
            "invalid",
            PermissionOutcome::Denied,
            format!("invalid_authority_mode:{value}"),
            risk,
            tool_name,
            project,
        ),
    }
}

/// Canonical authority profile projection (runtime_status / coding-task
/// startup / health surfaces). Never exposes tokens or raw policy internals.
pub(crate) fn authority_profile_payload_for(config: &EffectiveAuthorityConfig) -> Value {
    let auto = config.auto_authorize();
    json!({
        "mode": config.mode_name(),
        "source": config.source().as_str(),
        "project_write": auto,
        "shell": auto,
        "git": auto,
        "network": auto,
        "package_install": auto,
        "service_control": auto,
        // In trusted_agent mode external release actions execute only when the
        // user's task explicitly includes that action and target.
        "release": if auto { "user_task_scoped" } else { "human_approval" },
        "human_approval_required": config.human_approval_required(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_empty_resolve_to_trusted_agent() {
        assert_eq!(
            resolve_authority_mode(None).unwrap(),
            AuthorityMode::TrustedAgent
        );
        assert_eq!(
            resolve_authority_mode(Some("")).unwrap(),
            AuthorityMode::TrustedAgent
        );
        assert_eq!(
            resolve_authority_mode(Some("   ")).unwrap(),
            AuthorityMode::TrustedAgent
        );
    }

    #[test]
    fn known_modes_parse() {
        assert_eq!(
            resolve_authority_mode(Some("trusted_agent")).unwrap(),
            AuthorityMode::TrustedAgent
        );
        assert_eq!(
            resolve_authority_mode(Some("restricted")).unwrap(),
            AuthorityMode::Restricted
        );
    }

    #[test]
    fn unambiguous_legacy_configuration_migrates() {
        for (legacy, expected) in [
            ("dev_auto_approve", "trusted_agent"),
            ("require_approval", "restricted"),
        ] {
            let config = EffectiveAuthorityConfig::from_values(None, Some(legacy));
            assert_eq!(config.mode_name(), expected);
            assert_eq!(config.source(), AuthoritySource::LegacyEnv);
            assert_eq!(
                EffectiveAuthorityConfig::from_raw(Some(legacy)).mode_name(),
                expected
            );
            assert_eq!(
                EffectiveAuthorityConfig::from_values(Some(expected), Some(legacy)).mode_name(),
                expected
            );
        }
        for (current, legacy) in [
            (None, "audit_only"),
            (Some("trusted_agent"), "require_approval"),
            (Some("invalid"), "dev_auto_approve"),
        ] {
            assert_eq!(
                EffectiveAuthorityConfig::from_values(current, Some(legacy)).mode_name(),
                "invalid"
            );
        }
    }

    #[test]
    fn illegal_mode_is_explicit_error() {
        let err = resolve_authority_mode(Some("nope")).unwrap_err();
        assert_eq!(err.value, "nope");
        let message = err.to_string();
        assert!(message.contains(AUTHORITY_MODE_ENV), "{message}");
        assert!(message.contains("trusted_agent"), "{message}");
        assert!(message.contains("nope"), "{message}");
    }

    #[test]
    fn source_is_reported_for_default_and_env() {
        assert_eq!(
            EffectiveAuthorityConfig::from_raw(None).source(),
            AuthoritySource::Default
        );
        assert_eq!(
            EffectiveAuthorityConfig::from_raw(Some("restricted")).source(),
            AuthoritySource::Env
        );
        assert_eq!(AuthoritySource::Default.as_str(), "default");
        assert_eq!(AuthoritySource::Env.as_str(), "env:WEBCODEX_AUTHORITY_MODE");
        assert_eq!(
            AuthoritySource::LegacyEnvRejected.as_str(),
            "rejected_legacy_env:WEBCODEX_PERMISSION_MODE"
        );
    }
}
