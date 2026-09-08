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
