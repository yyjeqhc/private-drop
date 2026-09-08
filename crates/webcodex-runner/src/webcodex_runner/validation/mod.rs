//! Runner-side validation bridge: registry, execution, and adapters.
//!
//! Server sends declarative `ValidationBridgeRequest` values. The agent
//! resolves `adapter_id`, discovers the executable, builds argv, runs the tool
//! with bounded capture, parses structured output, relativizes paths, and
//! returns a sanitized `ValidationBridgeResponse`. No arbitrary shell commands
//! cross the bridge.

mod execute;
mod path;
mod pyright;
mod registry;

#[cfg(test)]
pub(crate) use registry::{adapter_metadata, registered_adapter_ids};

use super::config::RunnerPolicy;
use super::output::CommandResult;
use super::projects::load_runner_project_summaries_from_dir;
use super::shell::cwd_allowed;
use crate::validation_bridge::{
    failure_kinds, validate_bridge_request, ValidationBridgeRequest, ValidationBridgeResponse,
    ValidationBridgeResultEnvelope, VALIDATION_BRIDGE_PROTOCOL_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

pub(crate) fn handle_validation_request(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    payload: &ValidationBridgeRequest,
    shutdown: Option<&AtomicBool>,
) -> CommandResult {
    let start = Instant::now();
    match execute_validation_with_shutdown(policy, project_registry_dir, payload, shutdown) {
        Ok(response) => {
            let envelope = ValidationBridgeResultEnvelope::ok(response);
            CommandResult {
                exit_code: Some(0),
                stdout: Some(envelope.to_stdout_json()),
                stderr: Some(String::new()),
                duration_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            }
        }
        Err(envelope) => CommandResult {
            exit_code: Some(0),
            stdout: Some(envelope.to_stdout_json()),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
    }
}

fn execute_validation_with_shutdown(
    policy: &RunnerPolicy,
    project_registry_dir: &Path,
    request: &ValidationBridgeRequest,
    shutdown: Option<&AtomicBool>,
) -> Result<ValidationBridgeResponse, ValidationBridgeResultEnvelope> {
    if let Err(message) = validate_bridge_request(request) {
        return Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::INVALID_ARGUMENTS,
            message,
        ));
    }

    let meta = registry::lookup_adapter(&request.adapter_id).ok_or_else(|| {
        ValidationBridgeResultEnvelope::err(
            failure_kinds::ADAPTER_NOT_FOUND,
            format!("unknown validation adapter '{}'", request.adapter_id),
        )
    })?;

    if meta.language != request.language {
        return Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::LANGUAGE_ADAPTER_MISMATCH,
            format!(
                "adapter '{}' serves language '{}', not '{}'",
                meta.adapter_id, meta.language, request.language
            ),
        ));
    }
    if meta.validation_kind != request.validation_kind {
        return Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::LANGUAGE_ADAPTER_MISMATCH,
            format!(
                "adapter '{}' serves kind '{}', not '{}'",
                meta.adapter_id, meta.validation_kind, request.validation_kind
            ),
        ));
    }

    let project = resolve_runner_project(project_registry_dir, &request.project_id)?;
    let project_root = validate_project_root(policy, &project)?;

    match meta.adapter_id {
        "pyright" => Ok(pyright::run_pyright(
            &project_root,
            request,
            policy.max_timeout_secs,
            shutdown,
        )),
        other => Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::ADAPTER_NOT_FOUND,
            format!("adapter '{other}' is registered but not executable in this build"),
        )),
    }
}

/// Direct internal entry for unit/e2e tests that already have a project root.
#[cfg(test)]
pub(crate) fn execute_validation_at_root(
    project_root: &Path,
    request: &ValidationBridgeRequest,
    max_timeout_secs: u64,
) -> Result<ValidationBridgeResponse, ValidationBridgeResultEnvelope> {
    if let Err(message) = validate_bridge_request(request) {
        return Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::INVALID_ARGUMENTS,
            message,
        ));
    }
    let meta = registry::lookup_adapter(&request.adapter_id).ok_or_else(|| {
        ValidationBridgeResultEnvelope::err(
            failure_kinds::ADAPTER_NOT_FOUND,
            format!("unknown validation adapter '{}'", request.adapter_id),
        )
    })?;
    if meta.language != request.language || meta.validation_kind != request.validation_kind {
        return Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::LANGUAGE_ADAPTER_MISMATCH,
            "language/kind does not match adapter",
        ));
    }
    match meta.adapter_id {
        "pyright" => Ok(pyright::run_pyright(
            project_root,
            request,
            max_timeout_secs,
            None,
        )),
        other => Err(ValidationBridgeResultEnvelope::err(
            failure_kinds::ADAPTER_NOT_FOUND,
            format!("adapter '{other}' is not executable"),
        )),
    }
}

fn resolve_runner_project(
    project_registry_dir: &Path,
    project_id: &str,
) -> Result<PathBuf, ValidationBridgeResultEnvelope> {
    let projects = load_runner_project_summaries_from_dir(project_registry_dir);
    let project = projects
        .into_iter()
        .find(|p| p.id == project_id)
        .ok_or_else(|| {
            ValidationBridgeResultEnvelope::err(
                failure_kinds::UNKNOWN_PROJECT,
                "unknown agent project",
            )
        })?;
    Ok(PathBuf::from(project.path))
}

fn validate_project_root(
    policy: &RunnerPolicy,
    path: &Path,
) -> Result<PathBuf, ValidationBridgeResultEnvelope> {
    cwd_allowed(policy, path).map_err(|message| {
        ValidationBridgeResultEnvelope::err(failure_kinds::INVALID_PROJECT_PATH, message)
    })?;
    std::fs::canonicalize(path).map_err(|_| {
        ValidationBridgeResultEnvelope::err(
            failure_kinds::INVALID_PROJECT_PATH,
            "project root is not accessible",
        )
    })
}

/// Empty response skeleton used by adapters for early failures.
pub(crate) fn base_response(
    request: &ValidationBridgeRequest,
    tool_available: bool,
) -> ValidationBridgeResponse {
    ValidationBridgeResponse {
        protocol_version: VALIDATION_BRIDGE_PROTOCOL_VERSION,
        adapter_id: request.adapter_id.clone(),
        language: request.language.clone(),
        validation_kind: request.validation_kind.clone(),
        success: false,
        command_started: false,
        exit_code: None,
        duration_ms: 0,
        failure_kind: None,
        diagnostics: None,
        tool_available,
        stdout_bytes: 0,
        stdout_capped: false,
        stderr_capped: false,
        stderr_summary: None,
        message: None,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
