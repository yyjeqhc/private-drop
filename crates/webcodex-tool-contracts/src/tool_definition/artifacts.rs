use super::RunnerCapabilityRequirement::{FileRead, FileWrite};
use super::ToolVisibility::ModelVisible;
use super::{
    def, model_spec, permission_risk, requires_artifact_upload_path_binding, ToolDefinition,
    PERMISSION_RISK_ARTIFACT_WRITE, TOOL_CATEGORY_ARTIFACT,
};
use crate::audit_policy::*;
use crate::metadata::{
    ToolPathHint::Artifact,
    ToolRisk::{ProjectWrite, Read},
    PROJECT_READ, PROJECT_WRITE, TOOL_PROVIDER_CONTROL, TOOL_PROVIDER_RUNNER,
};
use crate::registry::input_schemas::{
    artifact_upload_abort_input_schema, artifact_upload_begin_input_schema,
    artifact_upload_chunk_input_schema, artifact_upload_finish_input_schema,
    export_project_artifact_input_schema, import_conversation_files_to_project_input_schema,
    read_project_artifact_input_schema, read_project_artifact_metadata_input_schema,
    save_project_artifact_input_schema,
};

pub(super) const DEFINITIONS: &[ToolDefinition] = &[
    permission_risk(
        model_spec(
            def(
            "save_project_artifact",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("mime_type", "mime_type", AuditValue::Copy),
                        AuditField::new("overwrite", "overwrite", AuditValue::Copy),
                        AuditField::new(
                            "content_base64_present",
                            "content_base64",
                            AuditValue::KeyPresent,
                        ),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides { fields: &[AuditField::new(
                        "content_base64_present",
                        "content_base64",
                        AuditValue::Present,
                    )], omit: &[] },
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            true,
            false,
            ),
            "Write a bounded binary project artifact from base64. Use for imported session files, generated images, PDFs, ZIP archives, and DOCX/PPTX/XLSX Office artifacts; not for UTF-8 source edits.",
            save_project_artifact_input_schema,
        ),
        PERMISSION_RISK_ARTIFACT_WRITE,
    ),
    permission_risk(
        model_spec(
            def(
            "import_conversation_files_to_project",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("output_dir", "output_dir", AuditValue::Copy),
                        AuditField::new("overwrite", "overwrite", AuditValue::Copy),
                        AuditField::new("session_id", "session_id", AuditValue::Copy),
                        AuditField::new("file_count", "openaiFileIdRefs", AuditValue::Count),
                        AuditField::new("targets_count", "targets", AuditValue::Count),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_CONTROL,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            true,
            false,
            ),
            "Import 1..10 current ChatGPT attachments into a Runner project using openaiFileIdRefs from the host file-reference mechanism. Do not base64-transfer files, construct download URLs, or use local /mnt/data paths; Control downloads and saves them as project artifacts.",
            import_conversation_files_to_project_input_schema,
        ),
        PERMISSION_RISK_ARTIFACT_WRITE,
    ),
    model_spec(
        def(
            "export_project_artifact",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
AuditField::new("project", "project", AuditValue::Copy),
AuditField::new("path", "path", AuditValue::Copy),
AuditField::new("session_id", "session_id", AuditValue::Copy),
],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Omit,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileRead),
            TOOL_PROVIDER_CONTROL,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            Artifact,
            false,
            false,
        ),
        "Create one short-lived authenticated MCP resource link for a bounded project artifact. The tool result contains metadata only; use resources/read on the returned ResourceLink to fetch the complete binary without routing base64 through model output.",
        export_project_artifact_input_schema,
    ),
    model_spec(
        def(
            "read_project_artifact_metadata",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("allow_missing", "allow_missing", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileRead),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            Artifact,
            false,
            false,
        ),
        "Read bounded metadata for a binary artifact; images include dimensions and zip archives are counted but never extracted. Set allow_missing=true to make a missing artifact a successful exists=false negative assertion.",
        read_project_artifact_metadata_input_schema,
    ),
    model_spec(
        def(
            "read_project_artifact",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("encoding", "encoding", AuditValue::Copy),
                        AuditField::new("offset", "offset", AuditValue::Copy),
                        AuditField::new("length", "length", AuditValue::Copy),
                        AuditField::new("as_image", "as_image", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileRead),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Observe,
                risk: Read,
                approval: super::ToolApprovalPolicy::None,
                idempotency: super::ToolIdempotency::PureRead,
            },
            Some(PROJECT_READ),
            true,
            Artifact,
            false,
            false,
        ),
        "Chunked content read for a project artifact. Returns base64 for one small segment plus full-file sha256/MIME metadata; not a large-file transfer tool.",
        read_project_artifact_input_schema,
    ),
    model_spec(
        def(
            "artifact_upload_begin",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("expected_bytes", "expected_bytes", AuditValue::Copy),
                        AuditField::new("mime_type", "mime_type", AuditValue::Copy),
                        AuditField::new("overwrite", "overwrite", AuditValue::Copy),
                        AuditField::new(
                            "expected_sha256_present",
                            "expected_sha256",
                            AuditValue::KeyPresent,
                        ),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides { fields: &[AuditField::new(
                        "expected_sha256_present",
                        "expected_sha256",
                        AuditValue::Present,
                    )], omit: &[] },
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            false,
            false,
        ),
        "Begin a bounded chunked binary artifact upload up to 256 MiB. Creates a project-local temporary upload session; finish commits atomically to the target path. For smoke octet-stream uploads, use artifacts/smoke/<name>.artifact or omit mime_type when appropriate.",
        artifact_upload_begin_input_schema,
    ),
    requires_artifact_upload_path_binding(model_spec(
        def(
            "artifact_upload_chunk",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("upload_id", "upload_id", AuditValue::Copy),
                        AuditField::new("offset", "offset", AuditValue::Copy),
                        AuditField::new(
                            "content_base64_present",
                            "content_base64",
                            AuditValue::KeyPresent,
                        ),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Overrides { fields: &[AuditField::new(
                        "content_base64_present",
                        "content_base64",
                        AuditValue::Present,
                    )], omit: &[] },
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            false,
            false,
        ),
        "Append one base64 chunk up to 1 MiB decoded to an active artifact upload. path is required and must exactly match artifact_upload_begin; this binds upload_id to the target path.",
        artifact_upload_chunk_input_schema,
    )),
    requires_artifact_upload_path_binding(permission_risk(
        model_spec(
            def(
            "artifact_upload_finish",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("upload_id", "upload_id", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            true,
            false,
            ),
            "Finish an active artifact upload. path is required and must exactly match artifact_upload_begin; this binds upload_id before atomic commit.",
            artifact_upload_finish_input_schema,
        ),
        PERMISSION_RISK_ARTIFACT_WRITE,
    )),
    requires_artifact_upload_path_binding(permission_risk(
        model_spec(
            def(
            "artifact_upload_abort",
            ToolAuditPolicy {
                request: AuditRequestPolicy {
                    fields: &[
                        AuditField::new("project", "project", AuditValue::Copy),
                        AuditField::new("path", "path", AuditValue::Copy),
                        AuditField::new("upload_id", "upload_id", AuditValue::Copy),
                    ],
                    transform: AuditTransform::Fields,
                    typed: AuditTypedPolicy::Same,
                },
                result: AuditResultPolicy::SessionEvidence,
            },
            ModelVisible,
            TOOL_CATEGORY_ARTIFACT,
            Some(FileWrite),
            TOOL_PROVIDER_RUNNER,
            super::ToolSemanticContract {
                effect: super::ToolEffect::Mutate,
                risk: ProjectWrite,
                approval: super::ToolApprovalPolicy::Standard,
                idempotency: super::ToolIdempotency::NonIdempotent,
            },
            Some(PROJECT_WRITE),
            true,
            Artifact,
            true,
            false,
            ),
            "Abort an active artifact upload. path is required and must exactly match artifact_upload_begin; this binds upload_id before cleanup and reports final_file_exists without touching the final target.",
            artifact_upload_abort_input_schema,
        ),
        PERMISSION_RISK_ARTIFACT_WRITE,
    )),
];
