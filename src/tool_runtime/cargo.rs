use serde_json::{json, Value};

use super::helpers::{
    bounded_tail, command_outcome_unknown_message, command_rejected_message,
    command_timeout_message, looks_like_command_timeout, resolve_sync_timeout_secs,
    sync_timeout_out_of_range_result, validate_project_relative_path,
    DEFAULT_CARGO_CHECK_TIMEOUT_SECS, DEFAULT_CARGO_FMT_TIMEOUT_SECS,
    DEFAULT_CARGO_TEST_TIMEOUT_SECS, MAX_VALIDATION_TIMEOUT_SECS, MIN_VALIDATION_TIMEOUT_SECS,
    SYNC_VALIDATION_WAIT_SECS,
};
use super::shell::{command_execution_state_name, ProjectCommandOutput};
use super::structured_execution::structured_job_observation;
use super::tool_result::ToolResult;
use super::validation_profile::{
    validation_adapter_for_tool, ValidationAdapter, ValidationCommandOptions,
};
use super::{ExecutionPurpose, ToolRuntime};
use crate::auth::AuthContext;
use crate::runner_http::ShellJobStartMetadata;
use crate::runner_protocol::{
    ShellCommandExecutionState, ShellJobOpRequest, ShellJobValidationMetadata,
    ShellJobValidationStep,
};
use webcodex_core::runner_job_lifecycle::RunnerJobLifecycle;
use webcodex_core::runtime_contract::STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS;
pub(crate) use webcodex_validation::parse_cargo_test_run_metadata;

const CARGO_STDIO_TAIL_CHARS: usize = 12_000;
const CARGO_VALIDATION_FAILURE_KIND: &str = "validation_failed";
const VALIDATION_FAILURE_GUIDANCE: &str =
    "command was started; inspect bounded validation evidence, fix the reported issue, then rerun the same structured validation tool.";

fn validate_cwd(cwd: Option<String>) -> Result<Option<String>, String> {
    match cwd {
        Some(raw) => {
            let trimmed = raw.trim().trim_start_matches("./").trim_end_matches('/');
            validate_project_relative_path(trimmed)?;
            if trimmed.is_empty() || trimmed == "." {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        None => Ok(None),
    }
}

pub(crate) fn count_rustc_diagnostics(text: &str, prefix: &str) -> usize {
    let stem = prefix.trim_end_matches(':');
    let coded_prefix = format!("{stem}[");
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with(prefix) || line.starts_with(&coded_prefix)
        })
        .count()
}

fn is_cargo_validation_failure(output: &ProjectCommandOutput, timeout_secs: u64) -> bool {
    output.execution_state == ShellCommandExecutionState::Completed
        && output.exit_code.is_some_and(|exit_code| exit_code != 0)
        && !looks_like_command_timeout(output.exit_code, &output.stderr, timeout_secs)
        && !looks_like_command_infrastructure_failure(&output.stderr)
}

fn looks_like_command_infrastructure_failure(stderr: &str) -> bool {
    let trimmed = stderr.trim_start();
    trimmed.starts_with("Failed to execute command:")
        || trimmed.starts_with("Failed to wait for command:")
        || trimmed.starts_with("Failed to collect command output:")
}

fn cargo_prestart_failure_kind(error: Option<&str>) -> &'static str {
    let lower = error.unwrap_or_default().to_ascii_lowercase();
    if lower.contains("permission") || lower.contains("denied") || lower.contains("not allowed") {
        "permission_denied"
    } else if lower.contains("cwd")
        || lower.contains("working directory")
        || lower.contains("directory")
    {
        "cwd_invalid"
    } else if lower.contains("project") {
        "project_not_found"
    } else {
        "executor_unavailable"
    }
}

/// Shared structured-validation runtime budget + effective sync window.
struct ValidationBudget {
    /// Total runtime budget of the command (`timeout_secs`), 1..=3600.
    effective_timeout_secs: u64,
    /// How long the tool call blocks in-process before promoting to a Job.
    sync_wait_secs: u64,
}

/// Resolve a read-only structured validation budget.
///
/// `timeout_secs` is the total runtime budget of the command, not the tool
/// call's synchronous wait. Explicit `sync_wait_secs` selects a 1..=60 grace
/// that cannot exceed the effective total budget. When omitted, compatibility
/// keeps `min(SYNC_VALIDATION_WAIT_SECS, effective_timeout)`; equal grace and
/// total budget leaves no Cargo handoff headroom.
fn resolve_validation_budget(
    tool_name: &str,
    timeout_secs: Option<u64>,
    sync_wait_secs: Option<u64>,
    default: u64,
) -> Result<ValidationBudget, ToolResult> {
    let value = timeout_secs.unwrap_or(default);
    if !(MIN_VALIDATION_TIMEOUT_SECS..=MAX_VALIDATION_TIMEOUT_SECS).contains(&value) {
        return Err(sync_timeout_out_of_range_result_with_range(
            tool_name,
            MIN_VALIDATION_TIMEOUT_SECS,
            MAX_VALIDATION_TIMEOUT_SECS,
            default,
        ));
    }
    let sync_wait_secs = match sync_wait_secs {
        Some(sync_wait)
            if !(MIN_VALIDATION_TIMEOUT_SECS..=STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS)
                .contains(&sync_wait) =>
        {
            return Err(validation_sync_wait_rejection(
                tool_name,
                format!(
                    "{tool_name} sync_wait_secs must be between 1 and {STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS}"
                ),
                format!(
                    "pass sync_wait_secs between 1 and {STRUCTURED_EXECUTION_SYNC_WAIT_MAX_SECS}, or omit it for the existing synchronous grace."
                ),
            ));
        }
        Some(sync_wait) if sync_wait > value => {
            return Err(validation_sync_wait_rejection(
                tool_name,
                format!(
                    "{tool_name} sync_wait_secs ({sync_wait}) must not exceed effective timeout_secs ({value})"
                ),
                "lower sync_wait_secs or raise timeout_secs within the validation budget; sync_wait_secs never extends total runtime.",
            ));
        }
        Some(sync_wait) => sync_wait,
        None => SYNC_VALIDATION_WAIT_SECS.min(value),
    };
    Ok(ValidationBudget {
        effective_timeout_secs: value,
        sync_wait_secs,
    })
}

fn validation_sync_wait_rejection(
    tool_name: &str,
    reason: impl Into<String>,
    guidance: impl Into<String>,
) -> ToolResult {
    ToolResult::err_with_output(
        command_rejected_message(reason.into(), guidance.into()),
        json!({
            "execution_source": tool_name,
            "execution_state": "not_started",
            "command_started": false,
            "command_completed": false,
            "failure_kind": "invalid_arguments",
            "tool_failure": true,
        }),
    )
}

fn sync_timeout_out_of_range_result_with_range(
    tool_name: &str,
    min: u64,
    max: u64,
    default: u64,
) -> ToolResult {
    super::tool_result::ToolResult::err_with_output(
        command_rejected_message(
            format!("{tool_name} timeout_secs must be between {min} and {max}"),
            format!(
                "pass timeout_secs between {min} and {max}, or omit it for the default of {default} seconds."
            ),
        ),
        json!({
            "command_started": false,
            "command_completed": false,
            "command_ok": false,
            "exit_code": null,
            "failure_kind": "invalid_arguments",
            "tool_failure": true,
        }),
    )
}

fn cargo_test_assertion_rejection(reason: impl AsRef<str>, guidance: &str) -> ToolResult {
    ToolResult::err_with_output(
        command_rejected_message(reason, guidance),
        json!({
            "execution_source": "cargo_test",
            "execution_state": "not_started",
            "command_started": false,
            "command_completed": false,
            "failure_kind": "invalid_arguments",
            "tool_failure": true,
        }),
    )
}

fn resolve_cargo_test_minimum(
    require_tests: Option<bool>,
    min_tests: Option<u64>,
    no_run: Option<bool>,
) -> Result<Option<u64>, ToolResult> {
    if let Some(minimum) = min_tests {
        if !(1..=crate::runner_protocol::CARGO_TEST_MIN_TESTS_MAX).contains(&minimum) {
            return Err(cargo_test_assertion_rejection(
                format!(
                    "cargo_test min_tests must be between 1 and {}",
                    crate::runner_protocol::CARGO_TEST_MIN_TESTS_MAX
                ),
                "pass a bounded positive min_tests value, or omit it.",
            ));
        }
    }
    let minimum = match (require_tests.unwrap_or(false), min_tests) {
        (true, Some(minimum)) => Some(minimum.max(1)),
        (true, None) => Some(1),
        (false, minimum) => minimum,
    };
    if minimum.is_some() && no_run.unwrap_or(false) {
        return Err(cargo_test_assertion_rejection(
            "cargo_test test-count assertions cannot be combined with no_run=true",
            "remove no_run to execute tests, or omit require_tests and min_tests when only compiling tests.",
        ));
    }
    Ok(minimum)
}

fn reject_structured_validation_ssh_resource(ssh_resource: Option<&str>) -> Option<ToolResult> {
    ssh_resource.map(|_| {
        ToolResult::err(command_rejected_message(
            "ssh_resource_unsupported_for_request: SSH resources do not support structured validation tools",
            "use the named runner host through run_shell or run_job instead.",
        ))
    })
}

impl ToolRuntime {
    #[cfg(test)]
    pub(crate) async fn cargo_fmt(
        &self,
        project: String,
        cwd: Option<String>,
        check: Option<bool>,
        timeout_secs: Option<u64>,
    ) -> ToolResult {
        self.cargo_fmt_with_context_inner(project, cwd, check, timeout_secs, None, None, None, None)
            .await
    }

    /// Entry used by dispatch: carries the Session/execution-context and auth
    /// so a long validation can promote to a Job that inherits the original
    /// Session ownership.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cargo_fmt_with_context(
        &self,
        project: String,
        cwd: Option<String>,
        check: Option<bool>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.cargo_fmt_with_context_inner(
            project,
            cwd,
            check,
            timeout_secs,
            sync_wait_secs,
            session_id,
            ssh_resource,
            auth,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn cargo_fmt_with_context_inner(
        &self,
        project: String,
        cwd: Option<String>,
        check: Option<bool>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let check = check.unwrap_or(false);
        if !check && sync_wait_secs.is_some() {
            return validation_sync_wait_rejection(
                "cargo_fmt",
                "cargo_fmt sync_wait_secs is available only with check=true",
                "remove sync_wait_secs for mutating cargo_fmt, or use check=true for read-only validation handoff.",
            );
        }
        // Both read-only and mutating structured Cargo formatting reject named
        // SSH resources before selecting an execution path. In particular, the
        // mutating sync path must never fall back to the Runner project root.
        if let Some(result) = reject_structured_validation_ssh_resource(ssh_resource) {
            return result;
        }
        // Non-check `cargo fmt` mutates source and keeps the existing explicit
        // synchronous semantics: it never auto-promotes to a Job after the
        // tool has returned. Only `check=true` gets the read-only handoff path.
        if !check {
            let timeout =
                match resolve_sync_timeout_secs(timeout_secs, DEFAULT_CARGO_FMT_TIMEOUT_SECS) {
                    Ok(timeout) => timeout,
                    Err(_) => {
                        return sync_timeout_out_of_range_result(
                            "cargo_fmt",
                            DEFAULT_CARGO_FMT_TIMEOUT_SECS,
                        )
                    }
                };
            let cwd = match validate_cwd(cwd) {
                Ok(cwd) => cwd,
                Err(e) => {
                    return ToolResult::err(command_rejected_message(
                        e,
                        "choose an existing project-relative cwd, then retry.",
                    ))
                }
            };
            let adapter = validation_adapter_for_tool("cargo_fmt")
                .expect("Rust validation profile must register cargo_fmt");
            let command = adapter
                .build_command(ValidationCommandOptions {
                    check: false,
                    ..ValidationCommandOptions::default()
                })
                .expect("cargo_fmt command builder is infallible");
            // `cargo fmt` (mutating) uses the plain sync path.
            self.run_cargo_command_sync(project, cwd, command, timeout, adapter)
                .await
        } else {
            self.run_readonly_validation(
                "cargo_fmt",
                ValidationRunRequest {
                    project,
                    cwd,
                    check: true,
                    filter: None,
                    all_targets: None,
                    all_features: None,
                    no_default_features: None,
                    features: None,
                    package: None,
                    no_run: None,
                    require_tests: None,
                    minimum_tests: None,
                    go_packages: None,
                    timeout_secs,
                    sync_wait_secs,
                    session_id,
                    ssh_resource,
                    auth,
                },
            )
            .await
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cargo_check(
        &self,
        project: String,
        cwd: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        timeout_secs: Option<u64>,
    ) -> ToolResult {
        self.cargo_check_with_context_inner(
            project,
            cwd,
            all_targets,
            all_features,
            no_default_features,
            features,
            package,
            timeout_secs,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cargo_check_with_context(
        &self,
        project: String,
        cwd: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.cargo_check_with_context_inner(
            project,
            cwd,
            all_targets,
            all_features,
            no_default_features,
            features,
            package,
            timeout_secs,
            sync_wait_secs,
            session_id,
            ssh_resource,
            auth,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn cargo_check_with_context_inner(
        &self,
        project: String,
        cwd: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.run_readonly_validation(
            "cargo_check",
            ValidationRunRequest {
                project,
                cwd,
                check: false,
                filter: None,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                no_run: None,
                require_tests: None,
                minimum_tests: None,
                go_packages: None,
                timeout_secs,
                sync_wait_secs,
                session_id,
                ssh_resource,
                auth,
            },
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cargo_test(
        &self,
        project: String,
        cwd: Option<String>,
        filter: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        no_run: Option<bool>,
        timeout_secs: Option<u64>,
    ) -> ToolResult {
        self.cargo_test_with_context_inner(
            project,
            cwd,
            filter,
            all_targets,
            all_features,
            no_default_features,
            features,
            package,
            no_run,
            None,
            None,
            timeout_secs,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn cargo_test_with_context(
        &self,
        project: String,
        cwd: Option<String>,
        filter: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        no_run: Option<bool>,
        require_tests: Option<bool>,
        min_tests: Option<u64>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.cargo_test_with_context_inner(
            project,
            cwd,
            filter,
            all_targets,
            all_features,
            no_default_features,
            features,
            package,
            no_run,
            require_tests,
            min_tests,
            timeout_secs,
            sync_wait_secs,
            session_id,
            ssh_resource,
            auth,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn cargo_test_with_context_inner(
        &self,
        project: String,
        cwd: Option<String>,
        filter: Option<String>,
        all_targets: Option<bool>,
        all_features: Option<bool>,
        no_default_features: Option<bool>,
        features: Option<String>,
        package: Option<String>,
        no_run: Option<bool>,
        require_tests: Option<bool>,
        min_tests: Option<u64>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let minimum_tests = match resolve_cargo_test_minimum(require_tests, min_tests, no_run) {
            Ok(minimum) => minimum,
            Err(result) => return result,
        };
        self.run_readonly_validation(
            "cargo_test",
            ValidationRunRequest {
                project,
                cwd,
                check: false,
                filter,
                all_targets,
                all_features,
                no_default_features,
                features,
                package,
                no_run,
                require_tests,
                minimum_tests,
                go_packages: None,
                timeout_secs,
                sync_wait_secs,
                session_id,
                ssh_resource,
                auth,
            },
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn go_test(
        &self,
        project: String,
        cwd: Option<String>,
        timeout_secs: Option<u64>,
    ) -> ToolResult {
        self.go_test_with_context(project, cwd, None, timeout_secs, None, None, None, None)
            .await
    }

    pub(crate) async fn go_test_with_context(
        &self,
        project: String,
        cwd: Option<String>,
        packages: Option<Vec<String>>,
        timeout_secs: Option<u64>,
        sync_wait_secs: Option<u64>,
        session_id: Option<String>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        self.run_readonly_validation(
            "go_test",
            ValidationRunRequest {
                project,
                cwd,
                check: false,
                filter: None,
                all_targets: None,
                all_features: None,
                no_default_features: None,
                features: None,
                package: None,
                no_run: None,
                require_tests: None,
                minimum_tests: None,
                go_packages: packages,
                timeout_secs,
                sync_wait_secs,
                session_id,
                ssh_resource,
                auth,
            },
        )
        .await
    }

    /// Run one read-only structured validation exactly once, synchronously
    /// waiting for up to the effective synchronous grace, then promoting the
    /// *same* execution to a queryable Job if it is still running.
    async fn run_readonly_validation(
        &self,
        tool_name: &str,
        request: ValidationRunRequest<'_>,
    ) -> ToolResult {
        let default = match tool_name {
            "cargo_check" => DEFAULT_CARGO_CHECK_TIMEOUT_SECS,
            "cargo_test" | "go_test" => DEFAULT_CARGO_TEST_TIMEOUT_SECS,
            "cargo_fmt" => DEFAULT_CARGO_FMT_TIMEOUT_SECS,
            _ => unreachable!("unknown read-only validation tool"),
        };
        let budget = match resolve_validation_budget(
            tool_name,
            request.timeout_secs,
            request.sync_wait_secs,
            default,
        ) {
            Ok(budget) => budget,
            Err(result) => return result,
        };
        let cwd = match validate_cwd(request.cwd) {
            Ok(cwd) => cwd,
            Err(e) => {
                return ToolResult::err(command_rejected_message(
                    e,
                    "choose an existing project-relative cwd, then retry.",
                ))
            }
        };
        let adapter = validation_adapter_for_tool(tool_name)
            .expect("structured validation profile must register the read-only tool");
        let validation_identity_kind =
            webcodex_tool_contracts::runtime_tool_session_evidence_policy(tool_name)
                .validation_identity;
        let validation_target_id = super::tool_audit::structured_validation_target_identity(
            validation_identity_kind,
            &json!({
                "cwd": cwd.as_deref(),
                "check": request.check,
                "filter": request.filter.as_deref(),
                "all_targets": request.all_targets,
                "all_features": request.all_features,
                "no_default_features": request.no_default_features,
                "features": request.features.as_deref(),
                "package": request.package.as_deref(),
                "no_run": request.no_run,
                "require_tests": request.require_tests,
                "min_tests": request.minimum_tests,
                "packages": request.go_packages.as_ref(),
            }),
        );
        let options = ValidationCommandOptions {
            check: request.check,
            filter: request.filter,
            all_targets: request.all_targets,
            all_features: request.all_features,
            no_default_features: request.no_default_features,
            features: request.features,
            package: request.package,
            no_run: request.no_run,
            go_packages: request.go_packages,
        };
        let command = match adapter.build_command(options.clone()) {
            Ok(command) => command,
            Err(e) => {
                return ToolResult::err(command_rejected_message(
                    e,
                    if tool_name == "go_test" {
                        "fix the Go package pattern format, then retry."
                    } else {
                        "fix the cargo argument format, then retry."
                    },
                ))
            }
        };
        // Pre-execution validation happens before any execution is created, so
        // a rejection never leaves a Job or a running process behind.
        let resolved = match self
            .resolve_project_for_auth(&request.project, request.auth)
            .await
        {
            Ok(config) => config,
            Err(e) => return ToolResult::err(command_rejected_message(
                e.to_message(),
                "verify the project id with list_projects, then retry with a registered project.",
            )),
        };
        let purpose = match adapter.validation_kind() {
            "test" => ExecutionPurpose::Test,
            "format" => ExecutionPurpose::Format,
            _ => ExecutionPurpose::Validation,
        };
        let timeout_secs = budget.effective_timeout_secs;
        let sync_wait_secs = budget.sync_wait_secs;
        let session_id = request.session_id.clone();
        let ssh_resource = request.ssh_resource.map(str::to_string);

        // Structured validation tools never execute through a named SSH resource.
        // Reject at the shared Agent entry so direct sync, short sync, and
        // long Job handoff paths cannot silently fall back to the project root.
        if let Some(result) = reject_structured_validation_ssh_resource(ssh_resource.as_deref()) {
            return result;
        }
        if tool_name == "go_test" || sync_wait_secs < timeout_secs {
            // The effective synchronous grace is shorter than the total
            // validation budget, so there is headroom to expose the same
            // execution as a Job. The agent path enqueues exactly one
            // structured validation Job, waits up to `sync_wait_secs`, and
            // hands off if it is still running.
            self.run_readonly_validation_agent(
                tool_name,
                &request.project,
                &resolved,
                cwd.as_deref(),
                &command,
                adapter,
                options,
                purpose,
                timeout_secs,
                sync_wait_secs,
                session_id,
                validation_target_id,
                request.minimum_tests,
                request.require_tests,
                request.no_run,
                ssh_resource.as_deref(),
                request.auth,
            )
            .await
        } else {
            // No handoff headroom: the requested budget is at most the
            // sync window, so there is no remaining runtime for a Job to
            // continue. Run synchronously through the existing capture
            // path and report a real terminal timeout at the budget
            // boundary. The command still starts exactly once.
            let output = match self
                    .run_project_command_capture(
                        &request.project,
                        command.clone(),
                        timeout_secs,
                        cwd.clone(),
                    )
                    .await
                {
                    Ok(output) => output,
                    Err(e) => {
                        return ToolResult::err(command_rejected_message(
                            e,
                            "verify the project id/cwd and agent connectivity, then retry or use run_shell for custom diagnostics.",
                        ))
                    }
                };
            self.build_cargo_result(
                &request.project,
                &command,
                cwd.as_deref(),
                &resolved,
                adapter,
                output,
                timeout_secs,
                sync_wait_secs,
                false,
                false,
                false,
                request.minimum_tests,
                request.require_tests,
                request.no_run,
            )
            .await
        }
    }

    /// Agent-backed read-only validation. Enqueues exactly one validation
    /// Job, waits up to `sync_wait_secs`, and returns either the in-window
    /// terminal result (discarding the now-hidden Job) or a Job handoff for
    /// the same execution.
    #[allow(clippy::too_many_arguments)]
    async fn run_readonly_validation_agent(
        &self,
        tool_name: &str,
        project: &str,
        config: &crate::projects::ProjectConfig,
        cwd: Option<&str>,
        command: &str,
        adapter: &'static dyn ValidationAdapter,
        options: ValidationCommandOptions,
        purpose: ExecutionPurpose,
        timeout_secs: u64,
        sync_wait_secs: u64,
        session_id: Option<String>,
        validation_target_id: Option<String>,
        minimum_tests: Option<u64>,
        require_tests: Option<bool>,
        no_run: Option<bool>,
        ssh_resource: Option<&str>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        let client_id = config.client_id.clone();
        let effective_cwd = match super::helpers::resolve_runner_cwd(config, cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                return ToolResult::err(command_rejected_message(
                    error,
                    "choose '.', an existing project-relative cwd, or a path inside the registered project root.",
                ))
            }
        };
        let resolved_cwd = super::helpers::project_relative_runner_cwd(config, &effective_cwd)
            .unwrap_or_else(|_| ".".to_string());
        let actual_shell = "configured";
        // The validation step is derived from the same options the tool would
        // have run synchronously, so the promoted Job executes the identical
        // command (not a re-constructed string). Passing the step in the job
        // metadata makes the request kind `start_validation_job`, so the
        // Runner runs the cargo program+argv directly (never the raw command
        // through a shell).
        let step = validation_step(tool_name, &options);
        let Ok(step) = step else {
            return ToolResult::err(command_rejected_message(
                "could not encode structured validation step",
                "fix the structured validation argument format, then retry.",
            ));
        };
        let dispatched_command = match serde_json::to_string(std::slice::from_ref(&step)) {
            Ok(command) => command,
            Err(_) => {
                return ToolResult::err(command_rejected_message(
                    "could not serialize structured validation step",
                    "fix the structured validation argument format, then retry.",
                ))
            }
        };
        let access = crate::runner_http::runner_access_from_auth(auth);
        let job = match self
            .runner_registry
            .start_job_with_metadata_for_access(
                ShellJobOpRequest {
                    op: "start".to_string(),
                    client_id: Some(client_id),
                    cwd: Some(effective_cwd),
                    command: Some(dispatched_command),
                    timeout_secs: Some(timeout_secs),
                    job_id: None,
                    since_stdout_line: None,
                    since_stderr_line: None,
                    tail_lines: None,
                    limit: None,
                    codex: None,
                },
                "tool_runtime".to_string(),
                ShellJobStartMetadata {
                    project_id: Some(project.to_string()),
                    session_id: session_id.clone(),
                    ssh_resource: ssh_resource.map(str::to_string),
                    project_cwd: Some(resolved_cwd.clone()),
                    purpose: Some(purpose.as_str().to_string()),
                    shell: Some(actual_shell.to_string()),
                    validation_steps: vec![step.clone()],
                    validation: Some(ShellJobValidationMetadata {
                        tool: tool_name.to_string(),
                        kind: adapter.validation_kind().to_string(),
                        steps: vec![step],
                        effective_timeout_secs: timeout_secs,
                        sync_wait_secs,
                        adapter: adapter.tool_identity().to_string(),
                        validation_target_id: validation_target_id.clone(),
                        minimum_tests,
                        require_tests,
                        no_run,
                    }),
                    visibility: crate::runner_http::ShellJobVisibility::HiddenUntilHandoff,
                    validation_identity: None,
                    validation_tool: None,
                    assertion_name: None,
                    structured_execution: None,
                    stdin: None,
                    detached_idempotency_key: None,
                },
                access.as_ref(),
                None,
            )
            .await
        {
            Ok(job) => job,
            Err(e) => {
                return ToolResult::err(command_rejected_message(
                    e,
                    "confirm the agent is connected and structured validation jobs are allowed, then retry.",
                ))
            }
        };
        let job_id = job.job_id.clone();
        let handoff = ValidationHandoff {
            execution_source: tool_name.to_string(),
            purpose: purpose.as_str().to_string(),
            job_id: job_id.clone(),
            effective_timeout_secs: timeout_secs,
            sync_wait_secs,
            project: project.to_string(),
            cwd: resolved_cwd,
            shell: actual_shell.to_string(),
            executor: "agent".to_string(),
            command_summary: crate::runner_http::command_preview(command),
            minimum_tests,
            require_tests,
            no_run,
            auth: access,
        };
        self.await_validation_job(job_id, sync_wait_secs, adapter, handoff)
            .await
    }

    /// Wait up to `sync_wait_secs` for a structured validation Job to reach a
    /// terminal state. If it completes in-window, return the terminal result
    /// and discard the hidden Job; if it is still active, return a Job handoff
    /// for the same execution.
    ///
    /// The `ValidationCleanupGuard` arms cleanup while the sync wait is in
    /// flight. If the MCP request is cancelled before handoff, a queued start is
    /// removed atomically; an accepted/running Job is marked cleanup-pending,
    /// stopped, and retained for reconciliation until a terminal Runner update
    /// confirms cleanup. Once a terminal result or handoff is produced, the
    /// guard is disarmed.
    async fn await_validation_job(
        &self,
        job_id: String,
        sync_wait_secs: u64,
        adapter: &'static dyn ValidationAdapter,
        handoff: ValidationHandoff,
    ) -> ToolResult {
        let mut guard = ValidationCleanupGuard::new(
            self.runner_registry.clone(),
            job_id.clone(),
            handoff.auth.clone(),
        );
        // Poll the job status up to the effective synchronous grace. Each poll
        // is cheap and total sleep stays bounded by that grace. The wait is
        // taken from the runtime's injectable clock so tests can shrink it.
        let wait = self
            .validation_sync_wait
            .min(std::time::Duration::from_secs(sync_wait_secs));
        let deadline = std::time::Instant::now() + wait;
        loop {
            let status = self
                .runner_registry
                .get_hidden_job_for_auth(handoff.auth.as_ref(), &job_id)
                .await;
            let (terminal, observed_status) = match status {
                Ok(status) => (
                    crate::tool_runtime::jobs::is_terminal_job_status(&status.status),
                    status.status,
                ),
                Err(_) => (false, "unknown".to_string()),
            };
            if terminal {
                // The terminal result builder removes the hidden Job record.
                let result = self
                    .validation_terminal_result(job_id, adapter, &observed_status, handoff)
                    .await;
                guard.disarm();
                return result;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        // Promote under the registry lock. A terminal update may race the
        // deadline; promote_hidden_job deliberately leaves such a record hidden
        // so the original Cargo call can still return its structured terminal
        // result instead of handing off an already-finished Job.
        let promoted = match self.runner_registry.promote_hidden_job(&job_id).await {
            Ok(job) => job,
            Err(error) => {
                return ToolResult::err(command_rejected_message(
                    error,
                    "query list_jobs and retry the validation if the job could not be handed off.",
                ));
            }
        };
        let latest_status = promoted.status.clone();
        if crate::tool_runtime::jobs::is_terminal_job_status(&latest_status) {
            let result = self
                .validation_terminal_result(job_id, adapter, &latest_status, handoff)
                .await;
            guard.disarm();
            return result;
        }
        let observation = match structured_job_observation(
            &self.runner_registry,
            handoff.auth.as_ref(),
            &job_id,
        )
        .await
        {
            Ok(observation) => observation,
            Err(error) => {
                return ToolResult::err(command_rejected_message(
                    error,
                    "observe the returned Job from list_jobs before deciding whether any retry is safe.",
                ));
            }
        };
        let latest_status = observation.job.status.clone();
        if crate::tool_runtime::jobs::is_terminal_job_status(&latest_status) {
            let result = self
                .validation_terminal_result(job_id, adapter, &latest_status, handoff)
                .await;
            guard.disarm();
            return result;
        }
        let observation_token = match observation.job.observation_token.clone() {
            Some(token) => token,
            None => {
                return ToolResult::err(
                    "promoted validation job has no canonical observation token".to_string(),
                );
            }
        };
        let (execution_state, command_started) = validation_handoff_execution_state(
            latest_status.as_str(),
            observation.job.started_at.is_some(),
        );
        let detected_summary = crate::tool_runtime::jobs::detected_job_summary_with_activity(
            Some(&handoff.command_summary),
            Some(&handoff.purpose),
            &latest_status,
            observation.job.exit_code.map(i64::from),
            &observation.stdout_tail,
            &observation.stderr_tail,
            observation.job.activity.as_ref(),
        );
        let payload = json!({
            "execution_source": handoff.execution_source,
            "purpose": handoff.purpose,
            "execution_state": execution_state,
            "job_id": handoff.job_id,
            "job_status": latest_status,
            "observation_token": observation_token,
            "activity": observation.job.activity,
            "promoted_to_job": true,
            "command_started": command_started,
            "command_completed": false,
            "effective_timeout_secs": handoff.effective_timeout_secs,
            "sync_wait_secs": handoff.sync_wait_secs,
            "project": handoff.project,
            "cwd": handoff.cwd,
            "shell": handoff.shell,
            "executor": handoff.executor,
            "command_summary": handoff.command_summary,
            "stdout_tail": observation.stdout_tail,
            "stderr_tail": observation.stderr_tail,
            "stdout_lines": observation.stdout_lines,
            "stderr_lines": observation.stderr_lines,
            "stdout_truncated": observation.stdout_truncated,
            "stderr_truncated": observation.stderr_truncated,
            "detected_summary": detected_summary,
            "terminal": false,
        });
        guard.disarm();
        ToolResult::ok(payload)
    }

    /// Build a terminal structured validation result from a Job's final state.
    async fn validation_terminal_result(
        &self,
        job_id: String,
        adapter: &'static dyn ValidationAdapter,
        job_status: &str,
        handoff: ValidationHandoff,
    ) -> ToolResult {
        let log = self
            .runner_registry
            .hidden_job_log_for_auth(handoff.auth.as_ref(), &job_id, Some(200))
            .await;
        let (job, stdout, stderr, stdout_source_truncated, stderr_source_truncated) = match log {
            Ok((job, stdout, stderr, next_stdout_line, next_stderr_line)) => {
                let stdout = stdout.unwrap_or_default();
                let stderr = stderr.unwrap_or_default();
                let stdout_source_truncated = job.stdout_log_truncated
                    || job.stdout_retained_from_line.is_some_and(|line| line > 1)
                    || stdout.lines().count() < next_stdout_line.saturating_sub(1);
                let stderr_source_truncated = job.stderr_log_truncated
                    || job.stderr_retained_from_line.is_some_and(|line| line > 1)
                    || stderr.lines().count() < next_stderr_line.saturating_sub(1);
                (
                    Some(job),
                    stdout,
                    stderr,
                    stdout_source_truncated,
                    stderr_source_truncated,
                )
            }
            Err(_) => (None, String::new(), String::new(), true, true),
        };
        let (stdout_tail, bounded_stdout_truncated) = bounded_tail(&stdout, CARGO_STDIO_TAIL_CHARS);
        let (stderr_tail, bounded_stderr_truncated) = bounded_tail(&stderr, CARGO_STDIO_TAIL_CHARS);
        let stdout_truncated = stdout_source_truncated || bounded_stdout_truncated;
        let stderr_truncated = stderr_source_truncated || bounded_stderr_truncated;
        let exit_code = job.as_ref().and_then(|job| job.exit_code);
        let lifecycle = RunnerJobLifecycle::from_wire(job_status).ok();
        let timed_out = lifecycle.is_some_and(RunnerJobLifecycle::is_timed_out);
        let process_passed =
            lifecycle == Some(RunnerJobLifecycle::Completed) && exit_code == Some(0);
        let mut payload = json!({
            "project": handoff.project,
            "command_summary": handoff.command_summary,
            "cwd": handoff.cwd,
            "shell": handoff.shell,
            "executor": handoff.executor,
            "execution_source": handoff.execution_source,
            "purpose": handoff.purpose,
            "execution_state": if timed_out { "timed_out" } else { "completed" },
            "exit_code": exit_code,
            "duration_ms": job.as_ref().and_then(|job| job.duration_ms),
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "passed": process_passed,
            "command_started": true,
            "command_completed": !timed_out,
            "promoted_to_job": false,
            "effective_timeout_secs": handoff.effective_timeout_secs,
            "sync_wait_secs": handoff.sync_wait_secs,
            "terminal": true,
        });
        if let Some(projection) = crate::tool_runtime::jobs::validation_job_projection_with_policy(
            Some(adapter.tool_identity()),
            Some(adapter.validation_kind()),
            job_status,
            exit_code.map(i64::from),
            &stdout_tail,
            &stderr_tail,
            stdout_truncated || stderr_truncated,
            handoff.minimum_tests,
            handoff.require_tests,
            handoff.no_run,
        ) {
            apply_validation_projection_fields(&mut payload, &projection);
        }
        let validation_passed = payload
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(process_passed);
        if validation_passed {
            // Discard the hidden Job record so a fast validation never leaves a
            // redundant visible job in list_jobs.
            self.runner_registry
                .remove_projected_hidden_terminal_job_record(&job_id)
                .await;
            ToolResult::ok(payload)
        } else {
            let failure_kind = if timed_out {
                "timeout"
            } else {
                CARGO_VALIDATION_FAILURE_KIND
            };
            payload["failure_kind"] = json!(failure_kind);
            let result = ToolResult {
                success: false,
                output: payload,
                error: Some(if timed_out {
                    command_timeout_message(
                        handoff.effective_timeout_secs,
                        &stdout_tail,
                        &stderr_tail,
                    )
                } else {
                    if process_passed {
                        "cargo_test test-count assertion was not proven; inspect test_count_assertion and rerun with a scope that executes enough tests.".to_string()
                    } else {
                        format!(
                            "structured validation command failed; {VALIDATION_FAILURE_GUIDANCE}"
                        )
                    }
                }),
            };
            self.runner_registry
                .remove_projected_hidden_terminal_job_record(&job_id)
                .await;
            result
        }
    }

    /// Build a structured cargo result for a command that ran synchronously in
    /// this process (local path). Mirrors the previous terminal structure.
    #[allow(clippy::too_many_arguments)]
    async fn build_cargo_result(
        &self,
        project: &str,
        command: &str,
        cwd: Option<&str>,
        config: &crate::projects::ProjectConfig,
        adapter: &'static dyn ValidationAdapter,
        output: ProjectCommandOutput,
        timeout_secs: u64,
        sync_wait_secs: u64,
        promoted_to_job: bool,
        source_stdout_truncated: bool,
        source_stderr_truncated: bool,
        minimum_tests: Option<u64>,
        require_tests: Option<bool>,
        no_run: Option<bool>,
    ) -> ToolResult {
        let execution_state = output.execution_state;
        let (stdout_tail, bounded_stdout_truncated) =
            bounded_tail(&output.stdout, CARGO_STDIO_TAIL_CHARS);
        let (stderr_tail, bounded_stderr_truncated) =
            bounded_tail(&output.stderr, CARGO_STDIO_TAIL_CHARS);
        let stdout_truncated = source_stdout_truncated || bounded_stdout_truncated;
        let stderr_truncated = source_stderr_truncated || bounded_stderr_truncated;
        let process_passed =
            execution_state == ShellCommandExecutionState::Completed && output.exit_code == Some(0);
        let validation_failed = is_cargo_validation_failure(&output, timeout_secs);
        let resolved_cwd = super::helpers::resolve_runner_cwd(config, cwd)
            .and_then(|path| super::helpers::project_relative_runner_cwd(config, &path))
            .unwrap_or_else(|_| ".".to_string());
        let shell = "configured";
        let executor = "agent";
        let purpose = match adapter.validation_kind() {
            "test" => "test",
            "format" => "format",
            _ => "validation",
        };
        let mut payload = json!({
            "project": project,
            "command_summary": crate::runner_http::command_preview(command),
            "cwd": resolved_cwd,
            "shell": shell,
            "executor": executor,
            "execution_source": adapter.tool_identity(),
            "purpose": purpose,
            "execution_state": command_execution_state_name(execution_state),
            "exit_code": output.exit_code,
            "duration_ms": output.duration_ms,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "stdout_lines": output.stdout.lines().count(),
            "stderr_lines": output.stderr.lines().count(),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "passed": process_passed,
            "command_started": execution_state != ShellCommandExecutionState::NotStarted,
            "command_completed": execution_state == ShellCommandExecutionState::Completed,
            "promoted_to_job": promoted_to_job,
            "effective_timeout_secs": timeout_secs,
            "sync_wait_secs": sync_wait_secs,
            "terminal": execution_state != ShellCommandExecutionState::OutcomeUnknown,
        });
        let terminal_status = match execution_state {
            ShellCommandExecutionState::NotStarted | ShellCommandExecutionState::OutcomeUnknown => {
                "lost"
            }
            ShellCommandExecutionState::TimedOut => "timeout",
            ShellCommandExecutionState::Completed if process_passed => "completed",
            ShellCommandExecutionState::Completed => "failed",
        };
        if let Some(projection) = crate::tool_runtime::jobs::validation_job_projection_with_policy(
            Some(adapter.tool_identity()),
            Some(adapter.validation_kind()),
            terminal_status,
            output.exit_code.map(i64::from),
            &stdout_tail,
            &stderr_tail,
            stdout_truncated || stderr_truncated,
            minimum_tests,
            require_tests,
            no_run,
        ) {
            apply_validation_projection_fields(&mut payload, &projection);
        }
        let validation_passed = payload
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(process_passed);
        match execution_state {
            ShellCommandExecutionState::Completed if validation_passed => ToolResult::ok(payload),
            ShellCommandExecutionState::NotStarted => {
                payload["failure_kind"] =
                    json!(cargo_prestart_failure_kind(output.error.as_deref()));
                ToolResult {
                    success: false,
                    output: payload,
                    error: Some(command_rejected_message(
                        output
                            .error
                            .as_deref()
                            .unwrap_or("the Runner rejected the structured validation command before process spawn"),
                        "correct the project, cwd, permissions, or Runner availability indicated by the rejection, then retry.",
                    )),
                }
            }
            ShellCommandExecutionState::OutcomeUnknown => {
                payload["failure_kind"] = json!("outcome_unknown");
                ToolResult {
                    success: false,
                    output: payload,
                    error: Some(command_outcome_unknown_message(
                        output.error.as_deref().unwrap_or(
                            "the executor did not return a trustworthy terminal validation result",
                        ),
                    )),
                }
            }
            ShellCommandExecutionState::TimedOut => {
                payload["failure_kind"] = json!("timeout");
                ToolResult {
                    success: false,
                    output: payload,
                    error: Some(command_timeout_message(
                        timeout_secs,
                        &stdout_tail,
                        &stderr_tail,
                    )),
                }
            }
            ShellCommandExecutionState::Completed => {
                payload["failure_kind"] = json!(if validation_failed {
                    CARGO_VALIDATION_FAILURE_KIND
                } else if process_passed {
                    CARGO_VALIDATION_FAILURE_KIND
                } else {
                    "process_exit"
                });
                ToolResult {
                    success: false,
                    output: payload,
                    error: Some(if process_passed {
                        "cargo_test test-count assertion was not proven; inspect test_count_assertion and rerun with a scope that executes enough tests.".to_string()
                    } else {
                        format!(
                            "structured validation command failed; {VALIDATION_FAILURE_GUIDANCE}"
                        )
                    }),
                }
            }
        }
    }

    /// The previous synchronous cargo path, used by mutating `cargo fmt` (and
    /// the old `run_cargo_command`). Kept separate so the read-only tools go
    /// through the shared exactly-once validation path.
    async fn run_cargo_command_sync(
        &self,
        project: String,
        cwd: Option<String>,
        command: String,
        timeout_secs: u64,
        adapter: &'static dyn ValidationAdapter,
    ) -> ToolResult {
        let config = match self.resolve_project(&project).await {
            Ok(config) => config,
            Err(e) => {
                return ToolResult::err(command_rejected_message(
                    e.to_message(),
                    "verify the project id/cwd and agent connectivity, then retry or use run_shell for custom diagnostics.",
                ))
            }
        };
        let output = match self
            .run_project_command_capture(
                &project,
                command.clone(),
                timeout_secs,
                cwd.clone(),
            )
            .await
        {
            Ok(output) => output,
            Err(e) => {
                return ToolResult::err(command_rejected_message(
                    e,
                    "verify the project id/cwd and agent connectivity, then retry or use run_shell for custom diagnostics.",
                ))
            }
        };
        self.build_cargo_result(
            &project,
            &command,
            cwd.as_deref(),
            &config,
            adapter,
            output,
            timeout_secs,
            0,
            false,
            false,
            false,
            None,
            None,
            None,
        )
        .await
    }
}

/// Structured validation request carried through the read-only tool path.
struct ValidationRunRequest<'a> {
    project: String,
    cwd: Option<String>,
    check: bool,
    filter: Option<String>,
    all_targets: Option<bool>,
    all_features: Option<bool>,
    no_default_features: Option<bool>,
    features: Option<String>,
    package: Option<String>,
    no_run: Option<bool>,
    require_tests: Option<bool>,
    minimum_tests: Option<u64>,
    go_packages: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    sync_wait_secs: Option<u64>,
    session_id: Option<String>,
    ssh_resource: Option<&'a str>,
    auth: Option<&'a AuthContext>,
}

/// Job handoff / terminal projection metadata for a promoted validation.
struct ValidationHandoff {
    execution_source: String,
    purpose: String,
    job_id: String,
    effective_timeout_secs: u64,
    sync_wait_secs: u64,
    project: String,
    cwd: String,
    shell: String,
    executor: String,
    command_summary: String,
    minimum_tests: Option<u64>,
    require_tests: Option<bool>,
    no_run: Option<bool>,
    auth: Option<webcodex_runner_registry::RunnerAccess>,
}

/// Build the canonical structured validation step for a read-only validation tool
/// from the same options the synchronous adapter would have used.
fn validation_step(
    tool_name: &str,
    options: &ValidationCommandOptions,
) -> Result<ShellJobValidationStep, String> {
    if tool_name != "go_test" && options.go_packages.is_some() {
        return Err("only go_test accepts Go package patterns".to_string());
    }
    let (name, program, args) = match tool_name {
        "cargo_fmt" => (
            "format",
            "cargo",
            vec!["fmt".to_string(), "--".to_string(), "--check".to_string()],
        ),
        "cargo_check" => {
            let mut args = vec!["check".to_string()];
            if options.all_targets.unwrap_or(true) {
                args.push("--all-targets".to_string());
            }
            if options.all_features.unwrap_or(false) {
                args.push("--all-features".to_string());
            }
            if options.no_default_features.unwrap_or(false) {
                args.push("--no-default-features".to_string());
            }
            push_paired_arg(&mut args, "--features", options.features.as_deref())?;
            push_paired_arg(&mut args, "-p", options.package.as_deref())?;
            ("check", "cargo", args)
        }
        "cargo_test" => {
            let mut args = vec!["test".to_string()];
            if let Some(filter) = options.filter.as_deref() {
                // Whitespace-only filter means "no filter", matching the
                // synchronous path. Option-like filters are rejected by the
                // shared filter contract before any argv is built.
                if let Some(normalized) =
                    crate::runner_protocol::normalize_rust_test_filter(filter)?
                {
                    args.push(normalized);
                }
            }
            if options.all_targets.unwrap_or(false) {
                args.push("--all-targets".to_string());
            }
            if options.all_features.unwrap_or(false) {
                args.push("--all-features".to_string());
            }
            if options.no_default_features.unwrap_or(false) {
                args.push("--no-default-features".to_string());
            }
            push_paired_arg(&mut args, "--features", options.features.as_deref())?;
            push_paired_arg(&mut args, "-p", options.package.as_deref())?;
            if options.no_run.unwrap_or(false) {
                args.push("--no-run".to_string());
            }
            ("test", "cargo", args)
        }
        "go_test" => {
            let packages =
                crate::runner_protocol::normalize_go_test_packages(options.go_packages.as_deref())
                    .map_err(|reason| format!("packages {reason}"))?;
            let mut args = vec!["test".to_string(), "-json".to_string()];
            args.extend(packages);
            ("test", "go", args)
        }
        _ => return Err("unknown validation tool".to_string()),
    };
    let step = ShellJobValidationStep {
        name: name.to_string(),
        program: program.to_string(),
        args,
        env: Vec::new(),
    };
    if !step.is_canonical() {
        return Err("structured validation step is not canonical".to_string());
    }
    Ok(step)
}

/// Append a value-taking Cargo flag with its already-normalized value.
/// Normalization is the shared `normalize_cargo_value` contract (a single
/// trim, non-empty, not `-`-prefixed, NUL/control-free, bounded), so the
/// structured argv matches what the synchronous path would have built. Values
/// are normalized here and written normalized into argv, never passed through
/// raw.
fn push_paired_arg(args: &mut Vec<String>, flag: &str, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(normalized) = crate::runner_protocol::normalize_cargo_value(value)? else {
        return Ok(());
    };
    args.push(flag.to_string());
    args.push(normalized);
    Ok(())
}

fn apply_validation_projection_fields(payload: &mut Value, projection: &Value) {
    for field in [
        "passed",
        "warnings_count",
        "errors_count",
        "tests_detected",
        "tests_run_count",
        "tests_passed",
        "tests_failed",
        "zero_tests_run",
        "test_count_assertion",
        "diagnostics",
    ] {
        if let Some(value) = projection.get(field) {
            if field != "passed" || value.is_boolean() {
                payload[field] = value.clone();
            }
        }
    }
}

/// Cancellation guard for a hidden structured validation Job during its
/// synchronous wait window.
///
/// If the MCP request is cancelled before the tool returns, Drop synchronously
/// records cleanup intent in the registry and then triggers asynchronous stop
/// delivery. The registry lifecycle retries delayed intents; active records are
/// retained for reconciliation until a terminal Runner update confirms cleanup.
/// Once the tool has produced a terminal result or public handoff, the guard is
/// disarmed.
struct ValidationCleanupGuard {
    clients: std::sync::Arc<crate::runner_http::RunnerRegistry>,
    job_id: String,
    auth: Option<webcodex_runner_registry::RunnerAccess>,
    armed: bool,
}

impl ValidationCleanupGuard {
    fn new(
        clients: std::sync::Arc<crate::runner_http::RunnerRegistry>,
        job_id: String,
        auth: Option<webcodex_runner_registry::RunnerAccess>,
    ) -> Self {
        Self {
            clients,
            job_id,
            auth,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ValidationCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Register intent before attempting any asynchronous work. This is the
        // durable in-process safety boundary: if the immediate processor is
        // delayed or the runtime is closing, the periodic registry lifecycle
        // still sees and retries the cleanup without deleting the active record.
        let job_id = self.job_id.clone();
        let auth = self.auth.clone();
        let clients = self.clients.clone();
        clients.record_hidden_cleanup_intent(job_id, auth);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                clients.process_hidden_cleanup_intents().await;
            });
        }
    }
}

fn validation_handoff_execution_state(status: &str, started: bool) -> (&'static str, bool) {
    let pending_status = matches!(
        RunnerJobLifecycle::from_wire(status),
        Ok(RunnerJobLifecycle::Queued
            | RunnerJobLifecycle::RunnerQueued
            | RunnerJobLifecycle::StartedLegacy)
    );
    let command_started = !pending_status || started;
    (
        if command_started { "running" } else { "queued" },
        command_started,
    )
}

#[cfg(test)]
mod validation_handoff_projection_tests {
    use super::validation_handoff_execution_state;

    #[test]
    fn fresh_started_evidence_prevents_false_queued_validation_handoff() {
        assert_eq!(
            validation_handoff_execution_state("agent_queued", false),
            ("queued", false)
        );
        assert_eq!(
            validation_handoff_execution_state("agent_queued", true),
            ("running", true)
        );
        assert_eq!(
            validation_handoff_execution_state("started", true),
            ("running", true)
        );
        assert_eq!(
            validation_handoff_execution_state("running", true),
            ("running", true)
        );
    }
}

#[cfg(test)]
mod structured_cargo_arg_parity_tests {
    use super::*;

    #[test]
    fn cargo_test_count_inputs_resolve_to_the_stricter_bounded_minimum() {
        assert_eq!(resolve_cargo_test_minimum(None, None, None).unwrap(), None);
        assert_eq!(
            resolve_cargo_test_minimum(Some(false), None, None).unwrap(),
            None
        );
        assert_eq!(
            resolve_cargo_test_minimum(Some(true), None, None).unwrap(),
            Some(1)
        );
        assert_eq!(
            resolve_cargo_test_minimum(Some(true), Some(6), None).unwrap(),
            Some(6)
        );
        assert_eq!(
            resolve_cargo_test_minimum(Some(false), Some(6), None).unwrap(),
            Some(6)
        );
        for result in [
            resolve_cargo_test_minimum(None, Some(0), None),
            resolve_cargo_test_minimum(
                None,
                Some(crate::runner_protocol::CARGO_TEST_MIN_TESTS_MAX + 1),
                None,
            ),
            resolve_cargo_test_minimum(Some(true), None, Some(true)),
            resolve_cargo_test_minimum(None, Some(6), Some(true)),
        ] {
            let rejection = result.expect_err("invalid assertion input must fail closed");
            assert!(!rejection.success);
            assert_eq!(rejection.output["execution_state"], "not_started");
            assert_eq!(rejection.output["command_started"], false);
            assert_eq!(rejection.output["failure_kind"], "invalid_arguments");
        }
    }

    /// The structured Job argv builder must normalize a value-taking Cargo
    /// argument identically to the synchronous command builder, so the same
    /// request produces the same effective arguments no matter how long it
    /// runs. Whitespace-padded inputs are normalized in both paths; invalid
    /// values fail closed before any argv is produced.
    #[test]
    fn job_and_sync_paths_produce_the_same_normalized_values() {
        for (tool_name, options) in [
            (
                "cargo_check",
                ValidationCommandOptions {
                    all_targets: Some(true),
                    all_features: Some(true),
                    no_default_features: Some(true),
                    features: Some("  serde  ".to_string()),
                    package: Some("  my-crate  ".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_test",
                ValidationCommandOptions {
                    filter: Some("  module::nested::test  ".to_string()),
                    all_targets: Some(true),
                    all_features: Some(true),
                    no_default_features: Some(true),
                    features: Some("  a  b  ".to_string()),
                    package: Some("  my-crate  ".to_string()),
                    no_run: Some(true),
                    ..ValidationCommandOptions::default()
                },
            ),
        ] {
            // Sync path: build_command normalizes via the shared contract and
            // shell-escapes. The parsed argv words after `cargo <sub>` must
            // contain the normalized values.
            let adapter = validation_adapter_for_tool(tool_name).unwrap();
            let sync = adapter
                .build_command(options.clone())
                .unwrap_or_else(|error| panic!("{tool_name} sync build: {error}"));
            assert!(
                sync.contains("serde") || sync.contains("a  b"),
                "{tool_name} sync command missing normalized feature: {sync}"
            );
            assert!(sync.contains("my-crate"), "{tool_name} sync: {sync}");
            if tool_name == "cargo_test" {
                assert!(
                    sync.contains("module::nested::test"),
                    "cargo_test sync missing normalized filter: {sync}"
                );
            }

            // Job path: validation_step writes normalized values into the
            // structured argv, never the raw padded strings.
            let step = validation_step(tool_name, &options).unwrap();
            let joined = step.args.join(" ");
            assert!(
                !joined.contains("  serde") && !joined.contains("serde  "),
                "{tool_name} job argv must contain normalized feature: {joined:?}"
            );
            assert!(
                joined.contains("serde") || joined.contains("a  b"),
                "{tool_name} job argv missing feature: {joined:?}"
            );
            assert!(
                !joined.contains("  my-crate") && !joined.contains("my-crate  "),
                "{tool_name} job argv must contain normalized package: {joined:?}"
            );
            assert!(joined.contains("my-crate"), "{tool_name} job: {joined:?}");
            if tool_name == "cargo_test" {
                assert!(
                    step.args.iter().any(|arg| arg == "module::nested::test"),
                    "cargo_test job argv must contain the normalized filter: {joined:?}"
                );
            }
            assert!(step.is_canonical(), "{tool_name} step must be canonical");
        }
    }

    #[test]
    fn go_test_job_and_sync_paths_share_normalized_package_scope() {
        let options = ValidationCommandOptions {
            go_packages: Some(vec![
                "./internal/control".to_string(),
                "./internal/node".to_string(),
            ]),
            ..ValidationCommandOptions::default()
        };
        let adapter = validation_adapter_for_tool("go_test").unwrap();
        assert_eq!(
            adapter.build_command(options.clone()).unwrap(),
            "go test -json './internal/control' './internal/node'"
        );
        let step = validation_step("go_test", &options).unwrap();
        assert_eq!(step.name, "test");
        assert_eq!(step.program, "go");
        assert_eq!(
            step.args,
            vec!["test", "-json", "./internal/control", "./internal/node"]
        );
        assert!(step.env.is_empty());
        assert!(step.is_structured_go_test_json());
        assert!(step.is_canonical());
    }

    #[test]
    fn invalid_cargo_values_fail_closed_on_both_paths() {
        for (tool_name, invalid) in [
            (
                "cargo_check",
                ValidationCommandOptions {
                    features: Some("--no-run".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_check",
                ValidationCommandOptions {
                    package: Some("--all-features".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_check",
                ValidationCommandOptions {
                    features: Some("line\nbreak".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_check",
                ValidationCommandOptions {
                    features: Some("a".repeat(crate::runner_protocol::CARGO_VALUE_MAX_BYTES + 1)),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_test",
                ValidationCommandOptions {
                    filter: Some("--all-features".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_test",
                ValidationCommandOptions {
                    filter: Some("line\nbreak".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
        ] {
            // Sync path: build_command must reject before any command string.
            let adapter = validation_adapter_for_tool(tool_name).unwrap();
            assert!(
                adapter.build_command(invalid.clone()).is_err(),
                "{tool_name} sync must reject {invalid:?}"
            );
            // Job path: validation_step must reject before any argv is built.
            assert!(
                validation_step(tool_name, &invalid).is_err(),
                "{tool_name} job must reject {invalid:?}"
            );
        }
    }

    #[test]
    fn whitespace_only_values_mean_option_omitted_on_both_paths() {
        for (tool_name, options) in [
            (
                "cargo_check",
                ValidationCommandOptions {
                    features: Some("   ".to_string()),
                    package: Some("   ".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
            (
                "cargo_test",
                ValidationCommandOptions {
                    filter: Some("   ".to_string()),
                    features: Some("   ".to_string()),
                    ..ValidationCommandOptions::default()
                },
            ),
        ] {
            let adapter = validation_adapter_for_tool(tool_name).unwrap();
            let sync = adapter.build_command(options.clone()).unwrap();
            assert!(
                !sync.contains("--features") && !sync.contains(" -p "),
                "{tool_name} whitespace-only values must be omitted: {sync}"
            );
            let step = validation_step(tool_name, &options).unwrap();
            assert!(
                !step
                    .args
                    .iter()
                    .any(|arg| arg == "--features" || arg == "-p"),
                "{tool_name} whitespace-only values must be omitted: {:?}",
                step.args
            );
            assert!(step.is_canonical(), "{tool_name} step must be canonical");
        }
    }
}
