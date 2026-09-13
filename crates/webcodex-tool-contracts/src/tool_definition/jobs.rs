use super::RunnerCapabilityRequirement::{
    AsyncJobs, DetachedProcess, PersistentShell, Shell, StructuredProcess, StructuredScript,
};
use super::ToolVisibility::{ModelHidden, ModelVisible};
use super::{
    adaptive_runtime_direct, context_reobservable, def, model_spec, permission_risk,
    require_all_scopes, requires_explicit_business_session, ToolDefinition, PERMISSION_RISK_JOB,
    TOOL_CATEGORY_JOB,
};
use crate::metadata::{
    ToolPathHint::None as NoPath,
    ToolRisk::{JobRun, Read},
    JOB_RUN, RUNTIME_READ, TOOL_PROVIDER_NATIVE, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    list_jobs_input_schema, observe_jobs_input_schema, open_session_shell_input_schema,
    run_detached_process_input_schema, run_job_input_schema, run_process_input_schema,
    run_script_input_schema, run_shell_input_schema, session_shell_exec_input_schema,
    session_shell_identity_input_schema, stop_job_input_schema,
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Run one one-shot executable with structured argv. This is the preferred route for one native executable with literal argv; Windows batch shims use the bounded Runner-owned quoting contract on executable. Use run_shell only when shell semantics or a short tightly related command chain is required. Do not open a persistent shell merely to run several commands: local persistence is only for same-process cwd/env/exports/functions/umask state; repeated commands on one named SSH resource preserve remote state. New persistent SSH targets use ssh_resource onboarding; one-shot/no-persistence SSH remains valid. Long work continues as the same execution and stays Runner-owned. If the native child must outlive the Runner because this Runner will restart, upgrade, stop, or be replaced, use run_detached_process from the start; duration alone is not a reason to detach.",
            run_process_input_schema,
        ),
        70,
    ),
    adaptive_runtime_direct(
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
                    super::ToolSessionEvidencePolicy::NONE,
                ),
                "Start a supervisor-owned detached native process as a durable Job when accepted work must outlive the initiating Runner process. Use it from the start when the workflow will restart, upgrade, stop, or replace this Runner and a native child must remain alive across Runner exit or replacement. Duration alone is not a reason to detach: ordinary long work stays Runner-owned. Ownership is handed off before payload start; after restart or upgrade, a replacement Runner can recover the same logical Job only when the supervisor/native identity and lifetime fence reconcile. A bounded replay key prevents duplicate dispatch while retained; expired keys are not retry tokens. Observe or stop with Job tools. No shell, script, SSH-resource, or retry fallback.",
                run_detached_process_input_schema,
            ),
            &[JOB_RUN, SCOPE_JOB_DETACH],
        ),
        72,
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
            super::ToolSessionEvidencePolicy::NONE,
        ),
        "Run bounded sh, bash, PowerShell, JavaScript, or TypeScript as typed Runner-owned script data. JavaScript is Node.js-backed fixed .mjs ESM. TypeScript uses Node native erasable type stripping in .mts ESM, requires Node.js 22.6+, does not type-check, and rejects enum and other transform-required syntax. The Runner owns runtime selection/flags; WebCodex does not install npm dependencies, run tsc, or fall back to Bun/Deno/tsx. Relative ESM imports resolve from the Runner-owned temporary module, not project cwd. Prefer run_process for native argv, run_script for program-like scripts, and run_shell when shell grammar is required. Long work continues as the same execution / same Job and is never restarted; script bodies never become shell command text. If a native child must outlive the Runner across restart/upgrade/stop/replacement, use run_detached_process from the start.",
        run_script_input_schema,
    ),
    adaptive_runtime_direct(
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Run one bounded shell command or short tightly related shell command chain. Use it for shell semantics such as &&, pipes, redirects, globbing, substitution, or one tightly related observation goal to reduce model/tool round trips. Keep run_process preferred for one native executable with literal argv. Do not chain independent effects or failure/permission boundaries such as validation, commit, push, deploy, or restart. Use run_script for program-like loops/conditionals/functions/traps/multi-stage logic. Persistent shell is only for same-process cwd/env/export/function/umask state or repeated commands on one named SSH resource. Longer shell work stays Runner-owned. If a native child must outlive the current Runner process across restart/upgrade/stop/replacement, use run_detached_process from the start; shell duration alone is not a reason to detach.",
            run_shell_input_schema,
        ),
        75,
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
                super::ToolSessionEvidencePolicy::NONE.persistent_shell(super::PersistentShellEvidenceAction::Open),
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
                super::ToolSessionEvidencePolicy::NONE.persistent_shell(super::PersistentShellEvidenceAction::Exec),
            ),
            "Execute one framed command in an existing Session persistent shell. Primary route is repeated commands on the same named SSH resource while retaining remote cwd/env/exports/functions/umask. Local persistent execution remains supported only when the same local shell process must retain state; ordinary one-shot work should use run_process, run_shell for shell semantics or short tightly related chains, and run_script for program-like shell content. Several commands alone are not a reason to open persistent shell. Commands are serialized in the same shell process.",
            session_shell_exec_input_schema,
    )),
    requires_explicit_business_session(context_reobservable(model_spec(
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
            super::ToolSessionEvidencePolicy::NONE.persistent_shell(super::PersistentShellEvidenceAction::Status),
        ),
        "Read Runner-authoritative state for an explicit Session persistent shell. This never sends input to the process.",
        session_shell_identity_input_schema,
    ))),
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
                super::ToolSessionEvidencePolicy::NONE.persistent_shell(super::PersistentShellEvidenceAction::Close),
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Start one Runner-owned asynchronous shell Job immediately and return its stable job_id. Use this only when asynchronous shell execution is intentional from the first call; ordinary work should start on its synchronous execution or structured validation tool and let long execution hand off as the same Job. Queued execution keeps its identity; observe before considering retry. Server disconnect/restart can reconcile the same Job while the owning Runner process remains, but a replacement Runner does not inherit ordinary Jobs. Do not use run_job when a native child must outlive the current Runner process across restart, upgrade, stop, or replacement; use run_detached_process from the start.",
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Stop one existing WebCodex Job by job_id. Requires confirm=true and preserves project/session ownership; log bodies are not returned.",
            stop_job_input_schema,
        ),
        PERMISSION_RISK_JOB,
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Primary continuation path for an already-known Job. If an initiating execution tool returned job_id, observe that exact Job directly; do not call list_jobs first. When observation_token is available, pass it unchanged as after_observation_token, then use each returned newer token on the next observation. Supports 1 to 8 Jobs, bounded baseline/delta logs, isolated item errors, and one shared bounded wait_secs (clamped to 60) so callers can wait for change instead of actively polling. reset remains explicit bounded recovery rather than exact delta. Use list_jobs only when Job identity is genuinely lost or inventory is explicitly needed; unknown_job exposes recovery_tool=list_jobs for that recovery path. Never launches, retries, stops, or subscribes.",
            observe_jobs_input_schema,
        ),
        80,
    ),
];

pub(super) const LISTING_DEFINITIONS: &[ToolDefinition] = &[
    adaptive_runtime_direct(
        context_reobservable(model_spec(
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
                super::ToolSessionEvidencePolicy::NONE,
            ),
            "Recovery and inventory primitive for caller-visible Jobs, not the normal continuation step. Do not call list_jobs when the initiating tool or current context already provides an exact job_id; continue that Job with observe_jobs instead. Use list_jobs when exact Job identity was lost, unknown_job explicitly requests inventory recovery, the user asks to enumerate background work, or multiple historical/parallel Jobs must be inspected. Exact project/session_id filters are preferred when known and combine with status using AND semantics. stdout/stderr bodies are never included; exact Job logs and continuation belong to observe_jobs.",
            list_jobs_input_schema,
        )),
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
        super::ToolSessionEvidencePolicy::NONE,
    ),
];
