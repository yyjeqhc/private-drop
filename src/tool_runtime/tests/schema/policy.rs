use super::*;

#[test]
fn required_runner_capability_matches_metadata_risk_table() {
    use crate::tool_runtime::metadata::{lookup_tool_metadata, ToolRisk, TOOL_PROVIDER_RUNNER};
    use crate::tool_runtime::tool_definition::is_model_visible_tool_name;

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
            "search_project_texts",
            ToolRisk::Read,
            RunnerCapabilityRequirement::Shell,
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
        // Hidden tools have no spec, so sample_tool_args cannot build args.
        // Their capability routing still matters (they dispatch); verify it
        // against the ToolDefinition directly. Visible tools go through the
        // full ToolCall path.
        if is_model_visible_tool_name(name) {
            let call = ToolCall::from_tool_name(name, sample_tool_args(name))
                .unwrap_or_else(|e| panic!("{name} should deserialize: {e}"));
            assert_eq!(
                required_runner_capability(&call),
                Some(capability),
                "{name} capability"
            );
        } else {
            assert_eq!(
                crate::tool_runtime::tool_definition::lookup_tool_definition(name)
                    .unwrap()
                    .runner_capability,
                Some(capability),
                "{name} (hidden) capability must match dispatch helper"
            );
        }
    }
}
