use super::*;

#[test]
fn tool_definitions_are_context_continuity_ssot() {
    use crate::metadata::ToolEffect;
    use crate::tool_definition::{
        runtime_tool_accepts_context_ack, runtime_tool_advances_context_checkpoint,
        runtime_tool_context_continuity_policy,
    };
    use crate::tool_policy::lookup_tool_definition;

    for (name, accepts_ack, advances_checkpoint) in [
        ("read_files", true, false),
        ("search_project_texts", true, false),
        ("tool_manifest", true, false),
        ("show_changes", true, false),
        ("work_on_project", true, false),
        ("apply_text_edits", true, true),
        ("run_process", true, true),
        ("observe_jobs", true, true),
    ] {
        let definition =
            lookup_tool_definition(name).unwrap_or_else(|| panic!("missing definition for {name}"));
        let direct = definition.context_continuity_policy();
        assert_eq!(
            runtime_tool_context_continuity_policy(name),
            direct,
            "{name}"
        );
        assert_eq!(direct.accepts_context_ack, accepts_ack, "{name}");
        assert_eq!(
            direct.advances_context_checkpoint(),
            advances_checkpoint,
            "{name}"
        );
        assert_eq!(
            runtime_tool_accepts_context_ack(name),
            accepts_ack,
            "{name}"
        );
        assert_eq!(
            runtime_tool_advances_context_checkpoint(name),
            advances_checkpoint,
            "{name}"
        );
    }

    assert_eq!(
        lookup_tool_definition("work_on_project")
            .unwrap()
            .metadata()
            .effect,
        ToolEffect::Mutate
    );
    assert_eq!(
        lookup_tool_definition("show_changes")
            .unwrap()
            .metadata()
            .effect,
        ToolEffect::Observe
    );
    assert_eq!(
        lookup_tool_definition("observe_jobs")
            .unwrap()
            .metadata()
            .effect,
        ToolEffect::Observe
    );
    assert!(!runtime_tool_advances_context_checkpoint("show_changes"));
    assert!(runtime_tool_advances_context_checkpoint("observe_jobs"));

    assert!(runtime_tool_accepts_context_ack("unknown_open_world_tool"));
    assert!(runtime_tool_advances_context_checkpoint(
        "unknown_open_world_tool"
    ));
}

#[test]
fn tool_definitions_drive_session_and_permission_policy() {
    use crate::metadata::{
        ToolApprovalPolicy, ToolAuthorityPolicy, ToolEffect, ToolIdempotency, ToolRisk,
        PROJECT_WRITE,
    };
    use crate::tool_definition::{
        runtime_tool_approval_policy, runtime_tool_captures_validation_output,
        runtime_tool_disabled_message, runtime_tool_extra_accepted_flattened_args,
        runtime_tool_is_change_summary_like, runtime_tool_is_git_like, runtime_tool_is_read_like,
        runtime_tool_is_shell_like, runtime_tool_is_write_like, runtime_tool_permission_risk,
        runtime_tool_requires_explicit_business_session, runtime_tool_requires_permission,
        runtime_tool_session_risk_class, tool_definitions, PERMISSION_RISK_ARTIFACT_WRITE,
        PERMISSION_RISK_DESTRUCTIVE, PERMISSION_RISK_JOB, PERMISSION_RISK_PATCH,
        PERMISSION_RISK_SHELL, PERMISSION_RISK_VALIDATION, PERMISSION_RISK_WRITE,
        TOOL_DISCOVERY_GROUPS, TOOL_DISCOVERY_GROUP_GIT,
    };
    use crate::tool_policy::lookup_tool_definition;

    let text_input = lookup_tool_definition("computer_input_text").expect("computer input tool");
    assert!(text_input.is_write_like());
    assert!(text_input.requires_permission());
    assert_eq!(text_input.metadata().risk, ToolRisk::ComputerControl);

    let application_launch = lookup_tool_definition("computer_launch_application")
        .expect("computer application launch tool");
    assert!(application_launch.is_write_like());
    assert!(application_launch.requires_permission());
    assert_eq!(
        application_launch.metadata().risk,
        ToolRisk::ComputerControl
    );

    for (name, effect, risk) in [
        ("apply_patch", ToolEffect::Mutate, ToolRisk::ProjectWrite),
        (
            "apply_text_edits",
            ToolEffect::Mutate,
            ToolRisk::ProjectWrite,
        ),
        (
            "delete_project_files",
            ToolEffect::Mutate,
            ToolRisk::ProjectWrite,
        ),
        ("run_shell", ToolEffect::Execute, ToolRisk::JobRun),
        ("cargo_check", ToolEffect::Execute, ToolRisk::JobRun),
        (
            "computer_control",
            ToolEffect::Execute,
            ToolRisk::ComputerControl,
        ),
        (
            "post_conversation_message",
            ToolEffect::Mutate,
            ToolRisk::CommunicationManage,
        ),
    ] {
        let metadata = lookup_tool_definition(name)
            .unwrap_or_else(|| panic!("{name} definition"))
            .metadata();
        assert_eq!(metadata.effect, effect, "{name}");
        assert_eq!(metadata.risk, risk, "{name}");
        assert_eq!(metadata.approval, ToolApprovalPolicy::Standard, "{name}");
        assert!(runtime_tool_requires_permission(name), "{name}");
    }

    let patch_metadata = lookup_tool_definition("apply_patch")
        .expect("apply_patch definition")
        .metadata();
    assert_eq!(
        patch_metadata.authority,
        ToolAuthorityPolicy::Require(PROJECT_WRITE)
    );
    assert_eq!(patch_metadata.effect, ToolEffect::Mutate);
    assert_eq!(patch_metadata.risk, ToolRisk::ProjectWrite);
    assert_eq!(patch_metadata.approval, ToolApprovalPolicy::Standard);
    assert_eq!(patch_metadata.idempotency, ToolIdempotency::NonIdempotent);

    let git_group = TOOL_DISCOVERY_GROUPS
        .iter()
        .find(|group| group.name == TOOL_DISCOVERY_GROUP_GIT)
        .expect("git discovery group")
        .tools
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    for definition in tool_definitions() {
        let metadata = definition.metadata();
        assert_eq!(
            definition.session_risk_class(),
            metadata.risk.session_risk_class(),
            "{} session risk class must derive from metadata risk",
            definition.name
        );
        assert_eq!(
            definition.is_read_like(),
            metadata.effect == ToolEffect::Observe,
            "{} read-like policy must derive from Effect",
            definition.name
        );
        assert_eq!(
            definition.is_write_like(),
            matches!(
                metadata.risk,
                ToolRisk::ProjectWrite
                    | ToolRisk::SkillManage
                    | ToolRisk::MemoryManage
                    | ToolRisk::CommunicationManage
                    | ToolRisk::ComputerControl
            ),
            "{} write-like policy must derive from metadata",
            definition.name
        );
        assert_eq!(
            definition.is_shell_like(),
            metadata.shell_like || metadata.risk == ToolRisk::JobRun,
            "{} shell-like guard policy must include job-run tools",
            definition.name
        );
        assert_eq!(
            definition.is_git_like(),
            git_group.contains(definition.name),
            "{} git-like ledger policy must mirror the git discovery group",
            definition.name
        );
        assert_eq!(
            definition.requires_permission(),
            metadata.approval.requires_permission(),
            "{} permission requirement must derive from ApprovalPolicy",
            definition.name
        );
        assert_eq!(
            runtime_tool_approval_policy(definition.name),
            metadata.approval,
            "{} approval facade must use canonical ToolDefinition metadata",
            definition.name
        );
        if metadata.destructive {
            assert_ne!(
                metadata.effect,
                ToolEffect::Observe,
                "{} destructive tools cannot be observations",
                definition.name
            );
        }
        if metadata.shell_like {
            assert_ne!(
                metadata.effect,
                ToolEffect::Observe,
                "{} open-world shell tools cannot be observations",
                definition.name
            );
        }
        if metadata.idempotency == ToolIdempotency::PureRead {
            assert_eq!(
                metadata.effect,
                ToolEffect::Observe,
                "{} PureRead requires an observation effect",
                definition.name
            );
        }
        if definition.visibility.is_model_visible() {
            assert_ne!(
                metadata.effect,
                ToolEffect::Unknown,
                "{} effect",
                definition.name
            );
            assert_ne!(
                metadata.approval,
                ToolApprovalPolicy::Unknown,
                "{} approval",
                definition.name
            );
            assert_ne!(
                metadata.idempotency,
                ToolIdempotency::Unknown,
                "{} idempotency",
                definition.name
            );
            assert_ne!(
                metadata.authority,
                ToolAuthorityPolicy::Unknown,
                "{} authority",
                definition.name
            );
        }
        assert_eq!(
            runtime_tool_session_risk_class(definition.name),
            definition.session_risk_class(),
            "{} session risk facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_is_read_like(definition.name),
            definition.is_read_like(),
            "{} read-like facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_is_write_like(definition.name),
            definition.is_write_like(),
            "{} write-like facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_is_shell_like(definition.name),
            definition.is_shell_like(),
            "{} shell-like facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_is_git_like(definition.name),
            definition.is_git_like(),
            "{} git-like facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_is_change_summary_like(definition.name),
            definition.is_change_summary_like(),
            "{} change-summary facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_captures_validation_output(definition.name),
            definition.captures_validation_output(),
            "{} validation-output facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_requires_explicit_business_session(definition.name),
            definition.requires_explicit_business_session(),
            "{} business-session facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_disabled_message(definition.name),
            definition.disabled_message(),
            "{} disabled facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_extra_accepted_flattened_args(definition.name),
            definition.extra_accepted_flattened_args(),
            "{} extra accepted flattened args facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_requires_permission(definition.name),
            definition.requires_permission(),
            "{} permission facade must use ToolDefinition",
            definition.name
        );
        assert_eq!(
            runtime_tool_permission_risk(definition.name),
            definition.permission_risk(),
            "{} permission risk facade must use ToolDefinition",
            definition.name
        );
    }

    let change_summary_tools = tool_definitions()
        .filter(|definition| definition.is_change_summary_like())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(
        change_summary_tools,
        vec![
            "git_diff_summary",
            "git_review_summary",
            "show_changes",
            "git_diff_hunks",
        ]
    );

    let validation_output_tools = tool_definitions()
        .filter(|definition| definition.captures_validation_output())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(
        validation_output_tools,
        vec!["cargo_fmt", "cargo_check", "cargo_test", "go_test"]
    );

    let explicit_business_session_tools = tool_definitions()
        .filter(|definition| definition.requires_explicit_business_session())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(
        explicit_business_session_tools,
        vec![
            "finish_coding_task",
            "session_summary",
            "update_session_context",
            "close_session",
            "validation_summary",
            "post_session_message",
            "list_session_messages",
            "get_session_assignment",
            "observe_session_messages",
            "resolve_session_message",
            "complete_session_message",
            "session_discussion_summary",
            "session_handoff_summary",
            "open_session_shell",
            "session_shell_exec",
            "session_shell_status",
            "close_session_shell"
        ]
    );

    let disabled_tools = tool_definitions()
        .filter(|definition| definition.disabled_message().is_some())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(disabled_tools, Vec::<&'static str>::new());

    let extra_accepted_flattened_arg_tools = tool_definitions()
        .filter(|definition| !definition.extra_accepted_flattened_args().is_empty())
        .map(|definition| {
            (
                definition.name,
                definition.extra_accepted_flattened_args().to_vec(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        extra_accepted_flattened_arg_tools,
        Vec::<(&str, Vec<&str>)>::new()
    );

    let unit_argument_tools = tool_definitions()
        .filter(|definition| definition.uses_unit_arguments())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(unit_argument_tools, vec!["computer_list_targets"]);

    let artifact_upload_path_binding_tools = tool_definitions()
        .filter(|definition| definition.requires_artifact_upload_path_binding())
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(
        artifact_upload_path_binding_tools,
        vec![
            "artifact_upload_chunk",
            "artifact_upload_finish",
            "artifact_upload_abort"
        ]
    );

    for (tool, risk) in [
        ("cargo_fmt", PERMISSION_RISK_VALIDATION),
        ("cargo_check", PERMISSION_RISK_VALIDATION),
        ("run_process", PERMISSION_RISK_SHELL),
        ("run_script", PERMISSION_RISK_SHELL),
        ("run_shell", PERMISSION_RISK_SHELL),
        ("run_job", PERMISSION_RISK_JOB),
        ("stop_job", PERMISSION_RISK_JOB),
        ("close_session_shell", PERMISSION_RISK_JOB),
        ("delete_project_files", PERMISSION_RISK_DESTRUCTIVE),
        ("save_project_artifact", PERMISSION_RISK_ARTIFACT_WRITE),
        (
            "import_conversation_files_to_project",
            PERMISSION_RISK_ARTIFACT_WRITE,
        ),
        ("artifact_upload_finish", PERMISSION_RISK_ARTIFACT_WRITE),
        ("artifact_upload_abort", PERMISSION_RISK_ARTIFACT_WRITE),
        ("computer_save_snapshot", PERMISSION_RISK_ARTIFACT_WRITE),
        ("apply_patch", PERMISSION_RISK_PATCH),
        ("apply_unified_diff", PERMISSION_RISK_PATCH),
        ("workspace_checkpoint_restore", PERMISSION_RISK_PATCH),
        ("write_project_file", PERMISSION_RISK_WRITE),
        ("apply_text_edits", PERMISSION_RISK_WRITE),
        ("assign_agent_task", PERMISSION_RISK_WRITE),
        ("reconcile_agent_task_coding_run", PERMISSION_RISK_WRITE),
        ("heartbeat_agent_task_attempt", PERMISSION_RISK_WRITE),
        ("complete_agent_task_attempt", PERMISSION_RISK_WRITE),
        ("update_agent_identity", PERMISSION_RISK_WRITE),
        ("attach_agent_endpoint", PERMISSION_RISK_WRITE),
        ("detach_agent_endpoint", PERMISSION_RISK_WRITE),
        ("consume_agent_deliveries", PERMISSION_RISK_WRITE),
        ("consume_agent_wake", PERMISSION_RISK_WRITE),
        ("coding_agent_cancel", PERMISSION_RISK_WRITE),
        ("computer_write_clipboard", PERMISSION_RISK_WRITE),
        ("computer_pointer_click", PERMISSION_RISK_WRITE),
        ("computer_control", PERMISSION_RISK_WRITE),
        ("computer_key_input", PERMISSION_RISK_WRITE),
        ("update_session_context", PERMISSION_RISK_WRITE),
        ("close_session", PERMISSION_RISK_WRITE),
        ("resolve_session_message", PERMISSION_RISK_WRITE),
        ("complete_session_message", PERMISSION_RISK_WRITE),
    ] {
        assert_eq!(runtime_tool_permission_risk(tool), risk, "{tool}");
    }

    let close_session = lookup_tool_definition("close_session").unwrap().metadata();
    assert_eq!(close_session.effect, ToolEffect::Mutate);
    assert_eq!(close_session.approval, ToolApprovalPolicy::None);
    assert!(!runtime_tool_requires_permission("close_session"));

    let cancel = lookup_tool_definition("coding_agent_cancel")
        .unwrap()
        .metadata();
    assert_eq!(cancel.effect, ToolEffect::Mutate);
    assert_eq!(cancel.risk, ToolRisk::RunControl);
    assert_eq!(cancel.approval, ToolApprovalPolicy::InheritFromStart);
    assert_eq!(cancel.idempotency, ToolIdempotency::DesiredState);
    assert!(!runtime_tool_requires_permission("coding_agent_cancel"));

    assert_eq!(
        runtime_tool_session_risk_class("__unknown__"),
        ToolRisk::Unknown.session_risk_class()
    );
    assert!(!runtime_tool_is_write_like("__unknown__"));
    assert!(!runtime_tool_is_shell_like("__unknown__"));
    assert!(runtime_tool_requires_permission("__unknown__"));
    assert_eq!(
        runtime_tool_permission_risk("__unknown__"),
        PERMISSION_RISK_WRITE
    );
    assert_eq!(
        runtime_tool_permission_risk("compat_patch_like"),
        PERMISSION_RISK_PATCH,
        "unknown compatibility names keep the legacy path/name fallback"
    );
    assert_ne!(
        runtime_tool_permission_risk("compat_patch_like"),
        runtime_tool_permission_risk("unknown_artifact"),
        "name-based patch fallback must not classify unrelated unknown names"
    );
}

#[test]
fn required_runner_capability_matches_metadata_risk_table() {
    use crate::metadata::{lookup_tool_metadata, ToolRisk, TOOL_PROVIDER_RUNNER};
    use crate::tool_definition::is_model_visible_tool_name;

    let cases = [
        (
            "run_process",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::StructuredProcess,
        ),
        (
            "run_detached_process",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::DetachedProcess,
        ),
        (
            "run_script",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::StructuredScript,
        ),
        (
            "coding_agent_start",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::CodingAgentRuns,
        ),
        (
            "start_agent_task_coding_run",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::CodingAgentRuns,
        ),
        (
            "run_shell",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "open_session_shell",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::PersistentShell,
        ),
        (
            "session_shell_exec",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::PersistentShell,
        ),
        (
            "session_shell_status",
            ToolRisk::Read,
            RunnerCapabilityRequirement::PersistentShell,
        ),
        (
            "close_session_shell",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::PersistentShell,
        ),
        (
            "apply_patch",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::ApplyPatch,
        ),
        (
            "apply_unified_diff",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "delete_project_files",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "git_restore_paths",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::StructuredProcess,
        ),
        (
            "git_commit_paths",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "discard_untracked",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::StructuredProcess,
        ),
        (
            "write_project_file",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "save_project_artifact",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "computer_save_snapshot",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "read_project_artifact_metadata",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "read_project_artifact",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "artifact_upload_begin",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "artifact_upload_chunk",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "artifact_upload_finish",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "artifact_upload_abort",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "apply_text_edits",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "git_status",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "git_diff",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "git_diff_hunks",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "git_review_summary",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "git_log",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "cargo_fmt",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "cargo_check",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "cargo_test",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "go_test",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::OwnerOnly,
        ),
        (
            "read_file",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "read_files",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "lsp_status",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "document_symbols",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "document_diagnostics",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "hover",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "workspace_symbols",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "goto_definition",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "find_references",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspReadOnlyNavigation,
        ),
        (
            "call_hierarchy",
            ToolRisk::Read,
            RunnerCapabilityRequirement::LspCallHierarchy,
        ),
        (
            "run_job",
            ToolRisk::JobRun,
            RunnerCapabilityRequirement::AsyncJobs,
        ),
        (
            "project_overview",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "list_project_files",
            ToolRisk::Read,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "list_project_tracked_files",
            ToolRisk::Read,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "search_project_text",
            ToolRisk::Read,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "search_project_texts",
            ToolRisk::Read,
            RunnerCapabilityRequirement::Shell,
        ),
        (
            "git_diff_summary",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "show_changes",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "workspace_hygiene_check",
            ToolRisk::Read,
            RunnerCapabilityRequirement::GitOrShell,
        ),
        (
            "workspace_checkpoint_create",
            ToolRisk::CheckpointManage,
            RunnerCapabilityRequirement::FileRead,
        ),
        (
            "workspace_checkpoint_restore",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::FileWrite,
        ),
        (
            "workspace_checkpoint_list",
            ToolRisk::Read,
            RunnerCapabilityRequirement::OwnerOnly,
        ),
        (
            "workspace_checkpoint_show",
            ToolRisk::Read,
            RunnerCapabilityRequirement::OwnerOnly,
        ),
        (
            "workspace_checkpoint_delete",
            ToolRisk::ProjectWrite,
            RunnerCapabilityRequirement::OwnerOnly,
        ),
    ];

    let specs = registered_tool_specs();
    let expected_project_tools = specs
        .iter()
        .filter_map(|spec| {
            let metadata = lookup_tool_metadata(&spec.name).unwrap();
            ((metadata.provider_id == TOOL_PROVIDER_RUNNER
                || spec.name.starts_with("workspace_checkpoint_")
                || spec.name == "computer_save_snapshot")
                && metadata.requires_project)
                .then_some(spec.name.as_str())
        })
        .collect::<BTreeSet<_>>();
    let table_project_tools = cases
        .iter()
        .map(|(name, _, _)| *name)
        .filter(|name| is_model_visible_tool_name(name))
        .collect::<BTreeSet<_>>();
    assert_eq!(table_project_tools, expected_project_tools);

    for (name, risk, capability) in cases {
        let metadata = lookup_tool_metadata(name).unwrap();
        assert_eq!(metadata.risk, risk, "{name} metadata risk");
        assert_eq!(
            lookup_tool_definition(name).unwrap().runner_capability,
            Some(capability),
            "{name} declarative Runner capability"
        );
    }
}

#[test]
fn policy_helpers_keep_unknown_non_runtime_names_fail_closed() {
    use crate::metadata::{
        lookup_tool_metadata, ToolAuthorityPolicy, ToolPathHint, ToolRisk, TOOL_PROVIDER_UNKNOWN,
    };
    use crate::tool_definition::{
        is_model_hidden_tool_name, is_model_visible_tool_name, lookup_tool_definition,
        runtime_tool_category, runtime_tool_is_read_like, runtime_tool_is_shell_like,
        runtime_tool_is_write_like, runtime_tool_metadata, runtime_tool_permission_risk,
        runtime_tool_requires_permission, runtime_tool_session_risk_class, PERMISSION_RISK_WRITE,
    };

    for name in ["__unknown_tool_for_policy_test__", "not_a_tool"] {
        let unknown = runtime_tool_metadata(name);
        assert_eq!(unknown.name, "<unknown>", "{name}");
        assert_eq!(unknown.provider_id, TOOL_PROVIDER_UNKNOWN, "{name}");
        assert_eq!(unknown.risk, ToolRisk::Unknown, "{name}");
        assert_eq!(unknown.authority, ToolAuthorityPolicy::Unknown, "{name}");
        assert!(!unknown.requires_project, "{name}");
        assert_eq!(unknown.path_hint, ToolPathHint::None, "{name}");
        assert_eq!(
            unknown.effect,
            crate::metadata::ToolEffect::Unknown,
            "{name}"
        );
        assert_eq!(
            unknown.approval,
            crate::metadata::ToolApprovalPolicy::Unknown,
            "{name}"
        );
        assert_eq!(
            unknown.idempotency,
            crate::metadata::ToolIdempotency::Unknown,
            "{name}"
        );
        assert!(!unknown.destructive, "{name}");
        assert!(!unknown.shell_like, "{name}");
        assert!(lookup_tool_metadata(name).is_none(), "{name}");
        assert!(lookup_tool_definition(name).is_none(), "{name}");
        assert!(!is_known_tool_name(name), "{name}");
        assert!(!is_model_visible_tool_name(name), "{name}");
        assert!(!is_model_hidden_tool_name(name), "{name}");
        assert_eq!(runtime_tool_category(name), "other", "{name}");
        assert_eq!(
            runtime_tool_session_risk_class(name),
            ToolRisk::Unknown.session_risk_class(),
            "{name}"
        );
        assert!(!runtime_tool_is_read_like(name), "{name}");
        assert!(!runtime_tool_is_write_like(name), "{name}");
        assert!(!runtime_tool_is_shell_like(name), "{name}");
        assert!(runtime_tool_requires_permission(name), "{name}");
        assert_eq!(
            runtime_tool_permission_risk(name),
            PERMISSION_RISK_WRITE,
            "{name}"
        );
        assert_agent_capability_lookup_rejects_non_runtime_name(name);
    }
}

fn assert_agent_capability_lookup_rejects_non_runtime_name(name: &str) {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(|| {
        let _ = crate::tool_definition::runtime_tool_runner_capability(name);
    });
    std::panic::set_hook(previous_hook);
    assert!(
        result.is_err(),
        "{name} must not resolve Runner capability through metadata fallback"
    );
}
