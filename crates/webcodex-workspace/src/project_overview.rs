//! Deterministic, bounded project-structure inspection shared by the runtime
//! local-executor parity path and `webcodex-runner`.
//!
//! This module reads directory entries and file types only. It never reads file
//! contents, follows symlinks, invokes a shell, or consults environment values.

use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};

pub const PROJECT_OVERVIEW_DEFAULT_MAX_DEPTH: usize = 2;
pub const PROJECT_OVERVIEW_MIN_MAX_DEPTH: usize = 1;
pub const PROJECT_OVERVIEW_MAX_MAX_DEPTH: usize = 4;
pub const PROJECT_OVERVIEW_DEFAULT_LIMIT: usize = 200;
pub const PROJECT_OVERVIEW_MIN_LIMIT: usize = 20;
pub const PROJECT_OVERVIEW_MAX_LIMIT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct ScanEntry {
    path: String,
    scoped_path: String,
    depth: usize,
    kind: EntryKind,
}

#[derive(Debug)]
struct PendingDirectory {
    absolute_path: PathBuf,
    scoped_path: String,
    depth: usize,
}

#[derive(Debug)]
struct KeyFile {
    path: String,
    kind: &'static str,
    reason: &'static str,
    priority: u8,
    lockfile: bool,
}

/// Normalize a caller-supplied project-relative directory path. The returned
/// string is empty for the project root and otherwise uses `/` separators.
pub fn normalize_project_overview_path(path: &str) -> Result<String, String> {
    if path.contains('\0') {
        return Err("path cannot contain NUL bytes".to_string());
    }
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(String::new());
    }
    let raw = Path::new(trimmed);
    if raw.is_absolute() {
        return Err("path must be project-relative".to_string());
    }
    let mut parts = Vec::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| "path must be valid UTF-8".to_string())?;
                parts.push(part.to_string());
            }
            Component::ParentDir => return Err("path cannot contain parent traversal".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be project-relative".to_string())
            }
        }
    }
    let normalized = parts.join("/");
    if is_project_overview_excluded_path(&normalized) {
        return Err("path is protected or excluded from project overview scans".to_string());
    }
    Ok(normalized)
}

pub fn effective_project_overview_max_depth(value: Option<usize>) -> usize {
    value.unwrap_or(PROJECT_OVERVIEW_DEFAULT_MAX_DEPTH).clamp(
        PROJECT_OVERVIEW_MIN_MAX_DEPTH,
        PROJECT_OVERVIEW_MAX_MAX_DEPTH,
    )
}

pub fn effective_project_overview_limit(value: Option<usize>) -> usize {
    value
        .unwrap_or(PROJECT_OVERVIEW_DEFAULT_LIMIT)
        .clamp(PROJECT_OVERVIEW_MIN_LIMIT, PROJECT_OVERVIEW_MAX_LIMIT)
}

/// Scan `requested_path` under `project_root` and build the agent-owned
/// project-overview payload. The runtime adds the opaque runtime project id.
pub fn build_project_overview(
    project_root: &Path,
    requested_path: &str,
    max_depth: Option<usize>,
    limit: Option<usize>,
) -> Result<Value, String> {
    let path = normalize_project_overview_path(requested_path)?;
    let max_depth = effective_project_overview_max_depth(max_depth);
    let limit = effective_project_overview_limit(limit);

    let canonical_root = project_root
        .canonicalize()
        .map_err(|error| format!("project root does not exist: {error}"))?;
    let requested = if path.is_empty() {
        canonical_root.clone()
    } else {
        canonical_root.join(&path)
    };
    let canonical_scope = requested
        .canonicalize()
        .map_err(|error| format!("path does not exist: {error}"))?;
    if !canonical_scope.starts_with(&canonical_root) {
        return Err("path is outside project directory".to_string());
    }
    if !canonical_scope.is_dir() {
        return Err("path is not a directory".to_string());
    }

    // Project detection trusts the git index: the author's own statement of
    // what belongs to the project. Untracked tool state (.opencode/, .codex/,
    // caches, virtualenvs) otherwise pollutes language/manifest detection —
    // the field test misclassified a pure-Python thesis repo as node because
    // of a gitignored .opencode/package.json. Non-git directories (and empty
    // indexes, e.g. fresh `git init`) fall back to the filesystem walk.
    let tracked = git_tracked_index(&canonical_root);

    let mut queue = VecDeque::from([PendingDirectory {
        absolute_path: canonical_scope,
        scoped_path: String::new(),
        depth: 0,
    }]);
    let mut entries = Vec::new();
    let mut limit_truncated = false;
    let mut depth_truncated = false;
    let mut skipped_symlink = false;
    let mut skipped_unreadable = false;
    let mut skipped_non_utf8 = false;

    'directories: while let Some(directory) = queue.pop_front() {
        if entries.len() >= limit {
            limit_truncated = true;
            break;
        }
        let read_dir = match std::fs::read_dir(&directory.absolute_path) {
            Ok(read_dir) => read_dir,
            Err(error) if directory.depth == 0 => {
                return Err(format!("failed to read directory: {error}"))
            }
            Err(_) => {
                skipped_unreadable = true;
                continue;
            }
        };
        let mut children = Vec::new();
        for child in read_dir {
            match child {
                Ok(child) => children.push(child),
                Err(_) => skipped_unreadable = true,
            }
        }
        children.sort_by_key(|left| left.file_name());

        for child in children {
            let Some(name) = child.file_name().to_str().map(str::to_string) else {
                skipped_non_utf8 = true;
                continue;
            };
            let scoped_path = join_relative(&directory.scoped_path, &name);
            let project_path = join_relative(&path, &scoped_path);
            if is_project_overview_excluded_path(&project_path) {
                continue;
            }
            let file_type = match child.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    skipped_unreadable = true;
                    continue;
                }
            };
            if file_type.is_symlink() {
                skipped_symlink = true;
                continue;
            }
            let kind = if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                continue;
            };
            if let Some((tracked_files, tracked_dirs)) = tracked.as_ref() {
                let known = match kind {
                    EntryKind::File => tracked_files.contains(&project_path),
                    EntryKind::Directory => tracked_dirs.contains(&project_path),
                };
                if !known {
                    continue;
                }
            }
            if entries.len() >= limit {
                limit_truncated = true;
                break 'directories;
            }
            let depth = directory.depth + 1;
            entries.push(ScanEntry {
                path: project_path,
                scoped_path: scoped_path.clone(),
                depth,
                kind,
            });
            if kind == EntryKind::Directory {
                if depth < max_depth {
                    queue.push_back(PendingDirectory {
                        absolute_path: child.path(),
                        scoped_path,
                        depth,
                    });
                } else {
                    depth_truncated = true;
                }
            }
        }
    }

    let project_types = project_types(&entries);
    let manifests = manifests(&entries);
    let key_files = key_files(&entries);
    let suggested_next_reads = suggested_next_reads(&key_files);
    let roots = roots(&entries);
    let top_level = top_level(&entries);
    let truncation_reason = match (limit_truncated, depth_truncated) {
        (true, true) => Some("limit_and_max_depth"),
        (true, false) => Some("limit"),
        (false, true) => Some("max_depth"),
        (false, false) => None,
    };
    let mut warnings = Vec::new();
    if skipped_symlink {
        warnings.push("symlinks_skipped");
    }
    if skipped_unreadable {
        warnings.push("unreadable_entries_skipped");
    }
    if skipped_non_utf8 {
        warnings.push("non_utf8_paths_skipped");
    }

    Ok(json!({
        "schema_version": 1,
        "path": path,
        "deterministic": true,
        "project_types": project_types,
        "manifests": manifests,
        "key_files": key_files_to_json(&key_files),
        "roots": roots,
        "top_level": top_level,
        "suggested_next_reads": suggested_next_reads,
        "scan": {
            "max_depth": max_depth,
            "limit": limit,
            "returned_entry_count": entries.len(),
            "truncated": limit_truncated || depth_truncated,
            "truncation_reason": truncation_reason,
        },
        "warnings": warnings,
    }))
}

fn join_relative(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn is_project_overview_excluded_path(path: &str) -> bool {
    crate::path_policy::sensitive_path(path) || path.split('/').any(is_excluded_component)
}

fn is_excluded_component(component: &str) -> bool {
    let lower = component.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".git"
            | "target"
            | "node_modules"
            | "vendor"
            | "dist"
            | "build"
            | ".next"
            | "coverage"
            | "cache"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".tox"
            | ".cache"
            | ".gradle"
            | ".parcel-cache"
            | ".pnpm-store"
            | ".turbo"
            | "project-registry"
            | "projects.d"
            | "secrets"
            | "secret"
            | "tokens"
            | "token"
            | "credentials"
            | "credential"
            | "passwords"
            | "password"
            | "runner.toml"
            | "agent.toml"
            | "webcodex.env"
            | ".env"
            | ".npmrc"
            | ".netrc"
            | ".pypirc"
            | ".ssh"
            | ".aws"
            | "id_rsa"
            | "id_ed25519"
    ) {
        return true;
    }
    lower.starts_with(".env.")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Tracked files and their ancestor directories from `git ls-files -z`,
/// project-root-relative with `/` separators. `None` when the directory is
/// not a usable git checkout or the index is empty, so the caller falls back
/// to the plain filesystem walk.
fn git_tracked_index(
    root: &std::path::Path,
) -> Option<(
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
)> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files = std::collections::HashSet::new();
    let mut dirs = std::collections::HashSet::new();
    for path in String::from_utf8_lossy(&output.stdout).split('\0') {
        if path.is_empty() {
            continue;
        }
        files.insert(path.to_string());
        let mut ancestor = path;
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if !dirs.insert(parent.to_string()) {
                break;
            }
            ancestor = parent;
        }
    }
    if files.is_empty() {
        return None;
    }
    Some((files, dirs))
}

fn project_types(entries: &[ScanEntry]) -> Vec<Value> {
    let mut evidence: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.kind == EntryKind::File) {
        if let Some((kind, _)) = manifest_kind(&entry.path) {
            evidence.entry(kind).or_default().push(entry.path.clone());
        }
    }
    const ORDER: &[&str] = &[
        "rust", "node", "python", "go", "jvm", "dotnet", "ruby", "php", "cpp",
    ];
    ORDER
        .iter()
        .filter_map(|kind| {
            evidence.remove(kind).map(|mut paths| {
                paths.sort();
                paths.dedup();
                json!({"kind": kind, "evidence": paths})
            })
        })
        .collect()
}

fn manifests(entries: &[ScanEntry]) -> Vec<Value> {
    let mut values = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
        .filter_map(|entry| {
            manifest_kind(&entry.path).map(|(_, kind)| {
                json!({
                    "path": entry.path,
                    "kind": kind,
                })
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["path"].as_str().unwrap_or_default())
    });
    values
}

fn manifest_kind(path: &str) -> Option<(&'static str, &'static str)> {
    let name = basename(path);
    match name {
        "Cargo.toml" => Some(("rust", "rust_manifest")),
        "package.json" => Some(("node", "node_manifest")),
        "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" | "Pipfile" => {
            Some(("python", "python_manifest"))
        }
        "go.mod" => Some(("go", "go_manifest")),
        "pom.xml" | "build.gradle" | "build.gradle.kts" => Some(("jvm", "jvm_manifest")),
        "Gemfile" => Some(("ruby", "ruby_manifest")),
        "composer.json" => Some(("php", "php_manifest")),
        "CMakeLists.txt" | "meson.build" => Some(("cpp", "cpp_manifest")),
        _ if name.ends_with(".sln") => Some(("dotnet", "dotnet_solution")),
        _ if name.ends_with(".csproj") => Some(("dotnet", "dotnet_project")),
        _ => None,
    }
}

fn key_files(entries: &[ScanEntry]) -> Vec<KeyFile> {
    let mut keys = entries
        .iter()
        .filter_map(classify_key_file)
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.path.cmp(&right.path))
    });
    keys
}

fn classify_key_file(entry: &ScanEntry) -> Option<KeyFile> {
    let name = basename(&entry.path);
    let lower_name = name.to_ascii_lowercase();
    let lower_scoped = entry.scoped_path.to_ascii_lowercase();
    let file = entry.kind == EntryKind::File;
    let directory = entry.kind == EntryKind::Directory;

    if file
        && (lower_name == "agents.md"
            || lower_name == "claude.md"
            || lower_scoped == ".github/copilot-instructions.md")
    {
        return Some(key(
            entry,
            "agent_instructions",
            "project-local agent guidance",
            0,
            false,
        ));
    }
    if file && is_readme(&lower_name) {
        return Some(key(entry, "readme", "project overview", 1, false));
    }
    if file && is_primary_key_manifest(name) {
        return Some(key(entry, "manifest", manifest_reason(name), 2, false));
    }
    if file && is_documentation_entrypoint(&lower_scoped, &lower_name) {
        return Some(key(
            entry,
            "documentation",
            "contribution or development documentation",
            3,
            false,
        ));
    }
    if file && is_lockfile(name) {
        return Some(key(entry, "lockfile", "dependency lockfile", 4, true));
    }
    if (file && is_ci_or_container_file(&lower_scoped, &lower_name))
        || (directory && lower_scoped == ".github/workflows")
    {
        return Some(key(
            entry,
            if lower_name.contains("docker") || lower_name.starts_with("compose.") {
                "container"
            } else {
                "ci"
            },
            "container or continuous-integration entrypoint",
            5,
            false,
        ));
    }
    None
}

fn key(
    entry: &ScanEntry,
    kind: &'static str,
    reason: &'static str,
    priority: u8,
    lockfile: bool,
) -> KeyFile {
    KeyFile {
        path: entry.path.clone(),
        kind,
        reason,
        priority,
        lockfile,
    }
}

fn is_readme(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "readme" | "readme.md" | "readme.rst" | "readme.txt"
    )
}

fn is_primary_key_manifest(name: &str) -> bool {
    matches!(
        name,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "go.mod"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "Gemfile"
            | "composer.json"
            | "CMakeLists.txt"
            | "meson.build"
    ) || name.ends_with(".sln")
        || name.ends_with(".csproj")
}

fn manifest_reason(name: &str) -> &'static str {
    match name {
        "Cargo.toml" => "Rust workspace or package metadata",
        "package.json" => "Node package metadata",
        "pyproject.toml" => "Python project metadata",
        "go.mod" => "Go module metadata",
        "pom.xml" | "build.gradle" | "build.gradle.kts" => "JVM build metadata",
        "Gemfile" => "Ruby dependency metadata",
        "composer.json" => "PHP package metadata",
        "CMakeLists.txt" | "meson.build" => "C or C++ build metadata",
        _ if name.ends_with(".sln") || name.ends_with(".csproj") => ".NET project metadata",
        _ => "build or package metadata",
    }
}

fn is_documentation_entrypoint(lower_scoped: &str, lower_name: &str) -> bool {
    matches!(lower_name, "contributing.md" | "developing.md")
        || matches!(lower_scoped, "docs/index.md" | "docs/readme.md")
}

fn is_lockfile(name: &str) -> bool {
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "uv.lock"
            | "Pipfile.lock"
            | "go.sum"
            | "composer.lock"
            | "Gemfile.lock"
    )
}

fn is_ci_or_container_file(lower_scoped: &str, lower_name: &str) -> bool {
    lower_name == "dockerfile"
        || matches!(
            lower_name,
            "docker-compose.yml"
                | "docker-compose.yaml"
                | "compose.yml"
                | "compose.yaml"
                | ".gitlab-ci.yml"
                | ".gitlab-ci.yaml"
                | "azure-pipelines.yml"
                | "jenkinsfile"
        )
        || lower_scoped.starts_with(".github/workflows/")
        || lower_scoped == ".circleci/config.yml"
}

fn key_files_to_json(keys: &[KeyFile]) -> Vec<Value> {
    keys.iter()
        .map(|key| {
            json!({
                "path": key.path,
                "kind": key.kind,
                "reason": key.reason,
            })
        })
        .collect()
}

fn suggested_next_reads(keys: &[KeyFile]) -> Vec<Value> {
    let mut candidates = keys.iter().filter(|key| !key.lockfile).collect::<Vec<_>>();
    if candidates.is_empty() {
        candidates = keys.iter().collect();
    }
    candidates
        .into_iter()
        .take(12)
        .map(|key| json!({"path": key.path, "reason": key.reason}))
        .collect()
}

fn roots(entries: &[ScanEntry]) -> Value {
    let mut source = Vec::new();
    let mut tests = Vec::new();
    let mut docs = Vec::new();
    let mut examples = Vec::new();
    let mut scripts = Vec::new();
    let mut ci = Vec::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory && entry.depth == 1)
    {
        let name = basename(&entry.path).to_ascii_lowercase();
        let target = match name.as_str() {
            "src" | "crates" | "packages" | "app" | "apps" | "lib" | "cmd" | "internal" => {
                Some(&mut source)
            }
            "tests" | "test" | "spec" | "__tests__" => Some(&mut tests),
            "docs" | "doc" => Some(&mut docs),
            "examples" | "example" => Some(&mut examples),
            "scripts" | "tools" => Some(&mut scripts),
            ".github" | ".gitlab" | "ci" => Some(&mut ci),
            _ => None,
        };
        if let Some(target) = target {
            target.push(entry.path.clone());
        }
    }
    for paths in [
        &mut source,
        &mut tests,
        &mut docs,
        &mut examples,
        &mut scripts,
        &mut ci,
    ] {
        paths.sort();
        paths.dedup();
    }
    json!({
        "source": source,
        "tests": tests,
        "docs": docs,
        "examples": examples,
        "scripts": scripts,
        "ci": ci,
        "classification_basis": "conventional_directory_name",
    })
}

fn top_level(entries: &[ScanEntry]) -> Vec<Value> {
    let mut values = entries
        .iter()
        .filter(|entry| entry.depth == 1)
        .map(|entry| {
            json!({
                "path": entry.path,
                "kind": match entry.kind {
                    EntryKind::File => "file",
                    EntryKind::Directory => "directory",
                },
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["path"].as_str().unwrap_or_default())
    });
    values
}

/// Fixed set of accepted project-type kinds. Synchronized with the
/// `project_types` ORDER used by [`project_types`].
const PROJECT_TYPE_KINDS: &[&str] = &[
    "rust", "node", "python", "go", "jvm", "dotnet", "ruby", "php", "cpp",
];

/// Fixed set of accepted manifest kinds. Synchronized with [`manifest_kind`].
const MANIFEST_KINDS: &[&str] = &[
    "rust_manifest",
    "node_manifest",
    "python_manifest",
    "go_manifest",
    "jvm_manifest",
    "ruby_manifest",
    "php_manifest",
    "cpp_manifest",
    "dotnet_solution",
    "dotnet_project",
];

/// Fixed set of accepted key-file kinds. Synchronized with [`classify_key_file`].
const KEY_FILE_KINDS: &[&str] = &[
    "agent_instructions",
    "readme",
    "manifest",
    "documentation",
    "lockfile",
    "container",
    "ci",
];

/// Fixed set of accepted top-level kinds.
const TOP_LEVEL_KINDS: &[&str] = &["file", "directory"];

/// Fixed set of accepted scan warning codes.
const WARNING_KINDS: &[&str] = &[
    "symlinks_skipped",
    "unreadable_entries_skipped",
    "non_utf8_paths_skipped",
];

/// Fixed set of accepted root classification buckets.
const ROOT_CLASSES: &[&str] = &["source", "tests", "docs", "examples", "scripts", "ci"];

/// Fixed set of accepted scan truncation reasons; `null` is also accepted.
const TRUNCATION_REASONS: &[&str] = &["limit", "max_depth", "limit_and_max_depth"];

/// A path is in `scope` (the normalized request path) when it equals `scope` or
/// lives underneath it. Both are normalized project-relative strings; `scope` is
/// empty for the project root.
fn path_within_scope(path: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}

/// Validate a single project-relative path field. The returned string is the
/// canonicalized form when the path is acceptable (the caller may use it to
/// re-emit the field without trusting extra trailing data).
fn validate_path_field(path: &str) -> Result<String, String> {
    if path.contains('\0') {
        return Err("path contains NUL".to_string());
    }
    if path.is_empty() {
        return Err("path is empty".to_string());
    }
    if path.contains('\\') {
        return Err("path must use forward slashes".to_string());
    }
    let raw = Path::new(path);
    if raw.is_absolute() {
        return Err("path must be project-relative".to_string());
    }
    let mut parts = Vec::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| "path must be valid UTF-8".to_string())?;
                parts.push(part.to_string());
            }
            Component::ParentDir => return Err("path cannot contain parent traversal".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be project-relative".to_string())
            }
        }
    }
    if parts.is_empty() {
        return Err("path is empty".to_string());
    }
    let normalized = parts.join("/");
    if normalized != path {
        return Err("path must be normalized".to_string());
    }
    if is_project_overview_excluded_path(&normalized) {
        return Err("path is protected or excluded".to_string());
    }
    Ok(normalized)
}

/// Validate and normalize a project-overview payload generated locally or
/// returned by a Runner. `expected_path` is the normalized request scope (empty
/// for the project root), `expected_max_depth` and `expected_limit` are the
/// effective clamped bounds the request resolved to. The function rejects
/// malformed payloads (absolute paths, parent traversal, unknown enums,
/// request-boundary mismatch, extra fields) and returns a payload that keeps
/// only the formal contract fields. Never trusts Runner-supplied extras.
pub fn validate_project_overview(
    payload: &Value,
    expected_path: &str,
    expected_max_depth: usize,
    expected_limit: usize,
) -> Result<Value, String> {
    let object = payload
        .as_object()
        .ok_or_else(|| "overview must be a JSON object".to_string())?;

    if object.get("schema_version") != Some(&json!(1)) {
        return Err("schema_version must be 1".to_string());
    }
    let deterministic = object
        .get("deterministic")
        .and_then(Value::as_bool)
        .ok_or_else(|| "deterministic must be boolean".to_string())?;
    if !deterministic {
        return Err("deterministic must be true".to_string());
    }
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "path must be a string".to_string())?;
    // Re-normalize and require the exact canonical echo. A Runner may not
    // substitute "." for the root or "./subdir" for "subdir", even though
    // those spellings resolve to the same scope.
    let normalized_path = normalize_project_overview_path(path)?;
    if path != normalized_path {
        return Err("path must use the canonical request form".to_string());
    }
    if normalized_path != expected_path {
        return Err("path does not match the request scope".to_string());
    }

    let scan = object
        .get("scan")
        .ok_or_else(|| "scan is required".to_string())?
        .as_object()
        .ok_or_else(|| "scan must be an object".to_string())?;
    let scan_max_depth =
        scan.get("max_depth")
            .and_then(Value::as_u64)
            .ok_or_else(|| "scan.max_depth must be an integer".to_string())? as usize;
    if scan_max_depth != expected_max_depth {
        return Err("scan.max_depth does not match the request".to_string());
    }
    let scan_limit = scan
        .get("limit")
        .and_then(Value::as_u64)
        .ok_or_else(|| "scan.limit must be an integer".to_string())? as usize;
    if scan_limit != expected_limit {
        return Err("scan.limit does not match the request".to_string());
    }
    let returned_entry_count = scan
        .get("returned_entry_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "scan.returned_entry_count must be an integer".to_string())?
        as usize;
    if returned_entry_count > scan_limit {
        return Err("scan.returned_entry_count exceeds scan.limit".to_string());
    }
    let truncated = scan
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| "scan.truncated must be a boolean".to_string())?;
    let truncation_reason = scan.get("truncation_reason");
    match truncation_reason {
        None | Some(Value::Null) => {
            if truncated {
                return Err("truncated is true without a truncation_reason".to_string());
            }
        }
        Some(Value::String(reason)) => {
            if !TRUNCATION_REASONS.contains(&reason.as_str()) {
                return Err("unknown scan.truncation_reason".to_string());
            }
            if !truncated {
                return Err("truncation_reason set without truncated".to_string());
            }
        }
        Some(_) => return Err("scan.truncation_reason must be null or string".to_string()),
    }
    if truncation_reason
        .and_then(Value::as_str)
        .is_some_and(|reason| matches!(reason, "limit" | "limit_and_max_depth"))
        && returned_entry_count != scan_limit
    {
        return Err("limit truncation requires returned_entry_count == scan.limit".to_string());
    }

    let project_types = object
        .get("project_types")
        .and_then(Value::as_array)
        .ok_or_else(|| "project_types must be an array".to_string())?;
    let mut normalized_project_types = Vec::with_capacity(project_types.len());
    for entry in project_types {
        let entry_obj = entry
            .as_object()
            .ok_or_else(|| "project type must be an object".to_string())?;
        let kind = entry_obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "project type kind must be a string".to_string())?;
        if !PROJECT_TYPE_KINDS.contains(&kind) {
            return Err("unknown project type kind".to_string());
        }
        let evidence = entry_obj
            .get("evidence")
            .and_then(Value::as_array)
            .ok_or_else(|| "project type evidence must be an array".to_string())?;
        let mut normalized_evidence = Vec::with_capacity(evidence.len());
        for evidence_path in evidence {
            let evidence_path = evidence_path
                .as_str()
                .ok_or_else(|| "evidence must be a string".to_string())?;
            let normalized = validate_path_field(evidence_path)?;
            if !path_within_scope(&normalized, &normalized_path) {
                return Err("evidence path is outside the request scope".to_string());
            }
            normalized_evidence.push(normalized);
        }
        if has_duplicates(&normalized_evidence) {
            return Err("evidence contains duplicate paths".to_string());
        }
        if !is_sorted(&normalized_evidence) {
            return Err("evidence must be sorted".to_string());
        }
        normalized_project_types.push(json!({
            "kind": kind,
            "evidence": normalized_evidence,
        }));
    }
    let project_type_kinds: Vec<&str> = normalized_project_types
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    if !is_subsequence_in_order(&project_type_kinds, PROJECT_TYPE_KINDS) {
        return Err("project types must follow the stable kind order".to_string());
    }
    if has_duplicates(&project_type_kinds) {
        return Err("project types contain duplicate kinds".to_string());
    }

    let manifests = object
        .get("manifests")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifests must be an array".to_string())?;
    let normalized_manifests =
        validate_path_kind_list(manifests, MANIFEST_KINDS, "manifest", &normalized_path)?;

    let key_files = object
        .get("key_files")
        .and_then(Value::as_array)
        .ok_or_else(|| "key_files must be an array".to_string())?;
    let mut normalized_key_files = Vec::with_capacity(key_files.len());
    for entry in key_files {
        let entry_obj = entry
            .as_object()
            .ok_or_else(|| "key file must be an object".to_string())?;
        let path = entry_obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "key file path must be a string".to_string())?;
        let path = validate_path_field(path)?;
        if !path_within_scope(&path, &normalized_path) {
            return Err("key file path is outside the request scope".to_string());
        }
        let kind = entry_obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "key file kind must be a string".to_string())?;
        if !KEY_FILE_KINDS.contains(&kind) {
            return Err("unknown key file kind".to_string());
        }
        let reason = entry_obj
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| "key file reason must be a string".to_string())?;
        normalized_key_files.push(json!({
            "path": path,
            "kind": kind,
            "reason": reason,
        }));
    }
    if has_duplicate_paths(&normalized_key_files) {
        return Err("key files contain duplicate paths".to_string());
    }
    if !key_files_in_stable_order(&normalized_key_files) {
        return Err("key files must follow the stable priority and path order".to_string());
    }

    let roots = object
        .get("roots")
        .and_then(Value::as_object)
        .ok_or_else(|| "roots must be an object".to_string())?;
    let mut normalized_roots = serde_json::Map::new();
    for class in ROOT_CLASSES {
        let class_paths = roots
            .get(*class)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("roots.{class} must be an array"))?;
        let mut normalized_class = Vec::with_capacity(class_paths.len());
        for path in class_paths {
            let path = path
                .as_str()
                .ok_or_else(|| format!("roots.{class} entry must be a string"))?;
            let path = validate_path_field(path)?;
            if !path_within_scope(&path, &normalized_path) {
                return Err(format!("roots.{class} path is outside the request scope"));
            }
            normalized_class.push(Value::String(path));
        }
        let class_paths: Vec<&str> = normalized_class
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        if has_duplicates(&class_paths) {
            return Err(format!("roots.{class} contains duplicate paths"));
        }
        if !is_sorted(&class_paths) {
            return Err(format!("roots.{class} must be sorted"));
        }
        normalized_roots.insert((*class).to_string(), Value::Array(normalized_class));
    }
    let classification_basis = roots
        .get("classification_basis")
        .and_then(Value::as_str)
        .ok_or_else(|| "classification_basis must be a string".to_string())?;
    if classification_basis != "conventional_directory_name" {
        return Err("classification_basis must be conventional_directory_name".to_string());
    }
    normalized_roots.insert(
        "classification_basis".to_string(),
        Value::String(classification_basis.to_string()),
    );

    let top_level = object
        .get("top_level")
        .and_then(Value::as_array)
        .ok_or_else(|| "top_level must be an array".to_string())?;
    let mut normalized_top_level = Vec::with_capacity(top_level.len());
    for entry in top_level {
        let entry_obj = entry
            .as_object()
            .ok_or_else(|| "top level entry must be an object".to_string())?;
        let path = entry_obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "top level path must be a string".to_string())?;
        let path = validate_path_field(path)?;
        if !path_within_scope(&path, &normalized_path) {
            return Err("top level path is outside the request scope".to_string());
        }
        let kind = entry_obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "top level kind must be a string".to_string())?;
        if !TOP_LEVEL_KINDS.contains(&kind) {
            return Err("unknown top level kind".to_string());
        }
        normalized_top_level.push(json!({"path": path, "kind": kind}));
    }
    if has_duplicate_paths(&normalized_top_level) {
        return Err("top level contains duplicate paths".to_string());
    }
    if !is_path_sorted(&normalized_top_level) {
        return Err("top level must be sorted by path".to_string());
    }
    if normalized_top_level.len() != returned_entry_count {
        // top_level only includes depth-1 entries, while returned_entry_count
        // covers every safe scanned entry across all depths. The deeper
        // entries therefore cannot be cross-checked against this counter, but
        // the count must never exceed scan.limit, which is enforced above.
        if normalized_top_level.len() > returned_entry_count {
            return Err("top_level exceeds returned_entry_count".to_string());
        }
    }

    let suggested = object
        .get("suggested_next_reads")
        .and_then(Value::as_array)
        .ok_or_else(|| "suggested_next_reads must be an array".to_string())?;
    let mut normalized_suggested = Vec::with_capacity(suggested.len());
    for entry in suggested {
        let entry_obj = entry
            .as_object()
            .ok_or_else(|| "suggested read must be an object".to_string())?;
        let path = entry_obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "suggested read path must be a string".to_string())?;
        let path = validate_path_field(path)?;
        if !path_within_scope(&path, &normalized_path) {
            return Err("suggested read path is outside the request scope".to_string());
        }
        let reason = entry_obj
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| "suggested read reason must be a string".to_string())?;
        normalized_suggested.push(json!({"path": path, "reason": reason}));
    }
    if has_duplicate_paths(&normalized_suggested) {
        return Err("suggested reads contain duplicate paths".to_string());
    }
    let mut expected_suggested = normalized_key_files
        .iter()
        .filter(|entry| entry.get("kind").and_then(Value::as_str) != Some("lockfile"))
        .map(|entry| {
            json!({
                "path": entry.get("path").cloned().unwrap_or(Value::Null),
                "reason": entry.get("reason").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    if expected_suggested.is_empty() {
        expected_suggested = normalized_key_files
            .iter()
            .map(|entry| {
                json!({
                    "path": entry.get("path").cloned().unwrap_or(Value::Null),
                    "reason": entry.get("reason").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
    }
    expected_suggested.truncate(12);
    if normalized_suggested != expected_suggested {
        return Err(
            "suggested reads must match the stable key-file recommendation order".to_string(),
        );
    }

    let warnings = object
        .get("warnings")
        .and_then(Value::as_array)
        .ok_or_else(|| "warnings must be an array".to_string())?;
    let mut normalized_warnings = Vec::with_capacity(warnings.len());
    for warning in warnings {
        let warning = warning
            .as_str()
            .ok_or_else(|| "warning must be a string".to_string())?;
        if !WARNING_KINDS.contains(&warning) {
            return Err("unknown warning".to_string());
        }
        normalized_warnings.push(warning.to_string());
    }
    if has_duplicates(&normalized_warnings) {
        return Err("warnings contain duplicates".to_string());
    }
    let warning_refs: Vec<&str> = normalized_warnings.iter().map(String::as_str).collect();
    if !is_subsequence_in_order(&warning_refs, WARNING_KINDS) {
        return Err("warnings must follow the stable order".to_string());
    }

    Ok(json!({
        "schema_version": 1,
        "path": normalized_path,
        "deterministic": true,
        "project_types": normalized_project_types,
        "manifests": normalized_manifests,
        "key_files": normalized_key_files,
        "roots": Value::Object(normalized_roots),
        "top_level": normalized_top_level,
        "suggested_next_reads": normalized_suggested,
        "scan": {
            "max_depth": scan_max_depth,
            "limit": scan_limit,
            "returned_entry_count": returned_entry_count,
            "truncated": truncated,
            "truncation_reason": match truncation_reason {
                None | Some(Value::Null) => Value::Null,
                Some(Value::String(reason)) => Value::String(reason.clone()),
                _ => unreachable!(),
            },
        },
        "warnings": normalized_warnings,
    }))
}

fn validate_path_kind_list(
    items: &[Value],
    allowed_kinds: &[&str],
    label: &str,
    scope: &str,
) -> Result<Vec<Value>, String> {
    let mut normalized = Vec::with_capacity(items.len());
    for entry in items {
        let entry_obj = entry
            .as_object()
            .ok_or_else(|| format!("{label} entry must be an object"))?;
        let path = entry_obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{label} path must be a string"))?;
        let path = validate_path_field(path)?;
        if !path_within_scope(&path, scope) {
            return Err(format!("{label} path is outside the request scope"));
        }
        let kind = entry_obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{label} kind must be a string"))?;
        if !allowed_kinds.contains(&kind) {
            return Err(format!("unknown {label} kind"));
        }
        normalized.push(json!({"path": path, "kind": kind}));
    }
    if has_duplicate_paths(&normalized) {
        return Err(format!("{label} entries contain duplicate paths"));
    }
    if !is_path_sorted(&normalized) {
        return Err(format!("{label} entries must be sorted by path"));
    }
    Ok(normalized)
}

fn key_file_priority(kind: &str) -> Option<u8> {
    match kind {
        "agent_instructions" => Some(0),
        "readme" => Some(1),
        "manifest" => Some(2),
        "documentation" => Some(3),
        "lockfile" => Some(4),
        "container" | "ci" => Some(5),
        _ => None,
    }
}

fn key_files_in_stable_order(items: &[Value]) -> bool {
    items.windows(2).all(|window| {
        let left_kind = window[0]
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let right_kind = window[1]
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let left_path = window[0]
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let right_path = window[1]
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match (key_file_priority(left_kind), key_file_priority(right_kind)) {
            (Some(left_priority), Some(right_priority)) => {
                left_priority < right_priority
                    || (left_priority == right_priority && left_path <= right_path)
            }
            _ => false,
        }
    })
}

fn has_duplicates<T: PartialEq>(items: &[T]) -> bool {
    for (index, item) in items.iter().enumerate() {
        if items[index + 1..].contains(item) {
            return true;
        }
    }
    false
}

fn is_sorted<T: Ord>(items: &[T]) -> bool {
    items.windows(2).all(|window| window[0] <= window[1])
}

fn has_duplicate_paths(items: &[Value]) -> bool {
    let paths: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .collect();
    has_duplicates(&paths)
}

fn is_path_sorted(items: &[Value]) -> bool {
    let paths: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .collect();
    is_sorted(&paths)
}

/// True when `items` form a subsequence of `order`: each `items` element is
/// found in `order` advancing forward, never looking back. Duplicates in
/// `items` are the caller's responsibility (checked separately).
fn is_subsequence_in_order(items: &[&str], order: &[&str]) -> bool {
    let mut order_iter = order.iter();
    for item in items {
        if !order_iter.any(|candidate| candidate == item) {
            return false;
        }
    }
    true
}

#[cfg(test)]
fn touch(root: &Path, path: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, b"fixture contents must never be read").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_overview_rust_fixture_is_bounded_safe_and_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        for path in [
            "AGENTS.md",
            "README.md",
            "Cargo.toml",
            "Cargo.lock",
            "src/lib.rs",
            "tests/basic.rs",
            "docs/README.md",
            ".github/workflows/ci.yml",
            "target/debug/output",
            ".env",
        ] {
            touch(temp.path(), path);
        }

        let first = build_project_overview(temp.path(), "", None, None).unwrap();
        let second = build_project_overview(temp.path(), "", None, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(first["deterministic"], true);
        assert!(first["project_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|kind| kind["kind"] == "rust"));
        let serialized = first.to_string();
        for expected in ["AGENTS.md", "README.md", "Cargo.toml"] {
            assert!(
                serialized.contains(expected),
                "missing {expected}: {serialized}"
            );
        }
        assert!(!serialized.contains("target"));
        assert!(!serialized.contains(".env"));
        assert!(!serialized.contains("fixture contents"));
        assert!(!serialized.contains(&temp.path().display().to_string()));
        assert_eq!(first["roots"]["source"], json!(["src"]));
        assert_eq!(first["roots"]["tests"], json!(["tests"]));
        assert_eq!(first["roots"]["docs"], json!(["docs"]));
        assert_eq!(first["roots"]["ci"], json!([".github"]));
        // The local builder output must satisfy the shared contract entry
        // used to validate Runner responses, so the two paths cannot drift.
        let normalized = validate_project_overview(&first, "", 2, 200).unwrap();
        assert_eq!(first, normalized);
    }

    #[test]
    fn project_overview_detects_stably_ordered_monorepo_types() {
        let temp = tempfile::tempdir().unwrap();
        for path in [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "crates/example/Cargo.toml",
            "packages/app/package.json",
        ] {
            touch(temp.path(), path);
        }
        let output = build_project_overview(temp.path(), "", Some(4), None).unwrap();
        let kinds = output["project_types"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["kind"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["rust", "node", "python"]);
        assert_eq!(output["roots"]["source"], json!(["crates", "packages"]));
        let normalized = validate_project_overview(&output, "", 4, 200).unwrap();
        assert_eq!(output, normalized);
    }

    #[test]
    fn project_overview_scopes_paths_and_clamps_bounds() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..25 {
            touch(
                temp.path(),
                &format!("crates/example/src/file_{index:02}.rs"),
            );
        }
        touch(temp.path(), "outside/Cargo.toml");
        let output =
            build_project_overview(temp.path(), "crates/example", Some(0), Some(1)).unwrap();
        assert_eq!(output["path"], "crates/example");
        assert_eq!(output["scan"]["max_depth"], 1);
        assert_eq!(output["scan"]["limit"], 20);
        assert_eq!(output["scan"]["truncated"], true);
        assert!(
            output["scan"]["truncation_reason"] == "max_depth"
                || output["scan"]["truncation_reason"] == "limit_and_max_depth"
        );
        let serialized = output.to_string();
        assert!(!serialized.contains("outside/Cargo.toml"));
        for entry in output["top_level"].as_array().unwrap() {
            assert!(entry["path"]
                .as_str()
                .unwrap()
                .starts_with("crates/example/"));
        }
        let normalized = validate_project_overview(&output, "crates/example", 1, 20).unwrap();
        assert_eq!(output, normalized);
    }

    #[test]
    fn project_overview_limit_truncation_is_normal_bounded_output() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..25 {
            touch(temp.path(), &format!("file_{index:02}.txt"));
        }

        let output = build_project_overview(temp.path(), "", Some(99), Some(1)).unwrap();
        assert_eq!(output["scan"]["max_depth"], 4);
        assert_eq!(output["scan"]["limit"], 20);
        assert_eq!(output["scan"]["returned_entry_count"], 20);
        assert_eq!(output["scan"]["truncated"], true);
        assert_eq!(output["scan"]["truncation_reason"], "limit");
        assert_eq!(output["top_level"].as_array().unwrap().len(), 20);
        let normalized = validate_project_overview(&output, "", 4, 20).unwrap();
        assert_eq!(output, normalized);
    }

    #[test]
    fn project_overview_rejects_escape_and_does_not_follow_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        assert!(build_project_overview(temp.path(), "../outside", None, None).is_err());
        assert!(build_project_overview(temp.path(), "/tmp", None, None).is_err());
        for protected in [".git", "target", "node_modules", ".env", "secrets"] {
            assert!(
                build_project_overview(temp.path(), protected, None, None).is_err(),
                "protected scope {protected} must be rejected before scanning"
            );
        }

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            touch(outside.path(), "Cargo.toml");
            std::os::unix::fs::symlink(outside.path(), temp.path().join("escape")).unwrap();
            let output = build_project_overview(temp.path(), "", None, None).unwrap();
            assert!(!output.to_string().contains("escape"));
            assert_eq!(output["warnings"], json!(["symlinks_skipped"]));
        }
    }
}

#[cfg(test)]
mod git_index_tests {
    use super::*;

    #[test]
    fn detection_trusts_the_git_index_over_untracked_tool_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.py"), "print()\n").unwrap();
        git(&["add", "pyproject.toml", "src/main.py"]);
        // Untracked tool state that used to pollute language detection.
        std::fs::create_dir_all(root.join(".opencode")).unwrap();
        std::fs::write(root.join(".opencode/package.json"), "{}").unwrap();

        let output = build_project_overview(root, "", None, None).unwrap();
        let kinds: Vec<&str> = output["project_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"python"), "kinds: {kinds:?}");
        assert!(
            !kinds.contains(&"node"),
            "untracked .opencode/package.json must not classify the project as node: {kinds:?}"
        );
        let manifests: Vec<&str> = output["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["path"].as_str())
            .collect();
        assert!(manifests.contains(&"pyproject.toml"));
        assert!(!manifests.iter().any(|path| path.contains(".opencode")));
    }

    #[test]
    fn non_git_directories_keep_the_filesystem_fallback() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("package.json"), "{}").unwrap();
        let output = build_project_overview(temp.path(), "", None, None).unwrap();
        let kinds: Vec<&str> = output["project_types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["kind"].as_str())
            .collect();
        assert!(
            kinds.contains(&"node"),
            "fallback scan must still detect: {kinds:?}"
        );
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn valid_root_payload() -> Value {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for path in [
            "AGENTS.md",
            "README.md",
            "Cargo.toml",
            "src/lib.rs",
            "tests/basic.rs",
            "docs/index.md",
            ".github/workflows/ci.yml",
        ] {
            touch(root, path);
        }
        // Initialize a git index so tracked-file detection covers every entry.
        for cmd in ["git init -q", "git add -A", "git commit -q -m seed"] {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(root)
                .status()
                .unwrap();
        }
        build_project_overview(root, "", Some(2), Some(120)).unwrap()
    }

    #[test]
    fn validate_accepts_a_well_formed_local_overview_and_drops_extras() {
        let mut payload = valid_root_payload();
        // A Runner echo of a non-canonical request path would be rejected, but
        // the local builder already emits the canonical form.
        let result = validate_project_overview(&payload, "", 2, 120);
        assert!(
            result.is_ok(),
            "well-formed payload rejected: {result:?}\n{}",
            serde_json::to_string_pretty(&payload).unwrap()
        );
        if let Some(object) = payload.as_object_mut() {
            object.insert("runner_secret".to_string(), json!("leak"));
            object.insert("canonical_root".to_string(), json!("/absolute/leak"));
        }
        let normalized = validate_project_overview(&payload, "", 2, 120).unwrap();
        let serialized = normalized.to_string();
        assert!(!serialized.contains("runner_secret"));
        assert!(!serialized.contains("canonical_root"));
        assert!(!serialized.contains("/absolute/leak"));
    }

    #[test]
    fn validate_rejects_request_boundary_mismatch() {
        let payload = valid_root_payload();
        assert!(validate_project_overview(&payload, "", 4, 120).is_err());
        assert!(validate_project_overview(&payload, "", 2, 500).is_err());
        assert!(validate_project_overview(&payload, "subdir", 2, 120).is_err());
    }

    #[test]
    fn validate_requires_the_exact_canonical_request_path() {
        let mut payload = valid_root_payload();
        // The formal root spelling is the empty string; equivalent but
        // non-canonical spellings must not be accepted from a Runner.
        payload["path"] = json!(".");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
        // A payload path that disagrees with the request scope is rejected
        // even when it is itself a valid project-relative path.
        payload["path"] = json!("subdir");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
        // A protected scope is rejected by normalization.
        payload["path"] = json!(".git");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
    }

    fn multi_type_payload() -> Value {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for path in ["Cargo.toml", "package.json", "pyproject.toml", "src/lib.rs"] {
            touch(root, path);
        }
        for cmd in ["git init -q", "git add -A", "git commit -q -m seed"] {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(root)
                .status()
                .unwrap();
        }
        build_project_overview(root, "", Some(2), Some(120)).unwrap()
    }

    #[test]
    fn validate_rejects_absolute_and_traversal_paths() {
        let mut payload = valid_root_payload();
        payload["top_level"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": "/etc/passwd", "kind": "file"}));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["manifests"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": "../outside/Cargo.toml", "kind": "rust_manifest"}));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
    }

    #[test]
    fn validate_rejects_unknown_enums_and_bad_types() {
        let mut payload = valid_root_payload();
        payload["project_types"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind": "cobol", "evidence": []}));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["key_files"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": "README.md", "kind": "mystery", "reason": "x"}));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["warnings"]
            .as_array_mut()
            .unwrap()
            .push(json!("nuclear_launch_detected"));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["scan"]["returned_entry_count"] = json!("plenty");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["warnings"] = json!({"note": "object not array"});
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_and_unstable_ordering() {
        let mut payload = valid_root_payload();
        let top = payload["top_level"].as_array_mut().unwrap();
        let first = top[0].clone();
        top.push(first);
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        let top = payload["top_level"].as_array_mut().unwrap();
        if top.len() >= 2 {
            top.swap(0, 1);
            assert!(validate_project_overview(&payload, "", 2, 120).is_err());
        }

        let mut payload = valid_root_payload();
        let key_files = payload["key_files"].as_array_mut().unwrap();
        if key_files.len() >= 2 {
            key_files.swap(0, 1);
            assert!(validate_project_overview(&payload, "", 2, 120).is_err());
        }

        let mut payload = valid_root_payload();
        let suggested = payload["suggested_next_reads"].as_array_mut().unwrap();
        if suggested.len() >= 2 {
            suggested.swap(0, 1);
            assert!(validate_project_overview(&payload, "", 2, 120).is_err());
        }
    }

    #[test]
    fn validate_rejects_truncated_without_reason_and_vice_versa() {
        let mut payload = valid_root_payload();
        payload["scan"]["truncated"] = json!(true);
        payload["scan"]["truncation_reason"] = Value::Null;
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["scan"]["truncated"] = json!(false);
        payload["scan"]["truncation_reason"] = json!("limit");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
    }

    #[test]
    fn validate_rejects_unknown_or_inconsistent_truncation_reason() {
        let mut payload = valid_root_payload();
        payload["scan"]["truncated"] = json!(true);
        payload["scan"]["truncation_reason"] = json!("heap_overflow");
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());

        let mut payload = valid_root_payload();
        payload["scan"]["truncated"] = json!(true);
        payload["scan"]["truncation_reason"] = json!("limit");
        assert_ne!(payload["scan"]["returned_entry_count"], json!(120));
        assert!(validate_project_overview(&payload, "", 2, 120).is_err());
    }

    #[test]
    fn validate_rejects_out_of_scope_paths() {
        let temp = tempfile::tempdir().unwrap();
        for path in ["Cargo.toml", "sub/README.md", "other/lib.rs"] {
            touch(temp.path(), path);
        }
        for cmd in ["git init -q", "git add -A", "git commit -q -m seed"] {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(temp.path())
                .status()
                .unwrap();
        }
        let payload = build_project_overview(temp.path(), "sub", Some(2), Some(120)).unwrap();
        let mut malformed = payload.clone();
        malformed["top_level"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": "other/lib.rs", "kind": "file"}));
        assert!(validate_project_overview(&malformed, "sub", 2, 120).is_err());
        // The well-formed sub-scope overview validates.
        assert!(validate_project_overview(&payload, "sub", 2, 120).is_ok());
    }

    #[test]
    fn validate_rejects_misordered_project_type_kinds() {
        let mut payload = multi_type_payload();
        // The builder emits kinds in PROJECT_TYPE_KINDS order (rust, node, ...).
        // Reversing the first two breaks the stable order invariant.
        if let Some(types) = payload["project_types"].as_array_mut() {
            if types.len() >= 2 {
                types.swap(0, 1);
            }
        }
        let result = validate_project_overview(&payload, "", 2, 120);
        assert!(result.is_err(), "misordered kinds accepted: {result:?}");
    }
}
