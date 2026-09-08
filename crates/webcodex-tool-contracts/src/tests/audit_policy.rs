use crate::{audit_policy::*, lookup_tool_definition, tool_definitions};

#[test]
fn every_canonical_definition_declares_valid_audit_policy() {
    let mut names = std::collections::BTreeSet::new();
    for definition in tool_definitions() {
        assert!(names.insert(definition.name));
        assert!(definition.audit.is_valid(), "{}", definition.name);
        assert_eq!(
            lookup_tool_definition(definition.name).unwrap().audit,
            definition.audit
        );
    }
    assert!(!names.is_empty());
    for name in ["unknown", "start_coding_task", "RunProcess", "run_process "] {
        assert!(lookup_tool_definition(name).is_none(), "{name}");
    }
}

#[test]
fn audit_policy_cannot_change_execution_authority_or_model_contract() {
    for definition in tool_definitions() {
        let mut omitted = *definition;
        omitted.audit = ToolAuditPolicy {
            request: AuditRequestPolicy {
                fields: &[],
                transform: AuditTransform::Fields,
                typed: AuditTypedPolicy::Omit,
            },
            result: AuditResultPolicy::Omit,
        };
        assert_eq!(omitted.metadata(), definition.metadata());
        assert_eq!(omitted.policy, definition.policy);
        assert_eq!(omitted.runner_capability, definition.runner_capability);
        assert_eq!(omitted.approval_policy(), definition.approval_policy());
        assert_eq!(
            omitted.requires_permission(),
            definition.requires_permission()
        );
        assert_eq!(omitted.permission_risk(), definition.permission_risk());
        assert_eq!(
            omitted.effect_annotations(),
            definition.effect_annotations()
        );
        assert_eq!(
            omitted.session_risk_class(),
            definition.session_risk_class()
        );
        assert_eq!(
            omitted.context_continuity_policy(),
            definition.context_continuity_policy()
        );
        assert_eq!(
            omitted.captures_validation_output(),
            definition.captures_validation_output()
        );
        assert_eq!(omitted.visibility, definition.visibility);
        assert_eq!(omitted.model_surface, definition.model_surface);
        if let (Some(before), Some(after)) = (definition.model_spec, omitted.model_spec) {
            assert_eq!(before.description, after.description);
            assert_eq!((before.input_schema)(), (after.input_schema)());
        }
    }
}

#[test]
fn malformed_audit_field_invalidates_policy() {
    let mut policy = lookup_tool_definition("memory_read").unwrap().audit;
    const BAD_REQUEST: &[AuditField] = &[AuditField::new("", "body", AuditValue::Copy)];
    policy.request.fields = BAD_REQUEST;
    assert!(!policy.is_valid());
    policy = lookup_tool_definition("memory_read").unwrap().audit;
    const BAD_RESULT: &[AuditField] = &[AuditField::new(
        "payload",
        "/bad~escape",
        AuditValue::Nullable,
    )];
    policy.result = AuditResultPolicy::Fields(BAD_RESULT);
    assert!(!policy.is_valid());
}
