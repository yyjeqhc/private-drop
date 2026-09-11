//! Runtime dispatch adapters for file, artifact, and text-edit tool calls.

use super::files::SearchRequest;
use super::project_resolution::{ProjectResolverError, ResolvedProject};
use super::{sessions::SessionTransport, ToolCall, ToolResult, ToolRuntime};
use crate::auth::AuthContext;

impl ToolRuntime {
    pub(crate) async fn dispatch_file_tool(
        &self,
        call: ToolCall,
        transport: SessionTransport,
        project_resolution: Option<Result<ResolvedProject, ProjectResolverError>>,
        auth: Option<&AuthContext>,
    ) -> ToolResult {
        match call {
            ToolCall::DeleteProjectFiles {
                project,
                paths,
                session_id: _,
            } => self.delete_project_files(project, paths).await,
            ToolCall::ReadFile {
                project,
                path,
                session_id: _,
                start_line,
                limit,
                with_line_numbers,
            } => match project_resolution {
                Some(Ok(resolved)) => {
                    self.read_file_resolved(&resolved, path, start_line, limit, with_line_numbers)
                        .await
                }
                Some(Err(error)) => error.into_tool_result(),
                None => {
                    self.read_file(project, path, start_line, limit, with_line_numbers)
                        .await
                }
            },
            ToolCall::ReadFiles {
                project,
                items,
                session_id: _,
                with_line_numbers,
                max_result_bytes: _,
            } => match project_resolution {
                Some(Ok(resolved)) => {
                    self.read_files_resolved(&resolved, items, with_line_numbers)
                        .await
                }
                Some(Err(error)) => error.into_tool_result(),
                None => self.read_files(project, items, with_line_numbers).await,
            },
            ToolCall::ListProjectFiles {
                project,
                session_id: _,
                path,
                limit,
                offset,
            } => self.list_project_files(project, path, limit, offset).await,
            ToolCall::ListProjectTrackedFiles {
                project,
                session_id: _,
                path,
                globs,
                depth,
                limit,
                offset,
            } => {
                self.list_project_tracked_files(project, path, globs, depth, limit, offset)
                    .await
            }
            ToolCall::ProjectOverview {
                project,
                session_id: _,
                path,
                max_depth,
                limit,
            } => self.project_overview(project, path, max_depth, limit).await,
            ToolCall::SearchProjectText {
                project,
                pattern,
                pattern_mode,
                session_id: _,
                path,
                limit,
                context_before,
                context_after,
                include_globs,
                exclude_globs,
                result_mode,
                timeout_secs,
            } => match project_resolution {
                Some(Ok(resolved)) => {
                    self.search_project_text_resolved(
                        &resolved,
                        &project,
                        SearchRequest {
                            pattern,
                            path,
                            limit,
                            context_before,
                            context_after,
                            include_globs,
                            exclude_globs,
                            result_mode,
                            timeout_secs,
                        },
                        pattern_mode,
                    )
                    .await
                }
                Some(Err(error)) => error.into_tool_result(),
                None => {
                    self.search_project_text(
                        project,
                        pattern,
                        pattern_mode,
                        path,
                        limit,
                        context_before,
                        context_after,
                        include_globs,
                        exclude_globs,
                        result_mode,
                        timeout_secs,
                    )
                    .await
                }
            },
            ToolCall::SearchProjectTexts {
                project,
                queries,
                session_id: _,
                max_result_bytes: _,
            } => match project_resolution {
                Some(Ok(resolved)) => self.search_project_texts_resolved(&resolved, queries).await,
                Some(Err(error)) => error.into_tool_result(),
                None => self.search_project_texts(project, queries).await,
            },
            ToolCall::WriteProjectFile {
                project,
                path,
                content,
                session_id: _,
                overwrite,
                expected_sha256,
            } => {
                self.write_project_file(project, path, content, overwrite, expected_sha256)
                    .await
            }
            ToolCall::SaveProjectArtifact {
                project,
                path,
                content_base64,
                session_id: _,
                mime_type,
                overwrite,
            } => {
                self.save_project_artifact(project, path, content_base64, mime_type, overwrite)
                    .await
            }
            ToolCall::ExportProjectArtifact {
                project: _,
                path,
                session_id: _,
            } => {
                if !matches!(transport, SessionTransport::Mcp) {
                    ToolResult::err(
                        "export_project_artifact is MCP-only; use read_project_artifact for bounded inspection outside MCP",
                    )
                } else {
                    match project_resolution {
                        Some(Ok(resolved)) => {
                            self.export_project_artifact_metadata_resolved(&resolved, path, auth)
                                .await
                        }
                        Some(Err(error)) => error.into_tool_result(),
                        None => ToolResult::err(
                            "export_project_artifact requires an exact resolved Runner project",
                        ),
                    }
                }
            }
            ToolCall::ReadProjectArtifactMetadata {
                project,
                path,
                session_id: _,
                allow_missing,
            } => {
                self.read_project_artifact_metadata(project, path, allow_missing)
                    .await
            }
            ToolCall::ReadProjectArtifact {
                project,
                path,
                session_id: _,
                encoding,
                offset,
                length,
                as_image,
            } => {
                if as_image == Some(true) && !matches!(transport, SessionTransport::Mcp) {
                    ToolResult::err(
                        "as_image is only supported over MCP; omit it to use the existing chunked artifact response",
                    )
                } else {
                    self.read_project_artifact(project, path, encoding, offset, length, as_image)
                        .await
                }
            }
            ToolCall::ArtifactUploadBegin {
                project,
                path,
                session_id: _,
                expected_bytes,
                expected_sha256,
                mime_type,
                overwrite,
            } => {
                self.artifact_upload_begin(
                    project,
                    path,
                    expected_bytes,
                    expected_sha256,
                    mime_type,
                    overwrite,
                )
                .await
            }
            ToolCall::ArtifactUploadChunk {
                project,
                path,
                upload_id,
                offset,
                content_base64,
                session_id: _,
            } => {
                self.artifact_upload_chunk(project, path, upload_id, offset, content_base64)
                    .await
            }
            ToolCall::ArtifactUploadFinish {
                project,
                path,
                upload_id,
                session_id: _,
            } => self.artifact_upload_finish(project, path, upload_id).await,
            ToolCall::ArtifactUploadAbort {
                project,
                path,
                upload_id,
                session_id: _,
            } => self.artifact_upload_abort(project, path, upload_id).await,
            ToolCall::ApplyTextEdits {
                project,
                changes,
                dry_run,
                session_id: _,
            } => self.apply_text_edits(project, changes, dry_run).await,
            _ => unreachable!("non-file tool routed to file dispatcher"),
        }
    }
}
