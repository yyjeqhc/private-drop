use super::ToolVisibility::ModelHidden;
use super::{
    context_reobservable, def, ToolDefinition, ToolOperatorExtensionFamily, TOOL_CATEGORY_RUNTIME,
};
use crate::metadata::{ToolPathHint::None as NoPath, ToolRisk::Read, ADMIN, TOOL_PROVIDER_CONTROL};

/// Operator-only forensic diagnostics. The tool is kernel-known so the shared
/// typed dispatcher can enforce its contract, but it is projected only by a
/// capable Stateless MCP 2026 operator surface.
pub(super) const DEFINITIONS: &[ToolDefinition] = &[context_reobservable(def(
    "read_tool_trace",
    super::ToolAuditPolicy::typed_fields(&[
        super::ToolAuditResultField::value("trace_ref"),
        super::ToolAuditResultField::value("trace_mode"),
        super::ToolAuditResultField::value("payload_count"),
        super::ToolAuditResultField::value("returned_count"),
        super::ToolAuditResultField::value("offset"),
        super::ToolAuditResultField::value("next_offset"),
        super::ToolAuditResultField::value("payload_index"),
        super::ToolAuditResultField::value("phase"),
        super::ToolAuditResultField::value("payload_bytes"),
        super::ToolAuditResultField::value("payload_sha256"),
        super::ToolAuditResultField::value("payload_available"),
        super::ToolAuditResultField::value("reason"),
        super::ToolAuditResultField::value("error_kind"),
    ]),
    ModelHidden,
    TOOL_CATEGORY_RUNTIME,
    None,
    TOOL_PROVIDER_CONTROL,
    super::ToolSemanticContract {
        effect: super::ToolEffect::Observe,
        risk: Read,
        approval: super::ToolApprovalPolicy::None,
        idempotency: super::ToolIdempotency::PureRead,
    },
    Some(ADMIN),
    false,
    NoPath,
    false,
    false,
    super::ToolSessionEvidencePolicy::NONE,
))
.with_operator_extension_family(ToolOperatorExtensionFamily::TraceDiagnostics)];
