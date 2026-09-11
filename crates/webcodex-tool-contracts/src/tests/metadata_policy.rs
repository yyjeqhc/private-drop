use super::*;

#[test]
fn tool_specs_annotations_are_canonical_semantic_projections() {
    use crate::metadata::{ToolApprovalPolicy, ToolEffect, ToolIdempotency, ToolRisk};

    let specs = registered_tool_specs();
    for spec in &specs {
        let metadata = lookup_tool_metadata(&spec.name)
            .unwrap_or_else(|| panic!("{} missing metadata", spec.name));
        let annotations = spec
            .annotations
            .as_object()
            .unwrap_or_else(|| panic!("{} annotations must be an object", spec.name));
        for field in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(
                annotations.contains_key(field),
                "{} missing annotation {}",
                spec.name,
                field
            );
        }
        assert_eq!(
            annotations["readOnlyHint"],
            metadata.effect.read_only_hint(),
            "{} readOnlyHint must derive from Effect",
            spec.name
        );
        assert_eq!(
            annotations["destructiveHint"], metadata.destructive,
            "{} destructiveHint must derive from canonical metadata",
            spec.name
        );
        assert_eq!(
            annotations["idempotentHint"],
            metadata.idempotency.mcp_hint(),
            "{} idempotentHint must derive from Idempotency",
            spec.name
        );
        assert_eq!(
            annotations["openWorldHint"], metadata.shell_like,
            "{} openWorldHint must remain independent from effect/approval",
            spec.name
        );
    }

    let cases = [
        ("read_files", ToolEffect::Observe, ToolIdempotency::PureRead),
        (
            "close_session",
            ToolEffect::Mutate,
            ToolIdempotency::DesiredState,
        ),
        (
            "coding_agent_start",
            ToolEffect::Execute,
            ToolIdempotency::Keyed,
        ),
        (
            "complete_session_message",
            ToolEffect::Mutate,
            ToolIdempotency::FencedReplay,
        ),
        (
            "write_project_file",
            ToolEffect::Mutate,
            ToolIdempotency::NonIdempotent,
        ),
    ];
    for (name, effect, idempotency) in cases {
        let metadata = lookup_tool_metadata(name).unwrap();
        assert_eq!(metadata.effect, effect, "{name}");
        assert_eq!(metadata.idempotency, idempotency, "{name}");
        let annotations = &spec_named(&specs, name).annotations;
        assert_eq!(
            annotations["readOnlyHint"],
            effect == ToolEffect::Observe,
            "{name}"
        );
        assert_eq!(
            annotations["idempotentHint"],
            idempotency.mcp_hint(),
            "{name}"
        );
    }

    for name in [
        "apply_patch",
        "apply_text_edits",
        "apply_unified_diff",
        "write_project_file",
        "workspace_checkpoint_restore",
        "save_project_artifact",
        "import_conversation_files_to_project",
        "artifact_upload_finish",
        "artifact_upload_abort",
        "assign_agent_task",
        "reconcile_agent_task_coding_run",
        "heartbeat_agent_task_attempt",
        "complete_agent_task_attempt",
        "update_agent_identity",
        "attach_agent_endpoint",
        "detach_agent_endpoint",
        "consume_agent_deliveries",
        "consume_agent_wake",
        "coding_agent_cancel",
        "computer_write_clipboard",
        "computer_pointer_click",
        "computer_control",
        "computer_key_input",
        "update_session_context",
        "close_session",
        "resolve_session_message",
        "complete_session_message",
        "cargo_fmt",
    ] {
        let metadata = lookup_tool_metadata(name).unwrap();
        assert!(metadata.destructive, "{name}");
        assert_eq!(
            spec_named(&specs, name).annotations["destructiveHint"],
            true,
            "{name} may replace, restore, delete, or discard existing state"
        );
    }

    for name in [
        "workspace_checkpoint_create",
        "git_commit_paths",
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "computer_save_snapshot",
        "start_agent_task_attempt",
        "create_conversation",
        "post_conversation_message",
        "post_session_message",
        "work_on_project",
        "cargo_check",
    ] {
        let metadata = lookup_tool_metadata(name).unwrap();
        assert!(!metadata.destructive, "{name}");
        assert_eq!(
            spec_named(&specs, name).annotations["destructiveHint"],
            false,
            "{name} is intentionally additive-only"
        );
    }

    let close = lookup_tool_metadata("close_session").unwrap();
    assert_eq!(close.approval, ToolApprovalPolicy::None);
    assert_eq!(close.risk, ToolRisk::SessionCollaborate);
    assert!(!runtime_tool_requires_permission("close_session"));

    let cancel = lookup_tool_metadata("coding_agent_cancel").unwrap();
    assert_eq!(cancel.effect, ToolEffect::Mutate);
    assert_eq!(cancel.risk, ToolRisk::RunControl);
    assert_eq!(cancel.approval, ToolApprovalPolicy::InheritFromStart);
    assert_eq!(cancel.idempotency, ToolIdempotency::DesiredState);
    assert_eq!(
        spec_named(&specs, "coding_agent_cancel").annotations["readOnlyHint"],
        false
    );

    let commit = lookup_tool_metadata("git_commit_paths").unwrap();
    assert_eq!(commit.effect, ToolEffect::Mutate);
    assert_eq!(commit.risk, ToolRisk::ProjectWrite);
    assert_eq!(commit.approval, ToolApprovalPolicy::Standard);
    assert_eq!(commit.idempotency, ToolIdempotency::NonIdempotent);
    assert_eq!(
        commit.authority,
        ToolAuthorityPolicy::RequireAll(&[PROJECT_WRITE, JOB_RUN])
    );
    assert_eq!(
        spec_named(&specs, "coding_agent_cancel").annotations["idempotentHint"],
        true
    );
    assert!(!runtime_tool_requires_permission("coding_agent_cancel"));

    assert_eq!(
        spec_named(&specs, "run_shell").annotations["openWorldHint"],
        true
    );
    assert_eq!(
        spec_named(&specs, "delete_project_files").annotations["destructiveHint"],
        true
    );
}
