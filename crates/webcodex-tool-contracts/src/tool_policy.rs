//! Runtime tool lookup and policy helpers derived from ToolDefinition.

use super::metadata::{
    tool_metadata, ToolApprovalPolicy, ToolEffect, ToolMetadata, ToolPathHint, ToolRisk,
};
use super::tool_definition::{
    tool_definitions, RunnerCapabilityRequirement, ToolAuditPolicy, ToolContextContinuityPolicy,
    ToolDefinition, ToolEffectAnnotations, ToolOperatorExtensionFamily, ToolSessionEvidencePolicy,
    PERMISSION_RISK_ARTIFACT_WRITE, PERMISSION_RISK_DESTRUCTIVE, PERMISSION_RISK_PATCH,
    PERMISSION_RISK_SHELL, PERMISSION_RISK_VALIDATION, PERMISSION_RISK_WRITE,
};

impl ToolDefinition {
    pub fn metadata(self) -> ToolMetadata {
        self.metadata
    }

    pub fn audit_policy(self) -> ToolAuditPolicy {
        self.audit
    }

    pub fn effect_annotations(self) -> ToolEffectAnnotations {
        ToolEffectAnnotations {
            read_only_hint: self.metadata.effect.read_only_hint(),
            destructive_hint: self.metadata.destructive,
            idempotent_hint: self.metadata.idempotency.mcp_hint(),
            open_world_hint: self.metadata.shell_like,
        }
    }

    pub fn session_risk_class(self) -> &'static str {
        self.metadata.risk.session_risk_class()
    }

    pub fn is_read_like(self) -> bool {
        self.metadata.effect == ToolEffect::Observe
    }

    pub fn is_write_like(self) -> bool {
        matches!(
            self.metadata.risk,
            ToolRisk::ProjectWrite
                | ToolRisk::SkillManage
                | ToolRisk::MemoryManage
                | ToolRisk::CommunicationManage
                | ToolRisk::ComputerControl
        )
    }

    pub fn is_shell_like(self) -> bool {
        self.metadata.shell_like || self.metadata.risk == ToolRisk::JobRun
    }

    pub fn is_git_like(self) -> bool {
        self.policy.git_like
    }

    pub fn is_change_summary_like(self) -> bool {
        self.policy.change_summary_like
    }

    pub fn captures_validation_output(self) -> bool {
        self.policy.captures_validation_output
    }

    pub fn adaptive_runtime_direct_rank(self) -> Option<u16> {
        self.model_surface.adaptive_runtime_direct_rank
    }

    pub fn context_continuity_policy(self) -> ToolContextContinuityPolicy {
        self.policy.context_continuity
    }

    pub fn session_evidence_policy(self) -> ToolSessionEvidencePolicy {
        self.session_evidence
    }

    #[cfg(any(test, feature = "root-test-support"))]
    pub fn requires_explicit_business_session(self) -> bool {
        self.policy.requires_explicit_business_session
    }

    pub fn uses_unit_arguments(self) -> bool {
        self.policy.unit_arguments
    }

    pub fn requires_artifact_upload_path_binding(self) -> bool {
        self.policy.requires_artifact_upload_path_binding
    }

    pub fn approval_policy(self) -> ToolApprovalPolicy {
        self.metadata.approval
    }

    pub fn requires_permission(self) -> bool {
        self.approval_policy().requires_permission()
    }

    pub fn permission_risk(self) -> &'static str {
        if self.captures_validation_output() {
            return PERMISSION_RISK_VALIDATION;
        }
        if let Some(permission_risk) = self.policy.permission_risk {
            return permission_risk;
        }
        permission_risk_from_metadata(self.metadata)
    }
}

fn permission_risk_from_metadata(metadata: ToolMetadata) -> &'static str {
    if metadata.shell_like {
        return PERMISSION_RISK_SHELL;
    }
    if metadata.destructive {
        return PERMISSION_RISK_DESTRUCTIVE;
    }
    if metadata.path_hint == ToolPathHint::Artifact {
        return PERMISSION_RISK_ARTIFACT_WRITE;
    }
    if metadata.path_hint == ToolPathHint::Patch {
        return PERMISSION_RISK_PATCH;
    }
    if matches!(
        metadata.risk,
        ToolRisk::ProjectWrite
            | ToolRisk::SkillManage
            | ToolRisk::MemoryManage
            | ToolRisk::ComputerControl
            | ToolRisk::CommunicationManage
    ) {
        return PERMISSION_RISK_WRITE;
    }
    PERMISSION_RISK_WRITE
}

fn fallback_permission_risk(name: &str, metadata: ToolMetadata) -> &'static str {
    if name.contains("patch") && metadata.path_hint != ToolPathHint::Patch {
        return PERMISSION_RISK_PATCH;
    }
    permission_risk_from_metadata(metadata)
}

pub fn lookup_tool_definition(name: &str) -> Option<&'static ToolDefinition> {
    tool_definitions().find(|definition| definition.name == name)
}

/// Returns the canonical static Stateless Operator extension family for a runtime
/// tool. Unknown and ordinary tools return `None`, so protocol admission fails
/// closed unless a ToolDefinition explicitly declares a family.
pub fn runtime_tool_operator_extension_family(name: &str) -> Option<ToolOperatorExtensionFamily> {
    lookup_tool_definition(name).and_then(|definition| definition.operator_extension_family)
}

pub fn runtime_tool_session_evidence_policy(name: &str) -> ToolSessionEvidencePolicy {
    lookup_tool_definition(name)
        .map(|definition| definition.session_evidence_policy())
        .unwrap_or(ToolSessionEvidencePolicy::NONE)
}

pub fn exploration_tool_names() -> impl Iterator<Item = &'static str> {
    tool_definitions()
        .filter(|definition| definition.session_evidence.exploration.is_exploration())
        .map(|definition| definition.name)
}

/// Audit policy lookup is intentionally optional. Unknown/non-runtime names
/// have no audit contract and callers must fail closed rather than infer one.
pub fn runtime_tool_audit_policy(name: &str) -> Option<ToolAuditPolicy> {
    lookup_tool_definition(name).map(|definition| definition.audit_policy())
}

fn definition_or_metadata_facade(name: &str) -> Result<&'static ToolDefinition, ToolMetadata> {
    lookup_tool_definition(name).ok_or_else(|| fallback_metadata_for_non_runtime_name(name))
}

fn fallback_metadata_for_non_runtime_name(name: &str) -> ToolMetadata {
    // Known runtime names must resolve through ToolDefinition. Non-runtime names
    // receive safe Unknown metadata; ToolCall still rejects them.
    tool_metadata(name)
}

/// Returns `true` if `name` is a recognized runtime tool name.
#[cfg(any(test, feature = "root-test-support"))]
pub fn is_known_tool_name(name: &str) -> bool {
    lookup_tool_definition(name).is_some()
}

#[cfg(any(test, feature = "root-test-support"))]
pub fn known_tool_names() -> impl Iterator<Item = &'static str> {
    tool_definitions().map(|definition| definition.name)
}

pub fn runtime_tool_metadata(name: &str) -> ToolMetadata {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.metadata(),
        Err(metadata) => metadata,
    }
}

fn tool_context_continuity_policy(name: &str) -> ToolContextContinuityPolicy {
    lookup_tool_definition(name)
        .map(|definition| definition.context_continuity_policy())
        .unwrap_or(ToolContextContinuityPolicy::CONSERVATIVE)
}

#[cfg(any(test, feature = "root-test-support"))]
pub fn runtime_tool_context_continuity_policy(name: &str) -> ToolContextContinuityPolicy {
    tool_context_continuity_policy(name)
}

pub fn runtime_tool_accepts_context_ack(name: &str) -> bool {
    tool_context_continuity_policy(name).accepts_context_ack
}

pub fn runtime_tool_advances_context_checkpoint(name: &str) -> bool {
    tool_context_continuity_policy(name).advances_context_checkpoint()
}

pub fn runtime_tool_effect_annotations(name: &str) -> ToolEffectAnnotations {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.effect_annotations(),
        Err(metadata) => ToolEffectAnnotations {
            read_only_hint: metadata.effect.read_only_hint(),
            destructive_hint: metadata.destructive,
            idempotent_hint: metadata.idempotency.mcp_hint(),
            open_world_hint: metadata.shell_like,
        },
    }
}

pub fn runtime_tool_runner_capability(name: &str) -> Option<RunnerCapabilityRequirement> {
    lookup_tool_definition(name)
        .unwrap_or_else(|| panic!("missing ToolDefinition for {name}"))
        .runner_capability
}

pub fn runtime_tool_category(name: &str) -> &'static str {
    lookup_tool_definition(name)
        .map(|definition| definition.category)
        .unwrap_or("other")
}

pub fn runtime_tool_session_risk_class(name: &str) -> &'static str {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.session_risk_class(),
        Err(metadata) => metadata.risk.session_risk_class(),
    }
}

pub fn runtime_tool_is_read_like(name: &str) -> bool {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.is_read_like(),
        Err(metadata) => metadata.effect == ToolEffect::Observe,
    }
}

pub fn runtime_tool_is_write_like(name: &str) -> bool {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.is_write_like(),
        Err(metadata) => matches!(
            metadata.risk,
            ToolRisk::ProjectWrite
                | ToolRisk::SkillManage
                | ToolRisk::MemoryManage
                | ToolRisk::CommunicationManage
                | ToolRisk::ComputerControl
        ),
    }
}

pub fn runtime_tool_is_shell_like(name: &str) -> bool {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.is_shell_like(),
        Err(metadata) => metadata.shell_like || metadata.risk == ToolRisk::JobRun,
    }
}

pub fn runtime_tool_is_git_like(name: &str) -> bool {
    lookup_tool_definition(name).is_some_and(|definition| definition.is_git_like())
}

pub fn runtime_tool_is_change_summary_like(name: &str) -> bool {
    lookup_tool_definition(name).is_some_and(|definition| definition.is_change_summary_like())
}

pub fn runtime_tool_captures_validation_output(name: &str) -> bool {
    lookup_tool_definition(name).is_some_and(|definition| definition.captures_validation_output())
}

#[cfg(any(test, feature = "root-test-support"))]
pub fn runtime_tool_requires_explicit_business_session(name: &str) -> bool {
    lookup_tool_definition(name)
        .is_some_and(|definition| definition.requires_explicit_business_session())
}

pub fn runtime_tool_approval_policy(name: &str) -> ToolApprovalPolicy {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.approval_policy(),
        Err(metadata) => metadata.approval,
    }
}

pub fn runtime_tool_requires_permission(name: &str) -> bool {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.requires_permission(),
        Err(metadata) => metadata.approval.requires_permission(),
    }
}

pub fn runtime_tool_permission_risk(name: &str) -> &'static str {
    match definition_or_metadata_facade(name) {
        Ok(definition) => definition.permission_risk(),
        Err(metadata) => fallback_permission_risk(name, metadata),
    }
}

pub fn is_model_visible_tool_name(name: &str) -> bool {
    lookup_tool_definition(name).is_some_and(|definition| definition.visibility.is_model_visible())
}

#[cfg(any(test, feature = "root-test-support"))]
pub fn is_model_hidden_tool_name(name: &str) -> bool {
    lookup_tool_definition(name).is_some_and(|definition| definition.visibility.is_model_hidden())
}

#[cfg(any(test, feature = "root-test-support"))]
pub fn model_hidden_tool_names() -> impl Iterator<Item = &'static str> {
    tool_definitions()
        .filter(|definition| definition.visibility.is_model_hidden())
        .map(|definition| definition.name)
}

pub fn model_visible_tool_definitions() -> impl Iterator<Item = &'static ToolDefinition> {
    tool_definitions().filter(|definition| definition.visibility.is_model_visible())
}

pub fn runtime_tool_adaptive_direct_rank(name: &str) -> Option<u16> {
    lookup_tool_definition(name)
        .filter(|definition| definition.visibility.is_model_visible())
        .and_then(|definition| definition.adaptive_runtime_direct_rank())
}

pub fn is_adaptive_runtime_direct_tool(name: &str) -> bool {
    runtime_tool_adaptive_direct_rank(name).is_some()
}

pub fn adaptive_runtime_direct_tool_definitions() -> Vec<&'static ToolDefinition> {
    let mut definitions = model_visible_tool_definitions()
        .filter(|definition| definition.adaptive_runtime_direct_rank().is_some())
        .collect::<Vec<_>>();
    definitions.sort_by_key(|definition| {
        (
            definition
                .adaptive_runtime_direct_rank()
                .expect("adaptive direct definition rank"),
            definition.name,
        )
    });
    definitions
}

pub fn model_visible_tool_names_csv() -> String {
    model_visible_tool_definitions()
        .map(|definition| definition.name)
        .collect::<Vec<_>>()
        .join(", ")
}
