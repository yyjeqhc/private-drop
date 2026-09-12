use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let repo_root = repository_root();
    println!("cargo:rerun-if-env-changed=WEBCODEX_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=WEBCODEX_GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=WEBCODEX_BUILT_AT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let head_path = git_metadata_path(&repo_root, "HEAD")
        .unwrap_or_else(|| repo_root.join(".git").join("HEAD"));
    println!("cargo:rerun-if-changed={}", head_path.display());
    // A symbolic branch may be stored only in packed-refs. Emitting a Cargo
    // dependency on the corresponding missing loose-ref path makes the build
    // script permanently dirty in managed worktrees. Watch an existing ancestor
    // while the loose ref is absent, so its creation refreshes build identity
    // even when reflogs are disabled. The next run watches the loose ref itself.
    if let Some(head_log) = git_metadata_path(&repo_root, "logs/HEAD").filter(|path| path.exists())
    {
        println!("cargo:rerun-if-changed={}", head_log.display());
    }
    if let Some(head_ref) = current_head_ref(&repo_root) {
        if let Some(head_ref_path) = git_metadata_path(&repo_root, &head_ref) {
            if let Some(existing_path) = head_ref_path.ancestors().find(|path| path.exists()) {
                println!("cargo:rerun-if-changed={}", existing_path.display());
            }
        }
    }
    if let Some(packed_refs) =
        git_metadata_path(&repo_root, "packed-refs").filter(|path| path.exists())
    {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }

    // Git metadata does not change for ordinary unstaged worktree edits. When
    // dirty state is derived locally, make those tracked files explicit Cargo
    // inputs so a same-HEAD rebuild cannot reuse stale build-script output.
    // CI/release callers that pin WEBCODEX_GIT_DIRTY intentionally skip these
    // worktree dependencies and keep their deterministic identity contract.
    let git_dirty_override = env_value("WEBCODEX_GIT_DIRTY");
    let git_dirty = git_dirty_override
        .clone()
        .unwrap_or_else(|| git_dirty_from_git(&repo_root));
    if git_dirty_override.is_none() {
        watch_git_dirty_inputs(&repo_root, &git_dirty);
    }

    let git_commit =
        env_value("WEBCODEX_GIT_COMMIT").unwrap_or_else(|| git_commit_from_git(&repo_root));
    // Release workflows pin WEBCODEX_BUILT_AT explicitly. For ordinary Git
    // worktrees, prefer stable inputs so the same commit does not invalidate
    // compiler caches merely because it was built at a different wall-clock
    // time. SOURCE_DATE_EPOCH remains an explicit reproducible-build override;
    // non-Git source trees retain the historical current-time fallback.
    let built_at = env_value("WEBCODEX_BUILT_AT")
        .or_else(|| env_value("SOURCE_DATE_EPOCH"))
        .or_else(|| git_commit_timestamp_from_git(&repo_root))
        .unwrap_or_else(current_unix_timestamp);

    println!("cargo:rustc-env=WEBCODEX_BUILD_GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=WEBCODEX_BUILD_GIT_DIRTY={git_dirty}");
    println!("cargo:rustc-env=WEBCODEX_BUILD_BUILT_AT={built_at}");
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn git_commit_from_git(repo_root: &Path) -> String {
    command_stdout(repo_root, ["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_commit_timestamp_from_git(repo_root: &Path) -> Option<String> {
    command_stdout(repo_root, ["show", "-s", "--format=%ct", "HEAD"])
}

fn current_head_ref(repo_root: &Path) -> Option<String> {
    command_stdout(repo_root, ["symbolic-ref", "--quiet", "HEAD"])
        .filter(|value| value.starts_with("refs/"))
        .filter(|value| !value.contains(".."))
}

fn git_metadata_path(repo_root: &Path, name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(command_stdout(
        repo_root,
        ["rev-parse", "--git-path", name],
    )?);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

fn watch_git_dirty_inputs(repo_root: &Path, git_dirty: &str) {
    if let Some(index_path) = git_metadata_path(repo_root, "index").filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", index_path.display());
    }

    let mut command = Command::new("git");
    command.current_dir(repo_root);
    if git_dirty == "true" {
        // Once already dirty, only the paths keeping us dirty need watching.
        // Newly dirtied files cannot change the boolean identity; if a watched
        // dirty path becomes clean, the next run re-evaluates and rotates this set.
        command.args(["diff-index", "--name-only", "-z", "HEAD", "--"]);
    } else {
        // A clean tree can become dirty through any tracked worktree path.
        command.args(["ls-files", "-z"]);
    }
    let Ok(output) = command.output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for path in output.stdout.split(|byte| *byte == 0) {
        if path.is_empty() || path.contains(&b'\n') || path.contains(&b'\r') {
            continue;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        println!("cargo:rerun-if-changed={}", repo_root.join(path).display());
    }
}

fn git_dirty_from_git(repo_root: &Path) -> String {
    match Command::new("git")
        .current_dir(repo_root)
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
    {
        Ok(status) if status.success() => "false".to_string(),
        Ok(status) if status.code() == Some(1) => "true".to_string(),
        _ => "unknown".to_string(),
    }
}

fn command_stdout<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let value = stdout.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn current_unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
