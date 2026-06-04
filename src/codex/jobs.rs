use super::command_workflow::require_active_goal;
use super::get_projects;
use super::shell::shell_escape;
use super::trusted::{
    check_background_escape, check_denylist, check_secret_read, validate_trusted_script,
};
use super::types::{job_response, JobOpRequest, JobOpResponse};
use crate::get_db;
use crate::projects::{ProjectConfig, SshConfig};
use salvo::prelude::*;

use super::run_project_cmd;
use super::security::is_sensitive_path;
use super::types::{JobInfo, JobMetadata};
use base64::Engine;
use std::path::{Component, Path, PathBuf};

const DEFAULT_JOB_MAX_RUNTIME_SECS: i64 = 3600;
const MAX_JOB_MAX_RUNTIME_SECS: i64 = 604800;

pub(super) fn validate_job_id(job_id: &str) -> Result<(), String> {
    if job_id.is_empty()
        || job_id.len() > 80
        || !job_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid job_id".to_string());
    }
    Ok(())
}

pub(super) fn validate_client_request_id(client_request_id: &str) -> Result<(), String> {
    if client_request_id.is_empty()
        || client_request_id.len() > 128
        || !client_request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("invalid client_request_id".to_string());
    }
    Ok(())
}

pub(super) fn validate_job_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("command cannot be empty".to_string());
    }
    if command.len() > 8000 {
        return Err("command is too long; maximum is 8000 bytes".to_string());
    }
    if command.contains('\0') {
        return Err("command cannot contain NUL bytes".to_string());
    }
    Ok(())
}

pub(super) fn validate_job_script_path(script_path: &str) -> Result<(), String> {
    let path = script_path.trim();
    if path.is_empty() {
        return Err("script_path cannot be empty".to_string());
    }
    if path.len() > 512 {
        return Err("script_path is too long; maximum is 512 bytes".to_string());
    }
    if path.contains('\0') {
        return Err("script_path cannot contain NUL bytes".to_string());
    }
    if path.starts_with('-') {
        return Err("script_path cannot start with '-'".to_string());
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err("script_path must be project-relative".to_string());
    }
    for component in p.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(
                    "script_path must not contain traversal or absolute components".to_string(),
                )
            }
        }
    }
    if is_sensitive_path(path) {
        return Err("script_path points to a sensitive or blocked path".to_string());
    }
    Ok(())
}

pub(super) fn validate_job_script_args(args: &[String]) -> Result<(), String> {
    if args.len() > 100 {
        return Err("script_args must contain at most 100 items".to_string());
    }
    for arg in args {
        if arg.len() > 2000 {
            return Err("script_args item is too long; maximum is 2000 bytes".to_string());
        }
        if arg.contains('\0') {
            return Err("script_args cannot contain NUL bytes".to_string());
        }
    }
    Ok(())
}

pub(super) fn build_script_job_command(
    script_path: &str,
    script_args: &[String],
) -> Result<String, String> {
    validate_job_script_path(script_path)?;
    validate_job_script_args(script_args)?;
    let script_q = shell_escape(script_path.trim());
    let args_q = script_args
        .iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = format!(
        "test -f {script} || {{ echo 'script_path not found or not a file' >&2; exit 127; }}; bash {script}",
        script = script_q
    );
    if !args_q.is_empty() {
        command.push(' ');
        command.push_str(&args_q);
    }
    Ok(command)
}

pub(super) fn validate_job_runtime(max_runtime_secs: Option<i64>) -> Result<i64, String> {
    let secs = max_runtime_secs.unwrap_or(DEFAULT_JOB_MAX_RUNTIME_SECS);
    if !(1..=MAX_JOB_MAX_RUNTIME_SECS).contains(&secs) {
        return Err(format!(
            "max_runtime_secs must be between 1 and {}",
            MAX_JOB_MAX_RUNTIME_SECS
        ));
    }
    Ok(secs)
}

pub(super) fn job_dir_rel(job_id: &str) -> String {
    format!(".codex/jobs/{}", job_id)
}

pub(super) fn local_job_dir(root: &Path, job_id: &str) -> PathBuf {
    root.join(job_dir_rel(job_id))
}

pub(super) fn read_file_to_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

pub(super) fn write_status_file(path: &Path, status: &str) {
    let _ = std::fs::write(path.join("status"), status);
}

pub(super) fn write_finished_at_file(path: &Path, timestamp: i64) {
    let _ = std::fs::write(path.join("finished_at"), timestamp.to_string());
}

pub(super) fn read_finished_at_file(path: &Path) -> Option<i64> {
    read_file_to_string(&path.join("finished_at")).and_then(|s| s.parse::<i64>().ok())
}

pub(super) fn tail_lines_from_text(text: &str, tail_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail_lines.max(1));
    lines[start..].join("\n")
}

/// Read lines starting from since_line (1-based). Returns (text, total_line_count).
pub(super) fn read_lines_from(text: &str, since_line: usize, max_lines: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if since_line == 0 || since_line > total {
        return (String::new(), total);
    }
    let start = since_line - 1; // convert to 0-based
    let end = (start + max_lines).min(total);
    (lines[start..end].join("\n"), total)
}

pub(super) fn tail_file(path: &Path, tail_lines: usize) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| tail_lines_from_text(&s, tail_lines))
}

/// Returns (text, total_line_count).
pub(super) fn tail_file_with_count(path: &Path, tail_lines: usize) -> (Option<String>, usize) {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let total = s.lines().count();
            (Some(tail_lines_from_text(&s, tail_lines)), total)
        }
        Err(_) => (None, 0),
    }
}

/// Returns (text, total_line_count).
pub(super) fn read_from_line_with_count(
    path: &Path,
    since_line: usize,
    max_lines: usize,
) -> (Option<String>, usize) {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let (text, total) = read_lines_from(&s, since_line, max_lines);
            (Some(text), total)
        }
        Err(_) => (None, 0),
    }
}

/// Check stderr content for OOM signals.
pub(super) fn detect_oom_hint(stderr: &str) -> Option<String> {
    let lower = stderr.to_ascii_lowercase();
    let oom_patterns = [
        "out of memory",
        "oom",
        "killed",
        "memory error",
        "memoryerror",
        "cuda out of memory",
        "cudaoutofmemoryerror",
        "cannot allocate memory",
        "bus error",
    ];
    if oom_patterns.iter().any(|p| lower.contains(p)) {
        Some("possible_oom".to_string())
    } else {
        None
    }
}

pub(super) fn read_job_metadata_local(root: &Path, job_id: &str) -> Result<JobMetadata, String> {
    validate_job_id(job_id)?;
    let path = local_job_dir(root, job_id).join("metadata.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read job metadata: {}", e))?;
    serde_json::from_str(&text).map_err(|e| format!("Failed to parse job metadata: {}", e))
}

pub(super) fn pid_running_local(pid: i64) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(super) fn update_job_status_local(root: &Path, meta: &JobMetadata) -> JobInfo {
    let dir = local_job_dir(root, &meta.job_id);
    let now = chrono::Utc::now().timestamp();
    let pid = read_file_to_string(&dir.join("pid")).and_then(|s| s.parse::<i64>().ok());
    let exit_code = read_file_to_string(&dir.join("exit_code")).and_then(|s| s.parse::<i32>().ok());
    let mut status =
        read_file_to_string(&dir.join("status")).unwrap_or_else(|| meta.status.clone());
    if status == "running" {
        if let Some(pid) = pid {
            if meta.started_at.unwrap_or(meta.created_at) + meta.max_runtime_secs < now {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
                status = "timeout".to_string();
                write_status_file(&dir, &status);
                write_finished_at_file(&dir, now);
            } else if !pid_running_local(pid) {
                status = if exit_code.unwrap_or(1) == 0 {
                    "completed".to_string()
                } else {
                    "failed".to_string()
                };
                write_status_file(&dir, &status);
                if read_finished_at_file(&dir).is_none() {
                    write_finished_at_file(&dir, now);
                }
            }
        }
    }
    let finished_at = read_finished_at_file(&dir).or(meta.finished_at);
    // Compute elapsed_secs: wall-clock time from started_at to finished_at (or now if running)
    let elapsed_secs = meta.started_at.map(|s| finished_at.unwrap_or(now) - s);
    // Detect OOM hint from stderr tail
    let oom_hint = tail_file(&dir.join("stderr.log"), 40)
        .as_deref()
        .and_then(|s| detect_oom_hint(s));
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        kind: meta.kind.clone(),
        suite: meta.suite.clone(),
        script_path: meta.script_path.clone(),
        reason: meta.reason.clone(),
        status,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
        elapsed_secs,
        oom_hint,
    }
}

pub(super) fn local_job_info(root: &Path, job_id: &str) -> Result<JobInfo, String> {
    let meta = read_job_metadata_local(root, job_id)?;
    Ok(update_job_status_local(root, &meta))
}

pub(super) fn create_local_job(
    proj: &ProjectConfig,
    project: &str,
    goal_id: &str,
    command: &str,
    client_request_id: Option<String>,
    kind: Option<String>,
    suite: Option<String>,
    script_path: Option<String>,
    reason: Option<String>,
    max_runtime_secs: i64,
) -> Result<JobInfo, String> {
    validate_job_command(command)?;
    let root = proj.root();
    let job_id = uuid::Uuid::new_v4().to_string();
    let dir = local_job_dir(&root, &job_id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create job dir: {}", e))?;
    let now = chrono::Utc::now().timestamp();
    let meta = JobMetadata {
        job_id: job_id.clone(),
        client_request_id,
        project: project.to_string(),
        goal_id: goal_id.to_string(),
        command: command.to_string(),
        kind,
        suite,
        script_path,
        reason,
        status: "running".to_string(),
        created_at: now,
        started_at: Some(now),
        finished_at: None,
        max_runtime_secs,
        executor: "local".to_string(),
        host: None,
        path: proj.path.clone(),
    };
    std::fs::write(
        dir.join("metadata.json"),
        serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Failed to write metadata: {}", e))?;
    std::fs::write(
        dir.join("command.sh"),
        format!("#!/usr/bin/env bash\n{}\n", command),
    )
    .map_err(|e| format!("Failed to write command.sh: {}", e))?;
    std::fs::write(dir.join("status"), "running")
        .map_err(|e| format!("Failed to write status: {}", e))?;
    let dir_s = dir.to_string_lossy().to_string();
    let wrapper = format!(
        "bash {0}/command.sh > {0}/stdout.log 2> {0}/stderr.log; code=$?; echo $code > {0}/exit_code; finished=$(date +%s); echo $finished > {0}/finished_at; if [ $code -eq 0 ]; then echo completed > {0}/status; else echo failed > {0}/status; fi",
        shell_escape(&dir_s)
    );
    let child = std::process::Command::new("setsid")
        .arg("sh")
        .arg("-c")
        .arg(wrapper)
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn job: {}", e))?;
    std::fs::write(dir.join("pid"), child.id().to_string())
        .map_err(|e| format!("Failed to write pid: {}", e))?;
    Ok(update_job_status_local(&root, &meta))
}

pub(super) fn b64_text(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

pub(super) fn remote_read_job_metadata(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> Result<JobMetadata, String> {
    validate_job_id(job_id)?;
    let cmd = format!("cat {}/metadata.json", shell_escape(&job_dir_rel(job_id)));
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to read job metadata: {}", stderr.trim()));
    }
    serde_json::from_str(&stdout).map_err(|e| format!("Failed to parse job metadata: {}", e))
}

pub(super) fn remote_job_status_string(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> (Option<String>, Option<i64>, Option<i32>, Option<i64>) {
    let dir = job_dir_rel(job_id);
    let cmd = format!(
        "printf 'STATUS='; cat {0}/status 2>/dev/null || true; printf '\nPID='; cat {0}/pid 2>/dev/null || true; printf '\nEXIT='; cat {0}/exit_code 2>/dev/null || true; printf '\nFINISHED='; cat {0}/finished_at 2>/dev/null || true; printf '\n'",
        shell_escape(&dir)
    );
    let (code, stdout, _, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return (None, None, None, None);
    }
    let mut status = None;
    let mut pid = None;
    let mut exit_code = None;
    let mut finished_at = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("STATUS=") {
            if !v.trim().is_empty() {
                status = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("PID=") {
            pid = v.trim().parse::<i64>().ok();
        } else if let Some(v) = line.strip_prefix("EXIT=") {
            exit_code = v.trim().parse::<i32>().ok();
        } else if let Some(v) = line.strip_prefix("FINISHED=") {
            finished_at = v.trim().parse::<i64>().ok();
        }
    }
    (status, pid, exit_code, finished_at)
}

pub(super) fn update_job_status_ssh(
    proj: &ProjectConfig,
    meta: &JobMetadata,
    ssh_config: Option<&SshConfig>,
) -> JobInfo {
    let now = chrono::Utc::now().timestamp();
    let dir = job_dir_rel(&meta.job_id);
    let (mut status, pid, exit_code, mut finished_at) =
        remote_job_status_string(proj, &meta.job_id, ssh_config);
    let mut status_value = status.take().unwrap_or_else(|| meta.status.clone());
    if status_value == "running" {
        if let Some(pid) = pid {
            if meta.started_at.unwrap_or(meta.created_at) + meta.max_runtime_secs < now {
                let cmd = format!(
                    "pid={}; kill_tree() {{ for child in $(pgrep -P \"$1\" 2>/dev/null || true); do kill_tree \"$child\"; done; kill -TERM \"$1\" 2>/dev/null || true; }}; kill_tree \"$pid\"; sleep 1; if kill -0 \"$pid\" 2>/dev/null; then kill_tree() {{ for child in $(pgrep -P \"$1\" 2>/dev/null || true); do kill_tree \"$child\"; done; kill -KILL \"$1\" 2>/dev/null || true; }}; kill_tree \"$pid\"; fi; now=$(date +%s); printf timeout > {}/status; echo $now > {}/finished_at",
                    pid,
                    shell_escape(&dir),
                    shell_escape(&dir)
                );
                let _ = run_project_cmd(proj, &cmd, 10, ssh_config);
                status_value = "timeout".to_string();
            } else {
                let cmd = format!("kill -0 {} 2>/dev/null", pid);
                let (code, _, _, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
                if code != 0 {
                    status_value = if exit_code.unwrap_or(1) == 0 {
                        "completed".to_string()
                    } else {
                        "failed".to_string()
                    };
                    let cmd = format!(
                        "printf {} > {}/status; test -f {}/finished_at || date +%s > {}/finished_at",
                        shell_escape(&status_value),
                        shell_escape(&dir),
                        shell_escape(&dir),
                        shell_escape(&dir)
                    );
                    let _ = run_project_cmd(proj, &cmd, 10, ssh_config);
                    let (_, _, _, refreshed_finished_at) =
                        remote_job_status_string(proj, &meta.job_id, ssh_config);
                    finished_at = refreshed_finished_at.or(finished_at);
                }
            }
        }
    }
    if status_value == "timeout" && finished_at.is_none() {
        let (_, _, _, refreshed_finished_at) =
            remote_job_status_string(proj, &meta.job_id, ssh_config);
        finished_at = refreshed_finished_at;
    }
    let resolved_finished_at = finished_at.or(meta.finished_at);
    let elapsed_secs = meta
        .started_at
        .map(|s| resolved_finished_at.unwrap_or(now) - s);
    // For SSH, we read stderr tail to detect OOM hints
    let oom_hint = {
        let stderr_tail = ssh_job_log_stderr_tail(proj, &meta.job_id, 40, ssh_config);
        detect_oom_hint(&stderr_tail)
    };
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        kind: meta.kind.clone(),
        suite: meta.suite.clone(),
        script_path: meta.script_path.clone(),
        reason: meta.reason.clone(),
        status: status_value,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at: resolved_finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
        elapsed_secs,
        oom_hint,
    }
}

pub(super) fn ssh_job_info(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> Result<JobInfo, String> {
    let meta = remote_read_job_metadata(proj, job_id, ssh_config)?;
    Ok(update_job_status_ssh(proj, &meta, ssh_config))
}

pub(super) fn create_ssh_job(
    proj: &ProjectConfig,
    project: &str,
    goal_id: &str,
    command: &str,
    client_request_id: Option<String>,
    kind: Option<String>,
    suite: Option<String>,
    script_path: Option<String>,
    reason: Option<String>,
    max_runtime_secs: i64,
    ssh_config: Option<&SshConfig>,
) -> Result<JobInfo, String> {
    validate_job_command(command)?;
    let job_id = uuid::Uuid::new_v4().to_string();
    let dir = job_dir_rel(&job_id);
    let now = chrono::Utc::now().timestamp();
    let meta = JobMetadata {
        job_id: job_id.clone(),
        client_request_id,
        project: project.to_string(),
        goal_id: goal_id.to_string(),
        command: command.to_string(),
        kind,
        suite,
        script_path,
        reason,
        status: "running".to_string(),
        created_at: now,
        started_at: Some(now),
        finished_at: None,
        max_runtime_secs,
        executor: "ssh".to_string(),
        host: proj.host.clone(),
        path: proj.path.clone(),
    };
    let meta_b64 = b64_text(&serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?);
    let command_b64 = b64_text(&format!("#!/usr/bin/env bash\n{}\n", command));
    let dir_q = shell_escape(&dir);
    let wrapper = format!(
        "bash {0}/command.sh > {0}/stdout.log 2> {0}/stderr.log; code=$?; echo $code > {0}/exit_code; finished=$(date +%s); echo $finished > {0}/finished_at; if [ $code -eq 0 ]; then echo completed > {0}/status; else echo failed > {0}/status; fi",
        dir_q
    );
    let cmd = format!(
        "mkdir -p {dir}; printf %s {meta} | base64 -d > {dir}/metadata.json; printf %s {cmd_b64} | base64 -d > {dir}/command.sh; printf running > {dir}/status; nohup setsid sh -c {wrapper} >/dev/null 2>&1 & echo $! > {dir}/pid",
        dir = dir_q,
        meta = shell_escape(&meta_b64),
        cmd_b64 = shell_escape(&command_b64),
        wrapper = shell_escape(&wrapper)
    );
    let (code, _, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to create SSH job: {}", stderr.trim()));
    }
    ssh_job_info(proj, &job_id, ssh_config)
}

pub(super) fn list_local_jobs(
    root: &Path,
    limit: usize,
    status_filter: Option<&str>,
) -> Vec<JobInfo> {
    let jobs_dir = root.join(".codex/jobs");
    let mut jobs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(jobs_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if jobs.len() >= limit {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if validate_job_id(&name).is_ok() {
                if let Ok(info) = local_job_info(root, &name) {
                    if status_filter.map(|s| s == info.status).unwrap_or(true) {
                        jobs.push(info);
                    }
                }
            }
        }
    }
    jobs.sort_by_key(|j| -j.created_at);
    jobs.truncate(limit);
    jobs
}

pub(super) fn filter_jobs_by_client_request_id(
    jobs: &mut Vec<JobInfo>,
    client_request_id: Option<&str>,
) {
    if let Some(client_request_id) = client_request_id {
        jobs.retain(|j| j.client_request_id.as_deref() == Some(client_request_id));
    }
}

pub(super) fn list_ssh_jobs(
    proj: &ProjectConfig,
    limit: usize,
    status_filter: Option<&str>,
    ssh_config: Option<&SshConfig>,
) -> Vec<JobInfo> {
    let cmd = "find .codex/jobs -mindepth 1 -maxdepth 1 -type d -printf '%f\\n' 2>/dev/null | sort | tail -n 200";
    let (code, stdout, _, _) = run_project_cmd(proj, cmd, 10, ssh_config);
    if code != 0 {
        return Vec::new();
    }
    let mut jobs = Vec::new();
    for job_id in stdout.lines() {
        if jobs.len() >= limit {
            break;
        }
        if validate_job_id(job_id).is_ok() {
            if let Ok(info) = ssh_job_info(proj, job_id, ssh_config) {
                if status_filter.map(|s| s == info.status).unwrap_or(true) {
                    jobs.push(info);
                }
            }
        }
    }
    jobs.sort_by_key(|j| -j.created_at);
    jobs.truncate(limit);
    jobs
}

pub(super) fn local_job_log(
    root: &Path,
    job_id: &str,
    tail_lines: usize,
    since_line: Option<usize>,
) -> Result<(String, String, usize), String> {
    validate_job_id(job_id)?;
    let dir = local_job_dir(root, job_id);
    let stdout_path = dir.join("stdout.log");
    let stderr_path = dir.join("stderr.log");
    let (stdout_text, total_lines) = if let Some(sl) = since_line.filter(|&n| n > 0) {
        read_from_line_with_count(&stdout_path, sl, tail_lines)
    } else {
        tail_file_with_count(&stdout_path, tail_lines)
    };
    let (stderr_text, _) = tail_file_with_count(&stderr_path, tail_lines);
    Ok((
        stdout_text.unwrap_or_default(),
        stderr_text.unwrap_or_default(),
        total_lines,
    ))
}

pub(super) fn ssh_job_log(
    proj: &ProjectConfig,
    job_id: &str,
    tail_lines: usize,
    ssh_config: Option<&SshConfig>,
) -> Result<(String, String), String> {
    let (out, err, _) = ssh_job_log_with_count(proj, job_id, tail_lines, None, ssh_config)?;
    Ok((out, err))
}

/// ssh_job_log with total_lines and since_line support. Returns (stdout, stderr, total_stdout_lines).
pub(super) fn ssh_job_log_with_count(
    proj: &ProjectConfig,
    job_id: &str,
    tail_lines: usize,
    since_line: Option<usize>,
    ssh_config: Option<&SshConfig>,
) -> Result<(String, String, usize), String> {
    validate_job_id(job_id)?;
    let dir = shell_escape(&job_dir_rel(job_id));
    let n = tail_lines.clamp(1, 1000);
    // Get total line count of stdout.log and the requested slice
    let (since_cmd, tail_cmd) = if let Some(sl) = since_line.filter(|&n| n > 0) {
        (
            format!("wc -l < {dir}/stdout.log 2>/dev/null || echo 0"),
            format!(
                "tail -n +{sl} {dir}/stdout.log 2>/dev/null | head -n {n} || true",
                sl = sl,
                n = n,
                dir = dir
            ),
        )
    } else {
        (
            format!("wc -l < {dir}/stdout.log 2>/dev/null || echo 0"),
            format!(
                "tail -n {n} {dir}/stdout.log 2>/dev/null || true",
                n = n,
                dir = dir
            ),
        )
    };
    let cmd = format!(
        "printf '__TOTAL__\\n'; {total}; printf '__STDOUT__\\n'; {stdout}; printf '\\n__STDERR__\\n'; tail -n {n} {dir}/stderr.log 2>/dev/null || true",
        total = since_cmd,
        stdout = tail_cmd,
        n = n,
        dir = dir
    );
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to read job log: {}", stderr.trim()));
    }
    let mut out = String::new();
    let mut err = String::new();
    let mut total_lines: usize = 0;
    let mut section = "";
    for line in stdout.lines() {
        match line {
            "__TOTAL__" => section = "total",
            "__STDOUT__" => section = "out",
            "__STDERR__" => section = "err",
            _ if section == "total" => {
                total_lines = line.trim().parse::<usize>().unwrap_or(0);
            }
            _ if section == "out" => {
                out.push_str(line);
                out.push('\n');
            }
            _ if section == "err" => {
                err.push_str(line);
                err.push('\n');
            }
            _ => {}
        }
    }
    Ok((
        out.trim_end().to_string(),
        err.trim_end().to_string(),
        total_lines,
    ))
}

/// Read only stderr tail for OOM detection.
pub(super) fn ssh_job_log_stderr_tail(
    proj: &ProjectConfig,
    job_id: &str,
    tail_lines: usize,
    ssh_config: Option<&SshConfig>,
) -> String {
    if validate_job_id(job_id).is_err() {
        return String::new();
    }
    let dir = shell_escape(&job_dir_rel(job_id));
    let n = tail_lines.clamp(1, 200);
    let cmd = format!("tail -n {n} {dir}/stderr.log 2>/dev/null || true");
    let (_, stdout, _, _) = run_project_cmd(proj, &cmd, 5, ssh_config);
    stdout
}

fn kill_local_tree(pid: i64, signal: &str) {
    let children = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default();
    for child in children.lines() {
        if let Ok(child_pid) = child.trim().parse::<i64>() {
            kill_local_tree(child_pid, signal);
        }
    }
    let _ = std::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
}

pub(super) fn stop_local_job(root: &Path, job_id: &str) -> Result<JobInfo, String> {
    validate_job_id(job_id)?;
    let meta = read_job_metadata_local(root, job_id)?;
    let dir = local_job_dir(root, job_id);
    if let Some(pid) = read_file_to_string(&dir.join("pid")).and_then(|s| s.parse::<i64>().ok()) {
        kill_local_tree(pid, "-TERM");
        std::thread::sleep(std::time::Duration::from_millis(300));
        if pid_running_local(pid) {
            kill_local_tree(pid, "-KILL");
        }
    }
    write_status_file(&dir, "stopped");
    write_finished_at_file(&dir, chrono::Utc::now().timestamp());
    Ok(update_job_status_local(root, &meta))
}

pub(super) fn stop_ssh_job(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> Result<JobInfo, String> {
    validate_job_id(job_id)?;
    let dir = job_dir_rel(job_id);
    let cmd = format!(
        "if test -f {0}/pid; then pid=$(cat {0}/pid); kill_tree() {{ for child in $(pgrep -P \"$1\" 2>/dev/null || true); do kill_tree \"$child\"; done; kill -TERM \"$1\" 2>/dev/null || true; }}; kill_tree \"$pid\"; sleep 1; if kill -0 \"$pid\" 2>/dev/null; then kill_tree() {{ for child in $(pgrep -P \"$1\" 2>/dev/null || true); do kill_tree \"$child\"; done; kill -KILL \"$1\" 2>/dev/null || true; }}; kill_tree \"$pid\"; fi; fi; now=$(date +%s); printf stopped > {0}/status; echo $now > {0}/finished_at",
        shell_escape(&dir)
    );
    let (code, _, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to stop job: {}", stderr.trim()));
    }
    ssh_job_info(proj, job_id, ssh_config)
}

pub(super) fn summarize_jobs_markdown(jobs: &[JobInfo], log_tails: &[(String, String)]) -> String {
    let mut md = String::from("# Codex job summary\n\n| job_id | kind | suite | status | exit_code | duration_secs | command |\n|---|---|---|---:|---:|---:|---|\n");
    for job in jobs {
        let duration = job
            .finished_at
            .or_else(|| Some(chrono::Utc::now().timestamp()))
            .unwrap_or(job.created_at)
            - job.started_at.unwrap_or(job.created_at);
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | `{}` |\n",
            job.job_id,
            job.kind.as_deref().unwrap_or("command"),
            job.suite.as_deref().unwrap_or(""),
            job.status,
            job.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "".to_string()),
            duration.max(0),
            job.command.replace('`', "'")
        ));
    }
    for (idx, job) in jobs.iter().enumerate() {
        let (stdout_tail, stderr_tail) = log_tails.get(idx).cloned().unwrap_or_default();
        md.push_str(&format!("\n## {}\n\n", job.job_id));
        md.push_str("### stdout tail\n\n```text\n");
        md.push_str(&stdout_tail);
        md.push_str("\n```\n\n### stderr tail\n\n```text\n");
        md.push_str(&stderr_tail);
        md.push_str("\n```\n");
    }
    md
}

// --- recover / status detail=basic helpers ---

/// Determine the effective detail level for op=status.
/// Only explicit "logs" triggers log reading; everything else is "basic".
/// This function is extracted for testability.
fn effective_status_detail(detail: Option<&str>) -> &'static str {
    match detail {
        Some("logs") => "logs",
        _ => "basic",
    }
}

/// Lightweight metadata-only job info for local jobs.
/// Reads metadata.json + status/exit_code/finished_at files only.
/// No kill -0, no OOM detection, no log reading.
fn recover_local_job_info(root: &Path, job_id: &str) -> Result<JobInfo, String> {
    let meta = read_job_metadata_local(root, job_id)?;
    let dir = local_job_dir(root, job_id);
    let status = read_file_to_string(&dir.join("status")).unwrap_or_else(|| meta.status.clone());
    let exit_code = read_file_to_string(&dir.join("exit_code")).and_then(|s| s.parse::<i32>().ok());
    let pid = read_file_to_string(&dir.join("pid")).and_then(|s| s.parse::<i64>().ok());
    let finished_at = read_finished_at_file(&dir).or(meta.finished_at);
    let now = chrono::Utc::now().timestamp();
    let elapsed_secs = meta.started_at.map(|s| finished_at.unwrap_or(now) - s);
    Ok(JobInfo {
        job_id: meta.job_id,
        client_request_id: meta.client_request_id,
        project: meta.project,
        goal_id: meta.goal_id,
        command: meta.command,
        kind: meta.kind,
        suite: meta.suite,
        script_path: meta.script_path,
        reason: meta.reason,
        status,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor,
        pid,
        exit_code,
        elapsed_secs,
        oom_hint: None, // No OOM detection in recover
    })
}

/// Lightweight metadata-only job info for SSH jobs.
/// Single SSH call: reads metadata.json + status/pid/exit_code/finished_at.
/// No kill -0, no kill_tree, no OOM detection, no log reading.
fn recover_ssh_job_info(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> Result<JobInfo, String> {
    validate_job_id(job_id)?;
    let dir = shell_escape(&job_dir_rel(job_id));
    // Combined single SSH call to read metadata + status files
    let cmd = format!(
        "cat {dir}/metadata.json; printf '\\n__RECOVER_STATUS__\\n'; cat {dir}/status 2>/dev/null || true; \
         printf '\\n__RECOVER_PID__\\n'; cat {dir}/pid 2>/dev/null || true; \
         printf '\\n__RECOVER_EXIT__\\n'; cat {dir}/exit_code 2>/dev/null || true; \
         printf '\\n__RECOVER_FINISHED__\\n'; cat {dir}/finished_at 2>/dev/null || true",
        dir = dir
    );
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to read job info: {}", stderr.trim()));
    }
    let mut section = "meta";
    let mut meta_json = String::new();
    let mut status_val: Option<String> = None;
    let mut pid: Option<i64> = None;
    let mut exit_code: Option<i32> = None;
    let mut finished_at: Option<i64> = None;
    for line in stdout.lines() {
        match line {
            "__RECOVER_STATUS__" => section = "status",
            "__RECOVER_PID__" => section = "pid",
            "__RECOVER_EXIT__" => section = "exit",
            "__RECOVER_FINISHED__" => section = "finished",
            _ => match section {
                "meta" => {
                    meta_json.push_str(line);
                    meta_json.push('\n');
                }
                "status" => {
                    if !line.trim().is_empty() {
                        status_val = Some(line.trim().to_string());
                    }
                }
                "pid" => {
                    pid = line.trim().parse::<i64>().ok();
                }
                "exit" => {
                    exit_code = line.trim().parse::<i32>().ok();
                }
                "finished" => {
                    finished_at = line.trim().parse::<i64>().ok();
                }
                _ => {}
            },
        }
    }
    let meta: JobMetadata = serde_json::from_str(meta_json.trim())
        .map_err(|e| format!("Failed to parse job metadata: {}", e))?;
    let status = status_val.unwrap_or_else(|| meta.status.clone());
    let resolved_finished_at = finished_at.or(meta.finished_at);
    let now = chrono::Utc::now().timestamp();
    let elapsed_secs = meta
        .started_at
        .map(|s| resolved_finished_at.unwrap_or(now) - s);
    Ok(JobInfo {
        job_id: meta.job_id,
        client_request_id: meta.client_request_id,
        project: meta.project,
        goal_id: meta.goal_id,
        command: meta.command,
        kind: meta.kind,
        suite: meta.suite,
        script_path: meta.script_path,
        reason: meta.reason,
        status,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at: resolved_finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor,
        pid,
        exit_code,
        elapsed_secs,
        oom_hint: None, // No OOM detection in recover
    })
}

/// Find a local job ID by client_request_id. Only reads metadata.json (no status update).
fn find_local_job_id_by_client_request_id(
    root: &Path,
    client_request_id: &str,
    goal_id: Option<&str>,
) -> Option<String> {
    let jobs_dir = root.join(".codex/jobs");
    let mut candidates: Vec<(i64, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&jobs_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if validate_job_id(&name).is_ok() {
                if let Ok(meta) = read_job_metadata_local(root, &name) {
                    if meta.client_request_id.as_deref() == Some(client_request_id) {
                        if goal_id.map(|g| g == meta.goal_id).unwrap_or(true) {
                            candidates.push((meta.created_at, name));
                        }
                    }
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.first().map(|(_, id)| id.clone())
}

/// Find an SSH job ID by client_request_id using a single grep-based SSH command.
/// Note: SSH recover by client_request_id scans recent job metadata only (last 100 dirs);
/// job_id is more direct when available.
fn find_ssh_job_id_by_client_request_id(
    proj: &ProjectConfig,
    client_request_id: &str,
    goal_id: Option<&str>,
    ssh_config: Option<&SshConfig>,
) -> Option<String> {
    // Single SSH call: scan recent job dirs, grep metadata for client_request_id
    let rid_escaped = shell_escape(&format!("\"client_request_id\": \"{}\"", client_request_id));
    let cmd = format!(
        "for d in $(find .codex/jobs -mindepth 1 -maxdepth 1 -type d -printf '%f\\n' 2>/dev/null | sort -r | head -100); do \
         if grep -qF {rid} \".codex/jobs/$d/metadata.json\" 2>/dev/null; then \
         printf '%s' \"$d\"; break; fi; done",
        rid = rid_escaped
    );
    let (code, stdout, _, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 || stdout.trim().is_empty() {
        return None;
    }
    let job_id = stdout.trim().to_string();
    if validate_job_id(&job_id).is_err() {
        return None;
    }
    // Verify goal_id if specified
    if let Some(goal_id) = goal_id {
        if let Ok(meta) = remote_read_job_metadata(proj, &job_id, ssh_config) {
            if meta.goal_id != goal_id {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(job_id)
}

/// Lightweight status update for local jobs (skip OOM detection).
fn update_job_status_local_basic(root: &Path, meta: &JobMetadata) -> JobInfo {
    let dir = local_job_dir(root, &meta.job_id);
    let now = chrono::Utc::now().timestamp();
    let pid = read_file_to_string(&dir.join("pid")).and_then(|s| s.parse::<i64>().ok());
    let exit_code = read_file_to_string(&dir.join("exit_code")).and_then(|s| s.parse::<i32>().ok());
    let mut status =
        read_file_to_string(&dir.join("status")).unwrap_or_else(|| meta.status.clone());
    if status == "running" {
        if let Some(pid) = pid {
            if meta.started_at.unwrap_or(meta.created_at) + meta.max_runtime_secs < now {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
                status = "timeout".to_string();
                write_status_file(&dir, &status);
                write_finished_at_file(&dir, now);
            } else if !pid_running_local(pid) {
                status = if exit_code.unwrap_or(1) == 0 {
                    "completed".to_string()
                } else {
                    "failed".to_string()
                };
                write_status_file(&dir, &status);
                if read_finished_at_file(&dir).is_none() {
                    write_finished_at_file(&dir, now);
                }
            }
        }
    }
    let finished_at = read_finished_at_file(&dir).or(meta.finished_at);
    let elapsed_secs = meta.started_at.map(|s| finished_at.unwrap_or(now) - s);
    // Skip OOM detection (no stderr read) for basic detail
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        kind: meta.kind.clone(),
        suite: meta.suite.clone(),
        script_path: meta.script_path.clone(),
        reason: meta.reason.clone(),
        status,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
        elapsed_secs,
        oom_hint: None,
    }
}

/// Lightweight local job info (basic detail, no OOM detection).
fn local_job_info_basic(root: &Path, job_id: &str) -> Result<JobInfo, String> {
    let meta = read_job_metadata_local(root, job_id)?;
    Ok(update_job_status_local_basic(root, &meta))
}

/// Lightweight status update for SSH jobs (skip kill -0, kill_tree, OOM detection).
/// Note: basic is metadata/status-file based and may be stale for SSH jobs;
/// use detail=logs, log, or summarize for deeper inspection.
fn update_job_status_ssh_basic(
    proj: &ProjectConfig,
    meta: &JobMetadata,
    ssh_config: Option<&SshConfig>,
) -> JobInfo {
    let now = chrono::Utc::now().timestamp();
    // Single SSH call to read status files (no kill -0, no process tree walk)
    let (mut status, pid, exit_code, mut finished_at) =
        remote_job_status_string(proj, &meta.job_id, ssh_config);
    let status_value = status.take().unwrap_or_else(|| meta.status.clone());
    // For basic detail, skip kill -0 / kill_tree / timeout checks entirely.
    // Status is taken at face value from the status file.
    if status_value == "timeout" && finished_at.is_none() {
        let (_, _, _, refreshed_finished_at) =
            remote_job_status_string(proj, &meta.job_id, ssh_config);
        finished_at = refreshed_finished_at;
    }
    let resolved_finished_at = finished_at.or(meta.finished_at);
    let elapsed_secs = meta
        .started_at
        .map(|s| resolved_finished_at.unwrap_or(now) - s);
    // Skip OOM detection (no stderr read) for basic detail
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        kind: meta.kind.clone(),
        suite: meta.suite.clone(),
        script_path: meta.script_path.clone(),
        reason: meta.reason.clone(),
        status: status_value,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at: resolved_finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
        elapsed_secs,
        oom_hint: None,
    }
}

/// Lightweight SSH job info (basic detail, no kill -0, no OOM detection).
fn ssh_job_info_basic(
    proj: &ProjectConfig,
    job_id: &str,
    ssh_config: Option<&SshConfig>,
) -> Result<JobInfo, String> {
    let meta = remote_read_job_metadata(proj, job_id, ssh_config)?;
    Ok(update_job_status_ssh_basic(proj, &meta, ssh_config))
}

#[handler]
pub async fn codex_job(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(projects) = get_projects(depot) else {
        res.render(Json(job_response(
            "unknown",
            false,
            Some("Projects not configured".to_string()),
        )));
        return;
    };
    let Some(db) = get_db(depot) else {
        res.render(Json(job_response(
            "unknown",
            false,
            Some("Database not configured".to_string()),
        )));
        return;
    };
    let body: JobOpRequest = match req.parse_json().await {
        Ok(b) => b,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(job_response(
                "unknown",
                false,
                Some(format!("Invalid JSON: {}", e)),
            )));
            return;
        }
    };
    let op = body.op.clone();
    let project = match body.project.clone() {
        Some(p) => p,
        None => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(job_response(
                &op,
                false,
                Some("project is required".to_string()),
            )));
            return;
        }
    };
    let proj = match projects.get_project(&project) {
        Ok(p) => p,
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(job_response(&op, false, Some(e))));
            return;
        }
    };
    let ssh_config = projects.ssh.as_ref();
    let client_request_id = body.client_request_id.as_deref();
    if let Some(client_request_id) = client_request_id {
        if let Err(e) = validate_client_request_id(client_request_id) {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(job_response(&op, false, Some(e))));
            return;
        }
    }
    match op.as_str() {
        "check" => {
            let Some(goal_id) = body.goal_id.as_deref() else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            if let Err(e) = require_active_goal(&db, goal_id, &project) {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(job_response(&op, false, Some(e))));
                return;
            }
            let Some(suite) = body.suite.as_deref() else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("suite is required".to_string()),
                )));
                return;
            };
            if !proj.is_check_allowed(suite) {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some(format!(
                        "Check '{}' is not allowed. Allowed: {}",
                        suite,
                        proj.allowed_checks.join(", ")
                    )),
                )));
                return;
            }
            let command = match proj.get_check_command(suite) {
                Ok(c) => c,
                Err(e) => {
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            };
            let max_runtime_secs = match validate_job_runtime(body.max_runtime_secs) {
                Ok(v) => v,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            };
            if let Some(client_request_id) = client_request_id {
                let mut existing = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                existing.retain(|j| j.goal_id == goal_id);
                filter_jobs_by_client_request_id(&mut existing, Some(client_request_id));
                existing.sort_by_key(|j| -j.created_at);
                if let Some(job) = existing.first().cloned() {
                    res.render(Json(JobOpResponse {
                        success: true,
                        op,
                        job_id: Some(job.job_id.clone()),
                        job_ids: vec![job.job_id.clone()],
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout_tail: None,
                        stderr_tail: None,
                        summary_markdown: None,
                        error: None,
                        log_total_lines: None,
                        next_cursor: None,
                        metadata_only: None,
                        logs_included: None,
                        warnings: Vec::new(),
                    }));
                    return;
                }
            }
            let reason = body
                .reason
                .clone()
                .or_else(|| Some(format!("run check {}", suite)));
            let result = if proj.is_ssh() {
                create_ssh_job(
                    proj,
                    &project,
                    goal_id,
                    &command,
                    body.client_request_id.clone(),
                    Some("check".to_string()),
                    Some(suite.to_string()),
                    None,
                    reason,
                    max_runtime_secs,
                    ssh_config,
                )
            } else {
                create_local_job(
                    proj,
                    &project,
                    goal_id,
                    &command,
                    body.client_request_id.clone(),
                    Some("check".to_string()),
                    Some(suite.to_string()),
                    None,
                    reason,
                    max_runtime_secs,
                )
            };
            match result {
                Ok(job) => res.render(Json(JobOpResponse {
                    success: true,
                    op,
                    job_id: Some(job.job_id.clone()),
                    job_ids: vec![job.job_id.clone()],
                    job: Some(job),
                    jobs: Vec::new(),
                    stdout_tail: None,
                    stderr_tail: None,
                    summary_markdown: None,
                    error: None,
                    log_total_lines: None,
                    next_cursor: None,
                    metadata_only: None,
                    logs_included: None,
                    warnings: Vec::new(),
                })),
                Err(e) => res.render(Json(job_response(&op, false, Some(e)))),
            }
        }
        "create" => {
            let Some(goal_id) = body.goal_id.as_deref() else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            if let Err(e) = require_active_goal(&db, goal_id, &project) {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(job_response(&op, false, Some(e))));
                return;
            }
            // Determine the command source: trusted script_text, command, or script_path
            let (command, job_kind, job_script_path, trusted_script_text) = match (
                body.script_text.as_deref(),
                body.trusted.unwrap_or(false),
                body.command.as_deref(),
                body.script_path.as_deref(),
            ) {
                // Trusted script_text mode
                (Some(script_text), true, None, None) => {
                    // Validate the trusted script
                    if let Err(e) = validate_trusted_script(script_text) {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(job_response(&op, false, Some(e))));
                        return;
                    }
                    // Safety checks
                    if let Some(err) = check_denylist(script_text) {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(job_response(&op, false, Some(err))));
                        return;
                    }
                    if let Some(err) = check_secret_read(script_text) {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(job_response(&op, false, Some(err))));
                        return;
                    }
                    if let Some(err) = check_background_escape(script_text) {
                        res.status_code(StatusCode::BAD_REQUEST);
                        res.render(Json(job_response(&op, false, Some(err))));
                        return;
                    }
                    // Build command that wraps the script with set -euo pipefail
                    // We embed the script in a heredoc-like fashion for bash -c
                    let escaped = script_text.replace("'", "'\\''");
                    let command = format!("set -euo pipefail; '{}'", escaped);
                    (
                        command,
                        "trusted_script".to_string(),
                        None,
                        Some(script_text.to_string()),
                    )
                }
                // script_text without trusted=true
                (Some(_), false, _, _) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("script_text requires trusted=true".to_string()),
                    )));
                    return;
                }
                // script_text conflicts with command/script_path
                (Some(_), true, Some(_), _) | (Some(_), true, _, Some(_)) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some(
                            "provide script_text (trusted) or command/script_path, not both"
                                .to_string(),
                        ),
                    )));
                    return;
                }
                // Original command or script_path mode (not trusted)
                (None, _, Some(_), Some(_)) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("provide either command or script_path, not both".to_string()),
                    )));
                    return;
                }
                (None, _, Some(command), None) => {
                    (command.to_string(), "command".to_string(), None, None)
                }
                (None, _, None, Some(script_path)) => {
                    match build_script_job_command(script_path, &body.script_args) {
                        Ok(command) => (
                            command,
                            "script".to_string(),
                            Some(script_path.to_string()),
                            None,
                        ),
                        Err(e) => {
                            res.status_code(StatusCode::BAD_REQUEST);
                            res.render(Json(job_response(&op, false, Some(e))));
                            return;
                        }
                    }
                }
                (None, _, None, None) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some(
                            "command, script_path, or script_text (with trusted=true) is required"
                                .to_string(),
                        ),
                    )));
                    return;
                }
            };
            let max_runtime_secs = match validate_job_runtime(body.max_runtime_secs) {
                Ok(v) => v,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            };
            if let Some(client_request_id) = client_request_id {
                let mut existing = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                existing.retain(|j| j.goal_id == goal_id);
                filter_jobs_by_client_request_id(&mut existing, Some(client_request_id));
                existing.sort_by_key(|j| -j.created_at);
                if let Some(job) = existing.first().cloned() {
                    res.render(Json(JobOpResponse {
                        success: true,
                        op,
                        job_id: Some(job.job_id.clone()),
                        job_ids: vec![job.job_id.clone()],
                        job: Some(job),
                        jobs: Vec::new(),
                        stdout_tail: None,
                        stderr_tail: None,
                        summary_markdown: None,
                        error: None,
                        log_total_lines: None,
                        next_cursor: None,
                        metadata_only: None,
                        logs_included: None,
                        warnings: Vec::new(),
                    }));
                    return;
                }
            }
            let result = if proj.is_ssh() {
                create_ssh_job(
                    proj,
                    &project,
                    goal_id,
                    &command,
                    body.client_request_id.clone(),
                    Some(job_kind.clone()),
                    None,
                    job_script_path.clone(),
                    body.reason.clone(),
                    max_runtime_secs,
                    ssh_config,
                )
            } else {
                create_local_job(
                    proj,
                    &project,
                    goal_id,
                    &command,
                    body.client_request_id.clone(),
                    Some(job_kind.clone()),
                    None,
                    job_script_path.clone(),
                    body.reason.clone(),
                    max_runtime_secs,
                )
            };
            let job = match result {
                Ok(job) => job,
                Err(e) => {
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            };
            // If trusted script_text was provided, write the script file in the job directory
            // for audit/reference purposes
            if let Some(ref script_text) = trusted_script_text {
                let job_dir = local_job_dir(&proj.root(), &job.job_id);
                let script_path = job_dir.join("script.sh");
                let script_content = format!(
                    "#!/usr/bin/env bash\nset -euo pipefail\n{}\n",
                    script_text.trim()
                );
                if let Err(e) = std::fs::write(&script_path, &script_content) {
                    // Non-fatal: the job is already running, just log the error
                    tracing::warn!("Failed to write trusted job script audit file: {}", e);
                }
            }
            res.render(Json(JobOpResponse {
                success: true,
                op,
                job_id: Some(job.job_id.clone()),
                job_ids: vec![job.job_id.clone()],
                job: Some(job),
                jobs: Vec::new(),
                stdout_tail: None,
                stderr_tail: None,
                summary_markdown: None,
                error: None,
                log_total_lines: None,
                next_cursor: None,
                metadata_only: None,
                logs_included: None,
                warnings: Vec::new(),
            }));
        }
        "create_batch" => {
            let Some(goal_id) = body.goal_id.as_deref() else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("goal_id is required".to_string()),
                )));
                return;
            };
            if let Err(e) = require_active_goal(&db, goal_id, &project) {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Json(job_response(&op, false, Some(e))));
                return;
            }
            if body.commands.is_empty() || body.commands.len() > 20 {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("commands must contain 1..20 items".to_string()),
                )));
                return;
            }
            let max_runtime_secs = match validate_job_runtime(body.max_runtime_secs) {
                Ok(v) => v,
                Err(e) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            };
            for command in &body.commands {
                if let Err(e) = validate_job_command(command) {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(&op, false, Some(e))));
                    return;
                }
            }
            if let Some(client_request_id) = client_request_id {
                let mut existing = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                existing.retain(|j| j.goal_id == goal_id);
                existing.retain(|j| {
                    j.client_request_id
                        .as_deref()
                        .map(|id| {
                            id == client_request_id
                                || id.starts_with(&format!("{}.", client_request_id))
                        })
                        .unwrap_or(false)
                });
                existing.sort_by_key(|j| -j.created_at);
                if !existing.is_empty() {
                    let job_ids = existing
                        .iter()
                        .map(|j| j.job_id.clone())
                        .collect::<Vec<_>>();
                    res.render(Json(JobOpResponse {
                        success: true,
                        op,
                        job_id: job_ids.first().cloned(),
                        job_ids,
                        job: existing.first().cloned(),
                        jobs: existing,
                        stdout_tail: None,
                        stderr_tail: None,
                        summary_markdown: None,
                        error: None,
                        log_total_lines: None,
                        next_cursor: None,
                        metadata_only: None,
                        logs_included: None,
                        warnings: Vec::new(),
                    }));
                    return;
                }
            }
            let mut jobs = Vec::new();
            for (idx, command) in body.commands.iter().enumerate() {
                let result = if proj.is_ssh() {
                    create_ssh_job(
                        proj,
                        &project,
                        goal_id,
                        command,
                        body.client_request_id
                            .as_ref()
                            .map(|id| format!("{}.{}", id, idx + 1)),
                        Some("command".to_string()),
                        None,
                        None,
                        body.reason.clone(),
                        max_runtime_secs,
                        ssh_config,
                    )
                } else {
                    create_local_job(
                        proj,
                        &project,
                        goal_id,
                        command,
                        body.client_request_id
                            .as_ref()
                            .map(|id| format!("{}.{}", id, idx + 1)),
                        Some("command".to_string()),
                        None,
                        None,
                        body.reason.clone(),
                        max_runtime_secs,
                    )
                };
                match result {
                    Ok(job) => jobs.push(job),
                    Err(e) => {
                        res.render(Json(job_response(&op, false, Some(e))));
                        return;
                    }
                }
            }
            let job_ids = jobs.iter().map(|j| j.job_id.clone()).collect::<Vec<_>>();
            res.render(Json(JobOpResponse {
                success: true,
                op,
                job_id: job_ids.first().cloned(),
                job_ids,
                job: jobs.first().cloned(),
                jobs,
                stdout_tail: None,
                stderr_tail: None,
                summary_markdown: None,
                error: None,
                log_total_lines: None,
                next_cursor: None,
                metadata_only: None,
                logs_included: None,
                warnings: Vec::new(),
            }));
        }
        "list" => {
            let limit = body.limit.clamp(1, 100);
            let status_filter = body.status.as_deref();
            let mut jobs = if proj.is_ssh() {
                list_ssh_jobs(proj, limit, status_filter, ssh_config)
            } else {
                list_local_jobs(&proj.root(), limit, status_filter)
            };
            if let Some(goal_id) = body.goal_id.as_deref() {
                jobs.retain(|j| j.goal_id == goal_id);
            }
            if let Some(client_request_id) = client_request_id {
                jobs.retain(|j| {
                    j.client_request_id
                        .as_deref()
                        .map(|id| {
                            id == client_request_id
                                || id.starts_with(&format!("{}.", client_request_id))
                        })
                        .unwrap_or(false)
                });
            }
            let job_ids = jobs.iter().map(|j| j.job_id.clone()).collect::<Vec<_>>();
            res.render(Json(JobOpResponse {
                success: true,
                op,
                job_id: job_ids.first().cloned(),
                job_ids,
                job: jobs.first().cloned(),
                jobs,
                stdout_tail: None,
                stderr_tail: None,
                summary_markdown: None,
                error: None,
                log_total_lines: None,
                next_cursor: None,
                metadata_only: None,
                logs_included: None,
                warnings: Vec::new(),
            }));
        }
        "status" => {
            let job_id_owned;
            let job_id = if let Some(job_id) = body.job_id.as_deref() {
                job_id
            } else if let Some(client_request_id) = client_request_id {
                let mut jobs = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                if let Some(goal_id) = body.goal_id.as_deref() {
                    jobs.retain(|j| j.goal_id == goal_id);
                }
                jobs.retain(|j| j.client_request_id.as_deref() == Some(client_request_id));
                jobs.sort_by_key(|j| -j.created_at);
                let Some(job) = jobs.first() else {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("job not found for client_request_id".to_string()),
                    )));
                    return;
                };
                job_id_owned = job.job_id.clone();
                &job_id_owned
            } else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("job_id or client_request_id is required".to_string()),
                )));
                return;
            };

            // Determine detail level:
            // - Default (None) → "basic" (lightweight, no OOM, no logs)
            // - Explicit "basic" → same as default
            // - Explicit "logs" → basic + include log tails in response
            // tail_lines does NOT implicitly trigger logs; only explicit detail=logs reads logs.
            let effective_detail = effective_status_detail(body.detail.as_deref());

            let job_result = if proj.is_ssh() {
                if effective_detail == "basic" {
                    ssh_job_info_basic(proj, job_id, ssh_config)
                } else {
                    // detail=logs: use basic status + read log tails
                    ssh_job_info_basic(proj, job_id, ssh_config)
                }
            } else if effective_detail == "basic" {
                local_job_info_basic(&proj.root(), job_id)
            } else {
                // detail=logs: use basic status + read log tails
                local_job_info_basic(&proj.root(), job_id)
            };

            match job_result {
                Ok(job) => {
                    if effective_detail == "logs" {
                        // Read log tails
                        let tail_lines = body.tail_lines.clamp(1, 1000);
                        let (stdout_tail, stderr_tail, log_total_lines) = if proj.is_ssh() {
                            match ssh_job_log_with_count(
                                proj,
                                job_id,
                                tail_lines,
                                body.since_line,
                                ssh_config,
                            ) {
                                Ok((out, err, total)) => (Some(out), Some(err), Some(total)),
                                Err(_) => (None, None, None),
                            }
                        } else {
                            match local_job_log(&proj.root(), job_id, tail_lines, body.since_line) {
                                Ok((out, err, total)) => (Some(out), Some(err), Some(total)),
                                Err(_) => (None, None, None),
                            }
                        };
                        let next_cursor =
                            log_total_lines.and_then(|t| if t > 0 { Some(t + 1) } else { None });
                        res.render(Json(JobOpResponse {
                            success: true,
                            op,
                            job_id: Some(job.job_id.clone()),
                            job_ids: vec![job.job_id.clone()],
                            job: Some(job),
                            jobs: Vec::new(),
                            stdout_tail,
                            stderr_tail,
                            summary_markdown: None,
                            error: None,
                            log_total_lines,
                            next_cursor,
                            metadata_only: Some(false),
                            logs_included: Some(true),
                            warnings: Vec::new(),
                        }))
                    } else {
                        // basic: no logs
                        res.render(Json(JobOpResponse {
                            success: true,
                            op,
                            job_id: Some(job.job_id.clone()),
                            job_ids: vec![job.job_id.clone()],
                            job: Some(job),
                            jobs: Vec::new(),
                            stdout_tail: None,
                            stderr_tail: None,
                            summary_markdown: None,
                            error: None,
                            log_total_lines: None,
                            next_cursor: None,
                            metadata_only: Some(false),
                            logs_included: Some(false),
                            warnings: Vec::new(),
                        }))
                    }
                }
                Err(e) => res.render(Json(job_response(&op, false, Some(e)))),
            }
        }
        "log" => {
            let job_id_owned;
            let job_id = if let Some(job_id) = body.job_id.as_deref() {
                job_id
            } else if let Some(client_request_id) = client_request_id {
                let mut jobs = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                if let Some(goal_id) = body.goal_id.as_deref() {
                    jobs.retain(|j| j.goal_id == goal_id);
                }
                jobs.retain(|j| j.client_request_id.as_deref() == Some(client_request_id));
                jobs.sort_by_key(|j| -j.created_at);
                let Some(job) = jobs.first() else {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("job not found for client_request_id".to_string()),
                    )));
                    return;
                };
                job_id_owned = job.job_id.clone();
                &job_id_owned
            } else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("job_id or client_request_id is required".to_string()),
                )));
                return;
            };
            let tail_lines = body.tail_lines.clamp(1, 1000);
            let since_line = body.since_line;
            let result: Result<(String, String, usize), String> = if proj.is_ssh() {
                ssh_job_log_with_count(proj, job_id, tail_lines, since_line, ssh_config)
            } else {
                local_job_log(&proj.root(), job_id, tail_lines, since_line)
            };
            match result {
                Ok((stdout_tail, stderr_tail, total_lines)) => {
                    // next_cursor points to the line after the last returned line
                    let next_cursor = if total_lines > 0 {
                        Some(total_lines + 1)
                    } else {
                        None
                    };
                    res.render(Json(JobOpResponse {
                        success: true,
                        op,
                        job_id: Some(job_id.to_string()),
                        job_ids: vec![job_id.to_string()],
                        job: None,
                        jobs: Vec::new(),
                        stdout_tail: Some(stdout_tail),
                        stderr_tail: Some(stderr_tail),
                        summary_markdown: None,
                        error: None,
                        log_total_lines: Some(total_lines),
                        next_cursor,
                        metadata_only: None,
                        logs_included: None,
                        warnings: Vec::new(),
                    }))
                }
                Err(e) => res.render(Json(job_response(&op, false, Some(e)))),
            }
        }
        "stop" => {
            let job_id_owned;
            let job_id = if let Some(job_id) = body.job_id.as_deref() {
                job_id
            } else if let Some(client_request_id) = client_request_id {
                let mut jobs = if proj.is_ssh() {
                    list_ssh_jobs(proj, 100, None, ssh_config)
                } else {
                    list_local_jobs(&proj.root(), 100, None)
                };
                if let Some(goal_id) = body.goal_id.as_deref() {
                    jobs.retain(|j| j.goal_id == goal_id);
                }
                jobs.retain(|j| j.client_request_id.as_deref() == Some(client_request_id));
                jobs.sort_by_key(|j| -j.created_at);
                let Some(job) = jobs.first() else {
                    res.status_code(StatusCode::NOT_FOUND);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("job not found for client_request_id".to_string()),
                    )));
                    return;
                };
                job_id_owned = job.job_id.clone();
                &job_id_owned
            } else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(job_response(
                    &op,
                    false,
                    Some("job_id or client_request_id is required".to_string()),
                )));
                return;
            };
            let result = if proj.is_ssh() {
                stop_ssh_job(proj, job_id, ssh_config)
            } else {
                stop_local_job(&proj.root(), job_id)
            };
            match result {
                Ok(job) => res.render(Json(JobOpResponse {
                    success: true,
                    op,
                    job_id: Some(job.job_id.clone()),
                    job_ids: vec![job.job_id.clone()],
                    job: Some(job),
                    jobs: Vec::new(),
                    stdout_tail: None,
                    stderr_tail: None,
                    summary_markdown: None,
                    error: None,
                    log_total_lines: None,
                    next_cursor: None,
                    metadata_only: None,
                    logs_included: None,
                    warnings: Vec::new(),
                })),
                Err(e) => res.render(Json(job_response(&op, false, Some(e)))),
            }
        }
        "summarize" => {
            let limit = body.limit.clamp(1, 100);
            let mut jobs = if proj.is_ssh() {
                list_ssh_jobs(proj, limit, body.status.as_deref(), ssh_config)
            } else {
                list_local_jobs(&proj.root(), limit, body.status.as_deref())
            };
            if let Some(goal_id) = body.goal_id.as_deref() {
                jobs.retain(|j| j.goal_id == goal_id);
            }
            if let Some(client_request_id) = client_request_id {
                jobs.retain(|j| {
                    j.client_request_id
                        .as_deref()
                        .map(|id| {
                            id == client_request_id
                                || id.starts_with(&format!("{}.", client_request_id))
                        })
                        .unwrap_or(false)
                });
            }
            let mut tails = Vec::new();
            for job in &jobs {
                let pair: (String, String) = if proj.is_ssh() {
                    ssh_job_log(
                        proj,
                        &job.job_id,
                        body.tail_lines.clamp(1, 1000),
                        ssh_config,
                    )
                    .unwrap_or_default()
                } else {
                    local_job_log(
                        &proj.root(),
                        &job.job_id,
                        body.tail_lines.clamp(1, 1000),
                        None,
                    )
                    .map(|(out, err, _)| (out, err))
                    .unwrap_or_default()
                };
                tails.push(pair);
            }
            let summary = summarize_jobs_markdown(&jobs, &tails);
            let job_ids = jobs.iter().map(|j| j.job_id.clone()).collect::<Vec<_>>();
            res.render(Json(JobOpResponse {
                success: true,
                op,
                job_id: job_ids.first().cloned(),
                job_ids,
                job: jobs.first().cloned(),
                jobs,
                stdout_tail: None,
                stderr_tail: None,
                summary_markdown: Some(summary),
                error: None,
                log_total_lines: None,
                next_cursor: None,
                metadata_only: None,
                logs_included: None,
                warnings: Vec::new(),
            }));
        }
        "recover" => {
            // Metadata-only recovery: no log reading, no process checks, no OOM detection.
            // Priority: job_id takes precedence over client_request_id when both provided.
            let job_id_owned;
            let resolved_job_id = if let Some(jid) = body.job_id.as_deref() {
                jid
            } else if let Some(crid) = client_request_id {
                let found = if proj.is_ssh() {
                    find_ssh_job_id_by_client_request_id(
                        proj,
                        crid,
                        body.goal_id.as_deref(),
                        ssh_config,
                    )
                } else {
                    find_local_job_id_by_client_request_id(
                        &proj.root(),
                        crid,
                        body.goal_id.as_deref(),
                    )
                };
                match found {
                    Some(id) => {
                        job_id_owned = id;
                        &job_id_owned
                    }
                    None => {
                        res.status_code(StatusCode::NOT_FOUND);
                        res.render(Json(JobOpResponse {
                            success: false,
                            op: op.clone(),
                            job_id: None,
                            job_ids: Vec::new(),
                            job: None,
                            jobs: Vec::new(),
                            stdout_tail: None,
                            stderr_tail: None,
                            summary_markdown: None,
                            error: Some("job not found for client_request_id".to_string()),
                            log_total_lines: None,
                            next_cursor: None,
                            metadata_only: Some(true),
                            logs_included: None,
                            warnings: Vec::new(),
                        }));
                        return;
                    }
                }
            } else {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(JobOpResponse {
                    success: false,
                    op: op.clone(),
                    job_id: None,
                    job_ids: Vec::new(),
                    job: None,
                    jobs: Vec::new(),
                    stdout_tail: None,
                    stderr_tail: None,
                    summary_markdown: None,
                    error: Some("job_id or client_request_id is required for recover".to_string()),
                    log_total_lines: None,
                    next_cursor: None,
                    metadata_only: None,
                    logs_included: None,
                    warnings: Vec::new(),
                }));
                return;
            };
            let result = if proj.is_ssh() {
                recover_ssh_job_info(proj, resolved_job_id, ssh_config)
            } else {
                recover_local_job_info(&proj.root(), resolved_job_id)
            };
            match result {
                Ok(job) => res.render(Json(JobOpResponse {
                    success: true,
                    op,
                    job_id: Some(job.job_id.clone()),
                    job_ids: vec![job.job_id.clone()],
                    job: Some(job),
                    jobs: Vec::new(),
                    stdout_tail: None,
                    stderr_tail: None,
                    summary_markdown: None,
                    error: None,
                    log_total_lines: None,
                    next_cursor: None,
                    metadata_only: Some(true),
                    logs_included: Some(false),
                    warnings: Vec::new(),
                })),
                Err(e) => res.render(Json(JobOpResponse {
                    success: false,
                    op,
                    job_id: None,
                    job_ids: Vec::new(),
                    job: None,
                    jobs: Vec::new(),
                    stdout_tail: None,
                    stderr_tail: None,
                    summary_markdown: None,
                    error: Some(e),
                    log_total_lines: None,
                    next_cursor: None,
                    metadata_only: Some(true),
                    logs_included: None,
                    warnings: Vec::new(),
                })),
            }
        }
        _ => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(job_response(
                &op,
                false,
                Some("unsupported job op".to_string()),
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a minimal local job directory for testing.
    fn create_test_job_dir(root: &Path, job_id: &str, client_request_id: Option<&str>) -> PathBuf {
        let dir = local_job_dir(root, job_id);
        fs::create_dir_all(&dir).unwrap();
        let now = chrono::Utc::now().timestamp();
        let meta = JobMetadata {
            job_id: job_id.to_string(),
            client_request_id: client_request_id.map(|s| s.to_string()),
            project: "testproj".to_string(),
            goal_id: "goal-1".to_string(),
            command: "echo hello".to_string(),
            kind: Some("command".to_string()),
            suite: None,
            script_path: None,
            reason: None,
            status: "completed".to_string(),
            created_at: now - 100,
            started_at: Some(now - 100),
            finished_at: Some(now - 50),
            max_runtime_secs: 3600,
            executor: "local".to_string(),
            host: None,
            path: root.to_string_lossy().to_string(),
        };
        fs::write(
            dir.join("metadata.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        fs::write(dir.join("status"), "completed").unwrap();
        fs::write(dir.join("exit_code"), "0").unwrap();
        fs::write(dir.join("pid"), "12345").unwrap();
        fs::write(dir.join("finished_at"), (now - 50).to_string()).unwrap();
        fs::write(dir.join("stdout.log"), "line1\nline2\nline3\n").unwrap();
        fs::write(dir.join("stderr.log"), "some error output\n").unwrap();
        dir
    }

    #[test]
    fn recover_local_job_reads_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let job_id = "test-recover-job-1";
        create_test_job_dir(root, job_id, Some("crid-001"));

        let info = recover_local_job_info(root, job_id).unwrap();
        assert_eq!(info.job_id, job_id);
        assert_eq!(info.client_request_id, Some("crid-001".to_string()));
        assert_eq!(info.status, "completed");
        assert_eq!(info.exit_code, Some(0));
        // recover does NOT read logs, so oom_hint is None
        assert!(info.oom_hint.is_none());
    }

    #[test]
    fn recover_local_job_does_not_read_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let job_id = "test-recover-no-logs";
        let dir = create_test_job_dir(root, job_id, None);

        // Write OOM-like content to stderr - recover should NOT detect it
        fs::write(dir.join("stderr.log"), "Out of memory\nKilled\n").unwrap();

        let info = recover_local_job_info(root, job_id).unwrap();
        // recover returns None for oom_hint (no OOM detection)
        assert!(info.oom_hint.is_none());
    }

    #[test]
    fn recover_missing_job_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let result = recover_local_job_info(root, "nonexistent-job");
        assert!(result.is_err());
    }

    #[test]
    fn find_local_job_by_client_request_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".codex/jobs")).unwrap();

        create_test_job_dir(root, "job-a", Some("crid-match"));
        create_test_job_dir(root, "job-b", Some("crid-other"));
        create_test_job_dir(root, "job-c", None);

        let found = find_local_job_id_by_client_request_id(root, "crid-match", None);
        assert_eq!(found, Some("job-a".to_string()));

        let found = find_local_job_id_by_client_request_id(root, "crid-other", None);
        assert_eq!(found, Some("job-b".to_string()));

        let not_found = find_local_job_id_by_client_request_id(root, "crid-missing", None);
        assert!(not_found.is_none());
    }

    #[test]
    fn find_local_job_by_client_request_id_with_goal_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".codex/jobs")).unwrap();

        create_test_job_dir(root, "job-x", Some("crid-goal"));

        let found = find_local_job_id_by_client_request_id(root, "crid-goal", Some("goal-1"));
        assert_eq!(found, Some("job-x".to_string()));

        let not_found =
            find_local_job_id_by_client_request_id(root, "crid-goal", Some("wrong-goal"));
        assert!(not_found.is_none());
    }

    #[test]
    fn local_job_info_basic_skips_oom_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let job_id = "test-basic-no-oom";
        let dir = create_test_job_dir(root, job_id, None);

        // Write OOM-like content to stderr - basic should NOT detect it
        fs::write(dir.join("stderr.log"), "CUDA out of memory\n").unwrap();
        // Make sure status shows the job is no longer running
        fs::write(dir.join("status"), "completed").unwrap();
        fs::write(dir.join("exit_code"), "1").unwrap();

        let info = local_job_info_basic(root, job_id).unwrap();
        // basic detail returns None for oom_hint (no OOM detection)
        assert!(info.oom_hint.is_none());
    }

    #[test]
    fn local_job_info_basic_returns_status_and_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let job_id = "test-basic-status";
        create_test_job_dir(root, job_id, None);

        let info = local_job_info_basic(root, job_id).unwrap();
        assert_eq!(info.status, "completed");
        assert_eq!(info.exit_code, Some(0));
    }

    #[test]
    fn local_job_log_reads_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let job_id = "test-log-read";
        let dir = create_test_job_dir(root, job_id, None);

        fs::write(dir.join("stdout.log"), "line1\nline2\nline3\n").unwrap();
        fs::write(dir.join("stderr.log"), "err1\nerr2\n").unwrap();

        let (stdout, stderr, total) = local_job_log(root, job_id, 10, None).unwrap();
        assert!(stdout.contains("line1"));
        assert!(stderr.contains("err1"));
        assert_eq!(total, 3);
    }

    #[test]
    fn openapi_schema_includes_recover_and_detail() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../../data/openapi.json")).unwrap();

        // Check "recover" is in op enum
        let op_enum: Vec<String> = spec["components"]["schemas"]["JobOpRequest"]["properties"]
            ["op"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            op_enum.contains(&"recover".to_string()),
            "op enum should contain 'recover', got: {:?}",
            op_enum
        );

        // Check "detail" field exists
        let detail = &spec["components"]["schemas"]["JobOpRequest"]["properties"]["detail"];
        assert!(
            !detail.is_null(),
            "detail field should exist in JobOpRequest"
        );

        // Check "metadata_only" and "logs_included" in JobOpResponse
        let resp_props = &spec["components"]["schemas"]["JobOpResponse"]["properties"];
        assert!(
            !resp_props["metadata_only"].is_null(),
            "metadata_only should exist in JobOpResponse"
        );
        assert!(
            !resp_props["logs_included"].is_null(),
            "logs_included should exist in JobOpResponse"
        );
        assert!(
            !resp_props["warnings"].is_null(),
            "warnings should exist in JobOpResponse"
        );
    }

    #[test]
    fn openapi_no_auto_upgrade_text() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../../data/openapi.json")).unwrap();

        // detail description must NOT contain "auto-upgrades"
        let detail_desc = spec["components"]["schemas"]["JobOpRequest"]["properties"]["detail"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(
            !detail_desc.contains("auto-upgrades"),
            "detail description should not contain 'auto-upgrades', got: {}",
            detail_desc
        );

        // detail description must contain "basic" and "logs"
        assert!(
            detail_desc.contains("basic"),
            "detail description should mention 'basic', got: {}",
            detail_desc
        );
        assert!(
            detail_desc.contains("logs"),
            "detail description should mention 'logs', got: {}",
            detail_desc
        );

        // runJobOp description must mention "recover"
        let job_desc = spec["paths"]["/api/codex/job"]["post"]["description"]
            .as_str()
            .unwrap();
        assert!(
            job_desc.contains("recover"),
            "runJobOp description should mention 'recover', got: {}",
            job_desc
        );
        assert!(
            job_desc.contains("detail=basic"),
            "runJobOp description should mention 'detail=basic', got: {}",
            job_desc
        );
        assert!(
            job_desc.contains("detail=logs"),
            "runJobOp description should mention 'detail=logs', got: {}",
            job_desc
        );
    }

    #[test]
    fn old_status_request_without_detail_deserializes() {
        // Simulate an old client sending a request without the detail field
        let request: JobOpRequest = serde_json::from_str(
            r#"{
                "op": "status",
                "project": "myproj",
                "job_id": "abc-123",
                "tail_lines": 80
            }"#,
        )
        .unwrap();
        assert_eq!(request.op, "status");
        assert!(request.detail.is_none());
        assert_eq!(request.tail_lines, 80);
    }

    #[test]
    fn effective_status_detail_default_is_basic() {
        assert_eq!(effective_status_detail(None), "basic");
    }

    #[test]
    fn effective_status_detail_basic_is_basic() {
        assert_eq!(effective_status_detail(Some("basic")), "basic");
    }

    #[test]
    fn effective_status_detail_logs_is_logs() {
        assert_eq!(effective_status_detail(Some("logs")), "logs");
    }

    #[test]
    fn effective_status_detail_unknown_is_basic() {
        assert_eq!(effective_status_detail(Some("anything_else")), "basic");
    }

    #[test]
    fn tail_lines_does_not_trigger_logs() {
        // Even with tail_lines > 0, detail=None must resolve to "basic"
        let request: JobOpRequest = serde_json::from_str(
            r#"{
                "op": "status",
                "project": "myproj",
                "job_id": "abc-123",
                "tail_lines": 80
            }"#,
        )
        .unwrap();
        assert_eq!(request.tail_lines, 80);
        assert!(request.detail.is_none());
        // effective_status_detail must return "basic" even though tail_lines > 0
        assert_eq!(effective_status_detail(request.detail.as_deref()), "basic");
    }
}
