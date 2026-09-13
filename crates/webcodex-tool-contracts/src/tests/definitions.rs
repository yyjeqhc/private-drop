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
    let apply_patch = lookup_tool_definition("apply_patch").expect("apply_patch definition");
    assert!(apply_patch.visibility.is_model_visible());
    assert_eq!(
        apply_patch.adaptive_runtime_direct_rank(),
        None,
        "specialized patching should stay ModelVisible but use Adaptive discovery/gateway"
    );
    assert!(
        derived
            .iter()
            .any(|definition| definition.name == "apply_text_edits"),
        "canonical ordinary edits must remain adaptive-direct"
    );

    for (name, expected_rank) in [
        ("rotate_agent_continuation_endpoint", 19),
        ("import_conversation_files_to_project", 55),
        ("export_project_artifact", 56),
        ("run_detached_process", 72),
        ("run_shell", 75),
        ("observe_jobs", 80),
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
        "attach_agent_endpoint",
        "apply_patch",
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

    let observe_jobs = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "observe_jobs")
        .expect("observe_jobs ToolSpec");
    let list_jobs = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "list_jobs")
        .expect("list_jobs ToolSpec");
    assert!(observe_jobs
        .description
        .contains("do not call list_jobs first"));
    assert!(observe_jobs.description.contains("after_observation_token"));
    assert!(observe_jobs.description.contains("bounded wait_secs"));
    assert!(list_jobs
        .description
        .contains("Recovery and inventory primitive"));
    assert!(list_jobs
        .description
        .contains("continue that Job with observe_jobs"));

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

#[test]
fn operator_extension_families_are_definition_owned_and_registry_derived() {
    use ToolOperatorExtensionFamily::{
        MemoryManagement, MemoryRuntime, SkillManagement, SkillRuntime, TraceDiagnostics,
    };

    let cases: &[(ToolOperatorExtensionFamily, &[&str])] = &[
        (SkillRuntime, &["skill_list", "skill_read_file"]),
        (
            SkillManagement,
            &[
                "skill_versions",
                "skill_install",
                "skill_activate",
                "skill_remove_revision",
            ],
        ),
        (MemoryRuntime, &["memory_search", "memory_read"]),
        (
            MemoryManagement,
            &[
                "memory_set",
                "memory_delete",
                "memory_scope_list",
                "memory_scope_purge",
            ],
        ),
        (TraceDiagnostics, &["read_tool_trace"]),
    ];

    for (family, expected_names) in cases {
        for name in *expected_names {
            assert_eq!(runtime_tool_operator_extension_family(name), Some(*family));
            assert_eq!(
                lookup_tool_definition(name)
                    .and_then(|definition| definition.operator_extension_family),
                Some(*family)
            );
        }

        let actual_names = match family {
            SkillRuntime => skill_runtime_tool_specs(),
            SkillManagement => skill_management_tool_specs(),
            MemoryRuntime => memory_runtime_tool_specs(),
            MemoryManagement => memory_management_tool_specs(),
            TraceDiagnostics => operator_diagnostic_tool_specs(),
        }
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
        assert_eq!(
            actual_names,
            expected_names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>()
        );
    }

    for definition in
        tool_definitions().filter(|definition| definition.operator_extension_family.is_some())
    {
        assert!(
            definition.visibility.is_model_hidden(),
            "{} operator extension must remain ModelHidden and surface-gated",
            definition.name
        );
    }

    let declared_names = tool_definitions()
        .filter(|definition| definition.operator_extension_family.is_some())
        .map(|definition| definition.name)
        .collect::<BTreeSet<_>>();
    let registry_names = stateless_operator_extension_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        declared_names,
        registry_names.iter().map(String::as_str).collect(),
        "registry operator-extension membership must be derived from ToolDefinition families"
    );

    assert_eq!(runtime_tool_operator_extension_family("run_shell"), None);
    assert_eq!(runtime_tool_operator_extension_family("unknown_tool"), None);
}
