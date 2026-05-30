use super::get_projects;
use super::security::is_sensitive_path;
use super::shell::{run_command, shell_escape};
use super::ssh::{build_ssh_target, run_ssh_patch};
use super::types::{PatchRequest, PatchResponse};
use crate::projects::{ProjectConfig, SshConfig};
use salvo::prelude::*;

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
        }
    }
    files
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
            return PatchResponse {
                success: false,
                changed_files: Some(changed),
                stdout: None,
                stderr: None,
                error: Some(e),
            }
        }
    };
    let remote_cmd = format!(
        "cd {} && git apply --check __PATCH__ && git apply __PATCH__",
        shell_escape(&proj.path)
    );
    let (code, stdout, stderr, _) =
        run_ssh_patch(&ssh_target, &proj.path, patch, &remote_cmd, ssh_config);
    if code == 0 {
        PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: None,
        }
    } else {
        PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(stdout),
            stderr: Some(stderr),
            error: Some("git apply failed".to_string()),
        }
    }
}

#[handler]
pub async fn codex_apply_patch(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Projects not configured".to_string()),
        }));
        return;
    };
    let body: PatchRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Invalid JSON: {}", e)),
            }));
            return;
        }
    };
    let proj = match projects.get_project(&body.project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(e),
            }));
            return;
        }
    };
    if !proj.allow_patch() {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch is not allowed for this project".to_string()),
        }));
        return;
    }
    if body.patch.is_empty() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Patch cannot be empty".to_string()),
        }));
        return;
    }

    // Validate changed file paths against sensitive paths
    let changed = parse_changed_files_from_patch(&body.patch);
    for file in &changed {
        if is_sensitive_path(file) {
            res.status_code(StatusCode::FORBIDDEN);
            res.render(Json(PatchResponse {
                success: false,
                changed_files: None,
                stdout: None,
                stderr: None,
                error: Some(format!("Cannot modify sensitive path: {}", file)),
            }));
            return;
        }
    }

    if proj.is_ssh() {
        // SSH executor: pipe patch via stdin
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

    // Local executor
    let root = proj.root();
    if !root.exists() {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some("Project root does not exist".to_string()),
        }));
        return;
    }

    // Write patch to temp file, run git apply
    let patch_file = root.join(format!(".codex-patch-{}.diff", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&patch_file, &body.patch) {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: None,
            stdout: None,
            stderr: None,
            error: Some(format!("Failed to write temp patch file: {}", e)),
        }));
        return;
    }

    // Dry run first
    let check_out = run_command(
        &format!("git apply --check '{}'", patch_file.display()),
        &root,
        60,
    );
    if check_out.0 != 0 {
        let _ = std::fs::remove_file(&patch_file);
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(check_out.1),
            stderr: Some(check_out.2),
            error: Some("git apply --check failed".to_string()),
        }));
        return;
    }

    // Apply for real
    let apply_out = run_command(&format!("git apply '{}'", patch_file.display()), &root, 60);
    let _ = std::fs::remove_file(&patch_file);

    if apply_out.0 == 0 {
        res.render(Json(PatchResponse {
            success: true,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: None,
        }));
    } else {
        res.render(Json(PatchResponse {
            success: false,
            changed_files: Some(changed),
            stdout: Some(apply_out.1),
            stderr: Some(apply_out.2),
            error: Some("git apply failed".to_string()),
        }));
    }
}
