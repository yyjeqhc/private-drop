use serde_json::{json, Value};
use std::sync::Arc;
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_protocol::{RunnerJobUpdateRequest, RunnerRequest, RunnerResultPayload};
use webcodex_core::ssh_resource::SshResourceRequest;
use webcodex_runner_registry::RunnerRegistryTelemetry;

#[derive(Debug, Default)]
struct ToolRequestTraceRunnerRegistryTelemetry;

impl RunnerRegistryTelemetry for ToolRequestTraceRunnerRegistryTelemetry {
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
    ) {
        let kind = operation.wire_kind();
        if let RunnerOperation::SshResource(ssh_operation) = operation {
            let payload = ssh_resource_trace_payload(request_id, client_id, ssh_operation);
            crate::tool_request_trace::record_runner_request_enqueued(
                &payload,
                request_id,
                client_id,
                kind,
                job_id,
                runner_instance_id,
                runner_transport,
                runner_version,
                runner_git_commit,
            );
        } else {
            crate::tool_request_trace::record_runner_request_enqueued(
                request,
                request_id,
                client_id,
                kind,
                job_id,
                runner_instance_id,
                runner_transport,
                runner_version,
                runner_git_commit,
            );
        }
    }

    fn runner_result_accepted(&self, request_id: &str, payload: &RunnerResultPayload) {
        crate::tool_request_trace::capture_runner_result(request_id, payload);
    }

    fn runner_result_finalized(&self, request_id: &str) {
        crate::tool_request_trace::finalize_runner_result_correlation(request_id);
    }

    fn runner_job_update_accepted(
        &self,
        request_id: Option<&str>,
        job_id: &str,
        payload: &RunnerJobUpdateRequest,
    ) {
        crate::tool_request_trace::capture_runner_job_update(request_id, job_id, payload);
    }

    fn runner_job_finalized(&self, request_id: Option<&str>, job_id: &str) {
        crate::tool_request_trace::finalize_runner_job_correlation(request_id, job_id);
    }
}

fn ssh_resource_trace_payload(
    request_id: &str,
    client_id: &str,
    request: &SshResourceRequest,
) -> Value {
    let (action, resource_name, target_present, default_cwd_present) = match request {
        SshResourceRequest::List => ("list", None, false, false),
        SshResourceRequest::Register {
            name, default_cwd, ..
        } => ("register", Some(name.as_str()), true, default_cwd.is_some()),
        SshResourceRequest::Remove { name, .. } => ("remove", Some(name.as_str()), false, false),
    };
    json!({
        "request_id": request_id,
        "client_id": client_id,
        "kind": "ssh_resource",
        "action": action,
        "resource_name": resource_name,
        "target_present": target_present,
        "default_cwd_present": default_cwd_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_resource_trace_projection_never_contains_target_or_default_cwd() {
        let target = "17724@w10";
        let cwd = "C:/private/work";
        let request = SshResourceRequest::Register {
            expected_revision: 7,
            name: "w10".to_string(),
            target: target.to_string(),
            default_cwd: Some(cwd.to_string()),
        };
        let payload = ssh_resource_trace_payload("request-1", "runner-1", &request);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains(target));
        assert!(!serialized.contains(cwd));
        assert_eq!(payload["request_id"], "request-1");
        assert_eq!(payload["client_id"], "runner-1");
        assert_eq!(payload["kind"], "ssh_resource");
        assert_eq!(payload["action"], "register");
        assert_eq!(payload["resource_name"], "w10");
        assert_eq!(payload["target_present"], true);
        assert_eq!(payload["default_cwd_present"], true);
    }
}

pub(crate) fn tool_request_trace_telemetry(
) -> Arc<dyn webcodex_runner_registry::RunnerRegistryTelemetry> {
    Arc::new(ToolRequestTraceRunnerRegistryTelemetry)
}
