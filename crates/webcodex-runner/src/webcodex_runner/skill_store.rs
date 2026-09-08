use super::artifacts::validate_artifact_runner_path;
use super::config::RunnerPolicy;
use super::output::CommandResult;
use super::shell::cwd_allowed;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};
use webcodex_core::skill_metadata::{
    parse_skill_metadata, SkillMetadata, MAX_SKILL_DEFINITION_BYTES,
};
use webcodex_core::skill_store::{
    valid_lower_sha256, valid_package_revision, valid_skill_key, valid_state_revision,
    RunnerSkillDescriptor, RunnerSkillVersion, SkillStoreActivateResponse,
    SkillStoreInstallResponse, SkillStoreListActiveResponse, SkillStoreReadResponse,
    SkillStoreRemoveResponse, SkillStoreRequest, SkillStoreVersionsResponse,
    MAX_OPERATOR_REVISIONS_PER_SKILL, MAX_OPERATOR_SKILLS, MAX_SKILL_STORE_ARCHIVE_BYTES,
    MAX_SKILL_STORE_FILE_BYTES, MAX_SKILL_STORE_FILE_COUNT, MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS,
    MAX_SKILL_STORE_PATH_CHARS, MAX_SKILL_STORE_PATH_DEPTH, MAX_SKILL_STORE_READ_LINES,
    MAX_SKILL_STORE_READ_TEXT_BYTES, MAX_SKILL_STORE_REPLAY_RECORDS,
    MAX_SKILL_STORE_REPLAY_RECORD_BYTES, MAX_SKILL_STORE_REPLAY_SCAN_ENTRIES,
    MAX_SKILL_STORE_TOTAL_BYTES, MAX_SKILL_STORE_VERSIONS_LIMIT,
    SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS, SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS,
    SKILL_STORE_RESPONSE_FORMAT,
};
use webcodex_workspace::file_read_range;
use zip::ZipArchive;

const STORE_DIR: &str = "runner-skills-v1";
const STORE_SCHEMA_VERSION: u32 = 1;
const STATE_MAX_BYTES: usize = 8 * 1024;
const VERSION_METADATA_MAX_BYTES: usize = 16 * 1024;
const STORE_LOCK_FILE: &str = ".store.lock";
const STORE_IDENTITY_FILE: &str = "identity.json";
const STORE_IDENTITY_MAX_BYTES: usize = 4 * 1024;
const INSTALLED_DIR: &str = "installed";
const STATE_DIR: &str = "state";
const STAGING_DIR: &str = "staging";
const REPLAY_DIR: &str = "replay";
const PACKAGE_DIR: &str = "package";
const VERSION_METADATA_FILE: &str = "metadata.json";
const DEFAULT_RESOURCE_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreIdentity {
    schema_version: u32,
    namespace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillActiveState {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_package_revision: Option<String>,
}

impl Default for SkillActiveState {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            active_package_revision: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledRevisionMetadata {
    schema_version: u32,
    skill_key: String,
    package_revision: String,
    definition_revision: String,
    artifact_sha256: String,
    name: String,
    description: String,
    file_count: usize,
    total_bytes: usize,
    installed_at_unix_ms: i64,
}

impl InstalledRevisionMetadata {
    fn public_version(&self) -> RunnerSkillVersion {
        RunnerSkillVersion {
            package_revision: self.package_revision.clone(),
            definition_revision: self.definition_revision.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            file_count: self.file_count,
            total_bytes: self.total_bytes,
            installed_at_unix_ms: self.installed_at_unix_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplayRecord {
    schema_version: u32,
    operation: String,
    intent_hash: String,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
}

#[derive(Debug)]
enum ReplayState {
    First,
    Claimed,
    Prepared,
    Completed(Value),
}

struct PreparedPackage {
    files: BTreeMap<String, Vec<u8>>,
    package_revision: String,
    definition_revision: String,
    artifact_sha256: String,
    metadata: SkillMetadata,
    file_count: usize,
    total_bytes: usize,
}

struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

struct SkillStore {
    root: PathBuf,
    namespace: String,
    #[cfg(test)]
    fail_next_state_write: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_next_replay_completion: std::sync::atomic::AtomicBool,
}

impl SkillStore {
    fn for_runner(client_id: &str, server_url: &str) -> Result<Self, String> {
        let server_url = server_url.trim().trim_end_matches('/');
        if client_id.trim().is_empty() || server_url.is_empty() {
            return Err("skill_store_unavailable".to_string());
        }
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex-runner-skill-store-v1\0");
        hasher.update(client_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(server_url.as_bytes());
        let storage_namespace = format!("{:x}", hasher.finalize());
        let root = webcodex_runner_config::paths::default_client_state_base_dir()?
            .join(STORE_DIR)
            .join(&storage_namespace);
        let mut store = Self {
            root,
            namespace: String::new(),
            #[cfg(test)]
            fail_next_state_write: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_next_replay_completion: std::sync::atomic::AtomicBool::new(false),
        };
        {
            let _lock = store.lock()?;
            store.namespace = store.load_or_create_identity_locked()?;
        }
        Ok(store)
    }

    #[cfg(test)]
    fn for_test(root: PathBuf, namespace: &str) -> Self {
        Self {
            root,
            namespace: namespace.to_string(),
            fail_next_state_write: std::sync::atomic::AtomicBool::new(false),
            fail_next_replay_completion: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn for_test_persisted(root: PathBuf) -> Result<Self, String> {
        let mut store = Self {
            root,
            namespace: String::new(),
            fail_next_state_write: std::sync::atomic::AtomicBool::new(false),
            fail_next_replay_completion: std::sync::atomic::AtomicBool::new(false),
        };
        {
            let _lock = store.lock()?;
            store.namespace = store.load_or_create_identity_locked()?;
        }
        Ok(store)
    }

    fn initialize(&self) -> Result<(), String> {
        ensure_dir(&self.root)?;
        for child in [INSTALLED_DIR, STATE_DIR, STAGING_DIR, REPLAY_DIR] {
            ensure_dir(&self.root.join(child))?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<StoreLock, String> {
        self.initialize()?;
        let path = self.root.join(STORE_LOCK_FILE);
        reject_symlink_or_non_file_if_exists(&path, "Skill store lock")?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| "skill_store_lock_unavailable".to_string())?;
        FileExt::lock_exclusive(&file).map_err(|_| "skill_store_lock_unavailable".to_string())?;
        let guard = StoreLock(file);
        // Crash debris cleanup must be serialized with live store mutations.
        // Cleaning staging before acquiring this lock could delete another
        // process's in-progress immutable package commit.
        self.cleanup_staging()?;
        Ok(guard)
    }

    fn load_or_create_identity_locked(&self) -> Result<String, String> {
        let path = self.root.join(STORE_IDENTITY_FILE);
        let identity = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err("skill_store_identity_invalid".to_string());
                }
                read_json_bounded::<StoreIdentity>(&path, STORE_IDENTITY_MAX_BYTES)
                    .map_err(|_| "skill_store_identity_invalid".to_string())?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let identity = StoreIdentity {
                    schema_version: STORE_SCHEMA_VERSION,
                    namespace: uuid::Uuid::new_v4().simple().to_string(),
                };
                atomic_write_json(&path, &identity, STORE_IDENTITY_MAX_BYTES)
                    .map_err(|_| "skill_store_identity_write_failed".to_string())?;
                identity
            }
            Err(_) => return Err("skill_store_identity_unavailable".to_string()),
        };
        if identity.schema_version != STORE_SCHEMA_VERSION
            || identity.namespace.len() != 32
            || !identity
                .namespace
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("skill_store_identity_invalid".to_string());
        }
        Ok(identity.namespace)
    }

    fn cleanup_staging(&self) -> Result<(), String> {
        let root = self.root.join(STAGING_DIR);
        ensure_dir(&root)?;
        let entries =
            fs::read_dir(&root).map_err(|_| "skill_store_staging_unavailable".to_string())?;
        for entry in entries {
            let entry = entry.map_err(|_| "skill_store_staging_unavailable".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "skill_store_staging_unavailable".to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("skill_store_staging_invalid".to_string());
            }
            fs::remove_dir_all(entry.path())
                .map_err(|_| "skill_store_staging_cleanup_failed".to_string())?;
        }
        Ok(())
    }

    fn installed_skill_dir(&self, skill_key: &str) -> PathBuf {
        self.root.join(INSTALLED_DIR).join(skill_key)
    }

    fn revision_dir(&self, skill_key: &str, package_revision: &str) -> PathBuf {
        self.installed_skill_dir(skill_key).join(package_revision)
    }

    fn state_path(&self, skill_key: &str) -> PathBuf {
        self.root.join(STATE_DIR).join(format!("{skill_key}.json"))
    }

    fn replay_path(&self, idempotency_key: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex-skill-store-replay-key-v1\0");
        hasher.update(idempotency_key.as_bytes());
        self.root
            .join(REPLAY_DIR)
            .join(format!("{:x}.json", hasher.finalize()))
    }

    fn skill_id(&self, skill_key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex-runner-skill-id-v1\0");
        hasher.update(self.namespace.as_bytes());
        hasher.update(b"\0");
        hasher.update(skill_key.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        format!("wc_skill_{}", &digest[..32])
    }

    fn list_skill_keys(&self) -> Result<Vec<String>, String> {
        let root = self.root.join(INSTALLED_DIR);
        let mut keys = Vec::new();
        for entry in fs::read_dir(root).map_err(|_| "skill_store_unavailable".to_string())? {
            let entry = entry.map_err(|_| "skill_store_unavailable".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "skill_store_unavailable".to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("skill_store_invalid_entry".to_string());
            }
            let key = entry
                .file_name()
                .to_str()
                .filter(|value| valid_skill_key(value))
                .ok_or_else(|| "skill_store_invalid_entry".to_string())?
                .to_string();
            keys.push(key);
            if keys.len() > MAX_OPERATOR_SKILLS {
                return Err("skill_store_skill_limit_exceeded".to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn list_version_metadata(
        &self,
        skill_key: &str,
    ) -> Result<Vec<InstalledRevisionMetadata>, String> {
        let root = self.installed_skill_dir(skill_key);
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("skill_store_unavailable".to_string()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err("skill_store_invalid_entry".to_string())
            }
            Ok(_) => {}
        }
        let mut versions = Vec::new();
        for entry in fs::read_dir(root).map_err(|_| "skill_store_unavailable".to_string())? {
            let entry = entry.map_err(|_| "skill_store_unavailable".to_string())?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|_| "skill_store_unavailable".to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("skill_store_invalid_entry".to_string());
            }
            let file_name = entry.file_name();
            let revision = file_name
                .to_str()
                .filter(|value| valid_package_revision(value))
                .ok_or_else(|| "skill_store_invalid_entry".to_string())?;
            let version = self.read_revision_metadata(skill_key, revision)?;
            versions.push(version);
            if versions.len() > MAX_OPERATOR_REVISIONS_PER_SKILL {
                return Err("skill_store_revision_limit_exceeded".to_string());
            }
        }
        versions.sort_by(|left, right| left.package_revision.cmp(&right.package_revision));
        Ok(versions)
    }

    fn read_revision_metadata(
        &self,
        skill_key: &str,
        package_revision: &str,
    ) -> Result<InstalledRevisionMetadata, String> {
        let revision_root = self.revision_dir(skill_key, package_revision);
        let revision_metadata = fs::symlink_metadata(&revision_root)
            .map_err(|_| "skill_store_revision_unavailable".to_string())?;
        if revision_metadata.file_type().is_symlink() || !revision_metadata.is_dir() {
            return Err("skill_store_revision_modified".to_string());
        }
        let path = revision_root.join(VERSION_METADATA_FILE);
        let metadata: InstalledRevisionMetadata =
            read_json_bounded(&path, VERSION_METADATA_MAX_BYTES)?;
        if metadata.schema_version != STORE_SCHEMA_VERSION
            || metadata.skill_key != skill_key
            || metadata.package_revision != package_revision
            || !valid_lower_sha256(&metadata.definition_revision)
            || !valid_lower_sha256(&metadata.artifact_sha256)
        {
            return Err("skill_store_revision_metadata_invalid".to_string());
        }
        Ok(metadata)
    }

    fn read_state(&self, skill_key: &str) -> Result<SkillActiveState, String> {
        let path = self.state_path(skill_key);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(SkillActiveState::default())
            }
            Err(_) => Err("skill_store_state_unavailable".to_string()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err("skill_store_state_invalid".to_string())
            }
            Ok(_) => {
                let state: SkillActiveState = read_json_bounded(&path, STATE_MAX_BYTES)?;
                if state.schema_version != STORE_SCHEMA_VERSION
                    || state
                        .active_package_revision
                        .as_deref()
                        .is_some_and(|value| !valid_package_revision(value))
                {
                    return Err("skill_store_state_invalid".to_string());
                }
                if let Some(active) = state.active_package_revision.as_deref() {
                    let revision = self.revision_dir(skill_key, active);
                    match fs::symlink_metadata(&revision) {
                        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
                        }
                        _ => return Err("skill_store_dangling_active_revision".to_string()),
                    }
                }
                Ok(state)
            }
        }
    }

    fn state_revision(
        &self,
        skill_key: &str,
        state: &SkillActiveState,
        versions: &[InstalledRevisionMetadata],
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex-skill-store-state-v1\0");
        hasher.update(self.namespace.as_bytes());
        hasher.update(b"\0");
        hasher.update(skill_key.as_bytes());
        hasher.update(b"\0");
        if let Some(active) = state.active_package_revision.as_deref() {
            hasher.update(active.as_bytes());
        }
        for version in versions {
            hasher.update((version.package_revision.len() as u64).to_be_bytes());
            hasher.update(version.package_revision.as_bytes());
        }
        format!("wc_skillstate_{:x}", hasher.finalize())
    }

    fn namespace_revision(&self, descriptors: &[RunnerSkillDescriptor]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"webcodex-skill-store-namespace-v1\0");
        hasher.update(self.namespace.as_bytes());
        for descriptor in descriptors {
            for value in [
                descriptor.skill_id.as_str(),
                descriptor.skill_key.as_str(),
                descriptor.name.as_str(),
                descriptor.description.as_str(),
                descriptor.package_revision.as_str(),
                descriptor.definition_revision.as_str(),
            ] {
                hasher.update((value.len() as u64).to_be_bytes());
                hasher.update(value.as_bytes());
            }
        }
        format!("wc_skillstore_{:x}", hasher.finalize())
    }

    fn list_active(&self) -> Result<SkillStoreListActiveResponse, String> {
        let _lock = self.lock()?;
        let mut skills = Vec::new();
        for skill_key in self.list_skill_keys()? {
            let state = self.read_state(&skill_key)?;
            let Some(active) = state.active_package_revision.as_deref() else {
                continue;
            };
            let metadata = self.read_revision_metadata(&skill_key, active)?;
            self.verify_definition_immutable(&skill_key, &metadata)?;
            skills.push(RunnerSkillDescriptor {
                skill_id: self.skill_id(&skill_key),
                skill_key,
                name: metadata.name,
                description: metadata.description,
                package_revision: metadata.package_revision,
                definition_revision: metadata.definition_revision,
            });
        }
        skills.sort_by(|left, right| left.skill_key.cmp(&right.skill_key));
        let namespace_revision = self.namespace_revision(&skills);
        Ok(SkillStoreListActiveResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            namespace_revision,
            skills,
        })
    }

    fn versions(
        &self,
        skill_key: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SkillStoreVersionsResponse, String> {
        if !valid_skill_key(skill_key) || !(1..=MAX_SKILL_STORE_VERSIONS_LIMIT).contains(&limit) {
            return Err("skill_store_invalid_request".to_string());
        }
        let _lock = self.lock()?;
        let versions = self.list_version_metadata(skill_key)?;
        let state = self.read_state(skill_key)?;
        let state_revision = self.state_revision(skill_key, &state, &versions);
        let total_count = versions.len();
        let offset = offset.min(total_count);
        let end = offset.saturating_add(limit).min(total_count);
        let next_offset = (end < total_count).then_some(end);
        Ok(SkillStoreVersionsResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            skill_id: self.skill_id(skill_key),
            skill_key: skill_key.to_string(),
            state_revision,
            active_package_revision: state.active_package_revision,
            total_count,
            offset,
            next_offset,
            versions: versions[offset..end]
                .iter()
                .map(InstalledRevisionMetadata::public_version)
                .collect(),
        })
    }

    fn read_resource(
        &self,
        skill_id: &str,
        path: &str,
        start_line: usize,
        limit: usize,
        expected_package_revision: Option<&str>,
        expected_definition_revision: Option<&str>,
    ) -> Result<SkillStoreReadResponse, String> {
        if !valid_runtime_skill_id(skill_id)
            || !(1..=MAX_SKILL_STORE_READ_LINES).contains(&limit)
            || start_line == 0
        {
            return Err("skill_store_invalid_request".to_string());
        }
        let path = validate_resource_path(path)?;
        let _lock = self.lock()?;
        let skill_key = self
            .list_skill_keys()?
            .into_iter()
            .find(|key| self.skill_id(key) == skill_id)
            .ok_or_else(|| "skill_not_found".to_string())?;
        let before = self.read_state(&skill_key)?;
        let active = before
            .active_package_revision
            .clone()
            .ok_or_else(|| "skill_not_found".to_string())?;
        if expected_package_revision.is_some_and(|expected| expected != active) {
            return Err("skill_package_changed".to_string());
        }
        let metadata = self.read_revision_metadata(&skill_key, &active)?;
        self.verify_definition_immutable(&skill_key, &metadata)?;
        if expected_definition_revision
            .is_some_and(|expected| expected != metadata.definition_revision)
        {
            return Err("skill_definition_changed".to_string());
        }
        let package_root = self.revision_dir(&skill_key, &active).join(PACKAGE_DIR);
        let target = resolve_package_regular_file(
            &package_root,
            &path,
            "skill_resource_not_found",
            "skill_resource_path_invalid",
        )?;
        if webcodex_core::sensitive_paths::is_secret_path(&path) {
            return Err("skill_sensitive_path".to_string());
        }
        let max_file_bytes = if path == "SKILL.md" {
            MAX_SKILL_DEFINITION_BYTES
        } else {
            DEFAULT_RESOURCE_MAX_BYTES
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
            MAX_SKILL_STORE_READ_TEXT_BYTES,
        )
        .map_err(|error| match error.reason {
            file_read_range::ReadFileReason::InvalidUtf8 => {
                "skill_resource_unsupported_encoding".to_string()
            }
            file_read_range::ReadFileReason::NotFound => "skill_resource_not_found".to_string(),
            file_read_range::ReadFileReason::RangeTooLarge => {
                "skill_read_result_too_large".to_string()
            }
            _ => "skill_resource_unavailable".to_string(),
        })?;
        // The package revision covers the full normalized package tree, not
        // only SKILL.md. Revalidate the complete immutable tree after reading
        // so an out-of-band regular-file edit cannot be returned under the old
        // package_revision. The cheap definition check above still fails early
        // for the common definition-tamper case.
        let verified_files = self.verify_package_immutable(&skill_key, &metadata)?;
        let verified_resource = verified_files
            .get(&path)
            .ok_or_else(|| "skill_store_revision_modified".to_string())?;
        if sha256_hex(verified_resource) != read.sha256 {
            return Err("skill_store_revision_modified".to_string());
        }
        let after = self.read_state(&skill_key)?;
        if after.active_package_revision.as_deref() != Some(active.as_str()) {
            return Err("skill_package_changed".to_string());
        }
        Ok(SkillStoreReadResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            skill_id: skill_id.to_string(),
            skill_key,
            name: metadata.name,
            description: metadata.description,
            package_revision: active,
            definition_revision: metadata.definition_revision,
            path,
            sha256: read.sha256,
            text: read.content,
            start_line: read.start_line,
            end_line: read.end_line,
            returned_lines: read.returned_lines,
            has_more: read.has_more,
            next_start_line: read.next_start_line,
        })
    }

    fn install(
        &self,
        policy: &RunnerPolicy,
        skill_key: &str,
        source_project_id: &str,
        source_project_root: &str,
        artifact_path: &str,
        expected_artifact_sha256: &str,
        idempotency_key: &str,
        activate: bool,
        expected_state_revision: Option<&str>,
    ) -> Result<SkillStoreInstallResponse, String> {
        validate_management_common(skill_key, idempotency_key)?;
        if source_project_id.trim().is_empty()
            || source_project_id.len() > 512
            || source_project_id.chars().any(char::is_control)
            || !valid_lower_sha256(expected_artifact_sha256)
            || expected_state_revision.is_some_and(|value| !valid_state_revision(value))
        {
            return Err("skill_store_invalid_request".to_string());
        }
        validate_artifact_runner_path(artifact_path)
            .map_err(|_| "skill_install_artifact_path_invalid".to_string())?;
        let intent_hash = hash_install_intent(
            skill_key,
            source_project_id,
            artifact_path,
            expected_artifact_sha256,
            activate,
            expected_state_revision,
        );
        let lock = self.lock()?;
        let replay = self.begin_replay(&lock, idempotency_key, "install", &intent_hash)?;
        if let ReplayState::Completed(value) = replay {
            let mut response: SkillStoreInstallResponse = serde_json::from_value(value)
                .map_err(|_| "skill_store_replay_invalid".to_string())?;
            response.replayed = true;
            return Ok(response);
        }
        if matches!(&replay, ReplayState::Prepared) {
            if let Some(metadata) =
                self.find_revision_by_artifact(skill_key, expected_artifact_sha256)?
            {
                self.verify_package_immutable(skill_key, &metadata)?;
                let versions = self.list_version_metadata(skill_key)?;
                let state = self.read_state(skill_key)?;
                if activate
                    && state.active_package_revision.as_deref()
                        != Some(metadata.package_revision.as_str())
                {
                    return Err("skill_install_reconcile_required".to_string());
                }
                let response = SkillStoreInstallResponse {
                    format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
                    skill_id: self.skill_id(skill_key),
                    skill_key: skill_key.to_string(),
                    package_revision: metadata.package_revision.clone(),
                    definition_revision: metadata.definition_revision.clone(),
                    artifact_sha256: metadata.artifact_sha256.clone(),
                    file_count: metadata.file_count,
                    total_bytes: metadata.total_bytes,
                    installed: false,
                    activated: false,
                    replayed: true,
                    state_revision: self.state_revision(skill_key, &state, &versions),
                    active_package_revision: state.active_package_revision,
                };
                self.complete_replay(&lock, idempotency_key, &response)?;
                return Ok(response);
            }
        }

        let source_root = Path::new(source_project_root)
            .canonicalize()
            .map_err(|_| "skill_install_source_project_unavailable".to_string())?;
        cwd_allowed(policy, &source_root)
            .map_err(|_| "skill_install_source_project_forbidden".to_string())?;
        let artifact_raw = source_root.join(artifact_path);
        let artifact_meta = fs::symlink_metadata(&artifact_raw)
            .map_err(|_| "skill_install_artifact_not_found".to_string())?;
        if artifact_meta.file_type().is_symlink() || !artifact_meta.is_file() {
            return Err("skill_install_artifact_path_invalid".to_string());
        }
        if artifact_meta.len() > MAX_SKILL_STORE_ARCHIVE_BYTES as u64 {
            return Err("skill_install_archive_too_large".to_string());
        }
        let artifact = artifact_raw
            .canonicalize()
            .map_err(|_| "skill_install_artifact_not_found".to_string())?;
        if !artifact.starts_with(&source_root) {
            return Err("skill_install_artifact_path_invalid".to_string());
        }
        let archive_bytes =
            fs::read(&artifact).map_err(|_| "skill_install_artifact_unavailable".to_string())?;
        if archive_bytes.len() > MAX_SKILL_STORE_ARCHIVE_BYTES {
            return Err("skill_install_archive_too_large".to_string());
        }
        let actual_artifact_sha256 = sha256_hex(&archive_bytes);
        if actual_artifact_sha256 != expected_artifact_sha256 {
            return Err("skill_install_artifact_changed".to_string());
        }
        let prepared = prepare_archive(&archive_bytes, actual_artifact_sha256)?;
        // A canonical package revision is independent of ZIP container encoding.
        // If the effect committed an already-installed package whose original
        // metadata records a different archive SHA, the pre-read artifact fast
        // path above cannot identify it. Reconcile by the expanded package
        // identity before applying the caller's now-stale CAS guard.
        if matches!(&replay, ReplayState::Prepared) {
            let versions = self.list_version_metadata(skill_key)?;
            if let Some(metadata) = versions
                .iter()
                .find(|metadata| metadata.package_revision == prepared.package_revision)
            {
                self.verify_package_immutable(skill_key, metadata)?;
                let state = self.read_state(skill_key)?;
                if activate
                    && state.active_package_revision.as_deref()
                        != Some(prepared.package_revision.as_str())
                {
                    return Err("skill_install_reconcile_required".to_string());
                }
                let response = SkillStoreInstallResponse {
                    format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
                    skill_id: self.skill_id(skill_key),
                    skill_key: skill_key.to_string(),
                    package_revision: prepared.package_revision.clone(),
                    definition_revision: prepared.definition_revision.clone(),
                    artifact_sha256: prepared.artifact_sha256.clone(),
                    file_count: prepared.file_count,
                    total_bytes: prepared.total_bytes,
                    installed: false,
                    activated: false,
                    replayed: true,
                    state_revision: self.state_revision(skill_key, &state, &versions),
                    active_package_revision: state.active_package_revision,
                };
                self.complete_replay(&lock, idempotency_key, &response)?;
                return Ok(response);
            }
        }

        let existing_keys = self.list_skill_keys()?;
        let is_new_key = !existing_keys.iter().any(|key| key == skill_key);
        if is_new_key && existing_keys.len() >= MAX_OPERATOR_SKILLS {
            return Err("skill_store_skill_limit_exceeded".to_string());
        }
        let before_versions = self.list_version_metadata(skill_key)?;
        if before_versions.len() >= MAX_OPERATOR_REVISIONS_PER_SKILL
            && !before_versions
                .iter()
                .any(|version| version.package_revision == prepared.package_revision)
        {
            return Err("skill_store_revision_limit_exceeded".to_string());
        }
        let before_state = self.read_state(skill_key)?;
        let before_state_revision = self.state_revision(skill_key, &before_state, &before_versions);
        if activate {
            if let Some(expected) = expected_state_revision {
                if expected != before_state_revision {
                    return Err("skill_state_changed".to_string());
                }
            } else if !before_versions.is_empty() || before_state.active_package_revision.is_some()
            {
                return Err("skill_expected_state_required".to_string());
            }
        }
        self.prepare_replay(&lock, idempotency_key)?;
        let installed = self.commit_prepared_package(skill_key, &prepared)?;
        let mut activated = false;
        let mut state = before_state;
        if activate
            && state.active_package_revision.as_deref() != Some(prepared.package_revision.as_str())
        {
            state.active_package_revision = Some(prepared.package_revision.clone());
            self.write_state(skill_key, &state)?;
            activated = true;
        }
        let versions = self.list_version_metadata(skill_key)?;
        let response = SkillStoreInstallResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            skill_id: self.skill_id(skill_key),
            skill_key: skill_key.to_string(),
            package_revision: prepared.package_revision.clone(),
            definition_revision: prepared.definition_revision.clone(),
            artifact_sha256: prepared.artifact_sha256.clone(),
            file_count: prepared.file_count,
            total_bytes: prepared.total_bytes,
            installed,
            activated,
            replayed: matches!(&replay, ReplayState::Claimed | ReplayState::Prepared),
            state_revision: self.state_revision(skill_key, &state, &versions),
            active_package_revision: state.active_package_revision,
        };
        self.complete_replay(&lock, idempotency_key, &response)?;
        Ok(response)
    }

    fn activate(
        &self,
        skill_key: &str,
        package_revision: &str,
        expected_state_revision: &str,
        idempotency_key: &str,
    ) -> Result<SkillStoreActivateResponse, String> {
        validate_management_common(skill_key, idempotency_key)?;
        if !valid_package_revision(package_revision)
            || !valid_state_revision(expected_state_revision)
        {
            return Err("skill_store_invalid_request".to_string());
        }
        let intent_hash = hash_simple_intent(
            "activate",
            &[skill_key, package_revision, expected_state_revision],
        );
        let lock = self.lock()?;
        let replay = self.begin_replay(&lock, idempotency_key, "activate", &intent_hash)?;
        if let ReplayState::Completed(value) = replay {
            let mut response: SkillStoreActivateResponse = serde_json::from_value(value)
                .map_err(|_| "skill_store_replay_invalid".to_string())?;
            response.replayed = true;
            return Ok(response);
        }
        let versions = self.list_version_metadata(skill_key)?;
        let target_metadata = versions
            .iter()
            .find(|version| version.package_revision == package_revision)
            .ok_or_else(|| "skill_package_not_found".to_string())?;
        self.verify_package_immutable(skill_key, target_metadata)?;
        let mut state = self.read_state(skill_key)?;
        if matches!(&replay, ReplayState::Prepared)
            && state.active_package_revision.as_deref() == Some(package_revision)
        {
            let response = SkillStoreActivateResponse {
                format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
                skill_id: self.skill_id(skill_key),
                skill_key: skill_key.to_string(),
                previous_active_package_revision: Some(package_revision.to_string()),
                active_package_revision: package_revision.to_string(),
                state_revision: self.state_revision(skill_key, &state, &versions),
                changed: false,
                replayed: true,
            };
            self.complete_replay(&lock, idempotency_key, &response)?;
            return Ok(response);
        }
        let current_revision = self.state_revision(skill_key, &state, &versions);
        if current_revision != expected_state_revision {
            return Err("skill_state_changed".to_string());
        }
        let previous = state.active_package_revision.clone();
        let changed = previous.as_deref() != Some(package_revision);
        if changed {
            self.prepare_replay(&lock, idempotency_key)?;
        }
        if changed {
            state.active_package_revision = Some(package_revision.to_string());
            self.write_state(skill_key, &state)?;
        }
        let response = SkillStoreActivateResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            skill_id: self.skill_id(skill_key),
            skill_key: skill_key.to_string(),
            previous_active_package_revision: previous,
            active_package_revision: package_revision.to_string(),
            state_revision: self.state_revision(skill_key, &state, &versions),
            changed,
            replayed: matches!(&replay, ReplayState::Claimed | ReplayState::Prepared),
        };
        self.complete_replay(&lock, idempotency_key, &response)?;
        Ok(response)
    }

    fn remove_revision(
        &self,
        skill_key: &str,
        package_revision: &str,
        expected_state_revision: &str,
        idempotency_key: &str,
    ) -> Result<SkillStoreRemoveResponse, String> {
        validate_management_common(skill_key, idempotency_key)?;
        if !valid_package_revision(package_revision)
            || !valid_state_revision(expected_state_revision)
        {
            return Err("skill_store_invalid_request".to_string());
        }
        let intent_hash = hash_simple_intent(
            "remove_revision",
            &[skill_key, package_revision, expected_state_revision],
        );
        let lock = self.lock()?;
        let replay = self.begin_replay(&lock, idempotency_key, "remove_revision", &intent_hash)?;
        if let ReplayState::Completed(value) = replay {
            let mut response: SkillStoreRemoveResponse = serde_json::from_value(value)
                .map_err(|_| "skill_store_replay_invalid".to_string())?;
            response.replayed = true;
            return Ok(response);
        }
        let before_versions = self.list_version_metadata(skill_key)?;
        let state = self.read_state(skill_key)?;
        if state.active_package_revision.as_deref() == Some(package_revision) {
            return Err("skill_active_revision_remove_forbidden".to_string());
        }
        let exists = before_versions
            .iter()
            .any(|version| version.package_revision == package_revision);
        if matches!(&replay, ReplayState::Prepared) && !exists {
            let response = SkillStoreRemoveResponse {
                format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
                skill_id: self.skill_id(skill_key),
                skill_key: skill_key.to_string(),
                package_revision: package_revision.to_string(),
                state_revision: self.state_revision(skill_key, &state, &before_versions),
                removed: false,
                replayed: true,
            };
            self.complete_replay(&lock, idempotency_key, &response)?;
            return Ok(response);
        }
        let current_revision = self.state_revision(skill_key, &state, &before_versions);
        if current_revision != expected_state_revision {
            return Err("skill_state_changed".to_string());
        }
        if exists {
            self.prepare_replay(&lock, idempotency_key)?;
            self.atomic_remove_revision(skill_key, package_revision)?;
        }
        let after_versions = self.list_version_metadata(skill_key)?;
        let response = SkillStoreRemoveResponse {
            format: SKILL_STORE_RESPONSE_FORMAT.to_string(),
            skill_id: self.skill_id(skill_key),
            skill_key: skill_key.to_string(),
            package_revision: package_revision.to_string(),
            state_revision: self.state_revision(skill_key, &state, &after_versions),
            removed: exists,
            replayed: matches!(&replay, ReplayState::Claimed | ReplayState::Prepared),
        };
        self.complete_replay(&lock, idempotency_key, &response)?;
        Ok(response)
    }

    fn verify_definition_immutable(
        &self,
        skill_key: &str,
        metadata: &InstalledRevisionMetadata,
    ) -> Result<(), String> {
        let package_root = self
            .revision_dir(skill_key, &metadata.package_revision)
            .join(PACKAGE_DIR);
        let path = resolve_package_regular_file(
            &package_root,
            "SKILL.md",
            "skill_store_revision_modified",
            "skill_store_revision_modified",
        )?;
        let bytes = read_file_bounded(&path, MAX_SKILL_DEFINITION_BYTES)
            .map_err(|_| "skill_store_revision_modified".to_string())?;
        if sha256_hex(&bytes) != metadata.definition_revision {
            return Err("skill_store_revision_modified".to_string());
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| "skill_store_revision_modified".to_string())?;
        let parsed =
            parse_skill_metadata(text).map_err(|_| "skill_store_revision_modified".to_string())?;
        if parsed.name != metadata.name || parsed.description != metadata.description {
            return Err("skill_store_revision_modified".to_string());
        }
        Ok(())
    }

    fn verify_package_immutable(
        &self,
        skill_key: &str,
        metadata: &InstalledRevisionMetadata,
    ) -> Result<BTreeMap<String, Vec<u8>>, String> {
        let package_root = self
            .revision_dir(skill_key, &metadata.package_revision)
            .join(PACKAGE_DIR);
        let files = snapshot_installed_package(&package_root)?;
        let total_bytes = files.values().map(Vec::len).sum::<usize>();
        if files.len() != metadata.file_count
            || total_bytes != metadata.total_bytes
            || compute_package_revision(&files) != metadata.package_revision
        {
            return Err("skill_store_revision_modified".to_string());
        }
        let definition = files
            .get("SKILL.md")
            .ok_or_else(|| "skill_store_revision_modified".to_string())?;
        if sha256_hex(definition) != metadata.definition_revision {
            return Err("skill_store_revision_modified".to_string());
        }
        let text = std::str::from_utf8(definition)
            .map_err(|_| "skill_store_revision_modified".to_string())?;
        let parsed =
            parse_skill_metadata(text).map_err(|_| "skill_store_revision_modified".to_string())?;
        if parsed.name != metadata.name || parsed.description != metadata.description {
            return Err("skill_store_revision_modified".to_string());
        }
        Ok(files)
    }

    fn commit_prepared_package(
        &self,
        skill_key: &str,
        prepared: &PreparedPackage,
    ) -> Result<bool, String> {
        ensure_dir(&self.installed_skill_dir(skill_key))?;
        let final_dir = self.revision_dir(skill_key, &prepared.package_revision);
        if final_dir.exists() {
            let existing = self.read_revision_metadata(skill_key, &prepared.package_revision)?;
            self.verify_package_immutable(skill_key, &existing)?;
            if existing.definition_revision != prepared.definition_revision
                || existing.name != prepared.metadata.name
                || existing.description != prepared.metadata.description
            {
                return Err("skill_store_revision_collision".to_string());
            }
            // package_revision is the canonical expanded package-tree identity.
            // The same package may arrive in a differently encoded ZIP whose
            // artifact SHA differs; reuse the immutable revision rather than
            // treating container representation as package identity.
            return Ok(false);
        }
        let staging = self
            .root
            .join(STAGING_DIR)
            .join(format!("install-{}", uuid::Uuid::new_v4().simple()));
        ensure_dir(&staging)?;
        let package = staging.join(PACKAGE_DIR);
        ensure_dir(&package)?;
        for (relative, bytes) in &prepared.files {
            let target = package.join(relative);
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)
                .map_err(|_| "skill_install_staging_write_failed".to_string())?;
            file.write_all(bytes)
                .map_err(|_| "skill_install_staging_write_failed".to_string())?;
            file.sync_all()
                .map_err(|_| "skill_install_staging_write_failed".to_string())?;
        }
        // Verify the tree as it actually exists on this filesystem before it
        // can become an immutable installed revision. This catches platform
        // path folding/normalization, staging tamper, or an incomplete write
        // that string-level ZIP validation alone cannot prove away.
        let staged_files = snapshot_installed_package(&package)
            .map_err(|_| "skill_install_staging_verification_failed".to_string())?;
        let staged_total_bytes = staged_files.values().map(Vec::len).sum::<usize>();
        if staged_files.len() != prepared.file_count
            || staged_total_bytes != prepared.total_bytes
            || compute_package_revision(&staged_files) != prepared.package_revision
        {
            return Err("skill_install_staging_verification_failed".to_string());
        }

        let metadata = InstalledRevisionMetadata {
            schema_version: STORE_SCHEMA_VERSION,
            skill_key: skill_key.to_string(),
            package_revision: prepared.package_revision.clone(),
            definition_revision: prepared.definition_revision.clone(),
            artifact_sha256: prepared.artifact_sha256.clone(),
            name: prepared.metadata.name.clone(),
            description: prepared.metadata.description.clone(),
            file_count: prepared.file_count,
            total_bytes: prepared.total_bytes,
            installed_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        };
        write_new_json(
            &staging.join(VERSION_METADATA_FILE),
            &metadata,
            VERSION_METADATA_MAX_BYTES,
        )?;
        fs::rename(&staging, &final_dir).map_err(|error| {
            if final_dir.exists() {
                "skill_store_revision_commit_raced".to_string()
            } else {
                format!("skill_store_revision_commit_failed:{error}")
            }
        })?;
        sync_parent(&final_dir)?;
        Ok(true)
    }

    fn atomic_remove_revision(
        &self,
        skill_key: &str,
        package_revision: &str,
    ) -> Result<(), String> {
        let source = self.revision_dir(skill_key, package_revision);
        let tombstone = self
            .root
            .join(STAGING_DIR)
            .join(format!("remove-{}", uuid::Uuid::new_v4().simple()));
        fs::rename(&source, &tombstone).map_err(|_| "skill_remove_revision_failed".to_string())?;
        sync_parent(&source)?;
        fs::remove_dir_all(&tombstone).map_err(|_| "skill_remove_cleanup_failed".to_string())?;
        Ok(())
    }

    fn write_state(&self, skill_key: &str, state: &SkillActiveState) -> Result<(), String> {
        #[cfg(test)]
        if self
            .fail_next_state_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err("skill_store_state_write_failed".to_string());
        }
        atomic_write_json(&self.state_path(skill_key), state, STATE_MAX_BYTES)
            .map_err(|_| "skill_store_state_write_failed".to_string())
    }

    fn atomic_write_replay_record<T: Serialize>(
        &self,
        path: &Path,
        value: &T,
    ) -> Result<(), String> {
        let replay_root = self.root.join(REPLAY_DIR);
        if path.parent() != Some(replay_root.as_path())
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(valid_replay_file_name)
        {
            return Err("skill_store_replay_path_invalid".to_string());
        }
        reject_symlink_or_non_file_if_exists(path, "Skill store replay record")?;

        // Keep atomic-write crash debris out of replay/. New-key GC treats every
        // non-record entry there as corruption, so a temp file left beside the
        // authoritative records could otherwise make a Runner crash permanently
        // block future Skill-management admission. staging/ is already cleaned
        // under the same store lock on the next operation.
        let staging = self
            .root
            .join(STAGING_DIR)
            .join(format!("replay-{}", uuid::Uuid::new_v4().simple()));
        ensure_dir(&staging)?;
        let staged = staging.join("record.json");
        write_new_json(&staged, value, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)?;
        replace_file_atomic(&staged, path)?;
        sync_parent(path)?;
        let _ = fs::remove_dir(&staging);
        Ok(())
    }

    fn begin_replay(
        &self,
        lock: &StoreLock,
        idempotency_key: &str,
        operation: &str,
        intent_hash: &str,
    ) -> Result<ReplayState, String> {
        self.begin_replay_at(
            lock,
            idempotency_key,
            operation,
            intent_hash,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    fn begin_replay_at(
        &self,
        lock: &StoreLock,
        idempotency_key: &str,
        operation: &str,
        intent_hash: &str,
        now_unix_ms: i64,
    ) -> Result<ReplayState, String> {
        if idempotency_key.is_empty()
            || idempotency_key.chars().count() > MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS
            || idempotency_key.chars().any(char::is_control)
        {
            return Err("skill_idempotency_key_invalid".to_string());
        }
        let path = self.replay_path(idempotency_key);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err("skill_store_replay_invalid_entry".to_string());
                }
                let record: ReplayRecord =
                    read_json_bounded(&path, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)
                        .map_err(|_| "skill_store_replay_invalid".to_string())?;
                if record.schema_version != STORE_SCHEMA_VERSION
                    || record.operation != operation
                    || record.intent_hash != intent_hash
                {
                    return Err("skill_idempotency_conflict".to_string());
                }
                return replay_state_from_record(record);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("skill_store_replay_unavailable".to_string()),
        }

        if now_unix_ms <= 0 {
            return Err("skill_store_replay_time_unavailable".to_string());
        }
        let retained = self.gc_expired_replays_locked_at(lock, now_unix_ms)?;
        if retained >= MAX_SKILL_STORE_REPLAY_RECORDS {
            return Err("skill_store_replay_capacity_exceeded".to_string());
        }
        let record = ReplayRecord {
            schema_version: STORE_SCHEMA_VERSION,
            operation: operation.to_string(),
            intent_hash: intent_hash.to_string(),
            status: "claimed".to_string(),
            created_at_unix_ms: Some(now_unix_ms),
            updated_at_unix_ms: Some(now_unix_ms),
            result: None,
        };
        self.atomic_write_replay_record(&path, &record)
            .map_err(|_| "skill_store_replay_write_failed".to_string())?;
        Ok(ReplayState::First)
    }

    fn gc_expired_replays_locked_at(
        &self,
        _lock: &StoreLock,
        now_unix_ms: i64,
    ) -> Result<usize, String> {
        let root = self.root.join(REPLAY_DIR);
        let entries =
            fs::read_dir(&root).map_err(|_| "skill_store_replay_unavailable".to_string())?;
        let mut scanned = 0usize;
        let mut retained = 0usize;
        let mut expired = Vec::<PathBuf>::new();
        for entry in entries {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_SKILL_STORE_REPLAY_SCAN_ENTRIES {
                return Err("skill_store_replay_capacity_unavailable".to_string());
            }
            let entry = entry.map_err(|_| "skill_store_replay_unavailable".to_string())?;
            let file_name = entry.file_name();
            if !file_name.to_str().is_some_and(valid_replay_file_name) {
                return Err("skill_store_replay_invalid_entry".to_string());
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "skill_store_replay_invalid_entry".to_string())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > MAX_SKILL_STORE_REPLAY_RECORD_BYTES as u64
            {
                return Err("skill_store_replay_invalid_entry".to_string());
            }
            let record: ReplayRecord =
                read_json_bounded(&path, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)
                    .map_err(|_| "skill_store_replay_invalid_entry".to_string())?;
            validate_replay_record_for_gc(&record)?;
            if replay_record_is_expired(&record, &metadata, now_unix_ms) {
                expired.push(path);
            } else {
                retained = retained.saturating_add(1);
            }
        }

        // Revalidate every candidate immediately before deletion. The store lock
        // excludes conforming writers; this second bounded check also fails
        // closed if an external local process replaced a scanned candidate with
        // a symlink, malformed body, or newly unexpired replay record.
        for path in &expired {
            let metadata = fs::symlink_metadata(path)
                .map_err(|_| "skill_store_replay_invalid_entry".to_string())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > MAX_SKILL_STORE_REPLAY_RECORD_BYTES as u64
            {
                return Err("skill_store_replay_invalid_entry".to_string());
            }
            let record: ReplayRecord = read_json_bounded(path, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)
                .map_err(|_| "skill_store_replay_invalid_entry".to_string())?;
            validate_replay_record_for_gc(&record)?;
            if !replay_record_is_expired(&record, &metadata, now_unix_ms) {
                return Err("skill_store_replay_changed_during_gc".to_string());
            }
        }
        for path in &expired {
            fs::remove_file(path).map_err(|_| "skill_store_replay_gc_failed".to_string())?;
        }
        if let Some(path) = expired.first() {
            sync_parent(path).map_err(|_| "skill_store_replay_gc_failed".to_string())?;
        }
        Ok(retained)
    }

    fn prepare_replay(&self, lock: &StoreLock, idempotency_key: &str) -> Result<(), String> {
        self.prepare_replay_at(lock, idempotency_key, chrono::Utc::now().timestamp_millis())
    }

    fn prepare_replay_at(
        &self,
        _lock: &StoreLock,
        idempotency_key: &str,
        now_unix_ms: i64,
    ) -> Result<(), String> {
        let path = self.replay_path(idempotency_key);
        let mut record: ReplayRecord =
            read_json_bounded(&path, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)?;
        match record.status.as_str() {
            "claimed" => {
                if now_unix_ms <= 0 {
                    return Err("skill_store_replay_time_unavailable".to_string());
                }
                record.status = "prepared".to_string();
                advance_replay_timestamp(&mut record, now_unix_ms);
                self.atomic_write_replay_record(&path, &record)
                    .map_err(|_| "skill_store_replay_write_failed".to_string())
            }
            "prepared" => {
                // A prepared retry can re-enter an idempotent effect boundary
                // when the prior effect is not yet observable. Refresh the
                // durable anchor before that retry so any newly possible effect
                // still receives the full effect-retention window.
                if now_unix_ms <= 0 {
                    return Err("skill_store_replay_time_unavailable".to_string());
                }
                advance_replay_timestamp(&mut record, now_unix_ms);
                self.atomic_write_replay_record(&path, &record)
                    .map_err(|_| "skill_store_replay_write_failed".to_string())
            }
            _ => Err("skill_store_replay_invalid".to_string()),
        }
    }

    fn complete_replay<T: Serialize>(
        &self,
        lock: &StoreLock,
        idempotency_key: &str,
        response: &T,
    ) -> Result<(), String> {
        self.complete_replay_at(
            lock,
            idempotency_key,
            response,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    fn complete_replay_at<T: Serialize>(
        &self,
        _lock: &StoreLock,
        idempotency_key: &str,
        response: &T,
        now_unix_ms: i64,
    ) -> Result<(), String> {
        let path = self.replay_path(idempotency_key);
        let mut record: ReplayRecord =
            read_json_bounded(&path, MAX_SKILL_STORE_REPLAY_RECORD_BYTES)?;
        if !matches!(record.status.as_str(), "claimed" | "prepared") {
            return Err("skill_store_replay_invalid".to_string());
        }
        if now_unix_ms <= 0 {
            return Err("skill_store_replay_commit_failed".to_string());
        }
        record.status = "completed".to_string();
        advance_replay_timestamp(&mut record, now_unix_ms);
        record.result = Some(
            serde_json::to_value(response).map_err(|_| "skill_store_replay_invalid".to_string())?,
        );
        #[cfg(test)]
        if self
            .fail_next_replay_completion
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err("skill_store_replay_commit_failed".to_string());
        }
        self.atomic_write_replay_record(&path, &record)
            .map_err(|_| "skill_store_replay_commit_failed".to_string())
    }

    fn find_revision_by_artifact(
        &self,
        skill_key: &str,
        artifact_sha256: &str,
    ) -> Result<Option<InstalledRevisionMetadata>, String> {
        Ok(self
            .list_version_metadata(skill_key)?
            .into_iter()
            .find(|metadata| metadata.artifact_sha256 == artifact_sha256))
    }
}

fn replay_state_from_record(record: ReplayRecord) -> Result<ReplayState, String> {
    match record.status.as_str() {
        "claimed" if record.result.is_none() => Ok(ReplayState::Claimed),
        "prepared" if record.result.is_none() => Ok(ReplayState::Prepared),
        "completed" => record
            .result
            .map(ReplayState::Completed)
            .ok_or_else(|| "skill_store_replay_invalid".to_string()),
        _ => Err("skill_store_replay_invalid".to_string()),
    }
}

fn validate_replay_record_for_gc(record: &ReplayRecord) -> Result<(), String> {
    if record.schema_version != STORE_SCHEMA_VERSION
        || !matches!(
            record.operation.as_str(),
            "install" | "activate" | "remove_revision"
        )
        || !valid_lower_sha256(&record.intent_hash)
    {
        return Err("skill_store_replay_invalid_entry".to_string());
    }
    match record.status.as_str() {
        "claimed" | "prepared" if record.result.is_none() => Ok(()),
        "completed" if record.result.is_some() => Ok(()),
        _ => Err("skill_store_replay_invalid_entry".to_string()),
    }
}

fn valid_replay_file_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(valid_lower_sha256)
}

fn replay_record_is_expired(
    record: &ReplayRecord,
    metadata: &fs::Metadata,
    now_unix_ms: i64,
) -> bool {
    if now_unix_ms <= 0 {
        return false;
    }
    let retention_secs = match record.status.as_str() {
        "claimed" => SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS,
        "prepared" | "completed" => SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS,
        _ => return false,
    };
    let Some(anchor_unix_ms) = replay_retention_anchor_unix_ms(record, metadata) else {
        return false;
    };
    let Some(age_ms) = now_unix_ms.checked_sub(anchor_unix_ms) else {
        return false;
    };
    if age_ms < 0 {
        return false;
    }
    let retention_ms = i64::try_from(retention_secs)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000));
    retention_ms.is_some_and(|retention_ms| age_ms >= retention_ms)
}

fn replay_retention_anchor_unix_ms(record: &ReplayRecord, metadata: &fs::Metadata) -> Option<i64> {
    match (record.created_at_unix_ms, record.updated_at_unix_ms) {
        (Some(created), Some(updated)) if created > 0 && updated >= created => Some(updated),
        // Only records from the pre-timestamp schema use mtime migration. A
        // partial or internally inconsistent timestamp pair is treated as
        // corrupted/ambiguous retention metadata and therefore never expires
        // automatically.
        (None, None) => metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
        _ => None,
    }
}

fn advance_replay_timestamp(record: &mut ReplayRecord, now_unix_ms: i64) {
    let created = record
        .created_at_unix_ms
        .filter(|value| *value > 0)
        .unwrap_or(now_unix_ms);
    let previous_updated = record
        .updated_at_unix_ms
        .filter(|value| *value >= created)
        .unwrap_or(created);
    record.created_at_unix_ms = Some(created);
    // A backwards wall-clock transition at the effect boundary must never make
    // prepared/completed retention shorter than its contract. Mark the record
    // conservatively non-expiring; capacity may fail closed until operator time
    // is repaired rather than evicting uncertain recovery state early.
    record.updated_at_unix_ms = Some(if now_unix_ms < previous_updated {
        i64::MAX
    } else {
        now_unix_ms.max(created)
    });
}

pub(crate) fn handle_skill_store_request(
    client_id: &str,
    server_url: &str,
    policy: &RunnerPolicy,
    request: SkillStoreRequest,
) -> CommandResult {
    let start = Instant::now();
    let store = match SkillStore::for_runner(client_id, server_url) {
        Ok(store) => store,
        Err(_) => return error_result(start, "skill_store_unavailable"),
    };
    let result = match request {
        SkillStoreRequest::ListActive => store
            .list_active()
            .and_then(|value| serialize_response(value)),
        SkillStoreRequest::Versions {
            skill_key,
            offset,
            limit,
        } => store
            .versions(&skill_key, offset, limit)
            .and_then(|value| serialize_response(value)),
        SkillStoreRequest::Read {
            skill_id,
            path,
            start_line,
            limit,
            expected_package_revision,
            expected_definition_revision,
        } => store
            .read_resource(
                &skill_id,
                &path,
                start_line,
                limit,
                expected_package_revision.as_deref(),
                expected_definition_revision.as_deref(),
            )
            .and_then(|value| serialize_response(value)),
        SkillStoreRequest::Install {
            skill_key,
            source_project_id,
            source_project_root,
            artifact_path,
            expected_artifact_sha256,
            idempotency_key,
            activate,
            expected_state_revision,
        } => store
            .install(
                policy,
                &skill_key,
                &source_project_id,
                &source_project_root,
                &artifact_path,
                &expected_artifact_sha256,
                &idempotency_key,
                activate,
                expected_state_revision.as_deref(),
            )
            .and_then(|value| serialize_response(value)),
        SkillStoreRequest::Activate {
            skill_key,
            package_revision,
            expected_state_revision,
            idempotency_key,
        } => store
            .activate(
                &skill_key,
                &package_revision,
                &expected_state_revision,
                &idempotency_key,
            )
            .and_then(|value| serialize_response(value)),
        SkillStoreRequest::RemoveRevision {
            skill_key,
            package_revision,
            expected_state_revision,
            idempotency_key,
        } => store
            .remove_revision(
                &skill_key,
                &package_revision,
                &expected_state_revision,
                &idempotency_key,
            )
            .and_then(|value| serialize_response(value)),
    };
    match result {
        Ok(stdout) => CommandResult {
            exit_code: Some(0),
            stdout: Some(stdout),
            stderr: Some(String::new()),
            duration_ms: Some(start.elapsed().as_millis() as u64),
            error: None,
        },
        Err(code) => error_result(start, &code),
    }
}

fn serialize_response<T: Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string(&value).map_err(|_| "skill_store_response_invalid".to_string())
}

fn error_result(start: Instant, code: &str) -> CommandResult {
    CommandResult {
        exit_code: None,
        stdout: None,
        stderr: None,
        duration_ms: Some(start.elapsed().as_millis() as u64),
        error: Some(code.to_string()),
    }
}

fn prepare_archive(
    archive_bytes: &[u8],
    artifact_sha256: String,
) -> Result<PreparedPackage, String> {
    let cursor = Cursor::new(archive_bytes);
    let mut archive =
        ZipArchive::new(cursor).map_err(|_| "skill_install_archive_malformed".to_string())?;
    if archive.len() > MAX_SKILL_STORE_FILE_COUNT.saturating_mul(2) {
        return Err("skill_install_file_count_exceeded".to_string());
    }
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut casefold = BTreeSet::<String>::new();
    let mut total_bytes = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| "skill_install_archive_malformed".to_string())?;
        let raw_name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| "skill_install_archive_path_invalid".to_string())?;
        let is_dir = entry.is_dir();
        let normalized = normalize_archive_path(raw_name, is_dir)?;
        reject_archive_special_entry(entry.unix_mode(), is_dir)?;
        if is_dir {
            continue;
        }
        if files.len() >= MAX_SKILL_STORE_FILE_COUNT {
            return Err("skill_install_file_count_exceeded".to_string());
        }
        let size = usize::try_from(entry.size())
            .map_err(|_| "skill_install_file_too_large".to_string())?;
        if size > MAX_SKILL_STORE_FILE_BYTES {
            return Err("skill_install_file_too_large".to_string());
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| "skill_install_total_too_large".to_string())?;
        if total_bytes > MAX_SKILL_STORE_TOTAL_BYTES {
            return Err("skill_install_total_too_large".to_string());
        }
        let folded = normalized.to_lowercase();
        if files.contains_key(&normalized) || !casefold.insert(folded) {
            return Err("skill_install_duplicate_path".to_string());
        }
        let mut bytes = Vec::with_capacity(size.min(MAX_SKILL_STORE_FILE_BYTES));
        entry
            .by_ref()
            .take((MAX_SKILL_STORE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "skill_install_archive_malformed".to_string())?;
        if bytes.len() != size || bytes.len() > MAX_SKILL_STORE_FILE_BYTES {
            return Err("skill_install_archive_size_mismatch".to_string());
        }
        files.insert(normalized, bytes);
    }
    let definition = files
        .get("SKILL.md")
        .ok_or_else(|| "skill_definition_missing".to_string())?;
    if definition.len() > MAX_SKILL_DEFINITION_BYTES {
        return Err("skill_definition_too_large".to_string());
    }
    let definition_text =
        std::str::from_utf8(definition).map_err(|_| "skill_definition_invalid_utf8".to_string())?;
    let metadata = parse_skill_metadata(definition_text).map_err(str::to_string)?;
    let definition_revision = sha256_hex(definition);
    let package_revision = compute_package_revision(&files);
    Ok(PreparedPackage {
        file_count: files.len(),
        total_bytes,
        files,
        package_revision,
        definition_revision,
        artifact_sha256,
        metadata,
    })
}

fn compute_package_revision(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-skill-package-v1\0");
    for (path, bytes) in files {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    format!("wc_skillpkg_{:x}", hasher.finalize())
}

fn snapshot_installed_package(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|_| "skill_store_revision_modified".to_string())?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("skill_store_revision_modified".to_string());
    }

    fn visit(
        directory: &Path,
        prefix: &str,
        files: &mut BTreeMap<String, Vec<u8>>,
        casefold: &mut BTreeSet<String>,
        total_bytes: &mut usize,
        entry_count: &mut usize,
    ) -> Result<(), String> {
        let entries =
            fs::read_dir(directory).map_err(|_| "skill_store_revision_modified".to_string())?;
        for entry in entries {
            let entry = entry.map_err(|_| "skill_store_revision_modified".to_string())?;
            *entry_count = entry_count
                .checked_add(1)
                .ok_or_else(|| "skill_store_revision_modified".to_string())?;
            if *entry_count > MAX_SKILL_STORE_FILE_COUNT.saturating_mul(2) {
                return Err("skill_store_revision_modified".to_string());
            }
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| "skill_store_revision_modified".to_string())?
                .to_string();
            if name.contains('\\') || name.chars().any(char::is_control) {
                return Err("skill_store_revision_modified".to_string());
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            validate_resource_path(&relative)
                .map_err(|_| "skill_store_revision_modified".to_string())?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| "skill_store_revision_modified".to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("skill_store_revision_modified".to_string());
            }
            if metadata.is_dir() {
                visit(&path, &relative, files, casefold, total_bytes, entry_count)?;
                continue;
            }
            if !metadata.is_file()
                || metadata.len() > MAX_SKILL_STORE_FILE_BYTES as u64
                || files.len() >= MAX_SKILL_STORE_FILE_COUNT
            {
                return Err("skill_store_revision_modified".to_string());
            }
            let folded = relative.to_lowercase();
            if files.contains_key(&relative) || !casefold.insert(folded) {
                return Err("skill_store_revision_modified".to_string());
            }
            let bytes = fs::read(&path).map_err(|_| "skill_store_revision_modified".to_string())?;
            if bytes.len() as u64 != metadata.len() || bytes.len() > MAX_SKILL_STORE_FILE_BYTES {
                return Err("skill_store_revision_modified".to_string());
            }
            *total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| "skill_store_revision_modified".to_string())?;
            if *total_bytes > MAX_SKILL_STORE_TOTAL_BYTES {
                return Err("skill_store_revision_modified".to_string());
            }
            files.insert(relative, bytes);
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    let mut casefold = BTreeSet::new();
    let mut total_bytes = 0usize;
    let mut entry_count = 0usize;
    visit(
        root,
        "",
        &mut files,
        &mut casefold,
        &mut total_bytes,
        &mut entry_count,
    )?;
    Ok(files)
}

fn normalize_archive_path(raw: &str, is_dir: bool) -> Result<String, String> {
    if raw.is_empty()
        || raw.chars().count() > MAX_SKILL_STORE_PATH_CHARS
        || raw.contains(['\\', '\0'])
        || raw.starts_with('/')
        || raw.as_bytes().get(1) == Some(&b':')
        || raw.chars().any(char::is_control)
    {
        return Err("skill_install_archive_path_invalid".to_string());
    }
    let normalized = if is_dir {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if normalized.is_empty() {
        return Err("skill_install_archive_path_invalid".to_string());
    }
    let components = normalized.split('/').collect::<Vec<_>>();
    if components.len() > MAX_SKILL_STORE_PATH_DEPTH
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
        || Path::new(normalized)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("skill_install_archive_path_invalid".to_string());
    }
    Ok(normalized.to_string())
}

fn reject_archive_special_entry(mode: Option<u32>, is_dir: bool) -> Result<(), String> {
    let Some(mode) = mode else { return Ok(()) };
    let kind = mode & 0o170000;
    if kind == 0 {
        return Ok(());
    }
    let allowed = if is_dir {
        kind == 0o040000
    } else {
        kind == 0o100000
    };
    if allowed {
        Ok(())
    } else {
        Err("skill_install_archive_special_entry".to_string())
    }
}

fn validate_resource_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty()
        || path.chars().count() > MAX_SKILL_STORE_PATH_CHARS
        || path.chars().any(char::is_control)
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.as_bytes().get(1) == Some(&b':')
    {
        return Err("skill_resource_path_invalid".to_string());
    }
    let normalized = path.replace('\\', "/");
    let components = normalized.split('/').collect::<Vec<_>>();
    if components.len() > MAX_SKILL_STORE_PATH_DEPTH
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
        || Path::new(&normalized)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("skill_resource_path_invalid".to_string());
    }
    Ok(normalized)
}

fn resolve_package_regular_file(
    root: &Path,
    relative: &str,
    missing_code: &str,
    invalid_code: &str,
) -> Result<PathBuf, String> {
    let root_metadata = fs::symlink_metadata(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing_code.to_string()
        } else {
            invalid_code.to_string()
        }
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(invalid_code.to_string());
    }
    let components = relative.split('/').collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                missing_code.to_string()
            } else {
                invalid_code.to_string()
            }
        })?;
        let last = index + 1 == components.len();
        if metadata.file_type().is_symlink()
            || (last && !metadata.is_file())
            || (!last && !metadata.is_dir())
        {
            return Err(invalid_code.to_string());
        }
    }
    let canonical_root = root.canonicalize().map_err(|_| invalid_code.to_string())?;
    let target = current
        .canonicalize()
        .map_err(|_| invalid_code.to_string())?;
    if !target.starts_with(&canonical_root) {
        return Err(invalid_code.to_string());
    }
    Ok(target)
}

fn validate_management_common(skill_key: &str, idempotency_key: &str) -> Result<(), String> {
    if !valid_skill_key(skill_key)
        || idempotency_key.is_empty()
        || idempotency_key.chars().count() > MAX_SKILL_STORE_IDEMPOTENCY_KEY_CHARS
        || idempotency_key.chars().any(char::is_control)
    {
        Err("skill_store_invalid_request".to_string())
    } else {
        Ok(())
    }
}

fn valid_runtime_skill_id(value: &str) -> bool {
    value.len() == "wc_skill_".len() + 32
        && value.starts_with("wc_skill_")
        && value["wc_skill_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hash_install_intent(
    skill_key: &str,
    source_project_id: &str,
    artifact_path: &str,
    archive_sha256: &str,
    activate: bool,
    expected_state_revision: Option<&str>,
) -> String {
    let expected = expected_state_revision.unwrap_or_default();
    hash_simple_intent(
        "install",
        &[
            skill_key,
            source_project_id,
            artifact_path,
            archive_sha256,
            if activate { "activate" } else { "inactive" },
            expected,
        ],
    )
}

fn hash_simple_intent(operation: &str, values: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-skill-store-intent-v1\0");
    hasher.update(operation.as_bytes());
    for value in values {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn ensure_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|_| "skill_store_directory_unavailable".to_string())?;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "skill_store_directory_unavailable".to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("skill_store_directory_invalid".to_string());
    }
    Ok(())
}

fn reject_symlink_or_non_file_if_exists(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(format!("{label} is unavailable")),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(format!("{label} must be a regular non-symlink file"))
        }
        Ok(_) => Ok(()),
    }
}

fn read_file_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "skill_store_file_unavailable".to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes as u64
    {
        return Err("skill_store_file_invalid".to_string());
    }
    let bytes = fs::read(path).map_err(|_| "skill_store_file_unavailable".to_string())?;
    if bytes.len() > max_bytes {
        return Err("skill_store_file_invalid".to_string());
    }
    Ok(bytes)
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max_bytes: usize,
) -> Result<T, String> {
    let bytes = read_file_bounded(path, max_bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| "skill_store_json_invalid".to_string())
}

fn write_new_json<T: Serialize>(path: &Path, value: &T, max_bytes: usize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "skill_store_json_invalid".to_string())?;
    if bytes.len() > max_bytes {
        return Err("skill_store_json_too_large".to_string());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    file.write_all(&bytes)
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    file.sync_all()
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T, max_bytes: usize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "skill_store_json_invalid".to_string())?;
    if bytes.len() > max_bytes {
        return Err("skill_store_json_too_large".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "skill_store_json_path_invalid".to_string())?;
    ensure_dir(parent)?;
    reject_symlink_or_non_file_if_exists(path, "Skill store JSON state")?;
    let temp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state")
    ));
    match fs::symlink_metadata(&temp) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("skill_store_temp_invalid".to_string());
            }
            fs::remove_file(&temp).map_err(|_| "skill_store_temp_cleanup_failed".to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("skill_store_temp_unavailable".to_string()),
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    file.write_all(&bytes)
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    file.sync_all()
        .map_err(|_| "skill_store_json_write_failed".to_string())?;
    replace_file_atomic(&temp, path)?;
    sync_parent(path)?;
    Ok(())
}

#[cfg(unix)]
fn replace_file_atomic(temp: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temp, path).map_err(|_| "skill_store_json_commit_failed".to_string())
}

#[cfg(windows)]
fn replace_file_atomic(temp: &Path, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } != 0
    {
        Ok(())
    } else {
        Err("skill_store_json_commit_failed".to_string())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_file_atomic(_temp: &Path, _path: &Path) -> Result<(), String> {
    Err("skill_store_atomic_replace_unsupported".to_string())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "skill_store_parent_invalid".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "skill_store_directory_sync_failed".to_string())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn zip_bytes(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        for (name, body, mode) in entries {
            let mut options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            if let Some(mode) = mode {
                options = options.unix_permissions(*mode);
            }
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn zip_bytes_stored(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        for (name, body, mode) in entries {
            let mut options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            if let Some(mode) = mode {
                options = options.unix_permissions(*mode);
            }
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn zip_with_symlink() -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        writer
            .start_file(
                "SKILL.md",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer
            .write_all(b"---\nname: x\ndescription: x\n---\n")
            .unwrap();
        writer
            .add_symlink("link", "target", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn retention_ms(seconds: u64) -> i64 {
        i64::try_from(seconds).unwrap() * 1000
    }

    fn replay_fixture(status: &str, timestamp_unix_ms: Option<i64>) -> ReplayRecord {
        ReplayRecord {
            schema_version: STORE_SCHEMA_VERSION,
            operation: "activate".to_string(),
            intent_hash: "a".repeat(64),
            status: status.to_string(),
            created_at_unix_ms: timestamp_unix_ms,
            updated_at_unix_ms: timestamp_unix_ms,
            result: (status == "completed").then(|| serde_json::json!({"ok": true})),
        }
    }

    fn write_replay_fixture(store: &SkillStore, key: &str, record: &ReplayRecord) {
        store.initialize().unwrap();
        fs::write(store.replay_path(key), serde_json::to_vec(record).unwrap()).unwrap();
    }

    fn replay_json_count(store: &SkillStore) -> usize {
        fs::read_dir(store.root.join(REPLAY_DIR))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(valid_replay_file_name)
            })
            .count()
    }

    #[test]
    fn replay_atomic_writes_keep_crash_debris_out_of_gc_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let now = 2_000_000_000_000i64;
        let intent = "a".repeat(64);

        let lock = store.lock().unwrap();
        assert!(matches!(
            store
                .begin_replay_at(&lock, "atomic-key", "activate", &intent, now)
                .unwrap(),
            ReplayState::First
        ));
        store
            .prepare_replay_at(&lock, "atomic-key", now + 1)
            .unwrap();
        store
            .complete_replay_at(
                &lock,
                "atomic-key",
                &serde_json::json!({"ok": true}),
                now + 2,
            )
            .unwrap();
        drop(lock);

        let replay_names = fs::read_dir(store.root.join(REPLAY_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(replay_names.len(), 1);
        assert!(replay_names.iter().all(|name| valid_replay_file_name(name)));

        // A crash may leave the staging subdirectory used by replay atomic
        // writes. Existing store-lock cleanup owns this debris and removes it
        // before a later operation; replay/ itself stays schema-clean for GC.
        let orphan = store.root.join(STAGING_DIR).join("replay-interrupted");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("record.json"), b"partial").unwrap();
        let lock = store.lock().unwrap();
        assert!(!orphan.exists());
        assert!(matches!(
            store
                .begin_replay_at(&lock, "after-crash", "activate", &"b".repeat(64), now + 3)
                .unwrap(),
            ReplayState::First
        ));
        drop(lock);
    }

    #[test]
    fn replay_retention_gc_respects_claimed_prepared_and_completed_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        store.initialize().unwrap();
        let now = 2_000_000_000_000i64;
        let claimed = retention_ms(SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS);
        let effect = retention_ms(SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS);
        assert_eq!(MAX_SKILL_STORE_REPLAY_RECORDS, 1024);
        assert_eq!(MAX_SKILL_STORE_REPLAY_SCAN_ENTRIES, 4096);
        assert_eq!(
            webcodex_core::skill_store::MAX_SKILL_STORE_REPLAY_RETAINED_BYTES,
            64 * 1024 * 1024
        );

        write_replay_fixture(
            &store,
            "claimed-fresh",
            &replay_fixture("claimed", Some(now - claimed + 1)),
        );
        write_replay_fixture(
            &store,
            "claimed-expired",
            &replay_fixture("claimed", Some(now - claimed)),
        );
        write_replay_fixture(
            &store,
            "prepared-fresh",
            &replay_fixture("prepared", Some(now - effect + 1)),
        );
        write_replay_fixture(
            &store,
            "prepared-expired",
            &replay_fixture("prepared", Some(now - effect)),
        );
        write_replay_fixture(
            &store,
            "completed-fresh",
            &replay_fixture("completed", Some(now - effect + 1)),
        );
        write_replay_fixture(
            &store,
            "completed-expired",
            &replay_fixture("completed", Some(now - effect)),
        );

        let lock = store.lock().unwrap();
        assert!(matches!(
            store
                .begin_replay_at(&lock, "new-key", "activate", &"b".repeat(64), now)
                .unwrap(),
            ReplayState::First
        ));
        drop(lock);

        for key in [
            "claimed-fresh",
            "prepared-fresh",
            "completed-fresh",
            "new-key",
        ] {
            assert!(store.replay_path(key).is_file(), "retained {key}");
        }
        for key in ["claimed-expired", "prepared-expired", "completed-expired"] {
            assert!(!store.replay_path(key).exists(), "expired {key}");
        }
        assert_eq!(replay_json_count(&store), 4);
    }

    #[test]
    fn claimed_intent_binding_is_retention_bounded_and_expired_key_can_be_reclaimed() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let now = 2_000_000_000_000i64;
        let intent_a = "a".repeat(64);
        let intent_b = "b".repeat(64);
        let lock = store.lock().unwrap();
        assert!(matches!(
            store
                .begin_replay_at(&lock, "bounded-key", "activate", &intent_a, now)
                .unwrap(),
            ReplayState::First
        ));
        let claimed: ReplayRecord = read_json_bounded(
            &store.replay_path("bounded-key"),
            MAX_SKILL_STORE_REPLAY_RECORD_BYTES,
        )
        .unwrap();
        assert_eq!(claimed.created_at_unix_ms, Some(now));
        assert_eq!(claimed.updated_at_unix_ms, Some(now));
        assert_eq!(
            store
                .begin_replay_at(&lock, "bounded-key", "activate", &intent_b, now + 1)
                .unwrap_err(),
            "skill_idempotency_conflict"
        );
        drop(lock);

        let after_retention = now + retention_ms(SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS);
        let lock = store.lock().unwrap();
        assert!(matches!(
            store
                .begin_replay_at(
                    &lock,
                    "gc-trigger",
                    "activate",
                    &"c".repeat(64),
                    after_retention
                )
                .unwrap(),
            ReplayState::First
        ));
        assert!(!store.replay_path("bounded-key").exists());
        assert!(matches!(
            store
                .begin_replay_at(&lock, "bounded-key", "activate", &intent_b, after_retention)
                .unwrap(),
            ReplayState::First
        ));
        drop(lock);
    }

    #[test]
    fn prepared_retry_refreshes_effect_retention_before_reentering_effect_boundary() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let first_prepare = 2_000_000_000_000i64;
        write_replay_fixture(
            &store,
            "prepared-retry-refresh",
            &replay_fixture("prepared", Some(first_prepare)),
        );
        let retry_prepare =
            first_prepare + retention_ms(SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS) - 1;
        let lock = store.lock().unwrap();
        store
            .prepare_replay_at(&lock, "prepared-retry-refresh", retry_prepare)
            .unwrap();
        drop(lock);
        let refreshed: ReplayRecord = read_json_bounded(
            &store.replay_path("prepared-retry-refresh"),
            MAX_SKILL_STORE_REPLAY_RECORD_BYTES,
        )
        .unwrap();
        assert_eq!(refreshed.status, "prepared");
        assert_eq!(refreshed.updated_at_unix_ms, Some(retry_prepare));

        let lock = store.lock().unwrap();
        assert_eq!(
            store
                .gc_expired_replays_locked_at(
                    &lock,
                    retry_prepare + retention_ms(SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS) - 1,
                )
                .unwrap(),
            1
        );
        drop(lock);
        assert!(store.replay_path("prepared-retry-refresh").is_file());
    }

    #[test]
    fn replay_capacity_rejects_new_key_but_preserves_existing_completed_and_prepared_recovery() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let archive = zip_bytes(&[(
            "SKILL.md",
            b"---\nname: demo\ndescription: capacity\n---\n",
            None,
        )]);
        fs::write(source.path().join("skill.zip"), &archive).unwrap();
        let source_root = source.path().to_string_lossy().to_string();
        let archive_sha = sha256_hex(&archive);
        let installed = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "skill.zip",
                &archive_sha,
                "setup-install",
                true,
                None,
            )
            .unwrap();
        let stale_expected = format!("wc_skillstate_{}", "f".repeat(64));
        let prepared_intent = hash_simple_intent(
            "activate",
            &["demo", &installed.package_revision, &stale_expected],
        );
        let now = chrono::Utc::now().timestamp_millis();
        let mut prepared_record = replay_fixture("prepared", Some(now));
        prepared_record.intent_hash = prepared_intent;
        write_replay_fixture(&store, "prepared-existing", &prepared_record);

        let existing = replay_json_count(&store);
        assert_eq!(existing, 2);
        for index in 0..(MAX_SKILL_STORE_REPLAY_RECORDS - existing) {
            write_replay_fixture(
                &store,
                &format!("capacity-fill-{index}"),
                &replay_fixture("completed", Some(now)),
            );
        }
        assert_eq!(replay_json_count(&store), MAX_SKILL_STORE_REPLAY_RECORDS);
        let before = store.versions("demo", 0, 64).unwrap();
        let new_key = "capacity-new-key";
        assert_eq!(
            store
                .activate(
                    "demo",
                    &installed.package_revision,
                    &before.state_revision,
                    new_key,
                )
                .unwrap_err(),
            "skill_store_replay_capacity_exceeded"
        );
        assert!(!store.replay_path(new_key).exists());
        let after = store.versions("demo", 0, 64).unwrap();
        assert_eq!(after.state_revision, before.state_revision);
        assert_eq!(
            after.active_package_revision,
            before.active_package_revision
        );
        assert!(store.replay_path("setup-install").is_file());
        assert!(store.replay_path("prepared-existing").is_file());

        let completed_replay = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "skill.zip",
                &archive_sha,
                "setup-install",
                true,
                None,
            )
            .unwrap();
        assert!(completed_replay.replayed);

        let prepared_recovery = store
            .activate(
                "demo",
                &installed.package_revision,
                &stale_expected,
                "prepared-existing",
            )
            .unwrap();
        assert!(prepared_recovery.replayed);
        assert!(!prepared_recovery.changed);
        assert_eq!(replay_json_count(&store), MAX_SKILL_STORE_REPLAY_RECORDS);
    }

    #[test]
    fn replay_capacity_gc_admits_new_key_only_by_expiring_eligible_record() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let now = 2_000_000_000_000i64;
        let claimed_retention = retention_ms(SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS);
        write_replay_fixture(
            &store,
            "expired-capacity-slot",
            &replay_fixture("claimed", Some(now - claimed_retention)),
        );
        for index in 0..(MAX_SKILL_STORE_REPLAY_RECORDS - 1) {
            write_replay_fixture(
                &store,
                &format!("retained-capacity-{index}"),
                &replay_fixture("completed", Some(now)),
            );
        }
        let retained_sample = store.replay_path("retained-capacity-0");
        let lock = store.lock().unwrap();
        assert!(matches!(
            store
                .begin_replay_at(&lock, "replacement-slot", "activate", &"b".repeat(64), now)
                .unwrap(),
            ReplayState::First
        ));
        drop(lock);
        assert!(!store.replay_path("expired-capacity-slot").exists());
        assert!(retained_sample.is_file());
        assert!(store.replay_path("replacement-slot").is_file());
        assert_eq!(replay_json_count(&store), MAX_SKILL_STORE_REPLAY_RECORDS);
    }

    #[test]
    fn replay_gc_fails_closed_on_wrong_name_malformed_and_overlarge_entries() {
        let now = 2_000_000_000_000i64;

        for case in ["wrong-name", "malformed", "overlarge"] {
            let root = tempfile::tempdir().unwrap();
            let store = SkillStore::for_test(root.path().join("store"), "runner");
            store.initialize().unwrap();
            let expired = store.replay_path("expired-but-must-survive-invalid-scan");
            fs::write(
                &expired,
                serde_json::to_vec(&replay_fixture(
                    "claimed",
                    Some(now - retention_ms(SKILL_STORE_REPLAY_CLAIMED_RETENTION_SECS)),
                ))
                .unwrap(),
            )
            .unwrap();
            match case {
                "wrong-name" => {
                    fs::write(store.root.join(REPLAY_DIR).join("not-a-replay.json"), b"{}")
                        .unwrap();
                }
                "malformed" => {
                    fs::write(store.replay_path("malformed-entry"), b"{").unwrap();
                }
                "overlarge" => {
                    fs::write(
                        store.replay_path("overlarge-entry"),
                        vec![b'x'; MAX_SKILL_STORE_REPLAY_RECORD_BYTES + 1],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let lock = store.lock().unwrap();
            assert_eq!(
                store
                    .begin_replay_at(&lock, "new-key", "activate", &"b".repeat(64), now)
                    .unwrap_err(),
                "skill_store_replay_invalid_entry",
                "{case}"
            );
            drop(lock);
            assert!(
                expired.is_file(),
                "two-phase GC must not delete before {case} failure"
            );
            assert!(!store.replay_path("new-key").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn replay_gc_never_follows_or_deletes_symlink_entries() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        store.initialize().unwrap();
        let outside_record = outside.path().join("record.json");
        fs::write(
            &outside_record,
            serde_json::to_vec(&replay_fixture("completed", Some(1))).unwrap(),
        )
        .unwrap();
        let linked = store.replay_path("linked-entry");
        symlink(&outside_record, &linked).unwrap();
        let lock = store.lock().unwrap();
        assert_eq!(
            store
                .begin_replay_at(
                    &lock,
                    "new-key",
                    "activate",
                    &"b".repeat(64),
                    2_000_000_000_000,
                )
                .unwrap_err(),
            "skill_store_replay_invalid_entry"
        );
        drop(lock);
        assert!(linked.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(outside_record.is_file());
        assert!(!store.replay_path("new-key").exists());
    }

    #[test]
    fn replay_scan_bound_fails_closed_while_exact_existing_key_still_replays() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let now = 2_000_000_000_000i64;
        for index in 0..=MAX_SKILL_STORE_REPLAY_SCAN_ENTRIES {
            write_replay_fixture(
                &store,
                &format!("scan-entry-{index}"),
                &replay_fixture("completed", Some(now)),
            );
        }
        let lock = store.lock().unwrap();
        assert_eq!(
            store
                .begin_replay_at(&lock, "scan-new", "activate", &"b".repeat(64), now)
                .unwrap_err(),
            "skill_store_replay_capacity_unavailable"
        );
        assert!(!store.replay_path("scan-new").exists());
        assert!(matches!(
            store
                .begin_replay_at(&lock, "scan-entry-0", "activate", &"a".repeat(64), now)
                .unwrap(),
            ReplayState::Completed(_)
        ));
        drop(lock);
    }

    #[test]
    fn replay_retention_survives_runner_restart_and_legacy_mtime_is_conservative() {
        let root = tempfile::tempdir().unwrap();
        let store_root = root.path().join("store");
        let store = SkillStore::for_test_persisted(store_root.clone()).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let effect = retention_ms(SKILL_STORE_REPLAY_EFFECT_RETENTION_SECS);
        write_replay_fixture(
            &store,
            "timestamped-prepared",
            &replay_fixture("prepared", Some(now)),
        );
        write_replay_fixture(
            &store,
            "legacy-completed",
            &replay_fixture("completed", None),
        );
        let legacy_mtime = fs::symlink_metadata(store.replay_path("legacy-completed"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        let legacy_mtime_ms = i64::try_from(legacy_mtime.as_millis()).unwrap();
        drop(store);

        let restarted = SkillStore::for_test_persisted(store_root).unwrap();
        // The timestamped record is written before the legacy record whose file
        // mtime is observed below. On a loaded filesystem those two clocks can
        // differ by multiple milliseconds, so use the earlier age basis when
        // proving both records are still just inside retention.
        let fresh_now = now.min(legacy_mtime_ms) + effect - 1;
        let lock = restarted.lock().unwrap();
        restarted
            .gc_expired_replays_locked_at(&lock, fresh_now)
            .unwrap();
        drop(lock);
        assert!(restarted.replay_path("timestamped-prepared").is_file());
        assert!(restarted.replay_path("legacy-completed").is_file());

        let mut partial_legacy = replay_fixture("prepared", None);
        partial_legacy.created_at_unix_ms = Some(now - effect * 20);
        write_replay_fixture(&restarted, "partial-timestamp", &partial_legacy);

        let mut clock_regressed = replay_fixture("claimed", Some(now));
        advance_replay_timestamp(&mut clock_regressed, now - 1);
        assert_eq!(clock_regressed.updated_at_unix_ms, Some(i64::MAX));
        write_replay_fixture(&restarted, "clock-regressed-transition", &clock_regressed);

        let abnormal_created = now + effect * 20;
        let mut invalid_order = replay_fixture("prepared", Some(abnormal_created));
        invalid_order.updated_at_unix_ms = Some(now);
        write_replay_fixture(&restarted, "invalid-clock-order", &invalid_order);
        let mut future_updated = replay_fixture("completed", Some(now));
        future_updated.updated_at_unix_ms = Some(i64::MAX);
        write_replay_fixture(&restarted, "future-clock", &future_updated);

        let final_now = (now + effect + 1).max(legacy_mtime_ms + effect + 1);
        let lock = restarted.lock().unwrap();
        restarted
            .gc_expired_replays_locked_at(&lock, final_now)
            .unwrap();
        drop(lock);
        assert!(!restarted.replay_path("timestamped-prepared").exists());
        assert!(!restarted.replay_path("legacy-completed").exists());
        assert!(restarted.replay_path("partial-timestamp").is_file());
        assert!(restarted
            .replay_path("clock-regressed-transition")
            .is_file());
        assert!(restarted.replay_path("invalid-clock-order").is_file());
        assert!(restarted.replay_path("future-clock").is_file());
    }

    #[test]
    fn prepared_effect_with_replay_completion_failure_recovers_same_key_without_second_effect() {
        let source = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let definition = b"---\nname: demo\ndescription: replay failure\n---\n";
        let archive_a = zip_bytes(&[("SKILL.md", definition, None), ("a.txt", b"a", None)]);
        let archive_b = zip_bytes(&[("SKILL.md", definition, None), ("a.txt", b"b", None)]);
        fs::write(source.path().join("a.zip"), &archive_a).unwrap();
        fs::write(source.path().join("b.zip"), &archive_b).unwrap();
        let source_root = source.path().to_string_lossy().to_string();
        let a = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "a.zip",
                &sha256_hex(&archive_a),
                "install-a-replay-failure",
                true,
                None,
            )
            .unwrap();
        let b = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "b.zip",
                &sha256_hex(&archive_b),
                "install-b-replay-failure",
                false,
                None,
            )
            .unwrap();
        let before = store.versions("demo", 0, 64).unwrap();
        assert_eq!(
            before.active_package_revision.as_deref(),
            Some(a.package_revision.as_str())
        );
        store
            .fail_next_replay_completion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store
                .activate(
                    "demo",
                    &b.package_revision,
                    &before.state_revision,
                    "activate-b-replay-failure",
                )
                .unwrap_err(),
            "skill_store_replay_commit_failed"
        );
        let after_effect = store.versions("demo", 0, 64).unwrap();
        assert_eq!(
            after_effect.active_package_revision.as_deref(),
            Some(b.package_revision.as_str())
        );
        let prepared: ReplayRecord = read_json_bounded(
            &store.replay_path("activate-b-replay-failure"),
            MAX_SKILL_STORE_REPLAY_RECORD_BYTES,
        )
        .unwrap();
        assert_eq!(prepared.status, "prepared");
        assert!(prepared.created_at_unix_ms.is_some());
        assert!(prepared.updated_at_unix_ms >= prepared.created_at_unix_ms);

        let recovered = store
            .activate(
                "demo",
                &b.package_revision,
                &before.state_revision,
                "activate-b-replay-failure",
            )
            .unwrap();
        assert!(recovered.replayed);
        assert!(!recovered.changed);
        let after_recovery = store.versions("demo", 0, 64).unwrap();
        assert_eq!(after_recovery.state_revision, after_effect.state_revision);
        let completed: ReplayRecord = read_json_bounded(
            &store.replay_path("activate-b-replay-failure"),
            MAX_SKILL_STORE_REPLAY_RECORD_BYTES,
        )
        .unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.result.is_some());
        assert!(completed.updated_at_unix_ms >= prepared.updated_at_unix_ms);
    }

    #[test]
    fn package_revision_covers_resource_tree_not_only_definition() {
        let a = zip_bytes(&[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: demo\n---\n",
                None,
            ),
            ("references/a.md", b"one", None),
        ]);
        let b = zip_bytes(&[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: demo\n---\n",
                None,
            ),
            ("references/a.md", b"two", None),
        ]);
        let pa = prepare_archive(&a, sha256_hex(&a)).unwrap();
        let pb = prepare_archive(&b, sha256_hex(&b)).unwrap();
        assert_eq!(pa.definition_revision, pb.definition_revision);
        assert_ne!(pa.package_revision, pb.package_revision);
    }

    #[test]
    fn identical_package_tree_reuses_revision_across_zip_encodings() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(store_root.path().join("store"), "runner-a");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let entries: &[(&str, &[u8], Option<u32>)] = &[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: same tree\n---\n",
                None,
            ),
            ("references/guide.md", b"same resource\n", None),
        ];
        let deflated = zip_bytes(entries);
        let stored = zip_bytes_stored(entries);
        assert_ne!(sha256_hex(&deflated), sha256_hex(&stored));
        let deflated_prepared = prepare_archive(&deflated, sha256_hex(&deflated)).unwrap();
        let stored_prepared = prepare_archive(&stored, sha256_hex(&stored)).unwrap();
        assert_eq!(
            deflated_prepared.package_revision,
            stored_prepared.package_revision
        );
        fs::write(source.path().join("deflated.zip"), &deflated).unwrap();
        fs::write(source.path().join("stored.zip"), &stored).unwrap();
        let source_root = source.path().to_string_lossy().to_string();

        let first = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "deflated.zip",
                &sha256_hex(&deflated),
                "install-deflated",
                false,
                None,
            )
            .unwrap();
        assert!(first.installed);
        let second = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "stored.zip",
                &sha256_hex(&stored),
                "install-stored",
                false,
                None,
            )
            .unwrap();
        assert!(!second.installed);
        assert_eq!(second.package_revision, first.package_revision);
        assert_eq!(store.versions("demo", 0, 64).unwrap().total_count, 1);
    }

    #[test]
    fn prepared_install_replay_recovers_equivalent_zip_encoding_without_second_effect() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(store_root.path().join("store"), "runner-a");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let entries: &[(&str, &[u8], Option<u32>)] = &[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: replay same tree\n---\n",
                None,
            ),
            ("references/guide.md", b"same resource\n", None),
        ];
        let deflated = zip_bytes(entries);
        let stored = zip_bytes_stored(entries);
        let deflated_sha = sha256_hex(&deflated);
        let stored_sha = sha256_hex(&stored);
        assert_ne!(deflated_sha, stored_sha);
        fs::write(source.path().join("deflated.zip"), &deflated).unwrap();
        fs::write(source.path().join("stored.zip"), &stored).unwrap();
        let source_root = source.path().to_string_lossy().to_string();

        let first = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "deflated.zip",
                &deflated_sha,
                "install-deflated-replay-base",
                false,
                None,
            )
            .unwrap();
        let before = store.versions("demo", 0, 64).unwrap();
        assert!(before.active_package_revision.is_none());

        store
            .fail_next_replay_completion
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store
                .install(
                    &policy,
                    "demo",
                    "project-a",
                    &source_root,
                    "stored.zip",
                    &stored_sha,
                    "install-stored-replay-failure",
                    true,
                    Some(&before.state_revision),
                )
                .unwrap_err(),
            "skill_store_replay_commit_failed"
        );
        let after_effect = store.versions("demo", 0, 64).unwrap();
        assert_eq!(
            after_effect.active_package_revision.as_deref(),
            Some(first.package_revision.as_str())
        );

        let recovered = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "stored.zip",
                &stored_sha,
                "install-stored-replay-failure",
                true,
                Some(&before.state_revision),
            )
            .unwrap();
        assert!(recovered.replayed);
        assert!(!recovered.installed);
        assert!(!recovered.activated);
        assert_eq!(recovered.package_revision, first.package_revision);
        assert_eq!(recovered.artifact_sha256, stored_sha);
        let after_recovery = store.versions("demo", 0, 64).unwrap();
        assert_eq!(after_recovery.state_revision, after_effect.state_revision);
        let completed: ReplayRecord = read_json_bounded(
            &store.replay_path("install-stored-replay-failure"),
            MAX_SKILL_STORE_REPLAY_RECORD_BYTES,
        )
        .unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.result.is_some());
    }

    #[test]
    fn archive_rejects_traversal_absolute_duplicate_case_and_special_entries() {
        for bytes in [
            zip_bytes(&[("../escape", b"x", None)]),
            zip_bytes(&[("/absolute", b"x", None)]),
            zip_bytes(&[
                ("SKILL.md", b"---\nname: x\ndescription: x\n---\n", None),
                ("A.md", b"x", None),
                ("a.md", b"y", None),
            ]),
            zip_with_symlink(),
        ] {
            assert!(prepare_archive(&bytes, sha256_hex(&bytes)).is_err());
        }
    }

    #[test]
    fn versions_clamps_out_of_range_offset_deterministically() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let versions = store.versions("demo", 99, 10).unwrap();
        assert_eq!(versions.total_count, 0);
        assert_eq!(versions.offset, 0);
        assert_eq!(versions.next_offset, None);
        assert!(versions.versions.is_empty());
    }

    #[test]
    fn store_identity_is_persisted_and_distinct_across_runner_stores() {
        let root = tempfile::tempdir().unwrap();
        let a_root = root.path().join("a");
        let b_root = root.path().join("b");
        let a = SkillStore::for_test_persisted(a_root.clone()).unwrap();
        let a_after_restart = SkillStore::for_test_persisted(a_root).unwrap();
        let b = SkillStore::for_test_persisted(b_root).unwrap();
        assert_eq!(a.skill_id("demo"), a_after_restart.skill_id("demo"));
        assert_ne!(a.skill_id("demo"), b.skill_id("demo"));
        assert_ne!(a.namespace, b.namespace);
    }

    #[test]
    fn failed_state_write_leaves_previous_active_pointer() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        let _lock = store.lock().unwrap();
        let skill = "demo";
        let old = SkillActiveState {
            schema_version: STORE_SCHEMA_VERSION,
            active_package_revision: None,
        };
        store.write_state(skill, &old).unwrap();
        store
            .fail_next_state_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let next = SkillActiveState {
            schema_version: STORE_SCHEMA_VERSION,
            active_package_revision: Some(format!("wc_skillpkg_{}", "a".repeat(64))),
        };
        assert!(store.write_state(skill, &next).is_err());
        assert_eq!(
            store.read_state(skill).unwrap().active_package_revision,
            None
        );
    }

    #[test]
    fn install_activate_rollback_remove_and_replay_are_consistent() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(store_root.path().join("store"), "runner-a");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let definition = b"---\nname: demo\ndescription: operator demo\n---\nGuidance only.\n";
        let archive_a = zip_bytes(&[
            ("SKILL.md", definition, None),
            ("references/guide.md", b"one\n", None),
        ]);
        let archive_b = zip_bytes(&[
            ("SKILL.md", definition, None),
            ("references/guide.md", b"two\n", None),
        ]);
        fs::write(source.path().join("a.zip"), &archive_a).unwrap();
        fs::write(source.path().join("b.zip"), &archive_b).unwrap();
        let source_root = source.path().to_string_lossy().to_string();
        let sha_a = sha256_hex(&archive_a);
        let sha_b = sha256_hex(&archive_b);

        let first = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "a.zip",
                &sha_a,
                "install-a",
                true,
                None,
            )
            .unwrap();
        assert!(first.installed && first.activated && !first.replayed);
        assert_eq!(
            first.active_package_revision.as_deref(),
            Some(first.package_revision.as_str())
        );
        let stable_skill_id = first.skill_id.clone();
        let active_a = store.list_active().unwrap();
        assert_eq!(active_a.skills.len(), 1);
        let namespace_a = active_a.namespace_revision.clone();

        let replay = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "a.zip",
                &sha_a,
                "install-a",
                true,
                None,
            )
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.package_revision, first.package_revision);
        assert_eq!(replay.skill_id, stable_skill_id);

        let second = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "b.zip",
                &sha_b,
                "install-b",
                false,
                None,
            )
            .unwrap();
        assert!(second.installed && !second.activated);
        assert_eq!(second.definition_revision, first.definition_revision);
        assert_ne!(second.package_revision, first.package_revision);
        assert_eq!(second.skill_id, stable_skill_id);
        let versions_before_b = store.versions("demo", 0, 64).unwrap();
        assert_eq!(versions_before_b.total_count, 2);
        assert_eq!(
            versions_before_b.active_package_revision.as_deref(),
            Some(first.package_revision.as_str())
        );

        store
            .fail_next_state_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store
                .activate(
                    "demo",
                    &second.package_revision,
                    &versions_before_b.state_revision,
                    "activate-b",
                )
                .unwrap_err(),
            "skill_store_state_write_failed"
        );
        assert_eq!(
            store.list_active().unwrap().skills[0].package_revision,
            first.package_revision
        );
        let activated_b = store
            .activate(
                "demo",
                &second.package_revision,
                &versions_before_b.state_revision,
                "activate-b",
            )
            .unwrap();
        assert!(activated_b.changed && activated_b.replayed);
        let active_b = store.list_active().unwrap();
        assert_eq!(active_b.skills[0].package_revision, second.package_revision);
        assert_ne!(active_b.namespace_revision, namespace_a);
        assert_eq!(active_b.skills[0].skill_id, stable_skill_id);
        assert_eq!(
            store
                .read_resource(
                    &stable_skill_id,
                    "references/guide.md",
                    1,
                    10,
                    Some(&first.package_revision),
                    Some(&first.definition_revision),
                )
                .unwrap_err(),
            "skill_package_changed"
        );
        let read_b = store
            .read_resource(
                &stable_skill_id,
                "references/guide.md",
                1,
                10,
                Some(&second.package_revision),
                Some(&second.definition_revision),
            )
            .unwrap();
        assert_eq!(read_b.text, "two");

        assert_eq!(
            store
                .activate(
                    "demo",
                    &first.package_revision,
                    &versions_before_b.state_revision,
                    "stale-rollback",
                )
                .unwrap_err(),
            "skill_state_changed"
        );
        let versions_on_b = store.versions("demo", 0, 64).unwrap();
        let rolled_back = store
            .activate(
                "demo",
                &first.package_revision,
                &versions_on_b.state_revision,
                "rollback-a",
            )
            .unwrap();
        assert_eq!(rolled_back.active_package_revision, first.package_revision);
        let versions_on_a = store.versions("demo", 0, 64).unwrap();
        assert_eq!(
            store
                .remove_revision(
                    "demo",
                    &first.package_revision,
                    &versions_on_a.state_revision,
                    "remove-active-a",
                )
                .unwrap_err(),
            "skill_active_revision_remove_forbidden"
        );
        let removed_b = store
            .remove_revision(
                "demo",
                &second.package_revision,
                &versions_on_a.state_revision,
                "remove-b",
            )
            .unwrap();
        assert!(removed_b.removed && !removed_b.replayed);
        // The old stale-rollback request failed before mutation and therefore
        // remains only claimed. Active is A again, but removing B changed the
        // full state_revision; a retry must not use the matching target alone
        // to bypass the old CAS expectation.
        assert_eq!(
            store
                .activate(
                    "demo",
                    &first.package_revision,
                    &versions_before_b.state_revision,
                    "stale-rollback",
                )
                .unwrap_err(),
            "skill_state_changed"
        );
        let removed_replay = store
            .remove_revision(
                "demo",
                &second.package_revision,
                &versions_on_a.state_revision,
                "remove-b",
            )
            .unwrap();
        assert!(removed_replay.replayed);
        assert_eq!(store.versions("demo", 0, 64).unwrap().total_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn resource_read_rejects_symlinked_package_components() {
        use std::os::unix::fs::symlink;

        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(store_root.path().join("store"), "runner-a");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let archive = zip_bytes(&[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: operator demo\n---\n",
                None,
            ),
            ("references/guide.md", b"safe\n", None),
        ]);
        fs::write(source.path().join("skill.zip"), &archive).unwrap();
        let source_root = source.path().to_string_lossy().to_string();
        let installed = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source_root,
                "skill.zip",
                &sha256_hex(&archive),
                "install",
                true,
                None,
            )
            .unwrap();
        let references = store
            .revision_dir("demo", &installed.package_revision)
            .join(PACKAGE_DIR)
            .join("references");
        fs::remove_dir_all(&references).unwrap();
        fs::write(external.path().join("guide.md"), b"outside\n").unwrap();
        symlink(external.path(), &references).unwrap();
        assert_eq!(
            store
                .read_resource(
                    &installed.skill_id,
                    "references/guide.md",
                    1,
                    10,
                    Some(&installed.package_revision),
                    Some(&installed.definition_revision),
                )
                .unwrap_err(),
            "skill_resource_path_invalid"
        );
    }

    #[test]
    fn resource_read_rejects_regular_file_tamper_under_existing_package_revision() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(store_root.path().join("store"), "runner-a");
        let policy = RunnerPolicy {
            allow_cwd_anywhere: true,
            ..RunnerPolicy::default()
        };
        let archive = zip_bytes(&[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: operator demo\n---\n",
                None,
            ),
            ("references/guide.md", b"original\n", None),
        ]);
        fs::write(source.path().join("skill.zip"), &archive).unwrap();
        let installed = store
            .install(
                &policy,
                "demo",
                "project-a",
                &source.path().to_string_lossy(),
                "skill.zip",
                &sha256_hex(&archive),
                "install-tamper-test",
                true,
                None,
            )
            .unwrap();
        let resource = store
            .revision_dir("demo", &installed.package_revision)
            .join(PACKAGE_DIR)
            .join("references/guide.md");
        fs::write(&resource, b"tampered\n").unwrap();

        let state = store.versions("demo", 0, 64).unwrap();
        assert_eq!(
            store
                .activate(
                    "demo",
                    &installed.package_revision,
                    &state.state_revision,
                    "activate-tampered",
                )
                .unwrap_err(),
            "skill_store_revision_modified"
        );

        assert_eq!(
            store
                .read_resource(
                    &installed.skill_id,
                    "references/guide.md",
                    1,
                    10,
                    Some(&installed.package_revision),
                    Some(&installed.definition_revision),
                )
                .unwrap_err(),
            "skill_store_revision_modified"
        );
    }

    #[test]
    fn interrupted_staging_is_cleaned_without_publishing_active_skill() {
        let root = tempfile::tempdir().unwrap();
        let store = SkillStore::for_test(root.path().join("store"), "runner");
        store.initialize().unwrap();
        let partial = store.root.join(STAGING_DIR).join("interrupted");
        fs::create_dir_all(partial.join(PACKAGE_DIR)).unwrap();
        fs::write(partial.join(PACKAGE_DIR).join("SKILL.md"), b"partial").unwrap();
        let active = store.list_active().unwrap();
        assert!(active.skills.is_empty());
        assert_eq!(
            fs::read_dir(store.root.join(STAGING_DIR)).unwrap().count(),
            0
        );
    }

    #[test]
    fn archive_requires_valid_definition_and_enforces_decompressed_total_bound() {
        let missing = zip_bytes(&[("references/a.md", b"x", None)]);
        assert_eq!(
            prepare_archive(&missing, sha256_hex(&missing))
                .err()
                .unwrap(),
            "skill_definition_missing"
        );
        let malformed = zip_bytes(&[("SKILL.md", b"not-frontmatter", None)]);
        assert!(prepare_archive(&malformed, sha256_hex(&malformed)).is_err());

        let large = vec![0u8; MAX_SKILL_STORE_FILE_BYTES];
        let definition = b"---\nname: demo\ndescription: demo\n---\n";
        let bomb = zip_bytes(&[
            ("SKILL.md", definition, None),
            ("a.bin", &large, None),
            ("b.bin", &large, None),
            ("c.bin", &large, None),
            ("d.bin", &large, None),
        ]);
        assert!(bomb.len() < MAX_SKILL_STORE_ARCHIVE_BYTES);
        assert_eq!(
            prepare_archive(&bomb, sha256_hex(&bomb)).err().unwrap(),
            "skill_install_total_too_large"
        );
    }
}
