use crate::projects::{ProjectConfig, SshConfig};
use std::time::Instant;

/// Build SSH target string [user@]host from project config.
pub(super) fn build_ssh_target(proj: &ProjectConfig) -> Result<String, String> {
    proj.ssh_target()
}

pub(super) fn ssh_option_args(config: Option<&SshConfig>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut args = Vec::new();
    if config.batch_mode || config.control_master {
        args.push("-o".to_string());
        args.push("BatchMode=yes".to_string());
    }
    if let Some(secs) = config.connect_timeout_secs {
        args.push("-o".to_string());
        args.push(format!("ConnectTimeout={secs}"));
    }
    if config.control_master {
        args.push("-o".to_string());
        args.push("ControlMaster=auto".to_string());
        if let Some(v) = &config.control_persist {
            args.push("-o".to_string());
            args.push(format!("ControlPersist={v}"));
        }
        if let Some(v) = &config.control_path {
            args.push("-o".to_string());
            args.push(format!("ControlPath={v}"));
        }
    }
    if let Some(secs) = config.server_alive_interval {
        args.push("-o".to_string());
        args.push(format!("ServerAliveInterval={secs}"));
    }
    if let Some(max) = config.server_alive_count_max {
        args.push("-o".to_string());
        args.push(format!("ServerAliveCountMax={max}"));
    }
    args
}

pub(super) fn build_ssh_command(
    ssh_target: &str,
    remote_cmd: &str,
    config: Option<&SshConfig>,
) -> std::process::Command {
    let mut command = std::process::Command::new("ssh");
    for arg in ssh_option_args(config) {
        command.arg(arg);
    }
    command.arg(ssh_target).arg("--").arg(remote_cmd);
    command
}

/// Run a command on a remote host via SSH.
/// The command is passed as separate arguments to ssh (no local shell wrapping).
/// Remote shell interprets the command string.
pub(super) fn run_ssh(
    ssh_target: &str,
    remote_cmd: &str,
    _timeout_secs: u64,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    let start = Instant::now();
    let result = build_ssh_command(ssh_target, remote_cmd, ssh_config).output();

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute SSH command: {}", e),
                elapsed,
            )
        }
    }
}

/// Run an SSH command that receives patch data via stdin.
/// Writes local patch content to a remote temp file via SSH stdin,
/// then runs the remote command with the temp file path.
pub(super) fn run_ssh_patch(
    ssh_target: &str,
    _project_path: &str,
    patch: &str,
    remote_cmd_template: &str,
    ssh_config: Option<&SshConfig>,
) -> (i32, String, String, u64) {
    let patch_id = uuid::Uuid::new_v4();
    let remote_patch = format!("/tmp/private-drop-patch-{}.diff", patch_id);
    let remote_cmd = format!(
        "cat > '{}' && {} && rm -f '{}'",
        remote_patch,
        remote_cmd_template.replace("__PATCH__", &remote_patch),
        remote_patch
    );
    let start = Instant::now();
    let result = build_ssh_command(ssh_target, &remote_cmd, ssh_config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(patch.as_bytes());
                // stdin is dropped here, closing the pipe
            }
            child.wait_with_output()
        });

    match result {
        Ok(output) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            (code, stdout, stderr, elapsed)
        }
        Err(e) => {
            let elapsed = start.elapsed().as_millis() as u64;
            (
                -1,
                String::new(),
                format!("Failed to execute SSH patch: {}", e),
                elapsed,
            )
        }
    }
}

pub(super) fn parse_ssh_batch_blocks(stdout: &str, count: usize, nonce: &str) -> Vec<String> {
    let mut blocks = vec![String::new(); count];
    let mut current: Option<usize> = None;
    let start_prefix = format!("__PDCTX_{}_START_", nonce);
    let end_prefix = format!("__PDCTX_{}_END_", nonce);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(&start_prefix) {
            if let Some(idx) = rest
                .strip_suffix("__")
                .and_then(|s| s.parse::<usize>().ok())
            {
                current = if idx < count { Some(idx) } else { None };
            }
            continue;
        }
        if line.starts_with(&end_prefix) {
            current = None;
            continue;
        }
        if let Some(idx) = current {
            blocks[idx].push_str(line);
            blocks[idx].push('\n');
        }
    }
    blocks
}
