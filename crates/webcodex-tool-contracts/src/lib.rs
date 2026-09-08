#![recursion_limit = "512"]

//! Declarative WebCodex tool contracts: catalog, schemas, metadata, and policy queries.
//!
//! Execution, authorization enforcement, connector orchestration, and side effects remain in
//! the root application crate.

pub mod audit_policy;
pub mod metadata;
pub mod registry;
pub mod tool_catalog;
pub mod tool_definition;
pub mod tool_policy;
pub mod tool_spec;

#[cfg(any(test, feature = "root-test-support"))]
pub mod test_support;

#[cfg(test)]
mod tests;

pub use metadata::*;
pub use registry::*;
pub use tool_catalog::*;
pub use tool_definition::*;
pub use tool_policy::*;
pub use tool_spec::{ToolSpec, MODEL_TOOL_DESCRIPTION_MAX_CHARS};
