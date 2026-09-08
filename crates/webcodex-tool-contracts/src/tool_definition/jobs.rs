use super::RunnerCapabilityRequirement::{
    AsyncJobs, DetachedProcess, PersistentShell, Shell, StructuredProcess, StructuredScript,
};
use super::ToolVisibility::{ModelHidden, ModelVisible};
use super::{
    adaptive_runtime_direct, def, model_spec, permission_risk, require_all_scopes,
    requires_explicit_business_session, ToolDefinition, PERMISSION_RISK_JOB, TOOL_CATEGORY_JOB,
};
use crate::metadata::{
    ToolPathHint::None as NoPath,
    ToolRisk::{JobRun, Read},
    JOB_RUN, RUNTIME_READ, TOOL_PROVIDER_NATIVE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    job_log_input_schema, job_status_input_schema, list_jobs_input_schema,
    observe_jobs_input_schema, open_session_shell_input_schema, run_detached_process_input_schema,
    run_job_input_schema, run_process_input_schema, run_script_input_schema,
    run_shell_input_schema, session_shell_exec_input_schema, session_shell_identity_input_schema,
    stop_job_input_schema,
};
use webcodex_core::authority::SCOPE_JOB_DETACH;

pub(super) const EXECUTION_DEFINITIONS: &[ToolDefinition] = &[
    adaptive_runtime_direct(
        model_spec(
            def(
                "run_process",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .session_input(super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                        "executable",
                        "args",
                        "stdin",
                        "process_summary",
                    ]))
                    .execution(super::ToolAuditExecutionPolicy::DIRECT_ARGV_TEST_COUNTS),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(StructuredProcess),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                true,
            ),
            "Run one isolated one-shot native executable with literal argv and no shell parsing. Ordinary local command sequences stay on structured tools/run_process; do not open a persistent shell merely to run several commands. Use local persistent shell only when the same local shell process must retain cwd/env/exports/functions/umask. For repeated commands on one named SSH resource with remote state, prefer persistent shell; a new persistent SSH target uses ssh_resource onboarding first. Explicit one-shot/no-persistence SSH remains valid here. Long work continues as the same execution and stays Runner-owned; discover run_detached_process only when accepted native work must outlive the Runner.",
            run_process_input_schema,
        ),
        70,
    ),
    require_all_scopes(
        model_spec(
            def(
                "run_detached_process",
                super::ToolAuditPolicy::TYPED_CANONICAL.session_input(
                    super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                        "executable",
                        "args",
                        "stdin",
                        "idempotency_key",
                        "process_summary",
                    ]),
                ),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(DetachedProcess),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::Keyed,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                true,
            ),
            "Start a supervisor-owned detached native process as a durable Job when accepted work must outlive the initiating Runner process, such as across Runner exit, restart, upgrade, or replacement. Ownership is handed off before payload start; a replacement Runner can recover the same logical Job only when the durable supervisor/native identity and lifetime fence reconcile. A bounded replay key blocks duplicate dispatch while the Job is active or retained; expired keys are not retry tokens. Observe or stop with Job tools. No shell, script, SSH-resource, or retry fallback.",
            run_detached_process_input_schema,
        ),
        &[JOB_RUN, SCOPE_JOB_DETACH],
    ),
    model_spec(
        def(
            "run_script",
            super::ToolAuditPolicy::TYPED_CANONICAL
                .session_input(super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                    "script",
                    "args",
                    "stdin",
                    "script_summary",
                ]))
                .execution(super::ToolAuditExecutionPolicy::SCRIPT_TEST_COUNTS),
            ModelVisible,
            TOOL_CATEGORY_JOB,
            Some(StructuredScript),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Execute,
                risk: JobRun,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(JOB_RUN),
            true,
            NoPath,
            true,
            true,
        ),
        "Run bounded sh, bash, or PowerShell content as typed script data from a Runner-owned file. Long work continues as the same execution, owned by the current Runner; the script body never becomes shell command text. If work must outlive the current Runner process, use a native executable and discover run_detached_process instead.",
        run_script_input_schema,
    ),
    model_spec(
        def(
            "run_shell",
            super::ToolAuditPolicy::TYPED_CANONICAL
                .session_input(super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                    "command",
                    "command_summary",
                ]))
                .execution(super::ToolAuditExecutionPolicy::TEST_COUNTS),
            ModelVisible,
            TOOL_CATEGORY_JOB,
            Some(Shell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Execute,
                risk: JobRun,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(JOB_RUN),
            true,
            NoPath,
            true,
            true,
        ),
        "Run one bounded shell command as an escape hatch for real shell syntax. Ordinary local command sequences should not move to persistent shell merely because several commands are needed; prefer structured validation/process/edit tools, then run_shell when shell syntax is required. Local persistent shell is only for true same-process cwd/env/export/function/umask state. Repeated commands on one named SSH resource are the primary persistent-shell route; new persistent SSH targets use ssh_resource onboarding and Runner restart. Longer shell work stays Runner-owned; use run_detached_process only for native argv work that must outlive the current Runner process.",
        run_shell_input_schema,
    ),
    requires_explicit_business_session(model_spec(
            def(
                "open_session_shell",
                super::ToolAuditPolicy::TYPED_CANONICAL,
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(PersistentShell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                true,
            ),
            "Open one bounded long-lived shell for an explicit Workflow Session. Primary use: one shell for repeated commands on the active named SSH resource in execution_context.resource, preserving remote cwd/env/exports/functions/umask. Local sh/bash or Windows PowerShell remains supported only when same local shell-process state is actually required, not merely for several commands. New SSH targets use ssh_resource list/register, Runner restart, list again, then update_session_context; no per-shell host/resource parameter. The SSH target does not need WebCodex Runner.",
            open_session_shell_input_schema,
    )),
    requires_explicit_business_session(model_spec(
            def(
                "session_shell_exec",
                super::ToolAuditPolicy::TYPED_CANONICAL.session_input(
                    super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                        "command",
                        "command_summary",
                    ]),
                ),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(PersistentShell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                true,
            ),
            "Execute one framed command in an existing Session persistent shell. Primary route is repeated commands on the same named SSH resource while retaining remote cwd/env/exports/functions/umask. Local persistent execution remains supported only when the same local shell process must retain state; ordinary local command sequences should use structured tools/run_process/run_script, with the shell escape hatch only for real shell syntax. Commands are serialized in the same shell process.",
            session_shell_exec_input_schema,
    )),
    requires_explicit_business_session(model_spec(
        def(
            "session_shell_status",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_JOB,
            Some(PersistentShell),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(RUNTIME_READ),
            true,
            NoPath,
            false,
            false,
        ),
        "Read Runner-authoritative state for an explicit Session persistent shell. This never sends input to the process.",
        session_shell_identity_input_schema,
    )),
    requires_explicit_business_session(permission_risk(
        model_spec(
            def(
                "close_session_shell",
                super::ToolAuditPolicy::TYPED_CANONICAL,
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(PersistentShell),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Mutate,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::DesiredState,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                false,
            ),
            "Idempotently close an explicit Session persistent shell and terminate its complete process group.",
            session_shell_identity_input_schema,
        ),
        PERMISSION_RISK_JOB,
    )),
    permission_risk(
        model_spec(
            def(
                "run_job",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .session_input(super::ToolAuditSessionInputPolicy::OmitTopLevel(&[
                        "command",
                        "command_summary",
                    ]))
                    .execution(super::ToolAuditExecutionPolicy::TEST_COUNTS),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                Some(AsyncJobs),
                TOOL_PROVIDER_RUNNER,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Execute,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::NonIdempotent,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                true,
            ),
            "Start one Runner-owned asynchronous shell Job and return its stable job_id. Queued execution keeps that identity; observe the existing Job before considering any retry. Server disconnect/restart can reconcile the same Job while the owning Runner process remains, but a replacement Runner does not inherit ordinary Jobs. If work must outlive the current Runner process, discover run_detached_process instead.",
            run_job_input_schema,
        ),
        TOOL_CATEGORY_JOB,
    ),
    permission_risk(
        model_spec(
            def(
                "stop_job",
                super::ToolAuditPolicy::TYPED_CANONICAL,
                ModelVisible,
                TOOL_CATEGORY_JOB,
                None,
                TOOL_PROVIDER_NATIVE,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Mutate,
                    risk: JobRun,
                    approval: super::ToolApprovalPolicy::Standard,
                    idempotency: super::ToolIdempotency::DesiredState,
                },
                Some(JOB_RUN),
                true,
                NoPath,
                true,
                false,
            ),
            "Stop one existing WebCodex Job by job_id. Requires confirm=true and preserves project/session ownership; log bodies are not returned.",
            stop_job_input_schema,
        ),
        PERMISSION_RISK_JOB,
    ),
    model_spec(
        def(
            "job_status",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_JOB,
            None,
            TOOL_PROVIDER_NATIVE,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(RUNTIME_READ),
            false,
            NoPath,
            false,
            false,
        ),
        "Read bounded lifecycle state for one existing Job. Never starts or retries work; command preview is opt-in and log bodies are excluded.",
        job_status_input_schema,
    ),
    model_spec(
        def(
            "job_log",
            super::ToolAuditPolicy::TYPED_CANONICAL,
            ModelVisible,
            TOOL_CATEGORY_JOB,
            None,
            TOOL_PROVIDER_NATIVE,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(RUNTIME_READ),
            false,
            NoPath,
            false,
            false,
        ),
        "Read bounded stdout/stderr for one Job. Return its opaque token to receive only new output; reset means a bounded recovery tail. wait_secs performs one bounded wait. Never starts or retries execution.",
        job_log_input_schema,
    ),
    adaptive_runtime_direct(
        model_spec(
            def(
                "observe_jobs",
                super::ToolAuditPolicy::TYPED_CANONICAL
                    .session_input(super::ToolAuditSessionInputPolicy::ObserveJobs),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                None,
                TOOL_PROVIDER_NATIVE,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Observe,
                    risk: Read,
                    approval: super::ToolApprovalPolicy::None,
                    idempotency: super::ToolIdempotency::PureRead,
                },
                Some(RUNTIME_READ),
                false,
                NoPath,
                false,
                false,
            ),
            "Observe 1 to 8 existing Jobs with bounded baseline/delta logs and isolated item errors. Optionally performs one shared wait for the batch and returns when any relevant Job changes; final items are non-waiting snapshots. Ordinary all-success observations may use a compact result projection; each opaque observation token is returned unchanged. reset remains explicit bounded recovery rather than exact delta. unknown_job exposes recovery_tool=list_jobs for direct caller-visible Job re-observation. Never launches, retries, stops, or subscribes.",
            observe_jobs_input_schema,
        ),
        80,
    ),
];

pub(super) const LISTING_DEFINITIONS: &[ToolDefinition] = &[
    adaptive_runtime_direct(
        model_spec(
            def(
                "list_jobs",
                super::ToolAuditPolicy::TYPED_CANONICAL.session_input(
                    super::ToolAuditSessionInputPolicy::OmitTopLevel(&["project", "session_id"]),
                ),
                ModelVisible,
                TOOL_CATEGORY_JOB,
                None,
                TOOL_PROVIDER_NATIVE,
                super::ToolSemanticContract {
                    effect: super::ToolEffect::Observe,
                    risk: Read,
                    approval: super::ToolApprovalPolicy::None,
                    idempotency: super::ToolIdempotency::PureRead,
                },
                Some(RUNTIME_READ),
                false,
                NoPath,
                false,
                false,
            ),
            "List bounded lifecycle metadata for caller-visible Jobs. Inside a coding Session, prefer exact project/session_id filters; status combines with them using AND semantics. stdout/stderr bodies are never included.",
            list_jobs_input_schema,
        ),
        85,
    ),
    def(
        "job_tail",
        super::ToolAuditPolicy::TYPED_CANONICAL,
        ModelHidden,
        TOOL_CATEGORY_JOB,
        None,
        TOOL_PROVIDER_NATIVE,
        super::ToolSemanticContract {
            effect: super::ToolEffect::Observe,
            risk: Read,
            approval: super::ToolApprovalPolicy::None,
            idempotency: super::ToolIdempotency::PureRead,
        },
        Some(RUNTIME_READ),
        false,
        NoPath,
        false,
        false,
    ),
];
