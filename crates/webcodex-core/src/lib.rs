//! Side-effect-free shared protocol contracts and helpers for WebCodex.

pub mod activity_contract;
pub mod apply_edits_shared;
pub mod apply_patch_shared;
pub mod artifact_policy;
pub mod audit_preview;
pub mod authority;
pub mod build_info;
pub mod coding_agent;
pub mod job_observation;
pub mod lsp_bridge;
pub mod mcp_gateway;
pub mod memory_contract;
pub mod plugin;
pub mod project_context_contract;
pub mod project_instructions;
pub mod project_listing;
pub mod runner_operation;
pub mod runner_protocol;
pub mod runtime_contract;
pub mod sensitive_paths;
pub mod sensitive_text;
pub mod shell_quote;
pub mod skill_metadata;
pub mod skill_store;
pub mod ssh_resource;
pub mod validation_bridge;
pub mod validation_evidence;

#[cfg(test)]
mod validation_evidence_tests;
pub mod validation_identity;
pub mod workflow_session_contract;
