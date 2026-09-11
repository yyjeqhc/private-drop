//! Runtime dispatch adapters for git-oriented tool calls.

use super::{ToolCall, ToolResult, ToolRuntime};

impl ToolRuntime {
    pub(crate) async fn dispatch_git_tool(&self, call: ToolCall) -> ToolResult {
        match call {
            ToolCall::GitRestorePaths {
                project,
                paths,
                session_id: _,
            } => self.git_restore_paths(project, paths).await,
            ToolCall::DiscardUntracked {
                project,
                paths,
                session_id: _,
            } => self.discard_untracked(project, paths).await,
            ToolCall::GitCommitPaths {
                project,
                expected_head,
                paths,
                message,
                session_id: _,
            } => {
                self.git_commit_paths(project, expected_head, paths, message)
                    .await
            }
            ToolCall::GitStatus {
                project,
                session_id: _,
            } => self.git_status(project).await,
            ToolCall::GitDiff {
                project,
                session_id: _,
                args,
            } => self.git_diff(project, args).await,
            ToolCall::GitDiffHunks {
                project,
                session_id: _,
                paths,
                max_hunks,
                max_hunk_lines,
                max_page_bytes,
                cached,
                base_commit,
                head_commit,
                continuation,
            } => {
                self.git_diff_hunks_continued_with_range_and_page_bytes(
                    project,
                    paths,
                    max_hunks,
                    max_hunk_lines,
                    max_page_bytes,
                    cached,
                    base_commit,
                    head_commit,
                    continuation,
                )
                .await
            }
            ToolCall::GitLog {
                project,
                limit,
                skip,
                session_id: _,
            } => self.git_log(project, limit, skip).await,
            ToolCall::GitReviewSummary {
                project,
                base_commit,
                head_commit,
                session_id: _,
            } => {
                self.git_review_summary(project, base_commit, head_commit)
                    .await
            }
            ToolCall::GitDiffSummary {
                project,
                session_id: _,
            } => self.git_diff_summary(project).await,
            ToolCall::ShowChanges {
                project,
                session_id,
                include_diff,
                max_hunks,
                max_hunk_lines,
                session_event_limit,
            } => {
                self.show_changes(
                    project,
                    session_id,
                    include_diff,
                    max_hunks,
                    max_hunk_lines,
                    session_event_limit,
                )
                .await
            }
            _ => unreachable!("non-git tool routed to git dispatcher"),
        }
    }
}
