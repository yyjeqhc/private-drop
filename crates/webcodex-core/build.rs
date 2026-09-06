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
    if let Some(head_ref) = current_head_ref(&repo_root) {
        let head_ref_path = git_metadata_path(&repo_root, &head_ref)
            .unwrap_or_else(|| repo_root.join(".git").join(&head_ref));
        println!("cargo:rerun-if-changed={}", head_ref_path.display());
    }
    if let Some(packed_refs) = git_metadata_path(&repo_root, "packed-refs") {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }

    let git_commit =
        env_value("WEBCODEX_GIT_COMMIT").unwrap_or_else(|| git_commit_from_git(&repo_root));
    let git_dirty =
        env_value("WEBCODEX_GIT_DIRTY").unwrap_or_else(|| git_dirty_from_git(&repo_root));
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
