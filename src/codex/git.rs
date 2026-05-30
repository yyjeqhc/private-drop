use super::shell::{shell_escape, shell_join_paths};
use super::types::{GitOperation, GitRequest};
use super::validate_edit_path;

pub(super) const MAX_GIT_PATHS: usize = 50;
pub(super) const MAX_GIT_PATH_LEN: usize = 512;

pub(super) fn validate_git_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("paths cannot be empty for this git operation".to_string());
    }
    if paths.len() > MAX_GIT_PATHS {
        return Err(format!("too many paths; maximum is {}", MAX_GIT_PATHS));
    }
    for path in paths {
        if path.chars().count() > MAX_GIT_PATH_LEN {
            return Err(format!(
                "path is too long; maximum is {} characters",
                MAX_GIT_PATH_LEN
            ));
        }
        validate_edit_path(path)?;
    }
    Ok(())
}

fn validate_git_commit_message(message: &str) -> Result<(), String> {
    let len = message.chars().count();
    if len == 0 {
        return Err("commit message cannot be empty".to_string());
    }
    if len > 200 {
        return Err("commit message is too long; maximum is 200 characters".to_string());
    }
    if message
        .chars()
        .any(|ch| ch == '\n' || ch == '\r' || ch == '\0')
    {
        return Err("commit message cannot contain newlines or NUL".to_string());
    }
    Ok(())
}

pub(super) fn git_command_for_request(body: &GitRequest) -> Result<String, String> {
    match body.operation {
        GitOperation::Status => Ok("git status --short".to_string()),
        GitOperation::Diff => {
            if body.paths.is_empty() {
                Ok("git diff".to_string())
            } else {
                validate_git_paths(&body.paths)?;
                Ok(format!("git diff -- {}", shell_join_paths(&body.paths)))
            }
        }
        GitOperation::Log => Ok("git log --oneline -n 20".to_string()),
        GitOperation::Add => {
            validate_git_paths(&body.paths)?;
            Ok(format!("git add -- {}", shell_join_paths(&body.paths)))
        }
        GitOperation::Commit => {
            validate_git_paths(&body.paths)?;
            let message = body
                .message
                .as_deref()
                .ok_or_else(|| "message is required for commit".to_string())?;
            validate_git_commit_message(message)?;
            let paths = shell_join_paths(&body.paths);
            let message = shell_escape(message);
            Ok(format!(
                "git add -- {paths} && if git diff --cached --quiet -- {paths}; then echo 'No staged changes to commit' >&2; exit 1; fi; git commit -m {message} --no-verify"
            ))
        }
        GitOperation::CommitAmendNoEdit => {
            validate_git_paths(&body.paths)?;
            let paths = shell_join_paths(&body.paths);
            Ok(format!(
                "git add -- {paths} && if git diff --cached --quiet -- {paths}; then echo 'No staged changes to amend' >&2; exit 1; fi; git commit --amend --no-edit --no-verify"
            ))
        }
    }
}
