//! Runner-configured live Skill root protocol.
//!
//! This contract deliberately carries no native filesystem path. Control may
//! ask one exact Runner to list its configured live Skills or read a resource
//! by opaque Skill identity; the Runner alone resolves that identity against
//! its current trusted `[skills].roots` configuration.

use crate::runtime_contract::{MAX_SKILL_READ_LINES, MAX_SKILL_RESOURCE_PATH_CHARS};
use crate::skill_metadata::{
    MAX_SKILL_DEFINITION_BYTES, MAX_SKILL_DESCRIPTION_CHARS, MAX_SKILL_NAME_CHARS,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

pub const CONFIGURED_SKILL_ROOTS_REQUEST_KIND: &str = "configured_skill_roots";
pub const CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT: &str = "webcodex.runner_configured_skills.v1";
pub const CONFIGURED_SKILL_ROOTS_REQUEST_MAX_BYTES: usize = 4 * 1024;
pub const CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES: usize = 256 * 1024;
pub const MAX_CONFIGURED_SKILL_PACKAGES: usize = 256;
pub const MAX_CONFIGURED_SKILL_DIAGNOSTICS: usize = 8;
pub const MAX_CONFIGURED_SKILL_ROOT_SCAN_ENTRIES: usize = 1024;
pub const MAX_CONFIGURED_SKILL_RESOURCE_FILE_BYTES: usize = 512 * 1024;
pub const MAX_CONFIGURED_SKILL_READ_TEXT_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfiguredSkillRootsRequest {
    List,
    Read {
        skill_id: String,
        path: String,
        start_line: usize,
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_definition_revision: Option<String>,
    },
}

impl ConfiguredSkillRootsRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::List => Ok(()),
            Self::Read {
                skill_id,
                path,
                start_line,
                limit,
                expected_definition_revision,
            } => {
                if !valid_configured_skill_id(skill_id) {
                    return Err("invalid configured Skill id");
                }
                normalize_configured_skill_resource_path(path)?;
                if *start_line == 0 || !(1..=MAX_SKILL_READ_LINES).contains(limit) {
                    return Err("invalid configured Skill read range");
                }
                if expected_definition_revision
                    .as_deref()
                    .is_some_and(|revision| !valid_lower_sha256(revision))
                {
                    return Err("invalid configured Skill definition revision");
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredSkillDescriptor {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub definition_revision: String,
}

impl ConfiguredSkillDescriptor {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_configured_skill_id(&self.skill_id)
            || !valid_lower_sha256(&self.definition_revision)
            || self.name.is_empty()
            || self.name.chars().count() > MAX_SKILL_NAME_CHARS
            || self.description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS
        {
            return Err("invalid configured Skill descriptor");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredSkillRootsListResponse {
    pub format: String,
    pub skills: Vec<ConfiguredSkillDescriptor>,
    pub invalid_count: usize,
    pub diagnostics: Vec<String>,
    pub discovery_truncated: bool,
}

impl ConfiguredSkillRootsListResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT
            || self.skills.len() > MAX_CONFIGURED_SKILL_PACKAGES
            || self.diagnostics.len() > MAX_CONFIGURED_SKILL_DIAGNOSTICS
            || self
                .diagnostics
                .iter()
                .any(|reason| !valid_diagnostic_reason(reason))
        {
            return Err("invalid configured Skill list response");
        }
        let mut seen = std::collections::BTreeSet::new();
        for skill in &self.skills {
            skill.validate()?;
            if !seen.insert(skill.skill_id.as_str()) {
                return Err("duplicate configured Skill id");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredSkillRootsReadResponse {
    pub format: String,
    pub skill_id: String,
    pub name: String,
    pub definition_revision: String,
    pub path: String,
    pub sha256: String,
    pub file_bytes: usize,
    pub total_lines: usize,
    pub text: String,
    pub start_line: usize,
    pub end_line: Option<usize>,
    pub returned_lines: usize,
    pub has_more: bool,
    pub next_start_line: Option<usize>,
}

impl ConfiguredSkillRootsReadResponse {
    pub fn validate_for_request(
        &self,
        request_skill_id: &str,
        request_path: &str,
        request_start_line: usize,
        request_limit: usize,
    ) -> Result<(), &'static str> {
        let normalized_path = normalize_configured_skill_resource_path(request_path)?;
        let max_file_bytes = if normalized_path == "SKILL.md" {
            MAX_SKILL_DEFINITION_BYTES
        } else {
            MAX_CONFIGURED_SKILL_RESOURCE_FILE_BYTES
        };
        if self.format != CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT
            || self.skill_id != request_skill_id
            || self.path != normalized_path
            || !valid_configured_skill_id(&self.skill_id)
            || self.name.is_empty()
            || self.name.chars().count() > MAX_SKILL_NAME_CHARS
            || !valid_lower_sha256(&self.definition_revision)
            || !valid_lower_sha256(&self.sha256)
            || self.file_bytes > max_file_bytes
            || self.text.len() > MAX_CONFIGURED_SKILL_READ_TEXT_BYTES
            || self.start_line != request_start_line
            || self.returned_lines > request_limit
            || self.has_more != self.next_start_line.is_some()
        {
            return Err("invalid configured Skill read response");
        }
        Ok(())
    }
}

pub fn valid_configured_skill_id(value: &str) -> bool {
    value.len() == "wc_skill_".len() + 32
        && value.starts_with("wc_skill_")
        && value["wc_skill_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn valid_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn normalize_configured_skill_resource_path(path: &str) -> Result<String, &'static str> {
    let trimmed = path.trim();
    #[cfg(windows)]
    if trimmed.contains(':') {
        // A colon beyond a drive prefix is an NTFS alternate data stream
        // selector. Configured Skill resources are ordinary package files; do
        // not let an opaque resource path reach directory-invisible streams.
        return Err("invalid configured Skill resource path");
    }
    if trimmed.is_empty()
        || trimmed.chars().count() > MAX_SKILL_RESOURCE_PATH_CHARS
        || trimmed.chars().any(char::is_control)
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || trimmed.as_bytes().get(1) == Some(&b':')
    {
        return Err("invalid configured Skill resource path");
    }
    let normalized = trimmed.replace('\\', "/");
    if normalized
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
        || Path::new(&normalized)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("invalid configured Skill resource path");
    }
    Ok(normalized)
}

fn valid_diagnostic_reason(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_response_uses_definition_file_size_bound() {
        let response = ConfiguredSkillRootsReadResponse {
            format: CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT.to_string(),
            skill_id: format!("wc_skill_{}", "a".repeat(32)),
            name: "demo".to_string(),
            definition_revision: "b".repeat(64),
            path: "SKILL.md".to_string(),
            sha256: "c".repeat(64),
            file_bytes: MAX_SKILL_DEFINITION_BYTES + 1,
            total_lines: 1,
            text: "x".to_string(),
            start_line: 1,
            end_line: Some(1),
            returned_lines: 1,
            has_more: false,
            next_start_line: None,
        };
        assert!(response
            .validate_for_request(&response.skill_id, "SKILL.md", 1, 1)
            .is_err());
    }

    #[test]
    fn resource_paths_are_relative_and_normalized() {
        assert_eq!(
            normalize_configured_skill_resource_path("references\\guide.md").unwrap(),
            "references/guide.md"
        );
        for invalid in [
            "../secret",
            "references/../secret",
            "/etc/passwd",
            "C:\\secret",
        ] {
            assert!(normalize_configured_skill_resource_path(invalid).is_err());
        }
        #[cfg(windows)]
        assert!(normalize_configured_skill_resource_path("references/guide.md:secret").is_err());
        #[cfg(not(windows))]
        assert_eq!(
            normalize_configured_skill_resource_path("references/guide.md:stream").unwrap(),
            "references/guide.md:stream"
        );
    }
}
