//! Central platform directory policy shared by `webcodex-cli` and
//! `webcodex-runner`.
//!
//! Both binaries must agree on where per-user configuration, credentials,
//! runner state and logs live, and the rules differ per platform:
//!
//! - **Unix**: XDG-style layout rooted at `$HOME` (`~/.config/webcodex`,
//!   `~/.local/state/webcodex`), with `/etc/webcodex` for effective-root.
//!   Existing behavior is preserved exactly.
//! - **Windows**: configuration and credentials live in `%APPDATA%\webcodex`
//!   (Roaming profile, follows the user) with `%USERPROFILE%\.config\webcodex`
//!   as the fallback; runner state/logs live in `%LOCALAPPDATA%\webcodex`
//!   (machine-local) with `%USERPROFILE%\.local\state\webcodex` and finally
//!   `%TEMP%\webcodex` as fallbacks. `HOME` and the XDG variables are
//!   deliberately *not* consulted on Windows: `HOME` is either absent or a Git
//!   Bash/MSYS POSIX-style path like `/c/Users/...` that Windows APIs cannot
//!   consume, and `APPDATA`/`LOCALAPPDATA` are the OS-native equivalents of
//!   the XDG homes.
//!
//! Each platform's derivation lives on its own `cfg` side; there is no shared
//! cross-platform fallback chain. No derivation in this module ever falls back
//! to the current working directory; when no usable per-user directory exists
//! the caller gets a `Result` error instead of silently writing into a
//! relative path.

use std::path::{Path, PathBuf};

/// Canonical Runner configuration filename for WebCodex 0.4 and later.
pub const RUNNER_CONFIG_FILE: &str = "runner.toml";
/// Pre-0.4 Runner configuration filename retained as a read compatibility alias.
pub const LEGACY_AGENT_CONFIG_FILE: &str = "agent.toml";
/// Canonical directory name for newly created Runner project registries.
pub const PROJECT_REGISTRY_DIR_NAME: &str = "project-registry";
/// Legacy Runner project-registry directory name accepted for compatibility.
pub const LEGACY_PROJECTS_DIR_NAME: &str = "projects.d";

fn path_entry_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
    }
}

/// Resolve an existing Runner config within one authoritative config directory.
///
/// `runner.toml` is canonical. `agent.toml` remains readable only when it is the
/// sole config entry. If both names exist, fail closed rather than choosing a
/// winner and risking split-brain configuration.
pub fn existing_runner_config_path(dir: &Path) -> Result<Option<PathBuf>, String> {
    let runner = dir.join(RUNNER_CONFIG_FILE);
    let legacy = dir.join(LEGACY_AGENT_CONFIG_FILE);
    let runner_exists = path_entry_exists(&runner)?;
    let legacy_exists = path_entry_exists(&legacy)?;
    match (runner_exists, legacy_exists) {
        (true, true) => Err(format!(
            "both {} and {} exist in {}; refusing to guess which Runner config is authoritative",
            RUNNER_CONFIG_FILE,
            LEGACY_AGENT_CONFIG_FILE,
            dir.display()
        )),
        (true, false) => Ok(Some(runner)),
        (false, true) => Ok(Some(legacy)),
        (false, false) => Ok(None),
    }
}

/// Resolve the Runner config path for one authoritative config directory.
/// Existing legacy-only directories keep using `agent.toml`; a new directory
/// gets the canonical `runner.toml` creation target.
pub fn resolve_runner_config_path(dir: &Path) -> Result<PathBuf, String> {
    Ok(existing_runner_config_path(dir)?.unwrap_or_else(|| dir.join(RUNNER_CONFIG_FILE)))
}

/// Select the Runner project registry beneath `base` without merging layouts.
///
/// Compatibility contract:
/// - only `project-registry/` exists: use it;
/// - only legacy `projects.d/` exists: keep using it;
/// - neither exists: select `project-registry/` for new installs;
/// - both exist: fail closed so records are never silently merged or shadowed.
pub fn select_project_registry_dir(base: &Path) -> Result<PathBuf, String> {
    let current = base.join(PROJECT_REGISTRY_DIR_NAME);
    let legacy = base.join(LEGACY_PROJECTS_DIR_NAME);
    match (path_entry_exists(&current)?, path_entry_exists(&legacy)?) {
        (true, true) => Err(format!(
            "both Runner project registry directories exist: {} and {}; consolidate project registration records into one directory and remove the other before continuing",
            current.display(),
            legacy.display()
        )),
        (true, false) | (false, false) => Ok(current),
        (false, true) => Ok(legacy),
    }
}

/// Per-user home directory.
///
/// - Windows: `USERPROFILE` (set by the OS at logon; `HOME` is ignored).
/// - Unix: `HOME`.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // `HOME` is never consulted on Windows: it is either absent or a Git
        // Bash/MSYS POSIX-style path (`/c/Users/...`) that Windows APIs cannot
        // consume, and in Win32 format it would be a *second* home that can
        // disagree with `USERPROFILE`.
        std::env::var_os("USERPROFILE")
            .filter(|value| !value.as_os_str().is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|value| !value.as_os_str().is_empty())
            .map(PathBuf::from)
    }
}

/// Effective-root detection. Only meaningful on Unix; on Windows always
/// `false` (there is no `/etc/webcodex` system scope).
pub fn is_effective_root() -> bool {
    #[cfg(unix)]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Uid:") {
                    let mut parts = rest.split_whitespace();
                    let _real = parts.next();
                    if let Some(effective) = parts.next() {
                        return effective == "0";
                    }
                }
            }
        }
        std::env::var("USER").is_ok_and(|u| u == "root")
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Base directory for per-user WebCodex configuration and credentials
/// (client profiles, `runner.toml`, Runner project registry, token files).
///
/// - Unix (root): `/etc/webcodex`
/// - Unix (user): `$XDG_CONFIG_HOME/webcodex`, else `$HOME/.config/webcodex`.
///   When `HOME` is also missing the caller gets an error (never `.`).
/// - Windows: `%APPDATA%\webcodex`, else `%USERPROFILE%\.config\webcodex`,
///   else an error. `HOME` and `XDG_CONFIG_HOME` are ignored.
pub fn default_client_config_base_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        // `APPDATA` must work on its own (a plain Windows logon always sets it
        // together with `USERPROFILE`, but either may be absent or unusable in
        // stripped-down environments and the two must not depend on each
        // other).
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(appdata).join("webcodex"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile).join(".config/webcodex"));
        }
        Err(
            "cannot determine the WebCodex config directory: set APPDATA or USERPROFILE"
                .to_string(),
        )
    }
    #[cfg(not(windows))]
    {
        // An explicit XDG_CONFIG_HOME wins even for root, matching the historical
        // CLI behavior (see `omitted_scope_hosted_status_keeps_xdg_profile_paths_for_root`).
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(config_home).join("webcodex"));
        }
        if is_effective_root() {
            return Ok(PathBuf::from("/etc/webcodex"));
        }
        let home = home_dir().ok_or_else(|| {
            "cannot determine user home: set HOME to derive the WebCodex config directory"
                .to_string()
        })?;
        Ok(home.join(".config/webcodex"))
    }
}

/// Base directory for per-user WebCodex state: hosted Runner state
/// (`runner.toml`), Runner logs, checkpoints and recovery data.
///
/// - Unix: `$XDG_STATE_HOME/webcodex`, else `$HOME/.local/state/webcodex`,
///   else `$TMPDIR/webcodex` (existing behavior preserved).
/// - Windows: `%LOCALAPPDATA%\webcodex`, else
///   `%USERPROFILE%\.local\state\webcodex`, else `%TEMP%\webcodex`. `HOME` and
///   `XDG_STATE_HOME` are ignored.
pub fn default_client_state_base_dir() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        // `LOCALAPPDATA` must work on its own, exactly like `APPDATA` above.
        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(local_appdata).join("webcodex"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile).join(".local/state/webcodex"));
        }
        // A volatile temp location is better than a relative path for state
        // that can be regenerated.
        Ok(std::env::temp_dir().join("webcodex"))
    }
    #[cfg(not(windows))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(state_home).join("webcodex"));
        }
        if let Some(home) = home_dir() {
            return Ok(home.join(".local/state/webcodex"));
        }
        // Existing Unix behavior: a volatile temp location is better than a
        // relative path for state that can be regenerated.
        Ok(std::env::temp_dir().join("webcodex"))
    }
}

/// The per-user home as an absolute path, for deriving systemd user service
/// paths. Mirrors the historical `current_user_home` contract.
pub fn user_home() -> Result<PathBuf, String> {
    let home =
        home_dir().ok_or_else(|| "HOME is required to derive user service paths".to_string())?;
    if !home.is_absolute() {
        return Err("HOME must be an absolute path to derive user service paths".to_string());
    }
    Ok(home)
}

/// The per-user config root (`~/.config` equivalent), absolute.
pub fn user_config_home() -> Result<PathBuf, String> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let config_home = PathBuf::from(config_home);
        if !config_home.is_absolute() {
            return Err(
                "XDG_CONFIG_HOME must be an absolute path to derive user service paths".to_string(),
            );
        }
        return Ok(config_home);
    }
    Ok(user_home()?.join(".config"))
}

/// Compare two paths for equality under the filesystem's case rules.
/// Windows filesystems are case-insensitive; Unix comparisons are exact.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let a = normalize_path_identity(a);
        let b = normalize_path_identity(b);
        a == b
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// True when `path` equals `root` or lives underneath it, honoring Windows
/// case-insensitivity and `\\?\` extended-length prefixes.
///
/// The comparison is component-wise: `C:\Users\Alice2` is *not* under
/// `C:\Users\Alice`, even though the string starts with it.
pub fn path_is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path_components = normalized_components(path);
        let root_components = normalized_components(root);
        path_components.len() >= root_components.len()
            && path_components[..root_components.len()] == root_components[..]
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

#[cfg(windows)]
fn normalized_components(path: &Path) -> Vec<String> {
    let stripped = normalize_path_identity(path);
    Path::new(&stripped)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// Windows project-path prefix classes used by ingress, canonical policy, and
/// user-facing adapters. This is grammar-based classification through
/// `std::path::Prefix`, never a textual prefix check.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsProjectPathKind {
    LocalDisk,
    NetworkShare,
    UnsupportedNamespace,
}

/// Classify an explicit Windows path prefix. Paths without a prefix return
/// `None` so raw relative/no-prefix inputs can continue to canonicalization.
#[cfg(windows)]
pub fn windows_project_path_kind(path: &Path) -> Option<WindowsProjectPathKind> {
    let std::path::Component::Prefix(prefix) = path.components().next()? else {
        return None;
    };
    Some(match prefix.kind() {
        std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_) => {
            WindowsProjectPathKind::LocalDisk
        }
        std::path::Prefix::UNC(_, _) | std::path::Prefix::VerbatimUNC(_, _) => {
            WindowsProjectPathKind::NetworkShare
        }
        std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_) => {
            WindowsProjectPathKind::UnsupportedNamespace
        }
    })
}

#[cfg(windows)]
pub fn is_windows_local_disk_path(path: &Path) -> bool {
    windows_project_path_kind(path) == Some(WindowsProjectPathKind::LocalDisk)
}

#[cfg(windows)]
pub fn is_windows_network_share_path(path: &Path) -> bool {
    windows_project_path_kind(path) == Some(WindowsProjectPathKind::NetworkShare)
}

#[cfg(windows)]
fn windows_unsupported_project_path_error(path: &Path) -> String {
    format!(
        "path {} uses an unsupported Windows project namespace; device and generic verbatim namespaces are not supported for projects",
        path.to_string_lossy()
    )
}

fn raw_project_path_has_parent_traversal(path: &Path) -> bool {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return true;
    }

    #[cfg(windows)]
    {
        // `Path::components` intentionally treats the Win32 verbatim namespace
        // specially and can normalize an explicit `..` away before callers see
        // it. Inspect the raw spelling too so authority is never granted to a
        // different directory merely because canonicalization collapsed it.
        return path
            .to_string_lossy()
            .split(['\\', '/'])
            .any(|component| component == "..");
    }

    #[cfg(not(windows))]
    false
}

/// Validate the raw project path before any canonicalization or filesystem I/O.
///
/// Explicit parent traversal is rejected on every platform before it can be
/// normalized away. On Windows explicit local disks and network shares
/// (`UNC` / `VerbatimUNC`) may proceed, while device namespaces and generic
/// verbatim namespaces fail closed. Other relative/no-prefix inputs continue to
/// canonicalization, where the canonical project policy applies.
pub fn validate_project_path_ingress(path: &Path) -> Result<(), String> {
    if raw_project_path_has_parent_traversal(path) {
        return Err("project path must not contain parent traversal".to_string());
    }

    #[cfg(windows)]
    if windows_project_path_kind(path) == Some(WindowsProjectPathKind::UnsupportedNamespace) {
        return Err(windows_unsupported_project_path_error(path));
    }

    Ok(())
}

/// System directories that must never become project roots through the broad
/// `allow_cwd_anywhere` relaxation. An explicit allowed root still authorizes
/// these paths intentionally. Windows network shares require explicit allowed-root
/// authority and never inherit authority from `allow_cwd_anywhere`.
const DANGEROUS_PROJECT_ROOTS: &[&str] = &[
    "/",
    "/etc",
    "/bin",
    "/sbin",
    "/usr",
    "/var",
    #[cfg(target_os = "macos")]
    "/private/etc",
    #[cfg(target_os = "macos")]
    "/private/var",
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/boot",
    #[cfg(windows)]
    "C:\\Windows",
    #[cfg(windows)]
    "C:\\Program Files",
    #[cfg(windows)]
    "C:\\Program Files (x86)",
];

#[cfg(windows)]
fn is_windows_drive_root(canonical_path: &Path) -> bool {
    let mut components = canonical_path.components();
    matches!(
        (components.next(), components.next()),
        (
            Some(std::path::Component::Prefix(_)),
            Some(std::path::Component::RootDir)
        ) if components.next().is_none()
    )
}

#[cfg(not(windows))]
fn is_windows_drive_root(_canonical_path: &Path) -> bool {
    false
}

/// Canonicalize the `allowed_roots` entries that can currently provide path
/// authority.
///
/// `allowed_roots` is an OR-set of independent authority candidates. A stale,
/// unmounted, unreadable, non-directory, or otherwise unresolvable candidate
/// cannot authorize anything, but it must not poison another usable root.
/// Callers still apply the authoritative path policy after this projection;
/// an empty result therefore remains fail-closed unless that policy explicitly
/// permits the target through `allow_cwd_anywhere`.
pub fn canonicalize_usable_allowed_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .filter(|root| root.is_dir())
        .collect()
}

/// Authoritative pure path-policy check for Runner project registration.
///
/// `canonical_path` and `canonical_allowed_roots` must already be canonicalized
/// by the caller. Explicit roots authorize local-disk and network-share projects.
/// `allow_cwd_anywhere` relaxes only ordinary local-disk paths: it never grants
/// authority to a Windows network share, dangerous system root, or drive root.
pub fn validate_project_path_policy(
    canonical_path: &Path,
    canonical_allowed_roots: &[PathBuf],
    allow_cwd_anywhere: bool,
) -> Result<(), String> {
    let path_str = canonical_path.to_string_lossy();

    #[cfg(windows)]
    let windows_kind = match windows_project_path_kind(canonical_path) {
        Some(WindowsProjectPathKind::LocalDisk) => WindowsProjectPathKind::LocalDisk,
        Some(WindowsProjectPathKind::NetworkShare) => WindowsProjectPathKind::NetworkShare,
        Some(WindowsProjectPathKind::UnsupportedNamespace) | None => {
            return Err(windows_unsupported_project_path_error(canonical_path));
        }
    };

    if canonical_allowed_roots
        .iter()
        .any(|root| path_is_within(canonical_path, root))
    {
        return Ok(());
    }

    #[cfg(windows)]
    if windows_kind == WindowsProjectPathKind::NetworkShare {
        return Err(format!(
            "path {} is outside allowed_roots; allow_cwd_anywhere does not authorize Windows network shares",
            path_str
        ));
    }

    if !allow_cwd_anywhere {
        return Err(format!(
            "path {} is outside allowed_roots and allow_cwd_anywhere is false",
            path_str
        ));
    }

    for dangerous in DANGEROUS_PROJECT_ROOTS {
        let dangerous_root = Path::new(dangerous);
        let is_dangerous = if dangerous_root == Path::new("/") {
            paths_equal(canonical_path, dangerous_root)
        } else {
            path_is_within(canonical_path, dangerous_root)
        };
        if is_dangerous {
            return Err(format!(
                "path {} is under a dangerous system root; register it under an explicit allowed_roots entry if intended",
                path_str
            ));
        }
    }

    if is_windows_drive_root(canonical_path) {
        return Err(format!(
            "path {} is a Windows drive root; register it under an explicit allowed_roots entry if intended",
            path_str
        ));
    }

    Ok(())
}

/// Stable filesystem-independent identity string for a path, used for project
/// id hashing and registry comparisons.
///
/// - Unix: the raw path bytes (unchanged historical behavior).
/// - Windows: strips `\\?\` / `\\?\UNC\` extended-length prefixes, normalizes
///   separators to `\`, and lowercases (Windows filesystems are
///   case-insensitive, so `C:\Foo` and `c:\foo` are the same directory).
pub fn normalize_path_identity(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        String::from_utf8_lossy(path.as_os_str().as_bytes()).into_owned()
    }
    #[cfg(not(unix))]
    {
        let mut text = path.to_string_lossy().replace('/', "\\");
        if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
            text = format!("\\\\{}", rest);
        } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
            text = rest.to_string();
        }
        while text.len() > 1 && text.ends_with('\\') {
            text.pop();
        }
        text.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Single shared env-test lock for the whole crate: `lib.rs` tests and
    // `paths` tests both mutate process environment variables and must
    // serialize against each other.
    use crate::TEST_ENV_LOCK;

    static TEST_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn test_temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "webcodex-runner-config-{label}-{}-{}",
            std::process::id(),
            TEST_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn runner_config_path_prefers_canonical_and_keeps_legacy_only_compatibility() {
        let dir = test_temp_dir("compat");
        assert_eq!(
            resolve_runner_config_path(&dir).unwrap(),
            dir.join(RUNNER_CONFIG_FILE)
        );

        std::fs::write(dir.join(LEGACY_AGENT_CONFIG_FILE), "legacy").unwrap();
        assert_eq!(
            resolve_runner_config_path(&dir).unwrap(),
            dir.join(LEGACY_AGENT_CONFIG_FILE)
        );

        std::fs::remove_file(dir.join(LEGACY_AGENT_CONFIG_FILE)).unwrap();
        std::fs::write(dir.join(RUNNER_CONFIG_FILE), "current").unwrap();
        assert_eq!(
            resolve_runner_config_path(&dir).unwrap(),
            dir.join(RUNNER_CONFIG_FILE)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runner_config_path_fails_closed_when_both_names_exist() {
        let dir = test_temp_dir("dual");
        std::fs::write(dir.join(RUNNER_CONFIG_FILE), "current").unwrap();
        std::fs::write(dir.join(LEGACY_AGENT_CONFIG_FILE), "legacy").unwrap();
        let error = resolve_runner_config_path(&dir).unwrap_err();
        assert!(error.contains(RUNNER_CONFIG_FILE));
        assert!(error.contains(LEGACY_AGENT_CONFIG_FILE));
        assert!(error.contains("refusing to guess"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn project_registry_selection_prefers_new_layout_for_new_installs() {
        let temp = test_temp_dir("new-layout");
        assert_eq!(
            select_project_registry_dir(&temp).unwrap(),
            temp.join(PROJECT_REGISTRY_DIR_NAME)
        );
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn project_registry_selection_preserves_single_existing_layout() {
        let temp = test_temp_dir("current-layout");
        let current = temp.join(PROJECT_REGISTRY_DIR_NAME);
        std::fs::create_dir(&current).unwrap();
        assert_eq!(select_project_registry_dir(&temp).unwrap(), current);
        std::fs::remove_dir_all(temp).unwrap();

        let temp = test_temp_dir("legacy-layout");
        let legacy = temp.join(LEGACY_PROJECTS_DIR_NAME);
        std::fs::create_dir(&legacy).unwrap();
        assert_eq!(select_project_registry_dir(&temp).unwrap(), legacy);
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn project_registry_selection_fails_closed_when_both_layouts_exist() {
        let temp = test_temp_dir("ambiguous-layout");
        let current = temp.join(PROJECT_REGISTRY_DIR_NAME);
        let legacy = temp.join(LEGACY_PROJECTS_DIR_NAME);
        std::fs::create_dir(&current).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        let error = select_project_registry_dir(&temp).unwrap_err();
        assert!(error.contains("both Runner project registry directories exist"));
        assert!(error.contains(&current.display().to_string()));
        assert!(error.contains(&legacy.display().to_string()));
        assert!(error.contains("consolidate project registration records"));
        std::fs::remove_dir_all(temp).unwrap();
    }

    /// RAII restore for environment variables: restores the previous value
    /// (or removes the variable) on drop, even if the test panics.
    struct EnvVarRestore {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarRestore {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            EnvVarRestore { name, previous }
        }

        fn remove(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            std::env::remove_var(name);
            EnvVarRestore { name, previous }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn home_dir_prefers_home_on_unix_and_ignores_home_on_windows() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::remove("USERPROFILE");
        #[cfg(unix)]
        assert_eq!(home_dir(), Some(PathBuf::from("/home/alice")));
        #[cfg(windows)]
        assert_eq!(
            home_dir(),
            None,
            "MSYS-style HOME must not be used on Windows"
        );

        // Even a Win32-format HOME must never win on Windows: `USERPROFILE` is
        // the OS-owned home and `HOME` is a foreign variable that can disagree
        // with it. On Unix `USERPROFILE` is equally foreign and `HOME` wins.
        let _h2 = EnvVarRestore::set("HOME", "C:\\Users\\alice");
        let _u2 = EnvVarRestore::set("USERPROFILE", "D:\\Users\\alice");
        #[cfg(unix)]
        assert_eq!(home_dir(), Some(PathBuf::from("C:\\Users\\alice")));
        #[cfg(windows)]
        assert_eq!(
            home_dir(),
            Some(PathBuf::from("D:\\Users\\alice")),
            "USERPROFILE must win over HOME on Windows"
        );

        // No USERPROFILE: Windows has no home at all, even with HOME set.
        let _u3 = EnvVarRestore::remove("USERPROFILE");
        #[cfg(windows)]
        assert_eq!(home_dir(), None, "HOME must not substitute for USERPROFILE");
        #[cfg(unix)]
        assert_eq!(home_dir(), Some(PathBuf::from("C:\\Users\\alice")));
    }

    #[test]
    fn home_dir_uses_userprofile_when_home_is_absent() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        #[cfg(windows)]
        assert_eq!(home_dir(), Some(PathBuf::from("C:\\Users\\alice")));
        #[cfg(not(windows))]
        assert_eq!(home_dir(), None);
    }

    #[test]
    fn config_base_never_falls_back_to_current_directory() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _a = EnvVarRestore::remove("APPDATA");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(windows)]
        assert!(
            default_client_config_base_dir().is_err(),
            "no usable per-user directory must be an error, never CWD"
        );
        // On Unix, effective root intentionally uses the system config scope
        // even without HOME; non-root must still fail closed rather than use CWD.
        #[cfg(not(windows))]
        if is_effective_root() {
            assert_eq!(
                default_client_config_base_dir().unwrap(),
                PathBuf::from("/etc/webcodex")
            );
        } else {
            assert!(default_client_config_base_dir().is_err());
        }
    }

    #[test]
    fn windows_config_base_uses_appdata_then_userprofile() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(windows)]
        {
            let _a = EnvVarRestore::set("APPDATA", "C:\\Users\\alice\\AppData\\Roaming");
            assert_eq!(
                default_client_config_base_dir().unwrap(),
                PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex")
            );
        }
        let _a2 = EnvVarRestore::remove("APPDATA");
        #[cfg(windows)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.config\\webcodex")
        );
    }

    #[test]
    fn windows_state_base_uses_localappdata_then_userprofile() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _x = EnvVarRestore::remove("XDG_STATE_HOME");
        #[cfg(windows)]
        {
            let _l = EnvVarRestore::set("LOCALAPPDATA", "C:\\Users\\alice\\AppData\\Local");
            assert_eq!(
                default_client_state_base_dir().unwrap(),
                PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex")
            );
        }
        let _l2 = EnvVarRestore::remove("LOCALAPPDATA");
        #[cfg(windows)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.local\\state\\webcodex")
        );
    }

    #[test]
    fn config_base_honors_xdg_on_unix_and_ignores_it_on_windows() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _a = EnvVarRestore::set("APPDATA", "C:\\Users\\alice\\AppData\\Roaming");
        let _x = EnvVarRestore::set("XDG_CONFIG_HOME", "/tmp/cfg");
        #[cfg(unix)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("/tmp/cfg/webcodex")
        );
        #[cfg(windows)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex"),
            "XDG_CONFIG_HOME must be ignored on Windows"
        );
        let _x2 = EnvVarRestore::remove("XDG_CONFIG_HOME");
        #[cfg(unix)]
        if is_effective_root() {
            assert_eq!(
                default_client_config_base_dir().unwrap(),
                PathBuf::from("/etc/webcodex")
            );
        } else {
            assert_eq!(
                default_client_config_base_dir().unwrap(),
                PathBuf::from("/home/alice/.config/webcodex")
            );
        }
        #[cfg(windows)]
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex")
        );
    }

    #[test]
    fn state_base_honors_xdg_on_unix_and_ignores_it_on_windows() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::set("HOME", "/home/alice");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _l = EnvVarRestore::set("LOCALAPPDATA", "C:\\Users\\alice\\AppData\\Local");
        let _x = EnvVarRestore::set("XDG_STATE_HOME", "/tmp/state");
        #[cfg(unix)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("/tmp/state/webcodex")
        );
        #[cfg(windows)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex"),
            "XDG_STATE_HOME must be ignored on Windows"
        );
        let _x2 = EnvVarRestore::remove("XDG_STATE_HOME");
        #[cfg(unix)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("/home/alice/.local/state/webcodex")
        );
        #[cfg(windows)]
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_appdata_and_localappdata_work_without_userprofile() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        let _s = EnvVarRestore::remove("XDG_STATE_HOME");

        let _a = EnvVarRestore::set("APPDATA", "C:\\Users\\alice\\AppData\\Roaming");
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Roaming\\webcodex"),
            "APPDATA alone must be enough for the config base"
        );
        let _l = EnvVarRestore::set("LOCALAPPDATA", "C:\\Users\\alice\\AppData\\Local");
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\AppData\\Local\\webcodex"),
            "LOCALAPPDATA alone must be enough for the state base"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_ignores_win32_home_when_userprofile_is_absent() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Even a perfectly Win32-shaped HOME must not be consulted.
        let _h = EnvVarRestore::set("HOME", "C:\\Users\\alice");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _a = EnvVarRestore::remove("APPDATA");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        assert!(
            default_client_config_base_dir().is_err(),
            "config base must not derive from HOME on Windows"
        );
        let _l = EnvVarRestore::remove("LOCALAPPDATA");
        let _s = EnvVarRestore::remove("XDG_STATE_HOME");
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            std::env::temp_dir().join("webcodex"),
            "state base must fall back to TEMP, not HOME, on Windows"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_userprofile_fallbacks_are_dot_config_and_dot_local_state() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _u = EnvVarRestore::set("USERPROFILE", "C:\\Users\\alice");
        let _a = EnvVarRestore::remove("APPDATA");
        let _l = EnvVarRestore::remove("LOCALAPPDATA");
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        let _s = EnvVarRestore::remove("XDG_STATE_HOME");
        assert_eq!(
            default_client_config_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.config\\webcodex")
        );
        assert_eq!(
            default_client_state_base_dir().unwrap(),
            PathBuf::from("C:\\Users\\alice\\.local\\state\\webcodex")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_project_prefix_classification_distinguishes_disk_network_and_namespace() {
        for local in [
            r"C:\repo",
            r"c:\repo",
            r"\\?\C:\repo",
            r"C:\Users\alice\proj\",
        ] {
            assert_eq!(
                windows_project_path_kind(Path::new(local)),
                Some(WindowsProjectPathKind::LocalDisk),
                "{local} must be classified as a local disk"
            );
            assert!(is_windows_local_disk_path(Path::new(local)));
        }

        for network in [r"\\server\share\repo", r"\\?\UNC\server\share\repo"] {
            assert_eq!(
                windows_project_path_kind(Path::new(network)),
                Some(WindowsProjectPathKind::NetworkShare),
                "{network} must be classified as a network share"
            );
            assert!(is_windows_network_share_path(Path::new(network)));
        }

        for unsupported in [
            r"\\.\device\repo",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\repo",
        ] {
            assert_eq!(
                windows_project_path_kind(Path::new(unsupported)),
                Some(WindowsProjectPathKind::UnsupportedNamespace),
                "{unsupported} must fail closed as an unsupported namespace"
            );
        }

        for unprefixed in [r"\repo", r"repo", "."] {
            assert_eq!(windows_project_path_kind(Path::new(unprefixed)), None);
        }
    }

    #[test]
    fn raw_project_ingress_rejects_parent_traversal_before_canonicalization() {
        for rejected in ["repo/../other", "../repo"] {
            let error = validate_project_path_ingress(Path::new(rejected)).unwrap_err();
            assert!(error.contains("parent traversal"), "{rejected}: {error}");
        }
        validate_project_path_ingress(Path::new("repo/.../child"))
            .expect("three dots are an ordinary path component");
    }

    #[cfg(windows)]
    #[test]
    fn windows_raw_project_ingress_allows_disk_and_network_share_prefixes() {
        for allowed in [
            r"C:\repo",
            r"\\?\C:\repo",
            r"\\server\share\repo",
            r"\\?\UNC\server\share\repo",
            ".",
            r"repo",
            r"some\repo",
            r"\repo",
        ] {
            validate_project_path_ingress(Path::new(allowed)).unwrap_or_else(|error| {
                panic!("{allowed} must proceed to canonicalization: {error}")
            });
        }

        for rejected in [
            r"\\.\device\repo",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\repo",
        ] {
            let error = validate_project_path_ingress(Path::new(rejected)).unwrap_err();
            assert!(
                error.contains("unsupported Windows project namespace"),
                "{error}"
            );
        }

        for rejected in [
            r"C:\repo\..\other",
            r"\\?\C:\repo\..\other",
            r"\\server\share\repo\..\other",
            r"\\?\UNC\server\share\repo\..\other",
            r"repo\..\other",
            "repo/../other",
        ] {
            let error = validate_project_path_ingress(Path::new(rejected)).unwrap_err();
            assert!(error.contains("parent traversal"), "{rejected}: {error}");
        }

        for allowed in [r"C:\repo\..name", r"repo\...\child"] {
            validate_project_path_ingress(Path::new(allowed))
                .unwrap_or_else(|error| panic!("{allowed} is not traversal: {error}"));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn project_path_policy_keeps_dangerous_roots_fail_closed_under_cwd_anywhere() {
        let error = validate_project_path_policy(Path::new("/etc"), &[], true).unwrap_err();
        assert!(error.contains("dangerous system root"), "{error}");

        validate_project_path_policy(Path::new("/tmp/webcodex-project"), &[], true)
            .expect("ordinary paths remain allowed by allow_cwd_anywhere");

        validate_project_path_policy(Path::new("/etc"), &[PathBuf::from("/etc")], true)
            .expect("an explicit allowed root must intentionally authorize /etc");
    }

    #[cfg(windows)]
    #[test]
    fn project_path_policy_requires_explicit_network_authority_and_keeps_namespace_fences() {
        let network = Path::new(r"\\?\UNC\SERVER\Share\Repo");
        validate_project_path_policy(network, &[PathBuf::from(r"\\server\share")], false)
            .expect("a matching explicit network root must authorize the project");
        validate_project_path_policy(
            Path::new(r"\\server\SHARE\Repo\Child"),
            &[PathBuf::from(r"\\?\UNC\server\share\repo")],
            false,
        )
        .expect("network containment must use Windows case-insensitive identity semantics");

        for allow_cwd_anywhere in [false, true] {
            let error = validate_project_path_policy(network, &[], allow_cwd_anywhere).unwrap_err();
            assert!(error.contains("outside allowed_roots"), "{error}");
            if allow_cwd_anywhere {
                assert!(
                    error.contains("does not authorize Windows network shares"),
                    "{error}"
                );
            }
        }

        for unsupported in [
            r"\\.\device\repo",
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\repo",
        ] {
            let error = validate_project_path_policy(
                Path::new(unsupported),
                &[PathBuf::from(unsupported)],
                true,
            )
            .unwrap_err();
            assert!(
                error.contains("unsupported Windows project namespace"),
                "{error}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn project_path_policy_preserves_windows_local_disk_and_drive_root_fences() {
        let drive_root = Path::new(r"C:\");
        let error = validate_project_path_policy(drive_root, &[], true).unwrap_err();
        assert!(error.contains("Windows drive root"), "{error}");
        validate_project_path_policy(drive_root, &[PathBuf::from(r"C:\")], true)
            .expect("an explicit local-disk root must authorize the drive root");

        validate_project_path_policy(Path::new(r"C:\Users\alice\repo"), &[], true)
            .expect("ordinary local-disk paths remain allowed by allow_cwd_anywhere");
    }

    #[test]
    fn state_base_falls_back_to_temp_only_when_no_home_anywhere() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _h = EnvVarRestore::remove("HOME");
        let _x = EnvVarRestore::remove("XDG_STATE_HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _l = EnvVarRestore::remove("LOCALAPPDATA");
        let _a = EnvVarRestore::remove("APPDATA");
        let base = default_client_state_base_dir().unwrap();
        assert!(base.is_absolute(), "state fallback must stay absolute");
        assert_eq!(base, std::env::temp_dir().join("webcodex"));
    }

    #[test]
    fn user_config_home_requires_absolute_home() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _x = EnvVarRestore::remove("XDG_CONFIG_HOME");
        let _u = EnvVarRestore::remove("USERPROFILE");
        let _h = EnvVarRestore::set("HOME", "/c/Users/alice");
        #[cfg(windows)]
        assert!(
            user_config_home().is_err(),
            "MSYS-style HOME is relative on Windows and must not be used"
        );
        #[cfg(unix)]
        assert_eq!(
            user_config_home().unwrap(),
            PathBuf::from("/c/Users/alice/.config")
        );
    }

    #[test]
    fn path_identity_is_case_insensitive_on_windows() {
        #[cfg(windows)]
        {
            assert_eq!(
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
                normalize_path_identity(Path::new(r"c:\foo\bar")),
            );
            assert_eq!(
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
                normalize_path_identity(Path::new(r"\\?\C:\Foo\Bar")),
            );
            assert_eq!(
                normalize_path_identity(Path::new(r"\\?\C:\Foo\Bar\")),
                normalize_path_identity(Path::new(r"C:\Foo\Bar")),
            );
            assert!(paths_equal(
                Path::new(r"C:\Foo\Bar"),
                Path::new(r"c:\foo\bar")
            ));
            assert!(path_is_within(
                Path::new(r"C:\Users\Alice\proj"),
                Path::new(r"c:\users\alice")
            ));
            assert!(!path_is_within(
                Path::new(r"C:\Users\Alice2\proj"),
                Path::new(r"c:\users\alice")
            ));
            assert_eq!(
                normalize_path_identity(Path::new(r"\\SERVER\Share\Repo\")),
                normalize_path_identity(Path::new(r"\\?\UNC\server\share\repo")),
            );
            assert!(paths_equal(
                Path::new(r"\\SERVER\Share\Repo\"),
                Path::new(r"\\?\UNC\server\share\repo")
            ));
            assert!(path_is_within(
                Path::new(r"\\?\UNC\SERVER\Share\Repo\Child"),
                Path::new(r"\\server\share\repo")
            ));
            assert!(!path_is_within(
                Path::new(r"\\server\share2\repo"),
                Path::new(r"\\server\share")
            ));
        }
        #[cfg(unix)]
        {
            assert_eq!(
                normalize_path_identity(Path::new("/home/alice/proj")),
                "/home/alice/proj"
            );
            assert!(paths_equal(
                Path::new("/home/alice/proj"),
                Path::new("/home/alice/proj")
            ));
            assert!(!paths_equal(
                Path::new("/home/alice/proj"),
                Path::new("/HOME/ALICE/PROJ")
            ));
        }
    }
}
