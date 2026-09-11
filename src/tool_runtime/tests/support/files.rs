use crate::tool_runtime::git::{
    collect_show_changes_untracked_previews_for_root, git_log_command, parse_show_changes_output,
    show_changes_command, split_show_changes_stdout,
};
use crate::tool_runtime::helpers::run_command_sync;
use crate::tool_runtime::{
    ApplyFileChangeInput, ApplyFileChangeKind, ApplyTextEditInput, ApplyTextEditKind,
};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub(in crate::tool_runtime::tests) fn init_git_repo(root: &Path) {
    for cmd in [
        "git init",
        "git config user.email webcodex-test@example.com",
        "git config user.name 'WebCodex Test'",
        "git config core.autocrlf false",
        "git config core.longpaths true",
    ] {
        let (exit_code, stdout, stderr, _) = run_command_sync(cmd, root, 30);
        assert_eq!(
            exit_code, 0,
            "git setup command failed: {cmd}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

pub(in crate::tool_runtime::tests) fn commit_file(
    root: &Path,
    path: &str,
    content: &str,
    subject: &str,
) {
    fs::write(root.join(path), content).unwrap();
    let add = std::process::Command::new("git")
        .args(["add", "--"])
        .arg(path)
        .current_dir(root)
        .output()
        .expect("run git add fixture command");
    assert!(
        add.status.success(),
        "git add fixture command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );

    let message_path = root.join(".git").join("webcodex-test-commit-message");
    fs::write(&message_path, subject).unwrap();
    let commit = std::process::Command::new("git")
        .args(["commit", "-F"])
        .arg(&message_path)
        .current_dir(root)
        .output()
        .expect("run git commit fixture command");
    let _ = fs::remove_file(&message_path);
    assert!(
        commit.status.success(),
        "git commit fixture command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&commit.stdout),
        String::from_utf8_lossy(&commit.stderr)
    );
}

pub(in crate::tool_runtime::tests) fn git_log_stdout(
    root: &Path,
    limit: usize,
    skip: usize,
) -> String {
    let command = git_log_command(limit, skip);
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, root, 30);
    assert_eq!(
        exit_code, 0,
        "git log helper command failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

pub(in crate::tool_runtime::tests) fn show_changes_output_from_command(
    root: &Path,
    include_diff: bool,
) -> Value {
    let command = show_changes_command(include_diff, 20, 80);
    let (exit_code, stdout, stderr, _) = run_command_sync(&command, root, 30);
    assert_eq!(
        exit_code, 0,
        "show_changes command failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let frames = split_show_changes_stdout(&stdout, include_diff);
    let mut output = parse_show_changes_output(
        "demo",
        &frames.status,
        &frames.head,
        &frames.stat,
        include_diff.then_some(frames.diff.as_str()),
        20,
        80,
        Some(exit_code),
        &stderr,
    );
    if include_diff {
        let untracked_paths = output["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|file| file["status"] == "untracked")
            .filter_map(|file| file["path"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let (previews, truncated) =
            collect_show_changes_untracked_previews_for_root(root, &untracked_paths);
        output["untracked_previews"] = json!(previews);
        output["untracked_previews_truncated"] = json!(truncated);
    }
    output
}

/// A small patch carrying a distinctive marker line so tests can prove the
/// patch body never leaks into the shell `command` string.
pub(in crate::tool_runtime::tests) fn marker_patch(filename: &str, marker: &str) -> String {
    format!(
        "diff --git a/{f} b/{f}\nnew file mode 100644\n--- /dev/null\n+++ b/{f}\n\
             @@ -0,0 +1 @@\n+{m}\n",
        f = filename,
        m = marker,
    )
}

/// A patch deliberately larger than the model-authored raw shell command
/// limit so tests can prove the patch still validates/applies via `stdin`
/// rather than the command string.
pub(in crate::tool_runtime::tests) fn large_marker_patch(filename: &str, marker: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "diff --git a/{f} b/{f}\nnew file mode 100644\n--- /dev/null\n+++ b/{f}\n\
             @@ -0,0 +1,300 @@\n",
        f = filename,
    ));
    s.push_str(&format!("+{m}\n", m = marker));
    for i in 0..299 {
        s.push_str(&format!("+line-{:04}-{}\n", i, "x".repeat(48)));
    }
    s
}

pub(in crate::tool_runtime::tests) fn text_edit(
    kind: ApplyTextEditKind,
    old_text: Option<&str>,
    new_text: Option<&str>,
    anchor_text: Option<&str>,
) -> ApplyTextEditInput {
    ApplyTextEditInput {
        kind,
        old_text: old_text.map(str::to_string),
        new_text: new_text.map(str::to_string),
        anchor_text: anchor_text.map(str::to_string),
        occurrence: None,
        line_scope: None,
    }
}

pub(in crate::tool_runtime::tests) fn edit_change(
    path: &str,
    expected_sha256: &str,
    edits: Vec<ApplyTextEditInput>,
) -> ApplyFileChangeInput {
    ApplyFileChangeInput {
        kind: ApplyFileChangeKind::Edit,
        path: path.to_string(),
        to_path: None,
        content: None,
        edits,
        expected_sha256: Some(expected_sha256.to_string()),
    }
}
