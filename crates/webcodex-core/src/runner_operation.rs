//! Canonical semantic Runner operations and the V2 request wire adapter.
//!
//! `RunnerRequest` intentionally remains the compatibility DTO used by all V2
//! transports. Production semantics should use `RunnerOperation`: the V2
//! `kind + optional payload bag` is decoded and validated once at the boundary,
//! then operation identity and its required payload travel together.

use crate::coding_agent::{validate_request as validate_coding_agent_request, CodingAgentRequest};
use crate::lsp_bridge::RunnerLspPayload;
use crate::mcp_gateway::{validate_request as validate_mcp_gateway_request, McpGatewayRequest};
use crate::plugin::{validate_request as validate_plugin_gateway_request, PluginGatewayRequest};
use crate::runner_protocol::{
    shell_computer_request_payload_max_bytes, validate_process_argv,
    validate_raw_shell_wire_command, validate_script_request, PersistentShellRequest,
    RunnerConfigOperationRequest, RunnerRequest, ShellFileOpRequest, ShellJobContext,
    ShellJobStructuredExecutionMetadata, ShellJobValidationStep, ShellProcessArgv,
    ShellScriptLanguage, ShellScriptPayload, PROCESS_CWD_MAX_BYTES, PROCESS_STDIN_MAX_BYTES,
    RUNNER_CONFIG_REQUEST_KIND, RUNNER_CONFIG_REQUEST_MAX_BYTES,
    STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS, STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS,
    STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS,
};
use crate::skill_store::SkillStoreRequest;
use crate::ssh_resource::{SshResourceRequest, SSH_RESOURCE_REQUEST_MAX_BYTES};
use crate::validation_bridge::{validate_bridge_request, ValidationBridgeRequest};

/// Request/envelope metadata that is independent of operation semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInvocationMetadata {
    pub request_id: String,
    pub client_id: String,
    pub requested_by: String,
    pub created_at: i64,
}

/// Canonical internal Runner invocation.
#[derive(Debug, Clone)]
pub struct RunnerInvocation {
    pub metadata: RunnerInvocationMetadata,
    pub operation: RunnerOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerShellOperation {
    pub cwd: Option<String>,
    pub command: String,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
    /// Present only for Session-bound SSH raw-shell execution.
    pub job_context: Option<ShellJobContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerProcessOperation {
    pub cwd: Option<String>,
    pub process: ShellProcessArgv,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerScriptOperation {
    pub cwd: Option<String>,
    pub script: ShellScriptPayload,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerJobShellOperation {
    pub job_id: String,
    pub cwd: Option<String>,
    pub command: String,
    pub timeout_secs: u64,
    pub context: ShellJobContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerJobValidationOperation {
    pub job_id: String,
    pub cwd: Option<String>,
    pub steps: Vec<ShellJobValidationStep>,
    pub timeout_secs: u64,
    pub context: ShellJobContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerJobProcessOperation {
    pub job_id: String,
    pub cwd: Option<String>,
    pub process: ShellProcessArgv,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
    pub context: ShellJobContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerJobScriptOperation {
    pub job_id: String,
    pub cwd: Option<String>,
    pub script: ShellScriptPayload,
    pub stdin: Option<String>,
    pub timeout_secs: u64,
    pub context: ShellJobContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerJobOperation {
    StartShell(RunnerJobShellOperation),
    StartValidation(RunnerJobValidationOperation),
    StartProcess(RunnerJobProcessOperation),
    StartDetachedProcess(RunnerJobProcessOperation),
    StartScript(RunnerJobScriptOperation),
    Stop { job_id: String },
}

impl RunnerJobOperation {
    pub fn job_id(&self) -> &str {
        match self {
            Self::StartShell(operation) => &operation.job_id,
            Self::StartValidation(operation) => &operation.job_id,
            Self::StartProcess(operation) | Self::StartDetachedProcess(operation) => {
                &operation.job_id
            }
            Self::StartScript(operation) => &operation.job_id,
            Self::Stop { job_id } => job_id,
        }
    }

    pub fn context(&self) -> Option<&ShellJobContext> {
        match self {
            Self::StartShell(operation) => Some(&operation.context),
            Self::StartValidation(operation) => Some(&operation.context),
            Self::StartProcess(operation) | Self::StartDetachedProcess(operation) => {
                Some(&operation.context)
            }
            Self::StartScript(operation) => Some(&operation.context),
            Self::Stop { .. } => None,
        }
    }

    pub fn cwd(&self) -> Option<&str> {
        match self {
            Self::StartShell(operation) => operation.cwd.as_deref(),
            Self::StartValidation(operation) => operation.cwd.as_deref(),
            Self::StartProcess(operation) | Self::StartDetachedProcess(operation) => {
                operation.cwd.as_deref()
            }
            Self::StartScript(operation) => operation.cwd.as_deref(),
            Self::Stop { .. } => None,
        }
    }

    pub fn is_detached_process(&self) -> bool {
        matches!(self, Self::StartDetachedProcess(_))
    }

    pub fn is_start(&self) -> bool {
        !matches!(self, Self::Stop { .. })
    }

    /// Derive the execution metadata that must be persisted in Job recovery
    /// context. Correlation-only validation identity/tool/assertion fields are
    /// copied from the admitted context; they never grant execution authority.
    pub fn expected_structured_execution(&self) -> Option<ShellJobStructuredExecutionMetadata> {
        let correlation = self
            .context()
            .and_then(|context| context.structured_execution.as_ref())
            .map(|metadata| {
                (
                    metadata.validation_identity.clone(),
                    metadata.validation_tool.clone(),
                    metadata.assertion_name.clone(),
                )
            })
            .unwrap_or((None, None, None));
        match self {
            Self::StartProcess(operation) => Some(ShellJobStructuredExecutionMetadata {
                execution_source: "run_process".to_string(),
                language: None,
                script_bytes: None,
                arg_count: operation.process.args.len(),
                stdin_present: operation.stdin.is_some(),
                validation_identity: correlation.0,
                validation_tool: correlation.1,
                assertion_name: correlation.2,
            }),
            Self::StartDetachedProcess(operation) => Some(ShellJobStructuredExecutionMetadata {
                execution_source: "run_detached_process".to_string(),
                language: None,
                script_bytes: None,
                arg_count: operation.process.args.len(),
                stdin_present: operation.stdin.is_some(),
                validation_identity: correlation.0,
                validation_tool: correlation.1,
                assertion_name: None,
            }),
            Self::StartScript(operation) => Some(ShellJobStructuredExecutionMetadata {
                execution_source: "run_script".to_string(),
                language: Some(operation.script.language),
                script_bytes: Some(operation.script.script.len()),
                arg_count: operation.script.args.len(),
                stdin_present: operation.stdin.is_some(),
                validation_identity: correlation.0,
                validation_tool: correlation.1,
                assertion_name: correlation.2,
            }),
            Self::StartShell(_) | Self::StartValidation(_) | Self::Stop { .. } => None,
        }
    }
}

/// Legacy V2 file-operation fields, now scoped under a closed file operation
/// instead of sharing the top-level Runner request payload bag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerFilePayload {
    pub cwd: Option<String>,
    pub path: String,
    pub content: Option<String>,
    pub max_bytes: Option<usize>,
    pub expected_sha256: Option<String>,
    pub expected_prefix: Option<String>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub create_dirs: bool,
}

macro_rules! runner_file_operations {
    ($(($variant:ident, $wire:literal, $server:literal)),+ $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum RunnerFileOperation {
            $($variant(RunnerFilePayload)),+
        }

        impl RunnerFileOperation {
            pub fn wire_kind(&self) -> &'static str {
                match self { $(Self::$variant(_) => $wire),+ }
            }

            pub fn server_op(&self) -> &'static str {
                match self { $(Self::$variant(_) => $server),+ }
            }

            pub fn payload(&self) -> &RunnerFilePayload {
                match self { $(Self::$variant(payload) => payload),+ }
            }

            fn from_wire_kind(kind: &str, payload: RunnerFilePayload) -> Result<Self, String> {
                validate_file_payload(kind, &payload)?;
                match kind {
                    $($wire => Ok(Self::$variant(payload))),+,
                    _ => Err(format!("unknown Runner V2 request kind: {kind}")),
                }
            }

            pub fn from_server_request(body: &ShellFileOpRequest) -> Result<Self, String> {
                let payload = RunnerFilePayload {
                    cwd: body.cwd.clone().map(|cwd| cwd.trim().to_string()),
                    path: body.path.trim().to_string(),
                    content: body.content.clone(),
                    max_bytes: body.max_bytes,
                    expected_sha256: body.expected_sha256.clone(),
                    expected_prefix: body.expected_prefix.clone(),
                    start_line: body.start_line,
                    end_line: body.end_line,
                    create_dirs: body.create_dirs,
                };
                match body.op.as_str() {
                    $($server => Self::from_wire_kind($wire, payload)),+,
                    _ => Err(format!("unsupported Runner file operation: {}", body.op)),
                }
            }
        }
    };
}

runner_file_operations!(
    (Read, "file_read", "read"),
    (Write, "file_write", "write"),
    (List, "file_list", "list"),
    (ProjectOverview, "file_project_overview", "project_overview"),
    (
        DeleteProjectFiles,
        "file_delete_project_files",
        "delete_project_files"
    ),
    (
        WriteProjectFile,
        "file_write_project_file",
        "write_project_file"
    ),
    (ApplyTextEdits, "file_apply_text_edits", "apply_text_edits"),
    (ApplyPatch, "file_apply_patch", "apply_patch"),
    (
        SaveProjectArtifact,
        "file_save_project_artifact",
        "save_project_artifact"
    ),
    (
        ReadProjectArtifactMetadata,
        "file_read_project_artifact_metadata",
        "read_project_artifact_metadata"
    ),
    (
        ReadProjectArtifact,
        "file_read_project_artifact",
        "read_project_artifact"
    ),
    (
        ReadProjectArtifactExportChunk,
        "file_read_project_artifact_export_chunk",
        "read_project_artifact_export_chunk"
    ),
    (
        ArtifactUploadBegin,
        "file_artifact_upload_begin",
        "artifact_upload_begin"
    ),
    (
        ArtifactUploadChunk,
        "file_artifact_upload_chunk",
        "artifact_upload_chunk"
    ),
    (
        ArtifactUploadFinish,
        "file_artifact_upload_finish",
        "artifact_upload_finish"
    ),
    (
        ArtifactUploadAbort,
        "file_artifact_upload_abort",
        "artifact_upload_abort"
    ),
    (
        CheckpointCreate,
        "file_checkpoint_create",
        "checkpoint_create"
    ),
    (
        CheckpointRestore,
        "file_checkpoint_restore",
        "checkpoint_restore"
    ),
    (
        SkillListPackages,
        "file_skill_list_packages",
        "skill_list_packages"
    ),
    (SkillReadFile, "file_skill_read_file", "skill_read_file"),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerProjectOperationKind {
    Register,
    Create,
    ResolveOrRegister,
    PrepareManagedWorktree,
    LifecycleEnable,
    LifecycleDisable,
    LifecycleUnregister,
}

impl RunnerProjectOperationKind {
    pub fn wire_kind(self) -> &'static str {
        match self {
            Self::Register => "register_project",
            Self::Create => "create_project",
            Self::ResolveOrRegister => "resolve_or_register_project",
            Self::PrepareManagedWorktree => "prepare_managed_worktree",
            Self::LifecycleEnable => "project_lifecycle_enable",
            Self::LifecycleDisable => "project_lifecycle_disable",
            Self::LifecycleUnregister => "project_lifecycle_unregister",
        }
    }

    pub fn from_wire(kind: &str) -> Option<Self> {
        Some(match kind {
            "register_project" => Self::Register,
            "create_project" => Self::Create,
            "resolve_or_register_project" => Self::ResolveOrRegister,
            "prepare_managed_worktree" => Self::PrepareManagedWorktree,
            "project_lifecycle_enable" => Self::LifecycleEnable,
            "project_lifecycle_disable" => Self::LifecycleDisable,
            "project_lifecycle_unregister" => Self::LifecycleUnregister,
            _ => return None,
        })
    }

    pub fn disables_execution(self) -> bool {
        matches!(self, Self::LifecycleDisable | Self::LifecycleUnregister)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerProjectOperation {
    pub kind: RunnerProjectOperationKind,
    /// Existing V2 JSON payload carried in `stdin`.
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerComputerOperationKind {
    ListWindows,
    ListApplications,
    LaunchApplication,
    ListDisplays,
    SnapshotDisplay,
    ReadClipboard,
    WriteClipboard,
    PointerMove,
    PointerClick,
    Snapshot,
    SnapshotRegion,
    AccessibilityStatus,
    AccessibilityTree,
    ElementState,
    ActivateWindow,
    Control,
    ScrollToElement,
    KeyInput,
    InputText,
}

impl RunnerComputerOperationKind {
    pub fn wire_kind(self) -> &'static str {
        match self {
            Self::ListWindows => "computer_list_windows",
            Self::ListApplications => "computer_list_applications",
            Self::LaunchApplication => "computer_launch_application",
            Self::ListDisplays => "computer_list_displays",
            Self::SnapshotDisplay => "computer_snapshot_display",
            Self::ReadClipboard => "computer_read_clipboard",
            Self::WriteClipboard => "computer_write_clipboard",
            Self::PointerMove => "computer_pointer_move",
            Self::PointerClick => "computer_pointer_click",
            Self::Snapshot => "computer_snapshot",
            Self::SnapshotRegion => "computer_snapshot_region",
            Self::AccessibilityStatus => "computer_accessibility_status",
            Self::AccessibilityTree => "computer_accessibility_tree",
            Self::ElementState => "computer_element_state",
            Self::ActivateWindow => "computer_activate_window",
            Self::Control => "computer_control",
            Self::ScrollToElement => "computer_scroll_to_element",
            Self::KeyInput => "computer_key_input",
            Self::InputText => "computer_input_text",
        }
    }

    pub fn from_wire(kind: &str) -> Option<Self> {
        Some(match kind {
            "computer_list_windows" => Self::ListWindows,
            "computer_list_applications" => Self::ListApplications,
            "computer_launch_application" => Self::LaunchApplication,
            "computer_list_displays" => Self::ListDisplays,
            "computer_snapshot_display" => Self::SnapshotDisplay,
            "computer_read_clipboard" => Self::ReadClipboard,
            "computer_write_clipboard" => Self::WriteClipboard,
            "computer_pointer_move" => Self::PointerMove,
            "computer_pointer_click" => Self::PointerClick,
            "computer_snapshot" => Self::Snapshot,
            "computer_snapshot_region" => Self::SnapshotRegion,
            "computer_accessibility_status" => Self::AccessibilityStatus,
            "computer_accessibility_tree" => Self::AccessibilityTree,
            "computer_element_state" => Self::ElementState,
            "computer_activate_window" => Self::ActivateWindow,
            "computer_control" => Self::Control,
            "computer_scroll_to_element" => Self::ScrollToElement,
            "computer_key_input" => Self::KeyInput,
            "computer_input_text" => Self::InputText,
            _ => return None,
        })
    }

    pub fn is_large_image(self) -> bool {
        matches!(self, Self::Snapshot | Self::SnapshotDisplay)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerComputerOperation {
    pub kind: RunnerComputerOperationKind,
    /// Existing bounded JSON payload carried in `stdin`.
    pub payload: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct RunnerPersistentShellOperation {
    pub request: PersistentShellRequest,
    pub job_context: Option<ShellJobContext>,
}

/// Closed canonical Runner operation. Variants carry the payload required to
/// execute that operation; callers never reconstruct semantics from optional V2
/// fields.
#[derive(Debug, Clone)]
pub enum RunnerOperation {
    RunShell(RunnerShellOperation),
    RunProcess(RunnerProcessOperation),
    RunScript(RunnerScriptOperation),
    RunInternalPosixScript(RunnerScriptOperation),
    Job(RunnerJobOperation),
    File(RunnerFileOperation),
    Project(RunnerProjectOperation),
    Computer(RunnerComputerOperation),
    Validation {
        payload: ValidationBridgeRequest,
        timeout_secs: u64,
    },
    Lsp {
        payload: RunnerLspPayload,
        timeout_secs: u64,
    },
    PersistentShell(RunnerPersistentShellOperation),
    McpGateway(McpGatewayRequest),
    PluginGateway(PluginGatewayRequest),
    CodingAgent(CodingAgentRequest),
    SkillStore(SkillStoreRequest),
    SshResource(SshResourceRequest),
    RunnerConfig(RunnerConfigOperationRequest),
}

impl RunnerOperation {
    pub fn wire_kind(&self) -> &'static str {
        match self {
            Self::RunShell(_) => "run_shell",
            Self::RunProcess(_) => "run_process",
            Self::RunScript(_) => "run_script",
            Self::RunInternalPosixScript(_) => "run_internal_posix_script",
            Self::Job(operation) => match operation {
                RunnerJobOperation::StartShell(_) => "start_job",
                RunnerJobOperation::StartValidation(_) => "start_validation_job",
                RunnerJobOperation::StartProcess(_) => "start_process_job",
                RunnerJobOperation::StartDetachedProcess(_) => "start_detached_process_job",
                RunnerJobOperation::StartScript(_) => "start_script_job",
                RunnerJobOperation::Stop { .. } => "stop_job",
            },
            Self::File(operation) => operation.wire_kind(),
            Self::Project(operation) => operation.kind.wire_kind(),
            Self::Computer(operation) => operation.kind.wire_kind(),
            Self::Validation { .. } => crate::validation_bridge::AGENT_VALIDATION_REQUEST_KIND,
            Self::Lsp { .. } => crate::lsp_bridge::AGENT_LSP_REQUEST_KIND,
            Self::PersistentShell(_) => "persistent_shell",
            Self::McpGateway(_) => "mcp_gateway",
            Self::PluginGateway(_) => "plugin_gateway",
            Self::CodingAgent(_) => "coding_agent",
            Self::SkillStore(_) => "skill_store",
            Self::SshResource(_) => "ssh_resource",
            Self::RunnerConfig(_) => RUNNER_CONFIG_REQUEST_KIND,
        }
    }

    pub fn job(&self) -> Option<&RunnerJobOperation> {
        match self {
            Self::Job(operation) => Some(operation),
            _ => None,
        }
    }

    pub fn is_large_native_image_request(&self) -> bool {
        match self {
            Self::Computer(operation) => operation.kind.is_large_image(),
            Self::File(RunnerFileOperation::ReadProjectArtifact(payload)) => payload
                .content
                .as_deref()
                .and_then(|content| serde_json::from_str::<serde_json::Value>(content).ok())
                .and_then(|value| value.get("mcp_image").and_then(serde_json::Value::as_bool))
                .unwrap_or(false),
            _ => false,
        }
    }
}

impl RunnerRequest {
    /// Decode and validate the V2 optional-field bag into one canonical closed
    /// semantic operation. Explicit unknown kinds and conflicting payloads fail
    /// closed. A *missing* `kind` still reaches this method as `run_shell`
    /// because the historical serde default remains on `RunnerRequest.kind`.
    pub fn decode_operation(&self) -> Result<RunnerOperation, String> {
        decode_operation(self)
    }

    pub fn decode_invocation(&self) -> Result<RunnerInvocation, String> {
        Ok(RunnerInvocation {
            metadata: RunnerInvocationMetadata {
                request_id: self.request_id.clone(),
                client_id: self.client_id.clone(),
                requested_by: self.requested_by.clone(),
                created_at: self.created_at,
            },
            operation: self.decode_operation()?,
        })
    }

    pub fn from_operation(
        metadata: RunnerInvocationMetadata,
        operation: RunnerOperation,
    ) -> Result<Self, String> {
        RunnerInvocation {
            metadata,
            operation,
        }
        .into_v2_request()
    }
}

impl RunnerInvocation {
    pub fn into_v2_request(self) -> Result<RunnerRequest, String> {
        encode_operation(self.metadata, self.operation)
    }
}

fn empty_wire(metadata: RunnerInvocationMetadata, kind: &str, timeout_secs: u64) -> RunnerRequest {
    RunnerRequest {
        request_id: metadata.request_id,
        client_id: metadata.client_id,
        kind: kind.to_string(),
        job_id: None,
        cwd: None,
        path: None,
        content: None,
        max_bytes: None,
        expected_sha256: None,
        expected_prefix: None,
        start_line: None,
        end_line: None,
        create_dirs: false,
        command: String::new(),
        process: None,
        script: None,
        stdin: None,
        timeout_secs,
        requested_by: metadata.requested_by,
        created_at: metadata.created_at,
        validation: None,
        lsp: None,
        job_context: None,
        persistent_shell: None,
        mcp_gateway: None,
        plugin_gateway: None,
        coding_agent: None,
    }
}

fn encode_operation(
    metadata: RunnerInvocationMetadata,
    operation: RunnerOperation,
) -> Result<RunnerRequest, String> {
    let kind = operation.wire_kind();
    let mut wire = empty_wire(metadata, kind, 30);
    match operation {
        RunnerOperation::RunShell(operation) => {
            validate_raw_shell_wire_command(&operation.command)?;
            wire.cwd = operation.cwd;
            wire.command = operation.command;
            wire.stdin = operation.stdin;
            wire.timeout_secs = operation.timeout_secs;
            wire.job_context = operation.job_context;
        }
        RunnerOperation::RunProcess(operation) => {
            validate_direct_process(&operation)?;
            wire.cwd = operation.cwd;
            wire.process = Some(operation.process);
            wire.stdin = operation.stdin;
            wire.timeout_secs = operation.timeout_secs;
        }
        RunnerOperation::RunScript(operation) => {
            validate_direct_script(&operation, false)?;
            wire.cwd = operation.cwd;
            wire.script = Some(operation.script);
            wire.stdin = operation.stdin;
            wire.timeout_secs = operation.timeout_secs;
        }
        RunnerOperation::RunInternalPosixScript(operation) => {
            validate_direct_script(&operation, true)?;
            wire.cwd = operation.cwd;
            wire.script = Some(operation.script);
            wire.timeout_secs = operation.timeout_secs;
        }
        RunnerOperation::Job(operation) => encode_job_operation(&mut wire, operation)?,
        RunnerOperation::File(operation) => {
            let payload = operation.payload().clone();
            validate_file_payload(operation.wire_kind(), &payload)?;
            wire.kind = operation.wire_kind().to_string();
            wire.cwd = payload.cwd;
            wire.path = Some(payload.path);
            wire.content = payload.content;
            wire.max_bytes = payload.max_bytes;
            wire.expected_sha256 = payload.expected_sha256;
            wire.expected_prefix = payload.expected_prefix;
            wire.start_line = payload.start_line;
            wire.end_line = payload.end_line;
            wire.create_dirs = payload.create_dirs;
            wire.timeout_secs = 30;
        }
        RunnerOperation::Project(operation) => {
            validate_json_payload(&operation.payload, "project operation")?;
            wire.kind = operation.kind.wire_kind().to_string();
            wire.stdin = Some(operation.payload);
            wire.timeout_secs = 30;
        }
        RunnerOperation::Computer(operation) => {
            validate_computer_payload(operation.kind, &operation.payload)?;
            wire.kind = operation.kind.wire_kind().to_string();
            wire.stdin = Some(operation.payload);
            wire.timeout_secs = operation.timeout_secs.max(1);
        }
        RunnerOperation::Validation {
            payload,
            timeout_secs,
        } => {
            validate_bridge_request(&payload)?;
            wire.validation = Some(payload);
            wire.timeout_secs = timeout_secs;
        }
        RunnerOperation::Lsp {
            payload,
            timeout_secs,
        } => {
            wire.lsp = Some(payload);
            wire.timeout_secs = timeout_secs.max(1);
        }
        RunnerOperation::PersistentShell(operation) => {
            validate_persistent_shell(&operation.request)?;
            wire.cwd = operation.request.cwd.clone();
            wire.command = operation.request.command.clone().unwrap_or_default();
            wire.timeout_secs = operation.request.timeout_secs.unwrap_or(30);
            wire.job_context = operation.job_context;
            wire.persistent_shell = Some(operation.request);
        }
        RunnerOperation::McpGateway(operation) => {
            validate_mcp_gateway_request(&operation)
                .map_err(|error| format!("invalid MCP gateway request: {error}"))?;
            wire.timeout_secs = 120;
            wire.mcp_gateway = Some(operation);
        }
        RunnerOperation::PluginGateway(operation) => {
            validate_plugin_gateway_request(&operation)
                .map_err(|error| format!("invalid Plugin gateway request: {error}"))?;
            wire.timeout_secs = 120;
            wire.plugin_gateway = Some(operation);
        }
        RunnerOperation::CodingAgent(operation) => {
            validate_coding_agent_request(&operation)
                .map_err(|error| format!("invalid CodingAgent request: {error}"))?;
            wire.timeout_secs = 120;
            wire.coding_agent = Some(operation);
        }
        RunnerOperation::SkillStore(operation) => {
            let content = serde_json::to_string(&operation)
                .map_err(|error| format!("could not encode Skill store request: {error}"))?;
            if content.len() > 32 * 1024 {
                return Err("Skill store request exceeds V2 payload bound".to_string());
            }
            wire.content = Some(content);
            wire.timeout_secs = 120;
        }
        RunnerOperation::SshResource(operation) => {
            operation
                .validate()
                .map_err(|error| format!("invalid SSH resource request: {error}"))?;
            let content = serde_json::to_string(&operation)
                .map_err(|error| format!("could not encode SSH resource request: {error}"))?;
            if content.len() > SSH_RESOURCE_REQUEST_MAX_BYTES {
                return Err("SSH resource request exceeds V2 payload bound".to_string());
            }
            wire.content = Some(content);
            wire.timeout_secs = 60;
        }
        RunnerOperation::RunnerConfig(operation) => {
            operation
                .validate()
                .map_err(|error| format!("invalid Runner config request: {error}"))?;
            let content = serde_json::to_string(&operation)
                .map_err(|error| format!("could not encode Runner config request: {error}"))?;
            if content.len() > RUNNER_CONFIG_REQUEST_MAX_BYTES {
                return Err("Runner config request exceeds V2 payload bound".to_string());
            }
            wire.content = Some(content);
            wire.timeout_secs = 30;
        }
    }
    Ok(wire)
}

fn encode_job_operation(
    wire: &mut RunnerRequest,
    operation: RunnerJobOperation,
) -> Result<(), String> {
    wire.kind = match &operation {
        RunnerJobOperation::StartShell(_) => "start_job",
        RunnerJobOperation::StartValidation(_) => "start_validation_job",
        RunnerJobOperation::StartProcess(_) => "start_process_job",
        RunnerJobOperation::StartDetachedProcess(_) => "start_detached_process_job",
        RunnerJobOperation::StartScript(_) => "start_script_job",
        RunnerJobOperation::Stop { .. } => "stop_job",
    }
    .to_string();
    match operation {
        RunnerJobOperation::StartShell(operation) => {
            validate_raw_shell_wire_command(&operation.command)?;
            validate_job_context_coherence(operation.cwd.as_deref(), &operation.context)?;
            wire.job_id = Some(operation.job_id);
            wire.cwd = operation.cwd;
            wire.command = operation.command;
            wire.timeout_secs = operation.timeout_secs;
            wire.job_context = Some(operation.context);
        }
        RunnerJobOperation::StartValidation(operation) => {
            validate_validation_steps(&operation.steps)?;
            validate_job_context_coherence(operation.cwd.as_deref(), &operation.context)?;
            let names = operation
                .steps
                .iter()
                .map(|step| step.name.clone())
                .collect::<Vec<_>>();
            if operation.context.validation_steps != names {
                return Err("Job validation context does not match operation steps".to_string());
            }
            wire.job_id = Some(operation.job_id);
            wire.cwd = operation.cwd;
            wire.command = serde_json::to_string(&operation.steps)
                .map_err(|error| format!("could not encode validation Job plan: {error}"))?;
            wire.timeout_secs = operation.timeout_secs;
            wire.job_context = Some(operation.context);
        }
        RunnerJobOperation::StartProcess(operation)
        | RunnerJobOperation::StartDetachedProcess(operation) => {
            validate_structured_job_common(
                operation.cwd.as_deref(),
                operation.stdin.as_deref(),
                operation.timeout_secs,
            )?;
            validate_process_argv(&operation.process)?;
            validate_job_context_coherence(operation.cwd.as_deref(), &operation.context)?;
            wire.job_id = Some(operation.job_id);
            wire.cwd = operation.cwd;
            wire.process = Some(operation.process);
            wire.stdin = operation.stdin;
            wire.timeout_secs = operation.timeout_secs;
            wire.job_context = Some(operation.context);
        }
        RunnerJobOperation::StartScript(operation) => {
            validate_script_request(
                &operation.script,
                operation.stdin.as_deref(),
                operation.cwd.as_deref(),
                operation.timeout_secs,
            )?;
            validate_job_context_coherence(operation.cwd.as_deref(), &operation.context)?;
            wire.job_id = Some(operation.job_id);
            wire.cwd = operation.cwd;
            wire.script = Some(operation.script);
            wire.stdin = operation.stdin;
            wire.timeout_secs = operation.timeout_secs;
            wire.job_context = Some(operation.context);
        }
        RunnerJobOperation::Stop { job_id } => {
            wire.job_id = Some(job_id);
            wire.timeout_secs = 1;
        }
    }
    Ok(())
}

fn decode_operation(wire: &RunnerRequest) -> Result<RunnerOperation, String> {
    let no_special = || ensure_special_payloads_absent(wire);
    match wire.kind.as_str() {
        "run_shell" => {
            no_special()?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some() {
                return Err("run_shell does not accept job_id".to_string());
            }
            validate_raw_shell_wire_command(&wire.command)?;
            Ok(RunnerOperation::RunShell(RunnerShellOperation {
                cwd: wire.cwd.clone(),
                command: wire.command.clone(),
                stdin: wire.stdin.clone(),
                timeout_secs: wire.timeout_secs,
                job_context: wire.job_context.clone(),
            }))
        }
        "run_process" => {
            ensure_only_process_payload(wire)?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some() || wire.job_context.is_some() || !wire.command.is_empty() {
                return Err("run_process contains incompatible execution fields".to_string());
            }
            let operation = RunnerProcessOperation {
                cwd: wire.cwd.clone(),
                process: wire
                    .process
                    .clone()
                    .ok_or_else(|| "run_process requires process payload".to_string())?,
                stdin: wire.stdin.clone(),
                timeout_secs: wire.timeout_secs,
            };
            validate_direct_process(&operation)?;
            Ok(RunnerOperation::RunProcess(operation))
        }
        "run_script" | "run_internal_posix_script" => {
            ensure_only_script_payload(wire)?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some() || wire.job_context.is_some() || !wire.command.is_empty() {
                return Err(format!(
                    "{} contains incompatible execution fields",
                    wire.kind
                ));
            }
            let operation = RunnerScriptOperation {
                cwd: wire.cwd.clone(),
                script: wire
                    .script
                    .clone()
                    .ok_or_else(|| format!("{} requires script payload", wire.kind))?,
                stdin: wire.stdin.clone(),
                timeout_secs: wire.timeout_secs,
            };
            let internal = wire.kind == "run_internal_posix_script";
            validate_direct_script(&operation, internal)?;
            Ok(if internal {
                RunnerOperation::RunInternalPosixScript(operation)
            } else {
                RunnerOperation::RunScript(operation)
            })
        }
        "start_job"
        | "start_validation_job"
        | "start_process_job"
        | "start_detached_process_job"
        | "start_script_job"
        | "stop_job" => decode_job_operation(wire).map(RunnerOperation::Job),
        kind if kind.starts_with("file_") => {
            ensure_special_payloads_absent(wire)?;
            if wire.job_id.is_some()
                || wire.stdin.is_some()
                || wire.job_context.is_some()
                || !wire.command.is_empty()
            {
                return Err("file operation contains incompatible execution fields".to_string());
            }
            let path = wire
                .path
                .clone()
                .ok_or_else(|| "file operation requires path".to_string())?;
            RunnerFileOperation::from_wire_kind(
                kind,
                RunnerFilePayload {
                    cwd: wire.cwd.clone(),
                    path,
                    content: wire.content.clone(),
                    max_bytes: wire.max_bytes,
                    expected_sha256: wire.expected_sha256.clone(),
                    expected_prefix: wire.expected_prefix.clone(),
                    start_line: wire.start_line,
                    end_line: wire.end_line,
                    create_dirs: wire.create_dirs,
                },
            )
            .map(RunnerOperation::File)
        }
        kind if RunnerProjectOperationKind::from_wire(kind).is_some() => {
            ensure_special_payloads_absent(wire)?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some()
                || wire.cwd.is_some()
                || !wire.command.is_empty()
                || wire.job_context.is_some()
            {
                return Err("project operation contains incompatible execution fields".to_string());
            }
            let payload = wire
                .stdin
                .clone()
                .ok_or_else(|| "project operation requires JSON payload".to_string())?;
            validate_json_payload(&payload, "project operation")?;
            Ok(RunnerOperation::Project(RunnerProjectOperation {
                kind: RunnerProjectOperationKind::from_wire(kind).expect("checked project kind"),
                payload,
            }))
        }
        kind if RunnerComputerOperationKind::from_wire(kind).is_some() => {
            ensure_special_payloads_absent(wire)?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some()
                || wire.cwd.is_some()
                || !wire.command.is_empty()
                || wire.job_context.is_some()
            {
                return Err("computer operation contains incompatible execution fields".to_string());
            }
            let operation_kind =
                RunnerComputerOperationKind::from_wire(kind).expect("checked computer kind");
            let payload = wire
                .stdin
                .clone()
                .ok_or_else(|| "computer operation requires JSON payload".to_string())?;
            validate_computer_payload(operation_kind, &payload)?;
            Ok(RunnerOperation::Computer(RunnerComputerOperation {
                kind: operation_kind,
                payload,
                timeout_secs: wire.timeout_secs,
            }))
        }
        crate::validation_bridge::AGENT_VALIDATION_REQUEST_KIND => {
            ensure_only_validation_payload(wire)?;
            ensure_empty_generic_execution_fields(wire, false)?;
            let payload = wire
                .validation
                .clone()
                .ok_or_else(|| "validation operation requires validation payload".to_string())?;
            validate_bridge_request(&payload)?;
            Ok(RunnerOperation::Validation {
                payload,
                timeout_secs: wire.timeout_secs,
            })
        }
        crate::lsp_bridge::AGENT_LSP_REQUEST_KIND => {
            ensure_only_lsp_payload(wire)?;
            ensure_empty_generic_execution_fields(wire, false)?;
            Ok(RunnerOperation::Lsp {
                payload: wire
                    .lsp
                    .clone()
                    .ok_or_else(|| "lsp operation requires lsp payload".to_string())?,
                timeout_secs: wire.timeout_secs,
            })
        }
        "persistent_shell" => {
            ensure_only_persistent_shell_payload(wire)?;
            ensure_no_file_fields(wire)?;
            if wire.job_id.is_some() || wire.stdin.is_some() {
                return Err("persistent_shell contains incompatible execution fields".to_string());
            }
            let request = wire
                .persistent_shell
                .clone()
                .ok_or_else(|| "persistent_shell requires persistent_shell payload".to_string())?;
            validate_persistent_shell(&request)?;
            if wire.cwd != request.cwd
                || wire.command != request.command.clone().unwrap_or_default()
                || wire.timeout_secs != request.timeout_secs.unwrap_or(30)
            {
                return Err(
                    "persistent_shell compatibility fields do not match typed payload".to_string(),
                );
            }
            Ok(RunnerOperation::PersistentShell(
                RunnerPersistentShellOperation {
                    request,
                    job_context: wire.job_context.clone(),
                },
            ))
        }
        "mcp_gateway" => {
            ensure_only_mcp_payload(wire)?;
            ensure_empty_generic_execution_fields(wire, false)?;
            let operation = wire
                .mcp_gateway
                .clone()
                .ok_or_else(|| "mcp_gateway requires mcp_gateway payload".to_string())?;
            validate_mcp_gateway_request(&operation)
                .map_err(|error| format!("invalid MCP gateway request: {error}"))?;
            Ok(RunnerOperation::McpGateway(operation))
        }
        "plugin_gateway" => {
            ensure_only_plugin_payload(wire)?;
            ensure_empty_generic_execution_fields(wire, false)?;
            let operation = wire
                .plugin_gateway
                .clone()
                .ok_or_else(|| "plugin_gateway requires plugin_gateway payload".to_string())?;
            validate_plugin_gateway_request(&operation)
                .map_err(|error| format!("invalid Plugin gateway request: {error}"))?;
            Ok(RunnerOperation::PluginGateway(operation))
        }
        "coding_agent" => {
            ensure_only_coding_agent_payload(wire)?;
            ensure_empty_generic_execution_fields(wire, false)?;
            let operation = wire
                .coding_agent
                .clone()
                .ok_or_else(|| "coding_agent requires coding_agent payload".to_string())?;
            validate_coding_agent_request(&operation)
                .map_err(|error| format!("invalid CodingAgent request: {error}"))?;
            Ok(RunnerOperation::CodingAgent(operation))
        }
        "skill_store" => {
            ensure_special_payloads_absent(wire)?;
            ensure_empty_generic_execution_fields(wire, true)?;
            let content = bounded_content(wire, 32 * 1024, "skill_store")?;
            let operation = serde_json::from_str::<SkillStoreRequest>(content)
                .map_err(|error| format!("invalid Skill store payload: {error}"))?;
            Ok(RunnerOperation::SkillStore(operation))
        }
        "ssh_resource" => {
            ensure_special_payloads_absent(wire)?;
            ensure_empty_generic_execution_fields(wire, true)?;
            let content = bounded_content(wire, SSH_RESOURCE_REQUEST_MAX_BYTES, "ssh_resource")?;
            let operation = serde_json::from_str::<SshResourceRequest>(content)
                .map_err(|error| format!("invalid SSH resource payload: {error}"))?;
            operation
                .validate()
                .map_err(|error| format!("invalid SSH resource request: {error}"))?;
            Ok(RunnerOperation::SshResource(operation))
        }
        RUNNER_CONFIG_REQUEST_KIND => {
            ensure_special_payloads_absent(wire)?;
            ensure_empty_generic_execution_fields(wire, true)?;
            let content = bounded_content(wire, RUNNER_CONFIG_REQUEST_MAX_BYTES, "runner_config")?;
            let operation = serde_json::from_str::<RunnerConfigOperationRequest>(content)
                .map_err(|error| format!("invalid Runner config payload: {error}"))?;
            operation
                .validate()
                .map_err(|error| format!("invalid Runner config request: {error}"))?;
            Ok(RunnerOperation::RunnerConfig(operation))
        }
        kind => Err(format!("unknown Runner V2 request kind: {kind}")),
    }
}

fn decode_job_operation(wire: &RunnerRequest) -> Result<RunnerJobOperation, String> {
    ensure_no_file_fields(wire)?;
    let job_id = wire
        .job_id
        .clone()
        .ok_or_else(|| format!("{} requires job_id", wire.kind))?;
    if wire.kind == "stop_job" {
        ensure_special_payloads_absent(wire)?;
        if wire.cwd.is_some()
            || wire.stdin.is_some()
            || wire.job_context.is_some()
            || !wire.command.is_empty()
        {
            return Err("stop_job contains incompatible execution fields".to_string());
        }
        return Ok(RunnerJobOperation::Stop { job_id });
    }
    let context = wire
        .job_context
        .clone()
        .ok_or_else(|| format!("{} requires job_context", wire.kind))?;
    validate_job_context_coherence(wire.cwd.as_deref(), &context)?;
    match wire.kind.as_str() {
        "start_job" => {
            ensure_special_payloads_absent(wire)?;
            if wire.stdin.is_some() {
                return Err("start_job does not accept stdin".to_string());
            }
            validate_raw_shell_wire_command(&wire.command)?;
            Ok(RunnerJobOperation::StartShell(RunnerJobShellOperation {
                job_id,
                cwd: wire.cwd.clone(),
                command: wire.command.clone(),
                timeout_secs: wire.timeout_secs,
                context,
            }))
        }
        "start_validation_job" => {
            ensure_special_payloads_absent(wire)?;
            if wire.stdin.is_some() {
                return Err("start_validation_job does not accept stdin".to_string());
            }
            let steps = serde_json::from_str::<Vec<ShellJobValidationStep>>(&wire.command)
                .map_err(|error| format!("invalid validation Job plan: {error}"))?;
            validate_validation_steps(&steps)?;
            let names = steps
                .iter()
                .map(|step| step.name.clone())
                .collect::<Vec<_>>();
            if context.validation_steps != names {
                return Err("validation Job plan does not match recovery context".to_string());
            }
            Ok(RunnerJobOperation::StartValidation(
                RunnerJobValidationOperation {
                    job_id,
                    cwd: wire.cwd.clone(),
                    steps,
                    timeout_secs: wire.timeout_secs,
                    context,
                },
            ))
        }
        "start_process_job" | "start_detached_process_job" => {
            ensure_only_process_payload(wire)?;
            if !wire.command.is_empty() || context.ssh_resource.is_some() {
                return Err("typed process Job contains incompatible execution fields".to_string());
            }
            let process = wire
                .process
                .clone()
                .ok_or_else(|| format!("{} requires process payload", wire.kind))?;
            validate_process_argv(&process)?;
            validate_structured_job_common(
                wire.cwd.as_deref(),
                wire.stdin.as_deref(),
                wire.timeout_secs,
            )?;
            let operation = RunnerJobProcessOperation {
                job_id,
                cwd: wire.cwd.clone(),
                process,
                stdin: wire.stdin.clone(),
                timeout_secs: wire.timeout_secs,
                context,
            };
            let job = if wire.kind == "start_detached_process_job" {
                RunnerJobOperation::StartDetachedProcess(operation)
            } else {
                RunnerJobOperation::StartProcess(operation)
            };
            if job
                .context()
                .and_then(|ctx| ctx.structured_execution.clone())
                != job.expected_structured_execution()
            {
                return Err(
                    "process Job recovery metadata does not match typed operation".to_string(),
                );
            }
            Ok(job)
        }
        "start_script_job" => {
            ensure_only_script_payload(wire)?;
            if !wire.command.is_empty() || context.ssh_resource.is_some() {
                return Err("typed script Job contains incompatible execution fields".to_string());
            }
            let script = wire
                .script
                .clone()
                .ok_or_else(|| "start_script_job requires script payload".to_string())?;
            validate_script_request(
                &script,
                wire.stdin.as_deref(),
                wire.cwd.as_deref(),
                wire.timeout_secs,
            )?;
            let job = RunnerJobOperation::StartScript(RunnerJobScriptOperation {
                job_id,
                cwd: wire.cwd.clone(),
                script,
                stdin: wire.stdin.clone(),
                timeout_secs: wire.timeout_secs,
                context,
            });
            if job
                .context()
                .and_then(|ctx| ctx.structured_execution.clone())
                != job.expected_structured_execution()
            {
                return Err(
                    "script Job recovery metadata does not match typed operation".to_string(),
                );
            }
            Ok(job)
        }
        _ => unreachable!("decode_job_operation called for non-job kind"),
    }
}

fn validate_direct_process(operation: &RunnerProcessOperation) -> Result<(), String> {
    validate_process_argv(&operation.process)?;
    validate_direct_structured_common(
        operation.cwd.as_deref(),
        operation.stdin.as_deref(),
        operation.timeout_secs,
    )
}

fn validate_direct_script(operation: &RunnerScriptOperation, internal: bool) -> Result<(), String> {
    validate_script_request(
        &operation.script,
        operation.stdin.as_deref(),
        operation.cwd.as_deref(),
        operation.timeout_secs,
    )?;
    if operation.timeout_secs > STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS {
        return Err(format!(
            "timeout_secs must be between 1 and {STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS}"
        ));
    }
    if internal
        && (operation.stdin.is_some()
            || operation.script.language != ShellScriptLanguage::Sh
            || !operation.script.args.is_empty())
    {
        return Err("internal POSIX script requires sh, no args, and no stdin".to_string());
    }
    Ok(())
}

fn validate_direct_structured_common(
    cwd: Option<&str>,
    stdin: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    validate_structured_text_fields(cwd, stdin)?;
    if timeout_secs == 0 || timeout_secs > STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS {
        return Err(format!(
            "timeout_secs must be between 1 and {STRUCTURED_EXECUTION_DIRECT_SYNC_TIMEOUT_MAX_SECS}"
        ));
    }
    Ok(())
}

fn validate_structured_job_common(
    cwd: Option<&str>,
    stdin: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    validate_structured_text_fields(cwd, stdin)?;
    if !(STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS..=STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS)
        .contains(&timeout_secs)
    {
        return Err(format!(
            "timeout_secs must be between {STRUCTURED_EXECUTION_TIMEOUT_MIN_SECS} and {STRUCTURED_EXECUTION_TIMEOUT_MAX_SECS}"
        ));
    }
    Ok(())
}

fn validate_structured_text_fields(cwd: Option<&str>, stdin: Option<&str>) -> Result<(), String> {
    if let Some(stdin) = stdin {
        if stdin.len() > PROCESS_STDIN_MAX_BYTES || stdin.contains('\0') {
            return Err("stdin is invalid or oversized".to_string());
        }
    }
    if let Some(cwd) = cwd {
        if cwd.len() > PROCESS_CWD_MAX_BYTES || cwd.contains('\0') {
            return Err("cwd is invalid or oversized".to_string());
        }
    }
    Ok(())
}

fn validate_validation_steps(steps: &[ShellJobValidationStep]) -> Result<(), String> {
    if !(1..=3).contains(&steps.len())
        || steps.iter().any(|step| !step.is_canonical())
        || steps.iter().enumerate().any(|(index, step)| {
            steps[..index]
                .iter()
                .any(|earlier| earlier.name == step.name)
        })
    {
        return Err("invalid structured validation plan".to_string());
    }
    Ok(())
}

fn validate_job_context_coherence(
    cwd: Option<&str>,
    context: &ShellJobContext,
) -> Result<(), String> {
    if context.cwd.as_deref() != cwd {
        return Err("job recovery context cwd does not match operation cwd".to_string());
    }
    if context.ssh_resource.is_some() && context.workflow_session_id.is_none() {
        return Err("job recovery SSH resource requires Workflow Session".to_string());
    }
    Ok(())
}

fn validate_file_payload(kind: &str, payload: &RunnerFilePayload) -> Result<(), String> {
    if payload.path.is_empty() || payload.path.contains('\0') {
        return Err("file operation path is required and cannot contain NUL".to_string());
    }
    if payload.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0')) {
        return Err("file operation cwd cannot contain NUL".to_string());
    }
    let write = kind == "file_write";
    if !write
        && (payload.expected_sha256.is_some()
            || payload.expected_prefix.is_some()
            || payload.create_dirs)
    {
        return Err(
            "write-only compatibility fields are present on non-write file operation".to_string(),
        );
    }
    if write && payload.content.is_none() {
        return Err("file_write requires content".to_string());
    }
    if kind == "file_read" {
        match (payload.start_line, payload.end_line) {
            (Some(start), Some(end)) if start > 0 && end >= start => {}
            (None, None) => {}
            _ => return Err("file_read line range is invalid".to_string()),
        }
    } else if payload.start_line.is_some() || payload.end_line.is_some() {
        // skill_read_file historically uses both fields.
        if kind != "file_skill_read_file"
            || payload.start_line.is_none()
            || payload.end_line.is_none()
            || payload.start_line == Some(0)
            || payload
                .end_line
                .zip(payload.start_line)
                .is_some_and(|(end, start)| end < start)
        {
            return Err("line range is incompatible with file operation".to_string());
        }
    }
    Ok(())
}

fn validate_json_payload(payload: &str, label: &str) -> Result<(), String> {
    if payload.contains('\0') {
        return Err(format!("{label} payload cannot contain NUL"));
    }
    serde_json::from_str::<serde_json::Value>(payload)
        .map(|_| ())
        .map_err(|error| format!("{label} payload is not valid JSON: {error}"))
}

fn validate_computer_payload(
    kind: RunnerComputerOperationKind,
    payload: &str,
) -> Result<(), String> {
    if payload.len() > shell_computer_request_payload_max_bytes(kind.wire_kind()) {
        return Err("computer request payload exceeds V2 bound".to_string());
    }
    validate_json_payload(payload, "computer operation")
}

fn validate_persistent_shell(request: &PersistentShellRequest) -> Result<(), String> {
    if !matches!(
        request.action.as_str(),
        "open" | "exec" | "status" | "close"
    ) {
        return Err("unsupported persistent shell action".to_string());
    }
    if request.shell_id.is_empty()
        || request.workflow_session_id.is_empty()
        || request.runtime_project_id.trim().is_empty()
        || request
            .command
            .as_deref()
            .is_some_and(|command| command.contains('\0'))
    {
        return Err("persistent shell payload is invalid".to_string());
    }
    Ok(())
}

fn bounded_content<'a>(
    wire: &'a RunnerRequest,
    max_bytes: usize,
    label: &str,
) -> Result<&'a str, String> {
    let content = wire
        .content
        .as_deref()
        .ok_or_else(|| format!("{label} requires content payload"))?;
    if content.len() > max_bytes || content.contains('\0') {
        return Err(format!("{label} content payload is invalid or oversized"));
    }
    Ok(content)
}

fn ensure_no_file_fields(wire: &RunnerRequest) -> Result<(), String> {
    if wire.path.is_some()
        || wire.content.is_some()
        || wire.max_bytes.is_some()
        || wire.expected_sha256.is_some()
        || wire.expected_prefix.is_some()
        || wire.start_line.is_some()
        || wire.end_line.is_some()
        || wire.create_dirs
    {
        return Err(format!("{} contains incompatible file fields", wire.kind));
    }
    Ok(())
}

fn ensure_special_payloads_absent(wire: &RunnerRequest) -> Result<(), String> {
    if wire.process.is_some()
        || wire.script.is_some()
        || wire.validation.is_some()
        || wire.lsp.is_some()
        || wire.persistent_shell.is_some()
        || wire.mcp_gateway.is_some()
        || wire.plugin_gateway.is_some()
        || wire.coding_agent.is_some()
    {
        return Err(format!("{} contains incompatible typed payload", wire.kind));
    }
    Ok(())
}

fn ensure_only_process_payload(wire: &RunnerRequest) -> Result<(), String> {
    if wire.script.is_some()
        || wire.validation.is_some()
        || wire.lsp.is_some()
        || wire.persistent_shell.is_some()
        || wire.mcp_gateway.is_some()
        || wire.plugin_gateway.is_some()
        || wire.coding_agent.is_some()
    {
        return Err(format!(
            "{} conflicts with non-process typed payload",
            wire.kind
        ));
    }
    Ok(())
}

fn ensure_only_script_payload(wire: &RunnerRequest) -> Result<(), String> {
    if wire.process.is_some()
        || wire.validation.is_some()
        || wire.lsp.is_some()
        || wire.persistent_shell.is_some()
        || wire.mcp_gateway.is_some()
        || wire.plugin_gateway.is_some()
        || wire.coding_agent.is_some()
    {
        return Err(format!(
            "{} conflicts with non-script typed payload",
            wire.kind
        ));
    }
    Ok(())
}

macro_rules! ensure_only_payload {
    ($name:ident, $allowed:ident) => {
        fn $name(wire: &RunnerRequest) -> Result<(), String> {
            if wire.process.is_some()
                || wire.script.is_some()
                || wire.validation.is_some() && stringify!($allowed) != "validation"
                || wire.lsp.is_some() && stringify!($allowed) != "lsp"
                || wire.persistent_shell.is_some() && stringify!($allowed) != "persistent_shell"
                || wire.mcp_gateway.is_some() && stringify!($allowed) != "mcp_gateway"
                || wire.plugin_gateway.is_some() && stringify!($allowed) != "plugin_gateway"
                || wire.coding_agent.is_some() && stringify!($allowed) != "coding_agent"
            {
                return Err(format!("{} contains conflicting typed payloads", wire.kind));
            }
            Ok(())
        }
    };
}

ensure_only_payload!(ensure_only_validation_payload, validation);
ensure_only_payload!(ensure_only_lsp_payload, lsp);
ensure_only_payload!(ensure_only_persistent_shell_payload, persistent_shell);
ensure_only_payload!(ensure_only_mcp_payload, mcp_gateway);
ensure_only_payload!(ensure_only_plugin_payload, plugin_gateway);
ensure_only_payload!(ensure_only_coding_agent_payload, coding_agent);

fn ensure_empty_generic_execution_fields(
    wire: &RunnerRequest,
    allow_content: bool,
) -> Result<(), String> {
    if wire.job_id.is_some()
        || wire.cwd.is_some()
        || wire.path.is_some()
        || (!allow_content && wire.content.is_some())
        || wire.max_bytes.is_some()
        || wire.expected_sha256.is_some()
        || wire.expected_prefix.is_some()
        || wire.start_line.is_some()
        || wire.end_line.is_some()
        || wire.create_dirs
        || !wire.command.is_empty()
        || wire.stdin.is_some()
        || wire.job_context.is_some()
    {
        return Err(format!(
            "{} contains incompatible generic fields",
            wire.kind
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> RunnerInvocationMetadata {
        RunnerInvocationMetadata {
            request_id: "req-1".to_string(),
            client_id: "runner-1".to_string(),
            requested_by: "tester".to_string(),
            created_at: 123,
        }
    }

    fn shell_wire() -> RunnerRequest {
        RunnerRequest::from_operation(
            metadata(),
            RunnerOperation::RunShell(RunnerShellOperation {
                cwd: Some("/tmp".to_string()),
                command: "printf ok".to_string(),
                stdin: None,
                timeout_secs: 30,
                job_context: None,
            }),
        )
        .unwrap()
    }

    #[test]
    fn omitted_v2_kind_keeps_legacy_run_shell_default() {
        let value = serde_json::json!({
            "request_id": "req-1",
            "client_id": "runner-1",
            "command": "printf ok",
            "timeout_secs": 30,
            "requested_by": "tester",
            "created_at": 123
        });
        let wire: RunnerRequest = serde_json::from_value(value).unwrap();
        assert_eq!(wire.kind, "run_shell");
        assert!(matches!(
            wire.decode_operation().unwrap(),
            RunnerOperation::RunShell(_)
        ));
    }

    #[test]
    fn explicit_unknown_kind_fails_closed() {
        let mut wire = shell_wire();
        wire.kind = "future_magic_operation".to_string();
        assert!(wire
            .decode_operation()
            .unwrap_err()
            .contains("unknown Runner V2 request kind"));
    }

    #[test]
    fn conflicting_process_and_script_payloads_fail_closed() {
        let process = RunnerProcessOperation {
            cwd: None,
            process: ShellProcessArgv {
                executable: "printf".to_string(),
                args: vec!["ok".to_string()],
            },
            stdin: None,
            timeout_secs: 30,
        };
        let mut wire =
            RunnerRequest::from_operation(metadata(), RunnerOperation::RunProcess(process))
                .unwrap();
        wire.script = Some(ShellScriptPayload {
            language: ShellScriptLanguage::Sh,
            script: "printf no".to_string(),
            args: Vec::new(),
        });
        assert!(wire.decode_operation().unwrap_err().contains("conflicts"));
    }

    #[test]
    fn canonical_process_round_trip_has_no_shell_or_script_payload() {
        let operation = RunnerOperation::RunProcess(RunnerProcessOperation {
            cwd: Some("subdir".to_string()),
            process: ShellProcessArgv {
                executable: "argv-helper".to_string(),
                args: vec!["literal space".to_string()],
            },
            stdin: Some("input".to_string()),
            timeout_secs: 60,
        });
        let wire = RunnerRequest::from_operation(metadata(), operation).unwrap();
        assert_eq!(wire.kind, "run_process");
        assert!(wire.command.is_empty());
        assert!(wire.script.is_none());
        assert!(wire.process.is_some());
        assert!(matches!(
            wire.decode_operation().unwrap(),
            RunnerOperation::RunProcess(_)
        ));
    }

    #[test]
    fn canonical_file_and_project_operations_round_trip() {
        let file = RunnerFileOperation::Read(RunnerFilePayload {
            cwd: Some("/repo".to_string()),
            path: "src/lib.rs".to_string(),
            content: None,
            max_bytes: Some(4096),
            expected_sha256: None,
            expected_prefix: None,
            start_line: Some(1),
            end_line: Some(20),
            create_dirs: false,
        });
        let wire = RunnerRequest::from_operation(metadata(), RunnerOperation::File(file)).unwrap();
        assert_eq!(wire.kind, "file_read");
        assert!(matches!(
            wire.decode_operation().unwrap(),
            RunnerOperation::File(RunnerFileOperation::Read(_))
        ));

        let project = RunnerProjectOperation {
            kind: RunnerProjectOperationKind::Register,
            payload: "{\"path\":\"/repo\"}".to_string(),
        };
        let wire =
            RunnerRequest::from_operation(metadata(), RunnerOperation::Project(project)).unwrap();
        assert_eq!(wire.kind, "register_project");
        assert!(wire.command.is_empty());
        assert!(matches!(
            wire.decode_operation().unwrap(),
            RunnerOperation::Project(_)
        ));
    }
}
