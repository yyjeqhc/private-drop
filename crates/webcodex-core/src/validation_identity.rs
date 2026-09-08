//! Stable validation identity parsing used across audit and Workflow Session layers.

use crate::runner_protocol::{
    normalize_cargo_value, normalize_go_test_packages, normalize_rust_test_filter,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const STRUCTURED_VALIDATION_TARGET_PREFIX: &str = "target:";
pub const VALIDATION_IDENTITY_HEX_LEN: usize = 24;
pub const GENERIC_VALIDATION_IDENTITY_PREFIX: &str = "command:";
const ASSERTION_VALIDATION_IDENTITY_PREFIX: &str = "assertion:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolValidationIdentityKind {
    None,
    CargoFmt,
    CargoCheck,
    CargoTest,
    GoTest,
}

impl ToolValidationIdentityKind {
    pub const fn tool_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::CargoFmt => Some("cargo_fmt"),
            Self::CargoCheck => Some("cargo_check"),
            Self::CargoTest => Some("cargo_test"),
            Self::GoTest => Some("go_test"),
        }
    }
}

pub fn is_structured_validation_target_identity(value: &str) -> bool {
    let Some(hex) = value.strip_prefix(STRUCTURED_VALIDATION_TARGET_PREFIX) else {
        return false;
    };
    hex.len() == VALIDATION_IDENTITY_HEX_LEN && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn is_validation_execution_identity(value: &str) -> bool {
    if is_structured_validation_target_identity(value) {
        return true;
    }
    [
        GENERIC_VALIDATION_IDENTITY_PREFIX,
        ASSERTION_VALIDATION_IDENTITY_PREFIX,
    ]
    .into_iter()
    .any(|prefix| {
        value.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() == VALIDATION_IDENTITY_HEX_LEN
                && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    })
}

pub fn assertion_validation_identity(assertion_name: &str) -> String {
    let assertion_name = assertion_name.trim();
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-validation-assertion-v1\0");
    hasher.update((assertion_name.len() as u64).to_le_bytes());
    hasher.update(assertion_name.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!(
        "{ASSERTION_VALIDATION_IDENTITY_PREFIX}{}",
        &digest[..VALIDATION_IDENTITY_HEX_LEN]
    )
}

pub fn structured_validation_target_identity(tool_name: &str, arguments: &Value) -> Option<String> {
    let obj = arguments.as_object()?;
    let cwd = normalized_validation_target_cwd(obj.get("cwd"))?;
    let semantic = match tool_name {
        "cargo_fmt" => serde_json::json!({
            "tool": tool_name,
            "kind": "format",
            "cwd": cwd,
            "check": obj.get("check").and_then(Value::as_bool).unwrap_or(false),
        }),
        "cargo_check" => {
            if obj.get("features_present").and_then(Value::as_bool) == Some(true)
                && obj.get("features").is_none()
            {
                return None;
            }
            let features = normalized_cargo_target_value(obj.get("features"))?;
            let package = normalized_cargo_target_value(obj.get("package"))?;
            serde_json::json!({
                "tool": tool_name,
                "kind": "check",
                "cwd": cwd,
                "package": package,
                "features": features,
                "all_targets": obj.get("all_targets").and_then(Value::as_bool).unwrap_or(true),
                "all_features": obj.get("all_features").and_then(Value::as_bool).unwrap_or(false),
                "no_default_features": obj.get("no_default_features").and_then(Value::as_bool).unwrap_or(false),
            })
        }
        "cargo_test" => {
            if obj.get("filter_present").and_then(Value::as_bool) == Some(true)
                && obj.get("filter").is_none()
            {
                return None;
            }
            if obj.get("features_present").and_then(Value::as_bool) == Some(true)
                && obj.get("features").is_none()
            {
                return None;
            }
            let filter = normalized_rust_test_target_filter(obj.get("filter"))?;
            let features = normalized_cargo_target_value(obj.get("features"))?;
            let package = normalized_cargo_target_value(obj.get("package"))?;
            serde_json::json!({
                "tool": tool_name,
                "kind": "test",
                "cwd": cwd,
                "package": package,
                "filter": filter,
                "features": features,
                "all_targets": obj.get("all_targets").and_then(Value::as_bool).unwrap_or(false),
                "all_features": obj.get("all_features").and_then(Value::as_bool).unwrap_or(false),
                "no_default_features": obj.get("no_default_features").and_then(Value::as_bool).unwrap_or(false),
                "no_run": obj.get("no_run").and_then(Value::as_bool).unwrap_or(false),
            })
        }
        "go_test" => {
            if obj.get("packages_present").and_then(Value::as_bool) == Some(true)
                && obj.get("packages").is_none()
            {
                return None;
            }
            let packages = normalized_go_test_target_packages(obj.get("packages"))?;
            serde_json::json!({
                "tool": tool_name,
                "kind": "test",
                "cwd": cwd,
                "packages": packages,
            })
        }
        _ => return None,
    };
    let encoded = serde_json::to_vec(&semantic).ok()?;
    let digest = format!("{:x}", Sha256::digest(encoded));
    Some(format!(
        "{STRUCTURED_VALIDATION_TARGET_PREFIX}{}",
        &digest[..VALIDATION_IDENTITY_HEX_LEN]
    ))
}

fn normalized_validation_target_cwd(value: Option<&Value>) -> Option<String> {
    let Some(value) = value else {
        return Some(".".to_string());
    };
    if value.is_null() {
        return Some(".".to_string());
    }
    let raw = value.as_str()?;
    let trimmed = raw.trim().trim_start_matches("./").trim_end_matches('/');
    Some(if trimmed.is_empty() || trimmed == "." {
        ".".to_string()
    } else {
        trimmed.to_string()
    })
}

fn normalized_cargo_target_value(value: Option<&Value>) -> Option<Option<String>> {
    let Some(value) = value else {
        return Some(None);
    };
    if value.is_null() {
        return Some(None);
    }
    normalize_cargo_value(value.as_str()?).ok()
}

fn normalized_rust_test_target_filter(value: Option<&Value>) -> Option<Option<String>> {
    let Some(value) = value else {
        return Some(None);
    };
    if value.is_null() {
        return Some(None);
    }
    normalize_rust_test_filter(value.as_str()?).ok()
}

fn normalized_go_test_target_packages(value: Option<&Value>) -> Option<Vec<String>> {
    let packages = match value {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        ),
        Some(_) => return None,
    };
    normalize_go_test_packages(packages.as_deref()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_identity_shapes_are_stable() {
        let assertion = assertion_validation_identity("cargo check");
        assert!(assertion.starts_with("assertion:"));
        assert!(is_validation_execution_identity(&assertion));
        assert!(is_structured_validation_target_identity(
            "target:0123456789abcdef01234567"
        ));
        assert!(!is_validation_execution_identity("target:short"));
    }
}
