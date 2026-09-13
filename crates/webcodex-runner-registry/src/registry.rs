use crate::receipts::ReceiptRegistryState;
use crate::{NoopRunnerRegistryTelemetry, RunnerAccess, RunnerRegistryTelemetry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

/// Server-side retained bytes for one stdout or stderr stream in an ordinary
/// completed Runner result. This is not a polling/WebSocket/QUIC wire limit.
pub(crate) const ORDINARY_RESULT_STREAM_RETENTION_BYTES: usize = 256 * 1024;
/// Server-side retained bytes for one live Job stdout or stderr stream. Kept
/// separate from ordinary results because Job cursors/truncation semantics are
/// independently owned even though the current value is the same.
pub(crate) const LIVE_JOB_STREAM_RETENTION_BYTES: usize = 256 * 1024;
/// Server-side retained bytes for one stdout or stderr stream returned by a
/// persistent-shell operation. This is an observation-retention bound, not a
/// transport envelope limit.
pub(crate) const PERSISTENT_SHELL_STREAM_RETENTION_BYTES: usize = 256 * 1024;
pub const RUNNER_ONLINE_WINDOW_SECS: i64 = 60;
pub(crate) const MAX_SHARED_KEY_RUNNERS_PER_GROUP: usize = 16;
pub(crate) const MAX_SHARED_KEY_RUNNERS_GLOBAL: usize = 1024;
pub(crate) const SHARED_KEY_OFFLINE_TTL_SECS: i64 = 24 * 60 * 60;
pub const DETACHED_IDEMPOTENCY_CONFLICT: &str = "detached_idempotency_conflict";
pub const DETACHED_IDEMPOTENCY_RECOVERY_PREFIX: &str = "detached_idempotency_recovery_required:";
pub const JOB_RECOVERY_GRACE_SECS: i64 = 120;
pub const JOB_RECOVERY_GRACE_MIN_SECS: i64 = 5;
pub const JOB_RECOVERY_GRACE_MAX_SECS: i64 = 3600;
pub const RECOVERY_SWEEP_INTERVAL_SECS: u64 = 30;
pub(crate) const MAX_RETIRED_INSTANCES_PER_RUNNER: usize = 16;
pub(crate) const MAX_QUEUED_REQUESTS_PER_RUNNER: usize = 256;

pub const TRANSPORT_POLLING: &str = "polling";
pub const TRANSPORT_WEBSOCKET: &str = "websocket";
pub const TRANSPORT_QUIC: &str = "quic";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerTransport {
    Polling,
    WebSocket,
    Quic,
}

impl RunnerTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Polling => TRANSPORT_POLLING,
            Self::WebSocket => TRANSPORT_WEBSOCKET,
            Self::Quic => TRANSPORT_QUIC,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SharedKeyRegistrationLimits {
    pub(crate) per_group: usize,
    pub(crate) global: usize,
    pub(crate) offline_ttl_secs: i64,
}

impl Default for SharedKeyRegistrationLimits {
    fn default() -> Self {
        Self {
            per_group: MAX_SHARED_KEY_RUNNERS_PER_GROUP,
            global: MAX_SHARED_KEY_RUNNERS_GLOBAL,
            offline_ttl_secs: SHARED_KEY_OFFLINE_TTL_SECS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunnerRegistry {
    pub(crate) inner: Arc<ReceiptRegistryState>,
    pub(crate) observation_epoch: Arc<str>,
    pub(crate) shared_key_limits: SharedKeyRegistrationLimits,
    pub(crate) telemetry: Arc<dyn RunnerRegistryTelemetry>,
    pub(crate) cleanup_intents: Arc<StdMutex<HashMap<String, Option<RunnerAccess>>>>,
    #[cfg(any(test, feature = "root-test-support"))]
    pub(crate) project_job_scan_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(any(test, feature = "root-test-support"))]
    pub(crate) filtered_job_refresh_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Default for RunnerRegistry {
    fn default() -> Self {
        Self::with_telemetry(Arc::new(NoopRunnerRegistryTelemetry))
    }
}

impl RunnerRegistry {
    pub fn with_telemetry(telemetry: Arc<dyn RunnerRegistryTelemetry>) -> Self {
        Self {
            inner: Arc::new(ReceiptRegistryState::new(None)),
            observation_epoch: Arc::from(uuid::Uuid::new_v4().to_string()),
            shared_key_limits: SharedKeyRegistrationLimits::default(),
            telemetry,
            cleanup_intents: Arc::new(StdMutex::new(HashMap::new())),
            #[cfg(any(test, feature = "root-test-support"))]
            project_job_scan_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(any(test, feature = "root-test-support"))]
            filtered_job_refresh_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn project_job_scan_count_for_test(&self) -> usize {
        self.project_job_scan_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn filtered_job_refresh_count_for_test(&self) -> usize {
        self.filtered_job_refresh_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn with_shared_key_limits_for_test(
        per_group: usize,
        global: usize,
        offline_ttl_secs: i64,
    ) -> Self {
        Self {
            shared_key_limits: SharedKeyRegistrationLimits {
                per_group,
                global,
                offline_ttl_secs,
            },
            ..Self::default()
        }
    }
}

pub(crate) fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(crate) fn clamp_grace(raw: i64) -> i64 {
    raw.clamp(JOB_RECOVERY_GRACE_MIN_SECS, JOB_RECOVERY_GRACE_MAX_SECS)
}

pub fn job_recovery_grace_secs() -> i64 {
    static JOB_RECOVERY_GRACE: OnceLock<i64> = OnceLock::new();
    *JOB_RECOVERY_GRACE.get_or_init(|| {
        std::env::var("WEBCODEX_JOB_RECOVERY_GRACE_SECS")
            .ok()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .map(clamp_grace)
            .unwrap_or(JOB_RECOVERY_GRACE_SECS)
    })
}
