use super::config::{configured_skill_root_identity, SkillsConfig};
use super::CommandResult;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use webcodex_core::configured_skills::{
    normalize_configured_skill_resource_path, ConfiguredSkillDescriptor,
    ConfiguredSkillRootsListResponse, ConfiguredSkillRootsReadResponse,
    ConfiguredSkillRootsRequest, CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT,
    CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES, MAX_CONFIGURED_SKILL_DIAGNOSTICS,
    MAX_CONFIGURED_SKILL_PACKAGES, MAX_CONFIGURED_SKILL_READ_TEXT_BYTES,
    MAX_CONFIGURED_SKILL_RESOURCE_FILE_BYTES, MAX_CONFIGURED_SKILL_ROOT_SCAN_ENTRIES,
};
use webcodex_core::skill_metadata::{parse_skill_metadata, MAX_SKILL_DEFINITION_BYTES};
use webcodex_workspace::file_read_range;

const SKILL_DEFINITION_FILE: &str = "SKILL.md";
const MAX_SKILL_PACKAGE_NAME_BYTES: usize = 160;

#[derive(Debug)]
struct LiveSkill {
    descriptor: ConfiguredSkillDescriptor,
    package_root: PathBuf,
}

#[derive(Debug, Default)]
struct LiveDiscovery {
    skills: Vec<LiveSkill>,
    invalid_count: usize,
    diagnostics: Vec<String>,
    discovery_truncated: bool,
}

pub(crate) fn handle_configured_skill_roots_request(
    config: &SkillsConfig,
    request: ConfiguredSkillRootsRequest,
) -> CommandResult {
    let start = Instant::now();
    let result = match request {
        ConfiguredSkillRootsRequest::List => discover(config).and_then(|discovery| {
            let mut response = ConfiguredSkillRootsListResponse {
                format: CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT.to_string(),
                skills: discovery
                    .skills
                    .into_iter()
                    .map(|skill| skill.descriptor)
                    .collect(),
                invalid_count: discovery.invalid_count,
                diagnostics: discovery.diagnostics,
                discovery_truncated: discovery.discovery_truncated,
            };
            serialize_list_bounded(&mut response)
        }),
        ConfiguredSkillRootsRequest::Read {
            skill_id,
            path,
            start_line,
            limit,
            expected_definition_revision,
        } => read_resource(
            config,
            &skill_id,
            &path,
            start_line,
            limit,
            expected_definition_revision.as_deref(),
        )
        .and_then(|response| serialize_bounded(&response)),
    };
    match result {
        Ok(stdout) => CommandResult {
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(code) => CommandResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: Some(code),
        },
    }
}

fn discover(config: &SkillsConfig) -> Result<LiveDiscovery, String> {
    let mut discovery = LiveDiscovery::default();
    let mut seen_ids = BTreeSet::new();
    for configured_root in &config.roots {
        if discovery.skills.len() >= MAX_CONFIGURED_SKILL_PACKAGES {
            discovery.discovery_truncated = true;
            break;
        }
        let root_metadata = match fs::symlink_metadata(configured_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                push_diagnostic(
                    &mut discovery.diagnostics,
                    "configured_skill_root_not_found",
                );
                continue;
            }
            Err(_) => {
                push_diagnostic(
                    &mut discovery.diagnostics,
                    "configured_skill_root_unavailable",
                );
                continue;
            }
        };
        if metadata_is_link_like(&root_metadata) {
            push_diagnostic(
                &mut discovery.diagnostics,
                "configured_skill_root_link_not_allowed",
            );
            continue;
        }
        if !root_metadata.is_dir() {
            push_diagnostic(
                &mut discovery.diagnostics,
                "configured_skill_root_not_directory",
            );
            continue;
        }
        let root = match configured_root.canonicalize() {
            Ok(root) if root.is_dir() => root,
            _ => {
                push_diagnostic(
                    &mut discovery.diagnostics,
                    "configured_skill_root_unavailable",
                );
                continue;
            }
        };
        let entries = match bounded_root_entries(&root) {
            Ok(entries) => entries,
            Err(code) => {
                if code == "configured_skill_root_scan_limit_exceeded" {
                    discovery.discovery_truncated = true;
                }
                push_diagnostic(&mut discovery.diagnostics, code);
                continue;
            }
        };
        for package_name in entries {
            if discovery.skills.len() >= MAX_CONFIGURED_SKILL_PACKAGES {
                discovery.discovery_truncated = true;
                break;
            }
            match load_live_skill(configured_root, &root, &package_name) {
                Ok(skill) => {
                    if !seen_ids.insert(skill.descriptor.skill_id.clone()) {
                        return Err("configured_skill_identity_collision".to_string());
                    }
                    discovery.skills.push(skill);
                }
                Err(code) => {
                    discovery.invalid_count = discovery.invalid_count.saturating_add(1);
                    push_diagnostic(&mut discovery.diagnostics, code);
                }
            }
        }
    }
    discovery
        .skills
        .sort_by(|left, right| left.descriptor.skill_id.cmp(&right.descriptor.skill_id));
    Ok(discovery)
}

fn bounded_root_entries(root: &Path) -> Result<Vec<String>, &'static str> {
    let read_dir = fs::read_dir(root).map_err(|_| "configured_skill_root_unavailable")?;
    let mut candidates = Vec::new();
    let mut scanned = 0usize;
    for entry in read_dir {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_CONFIGURED_SKILL_ROOT_SCAN_ENTRIES {
            return Err("configured_skill_root_scan_limit_exceeded");
        }
        let entry = entry.map_err(|_| "configured_skill_root_unavailable")?;
        let file_type = entry
            .file_type()
            .map_err(|_| "configured_skill_root_unavailable")?;
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            candidates.push(String::new());
            continue;
        };
        candidates.push(name);
    }
    candidates.sort();
    Ok(candidates)
}

fn load_live_skill(
    configured_root: &Path,
    canonical_root: &Path,
    package_name: &str,
) -> Result<LiveSkill, &'static str> {
    if !valid_package_name(package_name) {
        return Err("invalid_skill_package");
    }
    if webcodex_core::sensitive_paths::is_secret_path(package_name) {
        return Err("sensitive_skill_definition");
    }
    let package_path = canonical_root.join(package_name);
    let metadata = fs::symlink_metadata(&package_path).map_err(|_| "invalid_skill_package")?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err("invalid_skill_package");
    }
    let package_root = package_path
        .canonicalize()
        .map_err(|_| "invalid_skill_package")?;
    if !crate::runner_config::paths::path_is_within(&package_root, canonical_root) {
        return Err("invalid_skill_package");
    }
    let definition = resolve_regular_package_file(
        &package_root,
        canonical_root,
        SKILL_DEFINITION_FILE,
        "missing_skill_definition",
        "invalid_skill_package",
    )?;
    let bytes =
        read_bounded(&definition, MAX_SKILL_DEFINITION_BYTES).map_err(|code| match code {
            "too_large" => "skill_definition_too_large",
            "invalid_utf8" => "invalid_utf8_skill_definition",
            _ => "invalid_skill_package",
        })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "invalid_utf8_skill_definition")?;
    let skill_metadata = parse_skill_metadata(text)?;
    let definition_revision = sha256_hex(&bytes);
    let skill_id = configured_skill_id(configured_root, package_name);
    Ok(LiveSkill {
        descriptor: ConfiguredSkillDescriptor {
            skill_id,
            name: skill_metadata.name,
            description: skill_metadata.description,
            definition_revision,
        },
        package_root,
    })
}

fn read_resource(
    config: &SkillsConfig,
    skill_id: &str,
    requested_path: &str,
    start_line: usize,
    limit: usize,
    expected_definition_revision: Option<&str>,
) -> Result<ConfiguredSkillRootsReadResponse, String> {
    let path = normalize_configured_skill_resource_path(requested_path)
        .map_err(|_| "skill_resource_path_invalid".to_string())?;
    if webcodex_core::sensitive_paths::is_secret_path(&path) {
        return Err("skill_sensitive_path".to_string());
    }
    let discovery = discover(config)?;
    let skill = discovery
        .skills
        .into_iter()
        .find(|skill| skill.descriptor.skill_id == skill_id)
        .ok_or_else(|| "skill_not_found".to_string())?;
    if expected_definition_revision
        .is_some_and(|expected| expected != skill.descriptor.definition_revision)
    {
        return Err("skill_definition_changed".to_string());
    }
    let canonical_root = skill
        .package_root
        .parent()
        .ok_or_else(|| "skill_resource_path_invalid".to_string())?;
    let target = resolve_regular_package_file(
        &skill.package_root,
        canonical_root,
        &path,
        "skill_resource_not_found",
        "skill_resource_path_invalid",
    )
    .map_err(str::to_string)?;
    let relative = target
        .strip_prefix(&skill.package_root)
        .map_err(|_| "skill_resource_path_invalid".to_string())?
        .to_string_lossy();
    if webcodex_core::sensitive_paths::is_secret_path(relative.as_ref()) {
        return Err("skill_sensitive_path".to_string());
    }
    let max_file_bytes = if path == SKILL_DEFINITION_FILE {
        MAX_SKILL_DEFINITION_BYTES
    } else {
        MAX_CONFIGURED_SKILL_RESOURCE_FILE_BYTES
    };
    let file_bytes = target
        .metadata()
        .map_err(|_| "skill_resource_unavailable".to_string())?
        .len();
    if file_bytes > max_file_bytes as u64 {
        return Err("skill_resource_too_large".to_string());
    }
    let range = file_read_range::EffectiveRange::new(Some(start_line), Some(limit));
    let read = file_read_range::read_range_with_budget(
        &target,
        range,
        MAX_CONFIGURED_SKILL_READ_TEXT_BYTES,
    )
    .map_err(|error| match error.reason {
        file_read_range::ReadFileReason::InvalidUtf8 => {
            "skill_resource_unsupported_encoding".to_string()
        }
        file_read_range::ReadFileReason::NotFound => "skill_resource_not_found".to_string(),
        file_read_range::ReadFileReason::RangeTooLarge => "skill_read_result_too_large".to_string(),
        _ => "skill_resource_unavailable".to_string(),
    })?;
    let definition_after = resolve_regular_package_file(
        &skill.package_root,
        canonical_root,
        SKILL_DEFINITION_FILE,
        "skill_definition_changed",
        "skill_definition_changed",
    )
    .map_err(str::to_string)?;
    let definition_after = read_bounded(&definition_after, MAX_SKILL_DEFINITION_BYTES)
        .map_err(|_| "skill_definition_changed".to_string())?;
    if sha256_hex(&definition_after) != skill.descriptor.definition_revision
        || (path == SKILL_DEFINITION_FILE && read.sha256 != skill.descriptor.definition_revision)
    {
        return Err("skill_definition_changed".to_string());
    }
    let response = ConfiguredSkillRootsReadResponse {
        format: CONFIGURED_SKILL_ROOTS_RESPONSE_FORMAT.to_string(),
        skill_id: skill.descriptor.skill_id,
        name: skill.descriptor.name,
        definition_revision: skill.descriptor.definition_revision,
        path: path.clone(),
        sha256: read.sha256,
        file_bytes: file_bytes.min(usize::MAX as u64) as usize,
        total_lines: read.total_lines,
        text: read.content,
        start_line: read.start_line,
        end_line: read.end_line,
        returned_lines: read.returned_lines,
        has_more: read.has_more,
        next_start_line: read.next_start_line,
    };
    response
        .validate_for_request(skill_id, &path, start_line, limit)
        .map_err(|_| "configured_skill_response_invalid".to_string())?;
    Ok(response)
}

fn resolve_regular_package_file(
    package_root: &Path,
    canonical_root: &Path,
    relative: &str,
    missing_code: &'static str,
    invalid_code: &'static str,
) -> Result<PathBuf, &'static str> {
    let mut current = package_root.to_path_buf();
    let components = relative.split('/').collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                missing_code
            } else {
                invalid_code
            }
        })?;
        let last = index + 1 == components.len();
        if metadata_is_link_like(&metadata)
            || (last && !metadata.is_file())
            || (!last && !metadata.is_dir())
        {
            return Err(invalid_code);
        }
    }
    let target = current.canonicalize().map_err(|_| invalid_code)?;
    if !crate::runner_config::paths::path_is_within(&target, package_root)
        || !crate::runner_config::paths::path_is_within(&target, canonical_root)
    {
        return Err(invalid_code);
    }
    Ok(target)
}

fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn read_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, &'static str> {
    let file = File::open(path).map_err(|_| "unavailable")?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "unavailable")?;
    if bytes.len() > max_bytes {
        return Err("too_large");
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Err("invalid_utf8");
    }
    Ok(bytes)
}

fn configured_skill_id(root: &Path, package_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex.configured-skill-id.v1\0");
    hasher.update(configured_skill_root_identity(root).as_bytes());
    hasher.update(b"\0");
    hasher.update(package_name.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("wc_skill_{}", &digest[..32])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SKILL_PACKAGE_NAME_BYTES
        && !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && !name.chars().any(char::is_control)
}

fn push_diagnostic(diagnostics: &mut Vec<String>, code: &str) {
    if diagnostics.len() < MAX_CONFIGURED_SKILL_DIAGNOSTICS {
        diagnostics.push(code.to_string());
    }
}

fn serialize_list_bounded(
    response: &mut ConfiguredSkillRootsListResponse,
) -> Result<String, String> {
    loop {
        response
            .validate()
            .map_err(|_| "configured_skill_response_invalid".to_string())?;
        let output = serde_json::to_string(response)
            .map_err(|_| "configured_skill_response_invalid".to_string())?;
        if output.len() <= CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES {
            return Ok(output);
        }
        if response.skills.pop().is_none() {
            return Err("configured_skill_response_too_large".to_string());
        }
        response.discovery_truncated = true;
    }
}

fn serialize_bounded<T: serde::Serialize>(value: &T) -> Result<String, String> {
    let output = serde_json::to_string(value)
        .map_err(|_| "configured_skill_response_invalid".to_string())?;
    if output.len() > CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES {
        return Err("configured_skill_response_too_large".to_string());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, package: &str, name: &str, body: &str) {
        let package = root.join(package);
        fs::create_dir_all(package.join("references")).unwrap();
        fs::write(
            package.join(SKILL_DEFINITION_FILE),
            format!("---\nname: {name}\ndescription: configured test skill\n---\n{body}"),
        )
        .unwrap();
        fs::write(package.join("references/guide.md"), "line one\nline two\n").unwrap();
    }

    #[test]
    fn empty_roots_preserve_empty_source() {
        let result = handle_configured_skill_roots_request(
            &SkillsConfig::default(),
            ConfiguredSkillRootsRequest::List,
        );
        let response: ConfiguredSkillRootsListResponse =
            serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
        assert!(response.skills.is_empty());
        assert!(response.diagnostics.is_empty());
    }

    #[test]
    fn live_discovery_and_resource_read_observe_changes() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "demo", "demo", "version one");
        let config = SkillsConfig {
            roots: vec![temp.path().to_path_buf()],
        };
        let first = discover(&config).unwrap();
        assert_eq!(first.skills.len(), 1);
        let id = first.skills[0].descriptor.skill_id.clone();
        let revision = first.skills[0].descriptor.definition_revision.clone();
        let listed =
            handle_configured_skill_roots_request(&config, ConfiguredSkillRootsRequest::List);
        let listed_text = listed.stdout.as_deref().unwrap();
        assert!(!listed_text.contains(temp.path().to_string_lossy().as_ref()));
        let definition =
            read_resource(&config, &id, SKILL_DEFINITION_FILE, 1, 20, Some(&revision)).unwrap();
        assert!(definition.text.contains("version one"));
        let resource =
            read_resource(&config, &id, "references/guide.md", 1, 20, Some(&revision)).unwrap();
        assert_eq!(resource.text, "line one\nline two");

        write_skill(temp.path(), "demo", "demo", "version two");
        let second = discover(&config).unwrap();
        assert_eq!(second.skills[0].descriptor.skill_id, id);
        assert_ne!(second.skills[0].descriptor.definition_revision, revision);
        assert_eq!(
            read_resource(&config, &id, SKILL_DEFINITION_FILE, 1, 20, Some(&revision)).unwrap_err(),
            "skill_definition_changed"
        );
        let fresh_revision = &second.skills[0].descriptor.definition_revision;
        let fresh = read_resource(
            &config,
            &id,
            SKILL_DEFINITION_FILE,
            1,
            20,
            Some(fresh_revision),
        )
        .unwrap();
        assert!(fresh.text.contains("version two"));
        let serialized = serde_json::to_string(&fresh).unwrap();
        assert!(!serialized.contains(temp.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn missing_root_is_a_bounded_path_free_diagnostic_not_an_empty_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-live-skills");
        let result = handle_configured_skill_roots_request(
            &SkillsConfig {
                roots: vec![missing.clone()],
            },
            ConfiguredSkillRootsRequest::List,
        );
        assert_eq!(result.exit_code, Some(0));
        let stdout = result.stdout.as_deref().unwrap();
        assert!(!stdout.contains(missing.to_string_lossy().as_ref()));
        let response: ConfiguredSkillRootsListResponse = serde_json::from_str(stdout).unwrap();
        assert!(response.skills.is_empty());
        assert_eq!(
            response.diagnostics,
            vec!["configured_skill_root_not_found".to_string()]
        );
    }

    #[test]
    fn same_package_in_two_roots_has_distinct_opaque_identity() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_skill(first.path(), "same", "same", "first");
        write_skill(second.path(), "same", "same", "second");
        let discovery = discover(&SkillsConfig {
            roots: vec![first.path().to_path_buf(), second.path().to_path_buf()],
        })
        .unwrap();
        assert_eq!(discovery.skills.len(), 2);
        assert_ne!(
            discovery.skills[0].descriptor.skill_id,
            discovery.skills[1].descriptor.skill_id
        );
        for skill in discovery.skills {
            assert!(!skill
                .descriptor
                .skill_id
                .contains(&first.path().to_string_lossy().as_ref()));
            assert!(!skill
                .descriptor
                .skill_id
                .contains(&second.path().to_string_lossy().as_ref()));
        }
    }

    #[test]
    fn list_response_truncates_valid_unicode_descriptors_to_wire_budget() {
        let temp = tempfile::tempdir().unwrap();
        let description = "界".repeat(webcodex_core::skill_metadata::MAX_SKILL_DESCRIPTION_CHARS);
        for index in 0..MAX_CONFIGURED_SKILL_PACKAGES {
            let package = temp.path().join(format!("skill-{index:03}"));
            fs::create_dir_all(&package).unwrap();
            fs::write(
                package.join(SKILL_DEFINITION_FILE),
                format!("---\nname: skill-{index:03}\ndescription: {description}\n---\nbody\n"),
            )
            .unwrap();
        }

        let result = handle_configured_skill_roots_request(
            &SkillsConfig {
                roots: vec![temp.path().to_path_buf()],
            },
            ConfiguredSkillRootsRequest::List,
        );
        assert_eq!(result.exit_code, Some(0));
        let stdout = result.stdout.as_deref().unwrap();
        assert!(stdout.len() <= CONFIGURED_SKILL_ROOTS_RESPONSE_MAX_BYTES);
        let response: ConfiguredSkillRootsListResponse = serde_json::from_str(stdout).unwrap();
        response.validate().unwrap();
        assert!(response.discovery_truncated);
        assert!(!response.skills.is_empty());
        assert!(response.skills.len() < MAX_CONFIGURED_SKILL_PACKAGES);
    }

    #[test]
    fn configured_root_symlink_is_rejected() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = tempfile::tempdir().unwrap();
            write_skill(target.path(), "demo", "demo", "body");
            let holder = tempfile::tempdir().unwrap();
            let link = holder.path().join("skills-link");
            symlink(target.path(), &link).unwrap();
            let discovery = discover(&SkillsConfig { roots: vec![link] }).unwrap();
            assert!(discovery.skills.is_empty());
            assert!(discovery
                .diagnostics
                .iter()
                .any(|code| code == "configured_skill_root_link_not_allowed"));
        }
    }

    #[test]
    fn traversal_and_symlink_escape_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), "demo", "demo", "body");
        let config = SkillsConfig {
            roots: vec![temp.path().to_path_buf()],
        };
        let skill = discover(&config).unwrap().skills.remove(0);
        assert_eq!(
            read_resource(
                &config,
                &skill.descriptor.skill_id,
                "../secret",
                1,
                20,
                None
            )
            .unwrap_err(),
            "skill_resource_path_invalid"
        );
        assert_eq!(
            read_resource(
                &config,
                &skill.descriptor.skill_id,
                "/etc/passwd",
                1,
                20,
                None
            )
            .unwrap_err(),
            "skill_resource_path_invalid"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret.md"), "secret").unwrap();
            symlink(
                outside.path().join("secret.md"),
                temp.path().join("demo/references/escape.md"),
            )
            .unwrap();
            assert_eq!(
                read_resource(
                    &config,
                    &skill.descriptor.skill_id,
                    "references/escape.md",
                    1,
                    20,
                    None,
                )
                .unwrap_err(),
                "skill_resource_path_invalid"
            );
        }
    }

    #[test]
    fn sensitive_package_names_are_not_discovered() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(temp.path(), ".git", "hidden", "must stay hidden");
        let discovery = discover(&SkillsConfig {
            roots: vec![temp.path().to_path_buf()],
        })
        .unwrap();
        assert!(discovery.skills.is_empty());
        assert_eq!(discovery.invalid_count, 1);
        assert!(discovery
            .diagnostics
            .iter()
            .any(|code| code == "sensitive_skill_definition"));
    }

    #[test]
    fn malformed_and_oversized_definitions_are_bounded_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("bad")).unwrap();
        fs::write(temp.path().join("bad/SKILL.md"), "not frontmatter").unwrap();
        fs::create_dir_all(temp.path().join("huge")).unwrap();
        fs::write(
            temp.path().join("huge/SKILL.md"),
            vec![b'x'; MAX_SKILL_DEFINITION_BYTES + 1],
        )
        .unwrap();
        let discovery = discover(&SkillsConfig {
            roots: vec![temp.path().to_path_buf()],
        })
        .unwrap();
        assert_eq!(discovery.invalid_count, 2);
        assert!(discovery.diagnostics.len() <= MAX_CONFIGURED_SKILL_DIAGNOSTICS);
        assert!(discovery
            .diagnostics
            .iter()
            .any(|code| code == "skill_definition_too_large"));
    }
}
