use super::get_projects;
use super::security::is_sensitive_path;
use super::shell::{run_command, shell_escape};
use super::ssh::{build_ssh_target, run_ssh, run_ssh_patch};
use super::truncate_string;
use super::types::{PatchRequest, PatchResponse};
use crate::projects::{ProjectConfig, SshConfig};
use salvo::prelude::*;
use std::collections::BTreeSet;
use std::path::Path;

const CODEX_APPLY_PATCH_BACKEND: &str = "codex";
const BUILTIN_PATCH_BACKEND: &str = "builtin";
const DEFAULT_CODEX_APPLY_PATCH_BIN: &str = "/root/git/codex/codex-rs/target/debug/apply_patch";

fn patch_response(
    success: bool,
    backend: &str,
    changed_files: Option<Vec<String>>,
    stdout: Option<String>,
    stderr: Option<String>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    diff: Option<String>,
    error: Option<String>,
) -> PatchResponse {
    PatchResponse {
        success,
        backend: Some(backend.to_string()),
        changed_files,
        stdout,
        stderr,
        exit_code,
        duration_ms,
        diff,
        error,
    }
}

pub(super) fn parse_changed_files_from_patch(patch: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            // Format: diff --git a/path b/path
            if let Some(b_pos) = line.rfind(" b/") {
                let file = &line[b_pos + 3..];
                if !files.iter().any(|f: &String| f == file) {
                    files.push(file.to_string());
                }
            }
            continue;
        }
        for prefix in ["+++ b/", "--- a/"] {
            if let Some(file) = line.strip_prefix(prefix) {
                if file != "/dev/null" && !files.iter().any(|f: &String| f == file) {
                    files.push(file.to_string());
                }
            }
        }
    }
    files
}

pub(super) fn parse_changed_files_from_codex_patch(patch: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    for line in patch.lines() {
        for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                files.insert(path.trim().to_string());
            }
        }
    }
    files.into_iter().collect()
}

fn validate_patch_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("patch path cannot be empty".to_string());
    }
    if path.starts_with('/') {
        return Err(format!("Absolute paths are not allowed: {}", path));
    }
    if path.contains("..") {
        return Err(format!("Path traversal (..) is not allowed: {}", path));
    }
    if is_sensitive_path(path) {
        return Err(format!("Cannot modify sensitive path: {}", path));
    }
    Ok(())
}

fn validate_patch_paths(changed: &[String]) -> Result<(), String> {
    for file in changed {
        validate_patch_path(file)?;
    }
    Ok(())
}

fn git_diff_local(root: &Path) -> Option<String> {
    let (code, stdout, stderr, _) =
        run_command("git diff --no-ext-diff -- && git status --short", root, 60);
    if code == 0 {
        let (diff, _) = truncate_string(stdout, super::MAX_OUTPUT_LEN);
        Some(diff)
    } else {
        let (err, _) = truncate_string(stderr, super::MAX_OUTPUT_LEN);
        Some(format!("git diff failed:\n{}", err))
    }
}

fn git_diff_ssh(proj: &ProjectConfig, ssh_config: Option<&SshConfig>) -> Option<String> {
    let ssh_target = build_ssh_target(proj).ok()?;
    let remote_cmd = format!(
        "cd {} && git diff --no-ext-diff -- && git status --short",
        shell_escape(&proj.path)
    );
    let (code, stdout, stderr, _) = run_ssh(&ssh_target, &remote_cmd, 60, ssh_config);
    if code == 0 {
        let (diff, _) = truncate_string(stdout, super::MAX_OUTPUT_LEN);
        Some(diff)
    } else {
        let (err, _) = truncate_string(stderr, super::MAX_OUTPUT_LEN);
        Some(format!("git diff failed:\n{}", err))
    }
}

fn codex_apply_patch_bin() -> String {
    std::env::var("CODEX_APPLY_PATCH_BIN")
        .unwrap_or_else(|_| DEFAULT_CODEX_APPLY_PATCH_BIN.to_string())
}

pub(super) fn ssh_apply_patch(
    proj: &ProjectConfig,
    _project_name: &str,
    patch: &str,
    changed: Vec<String>,
    ssh_config: Option<&SshConfig>,
) -> PatchResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return patch_response(
                false,
                BUILTIN_PATCH_BACKEND,
                Some(changed),
                None,
                None,
                None,
                None,
                None,
                Some(e),
            )
        }
    };
    let remote_cmd = format!(
        "cd {} && git apply --check __PATCH__ && git apply __PATCH__",
        shell_escape(&proj.path)
    );
    let (code, stdout, stderr, duration_ms) =
        run_ssh_patch(&ssh_target, &proj.path, patch, &remote_cmd, ssh_config);
    let diff = git_diff_ssh(proj, ssh_config);
    if code == 0 {
        patch_response(
            true,
            BUILTIN_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            None,
        )
    } else {
        patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            Some("git apply failed".to_string()),
        )
    }
}

fn local_apply_builtin_patch(
    proj: &ProjectConfig,
    patch: &str,
    changed: Vec<String>,
) -> PatchResponse {
    let root = proj.root();
    if !root.exists() {
        return patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Project root does not exist".to_string()),
        );
    }

    let patch_file = root.join(format!(".codex-patch-{}.diff", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&patch_file, patch) {
        return patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            None,
            None,
            None,
            None,
            None,
            git_diff_local(&root),
            Some(format!("Failed to write temp patch file: {}", e)),
        );
    }

    let check_out = run_command(
        &format!(
            "git apply --check {}",
            shell_escape(&patch_file.display().to_string())
        ),
        &root,
        60,
    );
    if check_out.0 != 0 {
        let _ = std::fs::remove_file(&patch_file);
        return patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            Some(changed),
            Some(check_out.1),
            Some(check_out.2),
            Some(check_out.0),
            Some(check_out.3),
            git_diff_local(&root),
            Some("git apply --check failed".to_string()),
        );
    }

    let apply_out = run_command(
        &format!(
            "git apply {}",
            shell_escape(&patch_file.display().to_string())
        ),
        &root,
        60,
    );
    let _ = std::fs::remove_file(&patch_file);
    let diff = git_diff_local(&root);

    if apply_out.0 == 0 {
        patch_response(
            true,
            BUILTIN_PATCH_BACKEND,
            Some(changed),
            Some(apply_out.1),
            Some(apply_out.2),
            Some(apply_out.0),
            Some(apply_out.3),
            diff,
            None,
        )
    } else {
        patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            Some(changed),
            Some(apply_out.1),
            Some(apply_out.2),
            Some(apply_out.0),
            Some(apply_out.3),
            diff,
            Some("git apply failed".to_string()),
        )
    }
}

fn local_apply_codex_patch(
    proj: &ProjectConfig,
    patch: &str,
    changed: Vec<String>,
) -> PatchResponse {
    let root = proj.root();
    if !root.exists() {
        return patch_response(
            false,
            CODEX_APPLY_PATCH_BACKEND,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Project root does not exist".to_string()),
        );
    }
    let patch_file = std::env::temp_dir().join(format!(
        "private-drop-codex-patch-{}.patch",
        uuid::Uuid::new_v4()
    ));
    if let Err(e) = std::fs::write(&patch_file, patch) {
        return patch_response(
            false,
            CODEX_APPLY_PATCH_BACKEND,
            Some(changed),
            None,
            None,
            None,
            None,
            git_diff_local(&root),
            Some(format!("Failed to write temp patch file: {}", e)),
        );
    }
    let cmd = format!(
        "{} < {}",
        shell_escape(&codex_apply_patch_bin()),
        shell_escape(&patch_file.display().to_string())
    );
    let (code, stdout, stderr, duration_ms) = run_command(&cmd, &root, 60);
    let _ = std::fs::remove_file(&patch_file);
    let diff = git_diff_local(&root);
    if code == 0 {
        patch_response(
            true,
            CODEX_APPLY_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            None,
        )
    } else {
        patch_response(
            false,
            CODEX_APPLY_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            Some("codex apply_patch failed; worktree may contain partial changes".to_string()),
        )
    }
}

fn ssh_apply_codex_patch(
    proj: &ProjectConfig,
    patch: &str,
    changed: Vec<String>,
    ssh_config: Option<&SshConfig>,
) -> PatchResponse {
    let ssh_target = match build_ssh_target(proj) {
        Ok(t) => t,
        Err(e) => {
            return patch_response(
                false,
                CODEX_APPLY_PATCH_BACKEND,
                Some(changed),
                None,
                None,
                None,
                None,
                None,
                Some(e),
            )
        }
    };
    let remote_cmd = format!(
        "cd {} && patch_file=__PATCH__; {} < \"$patch_file\"; code=$?; rm -f \"$patch_file\"; exit $code",
        shell_escape(&proj.path),
        shell_escape(&codex_apply_patch_bin())
    );
    let (code, stdout, stderr, duration_ms) =
        run_ssh_patch(&ssh_target, &proj.path, patch, &remote_cmd, ssh_config);
    let diff = git_diff_ssh(proj, ssh_config);
    if code == 0 {
        patch_response(
            true,
            CODEX_APPLY_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            None,
        )
    } else {
        patch_response(
            false,
            CODEX_APPLY_PATCH_BACKEND,
            Some(changed),
            Some(stdout),
            Some(stderr),
            Some(code),
            Some(duration_ms),
            diff,
            Some("codex apply_patch failed; worktree may contain partial changes".to_string()),
        )
    }
}

#[handler]
pub async fn codex_apply_patch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(patch_response(
            false,
            BUILTIN_PATCH_BACKEND,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Projects not configured".to_string()),
        )));
        return;
    };
    let body: PatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(patch_response(
                false,
                BUILTIN_PATCH_BACKEND,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(format!("Invalid JSON: {}", e)),
            )));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(patch_response(
                false,
                body.backend.as_deref().unwrap_or(BUILTIN_PATCH_BACKEND),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(e),
            )));
            return;
        }
    };
    let backend = body
        .backend
        .as_deref()
        .or(proj.default_apply_patch_backend.as_deref())
        .unwrap_or(BUILTIN_PATCH_BACKEND);
    if backend != BUILTIN_PATCH_BACKEND && backend != CODEX_APPLY_PATCH_BACKEND {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(patch_response(
            false,
            backend,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("backend must be 'builtin' or 'codex'".to_string()),
        )));
        return;
    }
    if !proj.allow_patch() {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(patch_response(
            false,
            backend,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Patch is not allowed for this project".to_string()),
        )));
        return;
    }
    if body.patch.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(patch_response(
            false,
            backend,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Patch cannot be empty".to_string()),
        )));
        return;
    }

    let changed = if backend == CODEX_APPLY_PATCH_BACKEND {
        parse_changed_files_from_codex_patch(&body.patch)
    } else {
        parse_changed_files_from_patch(&body.patch)
    };
    if changed.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(patch_response(
            false,
            backend,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Patch does not declare any changed files".to_string()),
        )));
        return;
    }
    if let Err(e) = validate_patch_paths(&changed) {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(patch_response(
            false,
            backend,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(e),
        )));
        return;
    }

    if backend == CODEX_APPLY_PATCH_BACKEND {
        let resp = if proj.is_ssh() {
            ssh_apply_codex_patch(proj, &body.patch, changed, projects.ssh.as_ref())
        } else {
            local_apply_codex_patch(proj, &body.patch, changed)
        };
        res.render(Json(resp));
        return;
    }

    if proj.is_ssh() {
        let resp = ssh_apply_patch(
            proj,
            &body.project,
            &body.patch,
            changed,
            projects.ssh.as_ref(),
        );
        res.render(Json(resp));
        return;
    }

    res.render(Json(local_apply_builtin_patch(proj, &body.patch, changed)));
}
