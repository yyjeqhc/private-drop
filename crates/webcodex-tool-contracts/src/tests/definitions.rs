use super::*;

#[test]
fn tool_definitions_cover_known_names_and_public_specs() {
    let definition_names = tool_definitions()
        .map(|definition| definition.name)
        .collect::<BTreeSet<_>>();
    let definition_order = tool_definitions()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    let known_names = known_tool_names().collect::<BTreeSet<_>>();
    let hidden_names = model_hidden_tool_names().collect::<BTreeSet<_>>();
    let definition_hidden_names = tool_definitions()
        .filter(|definition| definition.visibility.is_model_hidden())
        .map(|definition| definition.name)
        .collect::<BTreeSet<_>>();
    for name in known_tool_names() {
        assert!(
            lookup_tool_definition(name).is_some(),
            "{name} missing ToolDefinition lookup"
        );
    }
    assert_eq!(definition_names, known_names);
    assert_eq!(definition_order, known_tool_names().collect::<Vec<_>>());
    assert_eq!(hidden_names, definition_hidden_names);

    let specs = registered_tool_specs();
    let spec_names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<BTreeSet<_>>();
    let visible_definition_names = model_visible_tool_definitions()
        .map(|definition| definition.name)
        .collect::<BTreeSet<_>>();
    let spec_order = specs
        .iter()
        .map(|spec| spec.name.clone())
        .collect::<Vec<_>>();
    let visible_definition_order = model_visible_tool_definitions()
        .map(|definition| definition.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(spec_names, visible_definition_names);
    assert_eq!(visible_definition_order, spec_order);
    assert_eq!(registered_tool_names(), visible_definition_order);
}

#[test]
fn every_runtime_tool_has_an_explicit_fail_closed_audit_contract() {
    for definition in tool_definitions() {
        assert_eq!(
            runtime_tool_audit_policy(definition.name),
            Some(definition.audit_policy()),
            "{} audit policy must resolve only through ToolDefinition",
            definition.name
        );
        assert!(
            matches!(
                definition.audit_policy().request,
                ToolAuditRequestPolicy::Typed | ToolAuditRequestPolicy::TypedDropNullValues
            ),
            "{} request audit must use the typed canonical boundary",
            definition.name
        );
        if let ToolAuditResultPolicy::Fields(fields) = definition.audit_policy().result {
            assert!(
                !fields.is_empty(),
                "{} narrowed result audit must declare at least one bounded field",
                definition.name
            );
        }
        match definition.audit_policy().session_input {
            ToolAuditSessionInputPolicy::OmitTopLevel(fields) => assert!(
                !fields.is_empty(),
                "{} Session input omission policy must name at least one field",
                definition.name
            ),
            ToolAuditSessionInputPolicy::Bounded
            | ToolAuditSessionInputPolicy::SearchProjectTexts
            | ToolAuditSessionInputPolicy::ObserveJobs => {}
        }
        match definition.audit_policy().context {
            ToolAuditContextPolicy::ResultProjection => assert!(
                matches!(
                    definition.audit_policy().result,
                    ToolAuditResultPolicy::Fields(_)
                ),
                "{} may reuse Session context only from a bounded field result projection",
                definition.name
            ),
            ToolAuditContextPolicy::Fields(fields) => assert!(
                !fields.is_empty(),
                "{} Session context policy must declare at least one bounded field",
                definition.name
            ),
            ToolAuditContextPolicy::Omit | ToolAuditContextPolicy::WorkingTreeStatus => {}
        }
        if definition.audit_policy().execution.detail == ToolAuditExecutionDetail::Omit {
            assert_eq!(
                definition.audit_policy().execution.shell,
                ToolAuditExecutionShell::Output,
                "{} omitted execution evidence must not declare synthetic shell provenance",
                definition.name
            );
        }
    }

    assert_eq!(
        runtime_tool_audit_policy("coding_agent_observe").map(|policy| policy.result),
        Some(ToolAuditResultPolicy::Semantic(
            ToolAuditSemanticResultPolicy::CodingAgentObservation
        ))
    );
    for name in ["git_commit_paths", "git_review_summary"] {
        assert_eq!(
            runtime_tool_audit_policy(name).map(|policy| policy.request),
            Some(ToolAuditRequestPolicy::TypedDropNullValues),
            "{name} must preserve legacy omission of invalid normalized commit values"
        );
    }
    assert_eq!(runtime_tool_audit_policy("unknown_open_world_tool"), None);
    assert_eq!(runtime_tool_audit_policy("start_coding_task"), None);
}

#[test]
fn adaptive_runtime_direct_declarations_are_visible_ranked_and_unique() {
    let mut seen_ranks = std::collections::BTreeMap::new();
    for definition in tool_definitions() {
        let Some(rank) = definition.adaptive_runtime_direct_rank() else {
            continue;
        };
        assert!(definition.visibility.is_model_visible());
        assert!(seen_ranks.insert(rank, definition.name).is_none());
    }

    let derived = adaptive_runtime_direct_tool_definitions();
    assert!(!derived.is_empty());
    for pair in derived.windows(2) {
        assert!(pair[0].adaptive_runtime_direct_rank() < pair[1].adaptive_runtime_direct_rank());
    }
    assert_eq!(derived.len(), seen_ranks.len());
    let apply_patch = derived
        .iter()
        .find(|definition| definition.name == "apply_patch")
        .expect("apply_patch must be adaptive-direct");
    let apply_text_edits = derived
        .iter()
        .find(|definition| definition.name == "apply_text_edits")
        .expect("apply_text_edits must be adaptive-direct");
    assert!(
        apply_text_edits.adaptive_runtime_direct_rank()
            < apply_patch.adaptive_runtime_direct_rank()
    );

    for (name, expected_rank) in [
        ("import_conversation_files_to_project", 55),
        ("export_project_artifact", 56),
        ("run_shell", 75),
    ] {
        let definition = derived
            .iter()
            .copied()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} must be adaptive-direct"));
        assert_eq!(
            definition.adaptive_runtime_direct_rank(),
            Some(expected_rank)
        );
    }

    for name in [
        "runner_config_check",
        "runner_config_reload",
        "ssh_resource",
        "open_session_shell",
        "session_shell_exec",
        "session_shell_status",
        "close_session_shell",
        "run_script",
        "save_project_artifact",
        "read_project_artifact",
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
        "go_test",
    ] {
        let definition = lookup_tool_definition(name).expect("model-visible long-tail definition");
        assert_eq!(
            definition.adaptive_runtime_direct_rank(),
            None,
            "{name} should stay behind adaptive discovery/gateway"
        );
    }

    for (name, expected_rank, expected_authority) in [
        ("session_discussion_summary", 15, RUNTIME_READ),
        ("list_jobs", 85, RUNTIME_READ),
        ("git_diff_hunks", 125, PROJECT_READ),
    ] {
        let definition = derived
            .iter()
            .copied()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} must be adaptive-direct"));
        assert_eq!(
            definition.adaptive_runtime_direct_rank(),
            Some(expected_rank)
        );
        assert_eq!(definition.metadata.effect, ToolEffect::Observe);
        assert_eq!(definition.metadata.risk, ToolRisk::Read);
        assert_eq!(definition.metadata.approval, ToolApprovalPolicy::None);
        assert_eq!(definition.metadata.idempotency, ToolIdempotency::PureRead);
        assert_eq!(
            definition.metadata.authority,
            ToolAuthorityPolicy::Require(expected_authority)
        );
    }

    let git_review = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "git_review_summary")
        .expect("git_review_summary ToolSpec");
    assert!(git_review.description.contains("git_diff_hunks/read_files"));
    assert!(!git_review.description.contains("git_diff_hunks/read_file "));
}

#[test]
fn tool_definitions_drive_metadata_visibility_and_categories() {
    for definition in tool_definitions() {
        let metadata = definition.metadata();
        let facade_metadata = lookup_tool_metadata(definition.name)
            .copied()
            .unwrap_or_else(|| panic!("{} missing metadata facade entry", definition.name));
        assert_eq!(metadata, facade_metadata);
        assert_eq!(metadata.name, definition.name);
        assert_eq!(
            definition.visibility.is_model_hidden(),
            is_model_hidden_tool_name(definition.name)
        );
        assert_eq!(definition.category, runtime_tool_category(definition.name));
        assert_eq!(definition.metadata().authority, metadata.authority);
    }
}
