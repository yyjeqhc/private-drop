//! Shared filesystem inspection and Git checkpoint implementations.

pub mod file_read_normalize;
pub mod file_read_range;
pub mod path_policy;
pub mod project_context;
pub mod project_overview;
#[cfg(feature = "workspace-checkpoints")]
pub mod workspace_checkpoint;
