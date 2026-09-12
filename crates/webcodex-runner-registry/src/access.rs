/// Authenticated, non-secret access projection understood by the Runner
/// registry. Authentication mechanisms, credential verification, token scopes,
/// and transport admission remain root concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerAccess {
    /// May observe Runner/Job state across owner and lightweight-group
    /// partitions. Root auth policy decides which authenticated callers receive
    /// this global visibility.
    pub global_visibility: bool,
    /// May bypass the managed-Runner owner check. Root must reserve this for
    /// bootstrap authority; ordinary admin-scoped callers remain owner-bound.
    pub owner_bypass: bool,
    pub username: Option<String>,
    pub group: Option<RunnerAccessGroup>,
}

pub use webcodex_core::runner_job_receipt::RunnerAccessGroup;

/// Opaque, stable, non-secret identity used only to partition detached Job
/// idempotency. Root authentication policy decides how credentials map to this
/// value; the registry never interprets credential kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedInitiatorIdentity(String);

impl DetachedInitiatorIdentity {
    pub fn from_stable_principal(principal: String) -> Self {
        Self(principal)
    }

    pub fn internal() -> Self {
        Self("internal".to_string())
    }

    pub fn as_stable_principal(&self) -> &str {
        &self.0
    }
}
