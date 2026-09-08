use super::{err_cmd, ok_cmd, CommandResult};
use crate::artifact_policy::MAX_MCP_IMAGE_BYTES;
#[cfg(test)]
use crate::runner_protocol::shell_computer_request_payload_max_bytes;
#[cfg(test)]
use crate::runner_protocol::RunnerRequest;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Instant;
use webcodex_computer::{
    ComputerAction, ComputerConfig, ComputerRuntime, PointerAction, SnapshotRegion,
    DEFAULT_ACCESSIBILITY_DEPTH, DEFAULT_ACCESSIBILITY_NODES, MAX_APPLICATIONS, MAX_DISPLAYS,
    MAX_WINDOWS,
};
#[cfg(test)]
use webcodex_core::runner_operation::RunnerOperation;
use webcodex_core::runner_operation::{RunnerComputerOperation, RunnerComputerOperationKind};

fn computer_runtime() -> &'static ComputerRuntime {
    static COMPUTER: OnceLock<ComputerRuntime> = OnceLock::new();
    COMPUTER.get_or_init(|| {
        ComputerRuntime::new(ComputerConfig {
            max_encoded_image_bytes: MAX_MCP_IMAGE_BYTES,
        })
    })
}

#[cfg(test)]
pub(crate) fn is_computer_request_kind(kind: &str) -> bool {
    RunnerComputerOperationKind::from_wire(kind).is_some()
}

fn ensure_exact_payload_fields(payload: &Value, expected: &[&str]) -> Result<(), String> {
    let object = payload
        .as_object()
        .ok_or_else(|| "invalid_request: computer payload must be an object".to_string())?;
    if object.len() != expected.len() || object.keys().any(|key| !expected.contains(&key.as_str()))
    {
        return Err("invalid_request: computer payload contains unsupported fields".to_string());
    }
    Ok(())
}

fn optional_snapshot_region(payload: &Value) -> Result<Option<SnapshotRegion>, String> {
    match payload.get("region") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| "invalid_request: snapshot region is invalid".to_string()),
    }
}

fn optional_snapshot_dimension(payload: &Value, field: &str) -> Result<Option<u32>, String> {
    match payload.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("invalid_request: snapshot {field} is invalid")),
    }
}

pub(crate) fn handle_computer_operation(operation: &RunnerComputerOperation) -> CommandResult {
    let start = Instant::now();
    let payload = match serde_json::from_str::<Value>(&operation.payload) {
        Ok(value) => value,
        Err(_) => {
            return err_cmd(
                start,
                "invalid_request: computer payload is not valid JSON".to_string(),
            )
        }
    };
    let result = match operation.kind {
        RunnerComputerOperationKind::ListWindows => {
            let limit = payload
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(MAX_WINDOWS)
                .clamp(1, MAX_WINDOWS);
            computer_runtime().list_windows(limit)
        }
        RunnerComputerOperationKind::ListDisplays => {
            ensure_exact_payload_fields(&payload, &["limit"])
                .and_then(|()| {
                    payload
                        .get("limit")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .filter(|limit| (1..=MAX_DISPLAYS).contains(limit))
                        .ok_or_else(|| {
                            "invalid_request: display discovery limit is invalid".to_string()
                        })
                })
                .and_then(|limit| computer_runtime().list_displays(limit))
        }
        RunnerComputerOperationKind::ListApplications => {
            ensure_exact_payload_fields(&payload, &["limit"])
                .and_then(|()| {
                    payload
                        .get("limit")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .filter(|limit| (1..=MAX_APPLICATIONS).contains(limit))
                        .ok_or_else(|| {
                            "invalid_request: application discovery limit is invalid".to_string()
                        })
                })
                .and_then(|limit| computer_runtime().list_applications(limit))
        }
        RunnerComputerOperationKind::LaunchApplication => {
            ensure_exact_payload_fields(&payload, &["application_id"])
                .and_then(|()| {
                    payload
                        .get("application_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: application_id is required".to_string())
                })
                .and_then(|application_id| computer_runtime().launch_application(application_id))
        }
        RunnerComputerOperationKind::AccessibilityStatus => {
            computer_runtime().accessibility_status()
        }
        RunnerComputerOperationKind::AccessibilityTree => {
            let surface_id = payload
                .get("surface_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "invalid_request: surface_id is required".to_string());
            let max_depth = payload
                .get("max_depth")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_ACCESSIBILITY_DEPTH);
            let max_nodes = payload
                .get("max_nodes")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_ACCESSIBILITY_NODES);
            surface_id.and_then(|surface_id| {
                computer_runtime().accessibility_tree(surface_id, max_depth, max_nodes)
            })
        }
        RunnerComputerOperationKind::ElementState => {
            ensure_exact_payload_fields(&payload, &["surface_id", "element_id"]).and_then(|()| {
                let surface_id = payload
                    .get("surface_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "invalid_request: surface_id is required".to_string())?;
                let element_id = payload
                    .get("element_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "invalid_request: element_id is required".to_string())?;
                computer_runtime().element_state(surface_id, element_id)
            })
        }
        RunnerComputerOperationKind::ActivateWindow => {
            ensure_exact_payload_fields(&payload, &["surface_id"])
                .and_then(|()| {
                    payload
                        .get("surface_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: surface_id is required".to_string())
                })
                .and_then(|surface_id| computer_runtime().activate_window(surface_id))
        }
        RunnerComputerOperationKind::Control => {
            ensure_exact_payload_fields(&payload, &["surface_id", "element_id", "action"]).and_then(
                |()| {
                    let surface_id = payload
                        .get("surface_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: surface_id is required".to_string());
                    let element_id = payload
                        .get("element_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: element_id is required".to_string());
                    let action = payload
                        .get("action")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: action is required".to_string())
                        .and_then(ComputerAction::parse);
                    surface_id.and_then(|surface_id| {
                        element_id.and_then(|element_id| {
                            action.and_then(|action| {
                                computer_runtime().control(surface_id, element_id, action)
                            })
                        })
                    })
                },
            )
        }
        RunnerComputerOperationKind::ScrollToElement => {
            ensure_exact_payload_fields(&payload, &["surface_id", "element_id"]).and_then(|()| {
                let surface_id = payload
                    .get("surface_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "invalid_request: surface_id is required".to_string())?;
                let element_id = payload
                    .get("element_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "invalid_request: element_id is required".to_string())?;
                computer_runtime().scroll_to_element(surface_id, element_id)
            })
        }
        RunnerComputerOperationKind::ReadClipboard => ensure_exact_payload_fields(&payload, &[])
            .and_then(|()| computer_runtime().read_clipboard()),
        RunnerComputerOperationKind::WriteClipboard => {
            ensure_exact_payload_fields(&payload, &["text"])
                .and_then(|()| {
                    payload
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: clipboard text is required".to_string())
                })
                .and_then(|text| computer_runtime().write_clipboard(text))
        }
        RunnerComputerOperationKind::KeyInput => {
            ensure_exact_payload_fields(&payload, &["surface_id", "key", "modifiers"]).and_then(
                |()| {
                    let surface_id = payload
                        .get("surface_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: surface_id is required".to_string())?;
                    let key = payload
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: key is required".to_string())?;
                    let modifier_values = payload
                        .get("modifiers")
                        .and_then(Value::as_array)
                        .ok_or_else(|| "invalid_request: modifiers must be an array".to_string())?;
                    let modifiers = modifier_values
                        .iter()
                        .map(|value| {
                            value.as_str().map(str::to_string).ok_or_else(|| {
                                "invalid_request: each modifier must be a string".to_string()
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    computer_runtime().key_input(surface_id, key, &modifiers)
                },
            )
        }
        RunnerComputerOperationKind::PointerMove | RunnerComputerOperationKind::PointerClick => {
            ensure_exact_payload_fields(&payload, &["display_id", "snapshot_generation", "x", "y"])
                .and_then(|()| {
                    let display_id = payload
                        .get("display_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: display_id is required".to_string())?;
                    let snapshot_generation = payload
                        .get("snapshot_generation")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .filter(|value| *value > 0)
                        .ok_or_else(|| {
                            "invalid_request: snapshot_generation must be a positive u32"
                                .to_string()
                        })?;
                    let x = payload
                        .get("x")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| "invalid_request: x must be a u32".to_string())?;
                    let y = payload
                        .get("y")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| "invalid_request: y must be a u32".to_string())?;
                    let action = match operation.kind {
                        RunnerComputerOperationKind::PointerMove => PointerAction::Move,
                        RunnerComputerOperationKind::PointerClick => PointerAction::Click,
                        _ => unreachable!("pointer branch is typed"),
                    };
                    computer_runtime().pointer_effect(action, display_id, snapshot_generation, x, y)
                })
        }
        RunnerComputerOperationKind::InputText => {
            ensure_exact_payload_fields(&payload, &["surface_id", "element_id", "text"]).and_then(
                |()| {
                    let surface_id = payload
                        .get("surface_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: surface_id is required".to_string());
                    let element_id = payload
                        .get("element_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: element_id is required".to_string());
                    let text = payload
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: text is required".to_string());
                    surface_id.and_then(|surface_id| {
                        element_id.and_then(|element_id| {
                            text.and_then(|text| {
                                computer_runtime().input_text(surface_id, element_id, text)
                            })
                        })
                    })
                },
            )
        }
        RunnerComputerOperationKind::SnapshotDisplay => {
            ensure_exact_payload_fields(&payload, &["display_id", "max_width", "max_height"])
                .and_then(|()| {
                    let display_id = payload
                        .get("display_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: display_id is required".to_string())?;
                    let max_width = optional_snapshot_dimension(&payload, "max_width")?;
                    let max_height = optional_snapshot_dimension(&payload, "max_height")?;
                    computer_runtime().snapshot_display(display_id, max_width, max_height)
                })
        }
        RunnerComputerOperationKind::Snapshot => {
            ensure_exact_payload_fields(&payload, &["surface_id"])
                .and_then(|()| {
                    payload
                        .get("surface_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "invalid_request: surface_id is required".to_string())
                })
                .and_then(|surface_id| computer_runtime().snapshot(surface_id, None, None, None))
        }
        RunnerComputerOperationKind::SnapshotRegion => ensure_exact_payload_fields(
            &payload,
            &["surface_id", "region", "max_width", "max_height"],
        )
        .and_then(|()| {
            let surface_id = payload
                .get("surface_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "invalid_request: surface_id is required".to_string())?;
            let region = optional_snapshot_region(&payload)?;
            let max_width = optional_snapshot_dimension(&payload, "max_width")?;
            let max_height = optional_snapshot_dimension(&payload, "max_height")?;
            if region.is_none() && max_width.is_none() && max_height.is_none() {
                return Err(
                    "invalid_request: region snapshot requires a region or output dimension bound"
                        .to_string(),
                );
            }
            computer_runtime().snapshot(surface_id, region, max_width, max_height)
        }),
    };
    match result {
        Ok(result) => ok_cmd(start, result),
        Err(error) => err_cmd(start, error),
    }
}

#[cfg(test)]
pub(crate) fn handle_computer_request(request: &RunnerRequest) -> CommandResult {
    let start = Instant::now();
    if !request.command.is_empty()
        || request.cwd.is_some()
        || request.path.is_some()
        || request.content.is_some()
        || request.process.is_some()
        || request.script.is_some()
        || request.job_id.is_some()
        || request.lsp.is_some()
        || request.job_context.is_some()
        || request.persistent_shell.is_some()
    {
        return err_cmd(
            start,
            "invalid_request: computer request contains unrelated execution fields".to_string(),
        );
    }
    let Some(raw) = request.stdin.as_deref() else {
        return err_cmd(
            start,
            "invalid_request: computer payload is required and bounded".to_string(),
        );
    };
    if raw.contains('\0') || serde_json::from_str::<Value>(raw).is_err() {
        return err_cmd(
            start,
            "invalid_request: computer payload is not valid JSON".to_string(),
        );
    }
    match request.decode_operation() {
        Ok(RunnerOperation::Computer(operation)) => handle_computer_operation(&operation),
        _ => err_cmd(
            start,
            "invalid_request: unsupported computer request kind".to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "computer_tests.rs"]
mod tests;
