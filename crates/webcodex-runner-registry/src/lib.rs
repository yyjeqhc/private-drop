//! Server-side authoritative Runner registration and lifecycle contracts.
//!
//! The registry implementation is extracted into this crate in the following
//! refactor step. Neutral contracts land first so root authentication and
//! telemetry policy can depend on the registry domain, never the reverse.

mod access;
mod access_control;
mod capabilities;
mod job_status;
mod job_updates;
mod jobs;
mod polling;
mod project_inventory;
mod projects;
mod protocol;
mod reconciliation;
mod registry;
mod requests;
mod runners;
mod state;
mod telemetry;
mod validation;

#[cfg(test)]
mod reconciliation_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use registry::clamp_grace;
#[cfg(test)]
pub(crate) use webcodex_core::{
    artifact_policy, job_observation, lsp_bridge, mcp_gateway, runner_protocol,
};

#[cfg(test)]
pub(crate) mod test_support {
    use crate::RunnerRegistry;
    use std::sync::atomic::{AtomicU64, Ordering};
    use webcodex_core::runner_protocol::{
        RunnerCapabilities, RunnerProjectSummary, RunnerRegisterRequest, ShellProjectInventoryPage,
        PROJECT_INVENTORY_PAGE_MAX_SUMMARIES, RUNNER_PROTOCOL_GENERATION_V2,
        RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES,
    };

    pub(crate) fn current_runner_capabilities(
        capabilities: RunnerCapabilities,
    ) -> RunnerCapabilities {
        let mut value =
            serde_json::to_value(capabilities).expect("serialize Runner test capabilities");
        let object = value
            .as_object_mut()
            .expect("Runner test capabilities must serialize as an object");
        for capability in RUNNER_PROTOCOL_GENERATION_V2_BASELINE_CAPABILITY_NAMES {
            object.insert((*capability).to_string(), serde_json::Value::Bool(true));
        }
        serde_json::from_value(value).expect("deserialize canonical Runner test capabilities")
    }

    pub(crate) fn current_runner_registration(
        mut registration: RunnerRegisterRequest,
    ) -> RunnerRegisterRequest {
        registration.runner_protocol_generation = RUNNER_PROTOCOL_GENERATION_V2;
        registration.capabilities = current_runner_capabilities(registration.capabilities);
        registration
    }

    static TEST_PROJECT_INVENTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    pub(crate) async fn apply_project_inventory_snapshot(
        registry: &RunnerRegistry,
        client_id: &str,
        runner_instance_id: &str,
        projects: Vec<RunnerProjectSummary>,
    ) {
        let snapshot_sequence = TEST_PROJECT_INVENTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let generation = format!("test-inventory-{snapshot_sequence}");
        let chunks = projects
            .chunks(PROJECT_INVENTORY_PAGE_MAX_SUMMARIES)
            .collect::<Vec<_>>();
        if chunks.is_empty() {
            registry
                .apply_project_inventory_page(
                    client_id,
                    runner_instance_id,
                    ShellProjectInventoryPage {
                        generation,
                        snapshot_sequence,
                        page_index: 0,
                        total_reported: 0,
                        complete: true,
                        projects: Vec::new(),
                    },
                )
                .await
                .expect("empty test project inventory snapshot");
            return;
        }
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            registry
                .apply_project_inventory_page(
                    client_id,
                    runner_instance_id,
                    ShellProjectInventoryPage {
                        generation: generation.clone(),
                        snapshot_sequence,
                        page_index: index as u32,
                        total_reported: projects.len(),
                        complete: index == last,
                        projects: chunk.to_vec(),
                    },
                )
                .await
                .expect("test project inventory page");
        }
    }
}

pub use access::{DetachedInitiatorIdentity, RunnerAccess, RunnerAccessGroup};
pub use capabilities::{RunnerFeature, RunnerFeatureSet};
pub use job_status::job_status_is_active;
pub use job_updates::{
    JobLogWait, JobLogWaitOutcome, ShellJobLogObservation, ShellJobStartMetadata,
    StructuredJobExecution,
};
pub use jobs::{command_preview, process_preview, script_preview, COMMAND_PREVIEW_MAX_CHARS};
pub(crate) use protocol::AcceptedRunnerProtocol;
pub use reconciliation::recovery_timeout_sweep;
pub use registry::{
    job_recovery_grace_secs, RunnerRegistry, RunnerTransport, DETACHED_IDEMPOTENCY_CONFLICT,
    DETACHED_IDEMPOTENCY_RECOVERY_PREFIX, JOB_RECOVERY_GRACE_MAX_SECS, JOB_RECOVERY_GRACE_MIN_SECS,
    JOB_RECOVERY_GRACE_SECS, RECOVERY_SWEEP_INTERVAL_SECS, RUNNER_ONLINE_WINDOW_SECS,
    TRANSPORT_POLLING, TRANSPORT_QUIC, TRANSPORT_WEBSOCKET,
};
pub(crate) use registry::{
    now_ts, LIVE_JOB_STREAM_RETENTION_BYTES, MAX_QUEUED_REQUESTS_PER_RUNNER,
    MAX_RETIRED_INSTANCES_PER_RUNNER, ORDINARY_RESULT_STREAM_RETENTION_BYTES,
};
pub use requests::EnqueueLspError;
pub use state::{RunnerSemanticView, ShellJobVisibility};
pub use telemetry::{NoopRunnerRegistryTelemetry, RunnerRegistryTelemetry};
