use super::command_workflow::require_active_goal;
use super::get_projects;
use super::types::{job_response, JobOpRequest, JobOpResponse};
use crate::get_db;
use crate::projects::{ProjectConfig, SshConfig};
use salvo::prelude::*;

use super::run_project_cmd;
use super::security::is_sensitive_path;
use super::shell::shell_escape;
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

pub(super) fn tail_file(path: &Path, tail_lines: usize) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| tail_lines_from_text(&s, tail_lines))
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
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        reason: meta.reason.clone(),
        status,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at,
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
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
                    "kill -TERM -{} 2>/dev/null || kill {} 2>/dev/null || true; sleep 1; kill -KILL -{} 2>/dev/null || true; now=$(date +%s); printf timeout > {}/status; echo $now > {}/finished_at",
                    pid,
                    pid,
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
    JobInfo {
        job_id: meta.job_id.clone(),
        client_request_id: meta.client_request_id.clone(),
        project: meta.project.clone(),
        goal_id: meta.goal_id.clone(),
        command: meta.command.clone(),
        reason: meta.reason.clone(),
        status: status_value,
        created_at: meta.created_at,
        started_at: meta.started_at,
        finished_at: finished_at.or(meta.finished_at),
        max_runtime_secs: meta.max_runtime_secs,
        executor: meta.executor.clone(),
        pid,
        exit_code,
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
) -> Result<(String, String), String> {
    validate_job_id(job_id)?;
    let dir = local_job_dir(root, job_id);
    Ok((
        tail_file(&dir.join("stdout.log"), tail_lines).unwrap_or_default(),
        tail_file(&dir.join("stderr.log"), tail_lines).unwrap_or_default(),
    ))
}

pub(super) fn ssh_job_log(
    proj: &ProjectConfig,
    job_id: &str,
    tail_lines: usize,
    ssh_config: Option<&SshConfig>,
) -> Result<(String, String), String> {
    validate_job_id(job_id)?;
    let dir = shell_escape(&job_dir_rel(job_id));
    let n = tail_lines.clamp(1, 1000);
    let cmd = format!(
        "printf '__STDOUT__\\n'; tail -n {} {}/stdout.log 2>/dev/null || true; printf '\\n__STDERR__\\n'; tail -n {} {}/stderr.log 2>/dev/null || true",
        n, dir, n, dir
    );
    let (code, stdout, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to read job log: {}", stderr.trim()));
    }
    let mut out = String::new();
    let mut err = String::new();
    let mut section = "";
    for line in stdout.lines() {
        match line {
            "__STDOUT__" => section = "out",
            "__STDERR__" => section = "err",
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
    Ok((out.trim_end().to_string(), err.trim_end().to_string()))
}

pub(super) fn stop_local_job(root: &Path, job_id: &str) -> Result<JobInfo, String> {
    validate_job_id(job_id)?;
    let meta = read_job_metadata_local(root, job_id)?;
    let dir = local_job_dir(root, job_id);
    if let Some(pid) = read_file_to_string(&dir.join("pid")).and_then(|s| s.parse::<i64>().ok()) {
        let group = format!("-{}", pid);
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(&group)
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));
        if pid_running_local(pid) {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(&group)
                .status();
        }
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
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
        "if test -f {0}/pid; then pid=$(cat {0}/pid); kill -TERM -$pid 2>/dev/null || kill $pid 2>/dev/null || true; sleep 1; kill -KILL -$pid 2>/dev/null || true; fi; now=$(date +%s); printf stopped > {0}/status; echo $now > {0}/finished_at",
        shell_escape(&dir)
    );
    let (code, _, stderr, _) = run_project_cmd(proj, &cmd, 10, ssh_config);
    if code != 0 {
        return Err(format!("Failed to stop job: {}", stderr.trim()));
    }
    ssh_job_info(proj, job_id, ssh_config)
}

pub(super) fn summarize_jobs_markdown(jobs: &[JobInfo], log_tails: &[(String, String)]) -> String {
    let mut md = String::from("# Codex job summary\n\n| job_id | status | exit_code | duration_secs | command |\n|---|---:|---:|---:|---|\n");
    for job in jobs {
        let duration = job
            .finished_at
            .or_else(|| Some(chrono::Utc::now().timestamp()))
            .unwrap_or(job.created_at)
            - job.started_at.unwrap_or(job.created_at);
        md.push_str(&format!(
            "| `{}` | {} | {} | {} | `{}` |\n",
            job.job_id,
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
            let command = match (body.command.as_deref(), body.script_path.as_deref()) {
                (Some(_), Some(_)) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("provide either command or script_path, not both".to_string()),
                    )));
                    return;
                }
                (Some(command), None) => command.to_string(),
                (None, Some(script_path)) => {
                    match build_script_job_command(script_path, &body.script_args) {
                        Ok(command) => command,
                        Err(e) => {
                            res.status_code(StatusCode::BAD_REQUEST);
                            res.render(Json(job_response(&op, false, Some(e))));
                            return;
                        }
                    }
                }
                (None, None) => {
                    res.status_code(StatusCode::BAD_REQUEST);
                    res.render(Json(job_response(
                        &op,
                        false,
                        Some("command or script_path is required".to_string()),
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
                    body.reason.clone(),
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
                })),
                Err(e) => res.render(Json(job_response(&op, false, Some(e)))),
            }
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
            let result = if proj.is_ssh() {
                ssh_job_info(proj, job_id, ssh_config)
            } else {
                local_job_info(&proj.root(), job_id)
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
                })),
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
            let result = if proj.is_ssh() {
                ssh_job_log(proj, job_id, tail_lines, ssh_config)
            } else {
                local_job_log(&proj.root(), job_id, tail_lines)
            };
            match result {
                Ok((stdout_tail, stderr_tail)) => res.render(Json(JobOpResponse {
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
                })),
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
                let pair = if proj.is_ssh() {
                    ssh_job_log(
                        proj,
                        &job.job_id,
                        body.tail_lines.clamp(1, 1000),
                        ssh_config,
                    )
                } else {
                    local_job_log(&proj.root(), &job.job_id, body.tail_lines.clamp(1, 1000))
                }
                .unwrap_or_default();
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
            }));
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
