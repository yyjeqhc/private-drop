use super::output::{ok_cmd, CommandResult};
use crate::workspace_checkpoint::{create_workspace_checkpoint, restore_workspace_checkpoint};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Instant;
use webcodex_core::runner_operation::{RunnerFileOperation, RunnerFilePayload};

#[cfg(test)]
pub(crate) fn is_checkpoint_request_kind(kind: &str) -> bool {
    matches!(kind, "file_checkpoint_create" | "file_checkpoint_restore")
}

pub(crate) fn handle_checkpoint_file_request(
    operation: &RunnerFileOperation,
    resolved: &Path,
    start: Instant,
) -> CommandResult {
    let request = operation.payload();
    let payload = match parse_payload(request) {
        Ok(payload) => payload,
        Err(err) => return ok_cmd(start, checkpoint_error("invalid_checkpoint_payload", err)),
    };
    let output = match operation {
        RunnerFileOperation::CheckpointCreate(_) => {
            let include_untracked = payload
                .get("include_untracked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            create_workspace_checkpoint(resolved, include_untracked)
        }
        RunnerFileOperation::CheckpointRestore(_) => {
            let Some(checkpoint) = payload.get("checkpoint") else {
                return ok_cmd(
                    start,
                    checkpoint_error("invalid_checkpoint_payload", "checkpoint is required"),
                );
            };
            restore_workspace_checkpoint(resolved, checkpoint)
        }
        _ => {
            return ok_cmd(
                start,
                checkpoint_error("invalid_checkpoint_payload", "not a checkpoint operation"),
            )
        }
    };
    ok_cmd(start, output)
}

fn parse_payload(request: &RunnerFilePayload) -> Result<Value, String> {
    let content = request
        .content
        .as_deref()
        .ok_or_else(|| "checkpoint request missing JSON payload".to_string())?;
    serde_json::from_str(content).map_err(|err| format!("invalid JSON payload: {err}"))
}

fn checkpoint_error(kind: &str, message: impl Into<String>) -> Value {
    json!({
        "error_kind": kind,
        "error": message.into(),
    })
}
