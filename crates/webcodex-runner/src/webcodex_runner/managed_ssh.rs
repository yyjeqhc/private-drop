use super::config::{SshConfig, SshResourceConfig};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use webcodex_core::ssh_resource::{
    normalize_ssh_resource_default_cwd, normalize_ssh_resource_target, validate_ssh_resource_name,
    SshResourceInventoryEntry, SshResourceRequest, SshResourceResponse, SshResourceSource,
    MANAGED_SSH_REGISTRY_MAX_BYTES, MANAGED_SSH_RESOURCE_MAX_COUNT,
};

const STORE_VERSION: u32 = 1;
const STORE_DIR: &str = "runner-managed-ssh-resources-v1";
const STORE_FILE: &str = "managed-ssh-resources.json";
const LOCK_FILE: &str = "managed-ssh-resources.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedResource {
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedRegistry {
    version: u32,
    revision: u64,
    resources: BTreeMap<String, ManagedResource>,
}

impl Default for ManagedRegistry {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            revision: 0,
            resources: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ManagedSshResourceStore {
    path: Option<PathBuf>,
    lock_path: Option<PathBuf>,
    startup_managed: SshConfig,
}

impl ManagedSshResourceStore {
    pub(crate) fn initialize(client_id: &str, server_url: &str, static_ssh: &SshConfig) -> Self {
        let root = match managed_state_root(client_id, server_url) {
            Ok(root) => root,
            Err(code) => {
                tracing::error!(code, "managed SSH resource state path unavailable; static SSH resources remain active");
                return Self {
                    path: None,
                    lock_path: None,
                    startup_managed: SshConfig::default(),
                };
            }
        };
        Self::initialize_at_root(&root, static_ssh)
    }

    fn initialize_at_root(root: &Path, static_ssh: &SshConfig) -> Self {
        if root.as_os_str().is_empty() {
            return Self {
                path: None,
                lock_path: None,
                startup_managed: SshConfig::default(),
            };
        }
        let path = root.join(STORE_FILE);
        let lock_path = root.join(LOCK_FILE);
        let startup_managed = match with_registry_lock(&lock_path, || load_registry(&path))
            .and_then(|registry| registry_to_ssh(&registry, static_ssh))
        {
            Ok(ssh) => ssh,
            Err(code) => {
                tracing::error!(code, "managed SSH resource registry unavailable at startup; static SSH resources remain active");
                SshConfig::default()
            }
        };
        Self {
            path: Some(path),
            lock_path: Some(lock_path),
            startup_managed,
        }
    }

    pub(crate) fn startup_managed(&self) -> &SshConfig {
        &self.startup_managed
    }

    pub(crate) fn merge_active(
        static_ssh: &SshConfig,
        managed: &SshConfig,
    ) -> Result<SshConfig, &'static str> {
        if managed
            .resources
            .keys()
            .any(|name| static_ssh.resources.contains_key(name))
        {
            return Err("ssh_resource_static_conflict");
        }
        let mut merged = static_ssh.clone();
        merged.resources.extend(managed.resources.clone());
        Ok(merged)
    }

    pub(crate) fn handle(
        &self,
        static_active: &SshConfig,
        request: SshResourceRequest,
    ) -> SshResourceResponse {
        let (Some(path), Some(lock_path)) = (self.path.as_deref(), self.lock_path.as_deref())
        else {
            return safe_error(
                "ssh_resource_registry_unavailable",
                "Managed SSH resource registry is unavailable for this Runner configuration",
            );
        };
        match with_registry_lock(lock_path, || {
            let registry = load_registry(path)?;
            ensure_no_static_collision(&registry, static_active)?;
            match request {
                SshResourceRequest::List => Ok(list_response(
                    registry,
                    static_active,
                    &self.startup_managed,
                )),
                SshResourceRequest::Register {
                    expected_revision,
                    name,
                    target,
                    default_cwd,
                } => self.register_locked(
                    path,
                    registry,
                    static_active,
                    expected_revision,
                    name,
                    target,
                    default_cwd,
                ),
                SshResourceRequest::Remove {
                    expected_revision,
                    name,
                } => self.remove_locked(path, registry, static_active, expected_revision, name),
            }
        }) {
            Ok(response) => response,
            Err(code) => safe_error(code, safe_error_message(code)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn register_locked(
        &self,
        path: &Path,
        mut registry: ManagedRegistry,
        static_active: &SshConfig,
        expected_revision: u64,
        name: String,
        target: String,
        default_cwd: Option<String>,
    ) -> Result<SshResourceResponse, &'static str> {
        if registry.revision != expected_revision {
            return Err("ssh_resource_registry_stale");
        }
        if static_active.resources.contains_key(&name) {
            return Err("ssh_resource_static_conflict");
        }
        let desired = ManagedResource {
            target: normalize_ssh_resource_target(&target)?,
            default_cwd: normalize_ssh_resource_default_cwd(default_cwd.as_deref())?,
        };
        if let Some(existing) = registry.resources.get(&name) {
            if existing != &desired {
                return Err("ssh_resource_name_conflict");
            }
            let active = self
                .startup_managed
                .resources
                .get(&name)
                .is_some_and(|active| managed_matches_config(existing, active));
            return Ok(SshResourceResponse::Register {
                revision: registry.revision,
                resource: name.clone(),
                persisted: true,
                active,
                restart_required: !active,
            });
        }
        if registry.resources.len() >= MANAGED_SSH_RESOURCE_MAX_COUNT {
            return Err("ssh_resource_invalid");
        }
        let active = self
            .startup_managed
            .resources
            .get(&name)
            .is_some_and(|current| managed_matches_config(&desired, current));
        registry.resources.insert(name.clone(), desired);
        registry.revision = registry
            .revision
            .checked_add(1)
            .ok_or("ssh_resource_registry_unavailable")?;
        write_registry(path, &registry)?;
        Ok(SshResourceResponse::Register {
            revision: registry.revision,
            resource: name,
            persisted: true,
            active,
            restart_required: !active,
        })
    }

    fn remove_locked(
        &self,
        path: &Path,
        mut registry: ManagedRegistry,
        static_active: &SshConfig,
        expected_revision: u64,
        name: String,
    ) -> Result<SshResourceResponse, &'static str> {
        if registry.revision != expected_revision {
            return Err("ssh_resource_registry_stale");
        }
        if static_active.resources.contains_key(&name) {
            return Err("ssh_resource_static_read_only");
        }
        if registry.resources.remove(&name).is_none() {
            return Err("ssh_resource_not_found");
        }
        let active = self.startup_managed.resources.contains_key(&name);
        registry.revision = registry
            .revision
            .checked_add(1)
            .ok_or("ssh_resource_registry_unavailable")?;
        write_registry(path, &registry)?;
        Ok(SshResourceResponse::Remove {
            revision: registry.revision,
            resource: name.clone(),
            persisted: true,
            active,
            restart_required: active,
        })
    }
}

fn managed_state_root(client_id: &str, server_url: &str) -> Result<PathBuf, &'static str> {
    let base = webcodex_runner_config::paths::default_client_state_base_dir()
        .map_err(|_| "ssh_resource_registry_unavailable")?;
    Ok(base
        .join(STORE_DIR)
        .join(storage_namespace(client_id, server_url)))
}

fn storage_namespace(client_id: &str, server_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"webcodex-runner-managed-ssh-v1\0");
    hasher.update(client_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(server_url.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn safe_error(code: &str, message: &str) -> SshResourceResponse {
    SshResourceResponse::error(code, message)
}

fn safe_error_message(code: &str) -> &'static str {
    match code {
        "ssh_resource_not_found" => "Managed SSH resource was not found",
        "ssh_resource_static_conflict" => {
            "Resource name is reserved by static Runner configuration"
        }
        "ssh_resource_static_read_only" => "Static Runner SSH resources are read-only",
        "ssh_resource_name_conflict" => {
            "Managed SSH resource name already has different configuration"
        }
        "ssh_resource_registry_stale" => {
            "Managed SSH resource registry changed; list resources again"
        }
        "ssh_resource_outcome_unknown" => {
            "Managed SSH resource durability is uncertain; list resources again"
        }
        "ssh_resource_invalid" => "Managed SSH resource request or registry is invalid",
        _ => "Managed SSH resource registry is unavailable",
    }
}

fn list_response(
    registry: ManagedRegistry,
    static_active: &SshConfig,
    startup_managed: &SshConfig,
) -> SshResourceResponse {
    let mut resources = Vec::new();
    for name in static_active.resources.keys() {
        resources.push(SshResourceInventoryEntry {
            name: name.clone(),
            source: SshResourceSource::Static,
            active: true,
            pending_restart: false,
        });
    }
    let mut managed_names = registry.resources.keys().cloned().collect::<BTreeSet<_>>();
    managed_names.extend(startup_managed.resources.keys().cloned());
    for name in managed_names {
        if static_active.resources.contains_key(&name) {
            continue;
        }
        let desired = registry.resources.get(&name);
        let active = startup_managed.resources.get(&name);
        let same = match (desired, active) {
            (Some(desired), Some(active)) => managed_matches_config(desired, active),
            _ => false,
        };
        resources.push(SshResourceInventoryEntry {
            name,
            source: SshResourceSource::Managed,
            active: active.is_some(),
            pending_restart: !same,
        });
    }
    resources.sort_by(|a, b| a.name.cmp(&b.name));
    SshResourceResponse::List {
        revision: registry.revision,
        resources,
    }
}

fn managed_matches_config(resource: &ManagedResource, active: &SshResourceConfig) -> bool {
    resource.target == active.host && resource.default_cwd == active.default_cwd
}

fn ensure_no_static_collision(
    registry: &ManagedRegistry,
    static_ssh: &SshConfig,
) -> Result<(), &'static str> {
    if registry
        .resources
        .keys()
        .any(|name| static_ssh.resources.contains_key(name))
    {
        return Err("ssh_resource_static_conflict");
    }
    Ok(())
}

fn registry_to_ssh(
    registry: &ManagedRegistry,
    static_ssh: &SshConfig,
) -> Result<SshConfig, &'static str> {
    validate_registry(registry)?;
    ensure_no_static_collision(registry, static_ssh)?;
    let mut resources = BTreeMap::new();
    for (name, resource) in &registry.resources {
        resources.insert(
            name.clone(),
            SshResourceConfig {
                host: resource.target.clone(),
                default_cwd: resource.default_cwd.clone(),
            },
        );
    }
    Ok(SshConfig { resources })
}

fn validate_registry(registry: &ManagedRegistry) -> Result<(), &'static str> {
    if registry.version != STORE_VERSION
        || registry.resources.len() > MANAGED_SSH_RESOURCE_MAX_COUNT
    {
        return Err("ssh_resource_invalid");
    }
    for (name, resource) in &registry.resources {
        validate_ssh_resource_name(name)?;
        normalize_ssh_resource_target(&resource.target)?;
        normalize_ssh_resource_default_cwd(resource.default_cwd.as_deref())?;
    }
    Ok(())
}

fn load_registry(path: &Path) -> Result<ManagedRegistry, &'static str> {
    reject_symlink_or_non_file_if_exists(path)?;
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MANAGED_SSH_REGISTRY_MAX_BYTES as u64 => {
            return Err("ssh_resource_invalid");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(_) => return Err("ssh_resource_registry_unavailable"),
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Err("ssh_resource_registry_unavailable"),
    };
    if bytes.len() > MANAGED_SSH_REGISTRY_MAX_BYTES {
        return Err("ssh_resource_invalid");
    }
    let registry: ManagedRegistry =
        serde_json::from_slice(&bytes).map_err(|_| "ssh_resource_invalid")?;
    validate_registry(&registry)?;
    Ok(registry)
}

fn with_registry_lock<T>(
    lock_path: &Path,
    f: impl FnOnce() -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    let parent = lock_path
        .parent()
        .ok_or("ssh_resource_registry_unavailable")?;
    ensure_store_directory(parent)?;
    reject_symlink_or_non_file_if_exists(lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(lock_path)
        .map_err(|_| "ssh_resource_registry_unavailable")?;
    ensure_private_file_permissions(lock_path)?;
    file.lock_exclusive()
        .map_err(|_| "ssh_resource_registry_unavailable")?;
    let result = f();
    let _ = file.unlock();
    result
}

fn write_registry(path: &Path, registry: &ManagedRegistry) -> Result<(), &'static str> {
    validate_registry(registry)?;
    let bytes = serde_json::to_vec(registry).map_err(|_| "ssh_resource_invalid")?;
    if bytes.len() > MANAGED_SSH_REGISTRY_MAX_BYTES {
        return Err("ssh_resource_invalid");
    }
    atomic_write_bytes(path, &bytes)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), &'static str> {
    let parent = path.parent().ok_or("ssh_resource_registry_unavailable")?;
    ensure_store_directory(parent)?;
    reject_symlink_or_non_file_if_exists(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("ssh_resource_registry_unavailable")?;
    let temp = parent.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|_| "ssh_resource_registry_unavailable")?;
    if file.write_all(bytes).is_err() || file.sync_all().is_err() {
        let _ = fs::remove_file(&temp);
        return Err("ssh_resource_registry_unavailable");
    }
    #[cfg(test)]
    if parent
        .join(".managed-ssh-resources.fail-before-replace")
        .exists()
    {
        let _ = fs::remove_file(&temp);
        return Err("ssh_resource_registry_unavailable");
    }
    if replace_file_atomic(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
        return Err("ssh_resource_registry_unavailable");
    }
    #[cfg(test)]
    if parent
        .join(".managed-ssh-resources.fail-after-replace")
        .exists()
    {
        return Err("ssh_resource_outcome_unknown");
    }
    sync_parent(path).map_err(|_| "ssh_resource_outcome_unknown")?;
    Ok(())
}

fn reject_symlink_or_non_file_if_exists(path: &Path) -> Result<(), &'static str> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("ssh_resource_registry_unavailable")
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("ssh_resource_registry_unavailable"),
    }
}

fn ensure_store_directory(path: &Path) -> Result<(), &'static str> {
    fs::create_dir_all(path).map_err(|_| "ssh_resource_registry_unavailable")?;
    let metadata = fs::symlink_metadata(path).map_err(|_| "ssh_resource_registry_unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("ssh_resource_registry_unavailable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "ssh_resource_registry_unavailable")?;
    }
    Ok(())
}

fn ensure_private_file_permissions(path: &Path) -> Result<(), &'static str> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "ssh_resource_registry_unavailable")?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn replace_file_atomic(temp: &Path, path: &Path) -> Result<(), ()> {
    fs::rename(temp, path).map_err(|_| ())
}

#[cfg(windows)]
fn replace_file_atomic(temp: &Path, path: &Path) -> Result<(), ()> {
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
    (unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } != 0)
        .then_some(())
        .ok_or(())
}

#[cfg(not(any(unix, windows)))]
fn replace_file_atomic(_temp: &Path, _path: &Path) -> Result<(), ()> {
    Err(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), &'static str> {
    File::open(path.parent().ok_or("ssh_resource_registry_unavailable")?)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "ssh_resource_registry_unavailable")
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), &'static str> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_ssh(name: &str) -> SshConfig {
        SshConfig {
            resources: BTreeMap::from([(
                name.to_string(),
                SshResourceConfig {
                    host: name.to_string(),
                    default_cwd: None,
                },
            )]),
        }
    }

    fn store(tmp: &tempfile::TempDir, static_ssh: &SshConfig) -> ManagedSshResourceStore {
        ManagedSshResourceStore::initialize_at_root(tmp.path(), static_ssh)
    }

    #[test]
    fn storage_namespace_is_stable_and_runner_identity_scoped() {
        let first = storage_namespace("runner-a", "https://server.example");
        assert_eq!(
            first,
            storage_namespace("runner-a", "https://server.example")
        );
        assert_ne!(
            first,
            storage_namespace("runner-b", "https://server.example")
        );
        assert_ne!(
            first,
            storage_namespace("runner-a", "https://other.example")
        );
        assert_eq!(first.len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn registry_state_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = SshConfig::default();
        let store = store(&tmp, &static_ssh);
        let response = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "private".to_string(),
                target: "private-target".to_string(),
                default_cwd: None,
            },
        );
        assert!(matches!(response, SshResourceResponse::Register { .. }));
        let dir_mode = fs::metadata(tmp.path()).unwrap().permissions().mode() & 0o777;
        let registry_mode = fs::metadata(tmp.path().join(STORE_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let lock_mode = fs::metadata(tmp.path().join(LOCK_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(registry_mode, 0o600);
        assert_eq!(lock_mode, 0o600);
    }

    fn list_revision(response: SshResourceResponse) -> u64 {
        match response {
            SshResourceResponse::List { revision, .. } => revision,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn register_is_durable_idempotent_and_conflicts_on_retarget() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = static_ssh("spe");
        let store = store(&tmp, &static_ssh);
        let revision = list_revision(store.handle(&static_ssh, SshResourceRequest::List));
        let request = SshResourceRequest::Register {
            expected_revision: revision,
            name: "w10".to_string(),
            target: "17724@w10".to_string(),
            default_cwd: Some("C:/work".to_string()),
        };
        let first = store.handle(&static_ssh, request.clone());
        assert!(matches!(
            first,
            SshResourceResponse::Register {
                persisted: true,
                active: false,
                restart_required: true,
                revision: 1,
                ..
            }
        ));
        let idempotent = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 1,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: Some("C:/work".to_string()),
            },
        );
        assert!(matches!(
            idempotent,
            SshResourceResponse::Register { revision: 1, .. }
        ));
        let conflict = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 1,
                name: "w10".to_string(),
                target: "other".to_string(),
                default_cwd: None,
            },
        );
        assert!(
            matches!(conflict, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_name_conflict")
        );
        let bytes = fs::read(tmp.path().join(STORE_FILE)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("17724@w10"));
    }

    #[test]
    fn static_names_are_reserved_and_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = static_ssh("spe");
        let store = store(&tmp, &static_ssh);
        let register = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "spe".to_string(),
                target: "anything".to_string(),
                default_cwd: None,
            },
        );
        assert!(
            matches!(register, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_static_conflict")
        );
        let remove = store.handle(
            &static_ssh,
            SshResourceRequest::Remove {
                expected_revision: 0,
                name: "spe".to_string(),
            },
        );
        assert!(
            matches!(remove, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_static_read_only")
        );
    }

    #[test]
    fn registration_and_removal_activate_only_on_next_store_initialization() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = static_ssh("spe");
        let first = store(&tmp, &static_ssh);
        let registered = first.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: None,
            },
        );
        assert!(matches!(
            registered,
            SshResourceResponse::Register { active: false, .. }
        ));
        assert!(!first.startup_managed.resources.contains_key("w10"));
        let first_effective =
            ManagedSshResourceStore::merge_active(&static_ssh, first.startup_managed()).unwrap();
        assert!(first_effective.resources.contains_key("spe"));
        assert!(!first_effective.resources.contains_key("w10"));

        let second = store(&tmp, &static_ssh);
        assert!(second.startup_managed.resources.contains_key("w10"));
        let second_effective =
            ManagedSshResourceStore::merge_active(&static_ssh, second.startup_managed()).unwrap();
        assert!(second_effective.resources.contains_key("spe"));
        assert!(second_effective.resources.contains_key("w10"));
        let removed = second.handle(
            &static_ssh,
            SshResourceRequest::Remove {
                expected_revision: 1,
                name: "w10".to_string(),
            },
        );
        assert!(matches!(
            removed,
            SshResourceResponse::Remove {
                active: true,
                restart_required: true,
                ..
            }
        ));
        assert!(second.startup_managed.resources.contains_key("w10"));
        let still_active =
            ManagedSshResourceStore::merge_active(&static_ssh, second.startup_managed()).unwrap();
        assert!(still_active.resources.contains_key("w10"));
        let third = store(&tmp, &static_ssh);
        assert!(!third.startup_managed.resources.contains_key("w10"));
        let third_effective =
            ManagedSshResourceStore::merge_active(&static_ssh, third.startup_managed()).unwrap();
        assert!(third_effective.resources.contains_key("spe"));
        assert!(!third_effective.resources.contains_key("w10"));
    }

    #[test]
    fn no_op_desired_state_changes_do_not_claim_a_restart() {
        let active_tmp = tempfile::tempdir().unwrap();
        let static_ssh = SshConfig::default();
        let first = store(&active_tmp, &static_ssh);
        let _ = first.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: None,
            },
        );
        let active = store(&active_tmp, &static_ssh);
        let idempotent = active.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 1,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: None,
            },
        );
        assert!(matches!(
            idempotent,
            SshResourceResponse::Register {
                revision: 1,
                active: true,
                restart_required: false,
                ..
            }
        ));

        let pending_tmp = tempfile::tempdir().unwrap();
        let pending = store(&pending_tmp, &static_ssh);
        let _ = pending.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "new".to_string(),
                target: "new".to_string(),
                default_cwd: None,
            },
        );
        let removed = pending.handle(
            &static_ssh,
            SshResourceRequest::Remove {
                expected_revision: 1,
                name: "new".to_string(),
            },
        );
        assert!(matches!(
            removed,
            SshResourceResponse::Remove {
                active: false,
                restart_required: false,
                ..
            }
        ));
    }

    #[test]
    fn stale_revision_and_corrupt_registry_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = SshConfig::default();
        let store = store(&tmp, &static_ssh);
        let _ = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "one".to_string(),
                target: "one".to_string(),
                default_cwd: None,
            },
        );
        let stale = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "two".to_string(),
                target: "two".to_string(),
                default_cwd: None,
            },
        );
        assert!(
            matches!(stale, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_registry_stale")
        );
        fs::write(tmp.path().join(STORE_FILE), b"not json").unwrap();
        let corrupt = store.handle(&static_ssh, SshResourceRequest::List);
        assert!(
            matches!(corrupt, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_invalid")
        );
    }

    #[test]
    fn list_never_exposes_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = static_ssh("spe");
        let store = store(&tmp, &static_ssh);
        let _ = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: None,
            },
        );
        let list = store.handle(&static_ssh, SshResourceRequest::List);
        let json = serde_json::to_string(&list).unwrap();
        assert!(json.contains("spe") && json.contains("w10"));
        assert!(!json.contains("17724@w10"));
    }

    #[test]
    fn failed_atomic_replace_preserves_old_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = SshConfig::default();
        let store = store(&tmp, &static_ssh);
        let first = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "one".to_string(),
                target: "one".to_string(),
                default_cwd: None,
            },
        );
        assert!(matches!(
            first,
            SshResourceResponse::Register { revision: 1, .. }
        ));
        let before = fs::read(tmp.path().join(STORE_FILE)).unwrap();
        fs::write(
            tmp.path()
                .join(".managed-ssh-resources.fail-before-replace"),
            b"1",
        )
        .unwrap();
        let failed = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 1,
                name: "two".to_string(),
                target: "two".to_string(),
                default_cwd: None,
            },
        );
        assert!(
            matches!(failed, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_registry_unavailable")
        );
        let after = fs::read(tmp.path().join(STORE_FILE)).unwrap();
        assert_eq!(
            before, after,
            "failed replace must preserve the prior durable registry"
        );
    }

    #[test]
    fn failure_after_atomic_replace_is_outcome_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let static_ssh = SshConfig::default();
        let store = store(&tmp, &static_ssh);
        fs::write(
            tmp.path().join(".managed-ssh-resources.fail-after-replace"),
            b"1",
        )
        .unwrap();
        let response = store.handle(
            &static_ssh,
            SshResourceRequest::Register {
                expected_revision: 0,
                name: "w10".to_string(),
                target: "17724@w10".to_string(),
                default_cwd: None,
            },
        );
        assert!(matches!(
            response,
            SshResourceResponse::Error { ref code, .. }
                if code == "ssh_resource_outcome_unknown"
        ));
        let registry = load_registry(&tmp.path().join(STORE_FILE)).unwrap();
        assert_eq!(registry.revision, 1);
        assert!(registry.resources.contains_key("w10"));
    }

    #[test]
    fn registry_count_and_serialized_size_are_bounded() {
        let too_many = ManagedRegistry {
            version: STORE_VERSION,
            revision: 1,
            resources: (0..=MANAGED_SSH_RESOURCE_MAX_COUNT)
                .map(|index| {
                    (
                        format!("r{index}"),
                        ManagedResource {
                            target: "host".to_string(),
                            default_cwd: None,
                        },
                    )
                })
                .collect(),
        };
        assert_eq!(validate_registry(&too_many), Err("ssh_resource_invalid"));

        let huge = ManagedRegistry {
            version: STORE_VERSION,
            revision: 1,
            resources: (0..MANAGED_SSH_RESOURCE_MAX_COUNT)
                .map(|index| {
                    (
                        format!("r{index}"),
                        ManagedResource {
                            target: "host".to_string(),
                            default_cwd: Some("x".repeat(
                                webcodex_core::ssh_resource::SSH_RESOURCE_DEFAULT_CWD_MAX_BYTES,
                            )),
                        },
                    )
                })
                .collect(),
        };
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            write_registry(&tmp.path().join(STORE_FILE), &huge),
            Err("ssh_resource_invalid")
        );
    }

    #[test]
    fn persisted_static_collision_keeps_static_usable_and_management_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = ManagedRegistry {
            version: STORE_VERSION,
            revision: 1,
            resources: BTreeMap::from([(
                "spe".to_string(),
                ManagedResource {
                    target: "other".to_string(),
                    default_cwd: None,
                },
            )]),
        };
        write_registry(&tmp.path().join(STORE_FILE), &registry).unwrap();
        let static_ssh = static_ssh("spe");
        let store = store(&tmp, &static_ssh);
        assert!(store.startup_managed.resources.is_empty());
        let active =
            ManagedSshResourceStore::merge_active(&static_ssh, store.startup_managed()).unwrap();
        assert_eq!(active.resources["spe"].host, "spe");
        let list = store.handle(&static_ssh, SshResourceRequest::List);
        assert!(
            matches!(list, SshResourceResponse::Error { ref code, .. } if code == "ssh_resource_static_conflict")
        );
    }
}
