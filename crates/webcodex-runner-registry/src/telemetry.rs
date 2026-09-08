use std::fmt::Debug;
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_protocol::{RunnerJobUpdateRequest, RunnerRequest, RunnerResultPayload};

/// Fail-open telemetry callbacks invoked only from authoritative registry
/// lifecycle points. Implementations must not re-enter the registry. Callback
/// results never participate in admission, lease, ownership, replay, or
/// dispatch decisions.
pub trait RunnerRegistryTelemetry: Debug + Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn request_enqueued(
        &self,
        request: &RunnerRequest,
        operation: &RunnerOperation,
        request_id: &str,
        client_id: &str,
        job_id: Option<&str>,
        runner_instance_id: Option<&str>,
        runner_transport: Option<&str>,
        runner_version: Option<&str>,
        runner_git_commit: Option<&str>,
    );

    fn runner_result_accepted(&self, request_id: &str, payload: &RunnerResultPayload);

    fn runner_result_finalized(&self, request_id: &str);

    fn runner_job_update_accepted(
        &self,
        request_id: Option<&str>,
        job_id: &str,
        payload: &RunnerJobUpdateRequest,
    );

    fn runner_job_finalized(&self, request_id: Option<&str>, job_id: &str);
}

#[derive(Debug, Default)]
pub struct NoopRunnerRegistryTelemetry;

impl RunnerRegistryTelemetry for NoopRunnerRegistryTelemetry {
    fn request_enqueued(
        &self,
        _request: &RunnerRequest,
        _operation: &RunnerOperation,
        _request_id: &str,
        _client_id: &str,
        _job_id: Option<&str>,
        _agent_instance_id: Option<&str>,
        _runner_transport: Option<&str>,
        _runner_version: Option<&str>,
        _runner_git_commit: Option<&str>,
    ) {
    }

    fn runner_result_accepted(&self, _request_id: &str, _payload: &RunnerResultPayload) {}

    fn runner_result_finalized(&self, _request_id: &str) {}

    fn runner_job_update_accepted(
        &self,
        _request_id: Option<&str>,
        _job_id: &str,
        _payload: &RunnerJobUpdateRequest,
    ) {
    }

    fn runner_job_finalized(&self, _request_id: Option<&str>, _job_id: &str) {}
}
