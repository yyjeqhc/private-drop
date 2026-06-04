use crate::json_error;
use salvo::prelude::*;

fn public_url() -> String {
    std::env::var("DROP_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:8080".to_string())
}

const PROJECT_SCHEMA_DESCRIPTION: &str = "Runtime-validated project name. Add or remove projects in projects.toml and restart the service; the OpenAPI schema intentionally does not enumerate project names.";

#[handler]
pub async fn openapi_json(res: &mut Response) {
    match serde_json::from_str::<serde_json::Value>(include_str!("../data/openapi.json")) {
        Ok(mut spec) => {
            spec["openapi"] = serde_json::Value::String("3.1.0".to_string());
            spec["servers"] = serde_json::json!([{
                "url": public_url(),
                "description": "Public server"
            }]);
            res.render(Json(spec));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Invalid OpenAPI schema: {}", e),
            ));
        }
    }
}

fn apply_project_description_to_schema(spec: &mut serde_json::Value, schema_names: &[&str]) {
    for name in schema_names {
        if let Some(project) = spec["components"]["schemas"][*name]["properties"].get_mut("project")
        {
            if let Some(obj) = project.as_object_mut() {
                obj.remove("enum");
                obj.insert(
                    "description".to_string(),
                    serde_json::json!(PROJECT_SCHEMA_DESCRIPTION),
                );
            }
        }
    }
}

fn apply_edit_timeout_guidance(spec: &mut serde_json::Value) {
    spec["paths"]["/api/codex/edit"]["post"]["description"] = serde_json::json!(
        "Apply structured file edits. For larger or multi-file edits, prefer response_mode=summary. If this times out, do not retry immediately; first check git_status or read the target file to confirm whether the edit was applied."
    );
    spec["components"]["schemas"]["EditRequest"]["properties"]["response_mode"]["description"] = serde_json::json!(
        "Response detail. For larger or multi-file edits, use summary to reduce timeout risk."
    );
}

fn apply_job_recovery_guidance(spec: &mut serde_json::Value) {
    // The description is already set in data/openapi.json; this function
    // adds the detail field description programmatically in case the static
    // schema is missing it.
    if spec["components"]["schemas"]["JobOpRequest"]["properties"]["detail"].is_null() {
        spec["components"]["schemas"]["JobOpRequest"]["properties"]["detail"] = serde_json::json!({
            "type": "string",
            "enum": ["basic", "logs"],
            "description": "For op=status: basic (default, lightweight, no logs) or logs (include stdout/stderr tails). tail_lines only affects detail=logs or op=log."
        });
    }
}

fn apply_context_batch_guidance(spec: &mut serde_json::Value) {
    // Ensure context_batch endpoint description includes batch-size guidance.
    // The static openapi.json already has the full description; this is a fallback
    // in case the static schema is missing it.
    let endpoint = &mut spec["paths"]["/api/codex/context_batch"]["post"]["description"];
    if endpoint
        .as_str()
        .map_or(true, |s| !s.contains("preflight_rejected"))
    {
        *endpoint = serde_json::json!(
            "Batch context observations for one project. For SSH projects or large reads, keep batches small: at most 8 items for SSH, max_total_chars <=80000. If rejected as too large (preflight_rejected=true), split by file/section instead of retrying the same request. git_diff and agent_context are heavy; avoid mixing many read_file items with them."
        );
    }
}

fn apply_trusted_command_guidance(spec: &mut serde_json::Value) {
    // Update command_request_op description to mention trusted raw mode
    let cr_op_desc = &mut spec["paths"]["/api/codex/command_request_op"]["post"]["description"];
    if cr_op_desc
        .as_str()
        .map_or(true, |s| !s.contains("create_trusted_raw"))
    {
        *cr_op_desc = serde_json::json!(
            "Command request operations with goal-based approval. Trusted raw mode: use create_trusted_raw_and_approve for short multi-line shell commands (timeout default 120s, max 1800s). For long-running scripts use runJobOp create with trusted=true + script_text. Use response_mode=summary for large output. If timeout, recover with runJobOp recover/status first."
        );
    }

    // Update job description to mention trusted script_text
    let job_desc = &mut spec["paths"]["/api/codex/job"]["post"]["description"];
    if job_desc
        .as_str()
        .map_or(true, |s| !s.contains("script_text"))
    {
        *job_desc = serde_json::json!(
            "Job operations. create: use command, script_path, or trusted=true with script_text for multi-line scripts. recover/status first if timeout. detail=basic (default) or detail=logs."
        );
    }

    // Add fields to CommandRequestOpRequest
    {
        let cr_props = &mut spec["components"]["schemas"]["CommandRequestOpRequest"]["properties"];
        if cr_props["script_text"].is_null() {
            cr_props["script_text"] = serde_json::json!({
                "type": "string",
                "description": "For create_trusted_raw: multi-line shell script content. Supports grep, python one-liners, file stats."
            });
        }
        if cr_props["timeout_secs"].is_null() {
            cr_props["timeout_secs"] = serde_json::json!({
                "type": "integer",
                "description": "For create_trusted_raw: timeout in seconds. Default 120, max 1800."
            });
        }
        if cr_props["response_mode"].is_null() {
            cr_props["response_mode"] = serde_json::json!({
                "type": "string",
                "enum": ["summary", "full", "minimal"],
                "description": "For create_trusted_raw: summary (default, tail only), full (more output, still truncated), minimal (success/exit_code/cwd only)."
            });
        }
        // Add create_trusted_raw and create_trusted_raw_and_approve to op enum
        if let Some(op_enum) = cr_props["op"]["enum"].as_array_mut() {
            let ops: Vec<String> = op_enum
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            if !ops.contains(&"create_trusted_raw".to_string()) {
                op_enum.push(serde_json::json!("create_trusted_raw"));
            }
            if !ops.contains(&"create_trusted_raw_and_approve".to_string()) {
                op_enum.push(serde_json::json!("create_trusted_raw_and_approve"));
            }
        }
    }

    // Add fields to JobOpRequest
    {
        let job_props = &mut spec["components"]["schemas"]["JobOpRequest"]["properties"];
        if job_props["script_text"].is_null() {
            job_props["script_text"] = serde_json::json!({
                "type": "string",
                "description": "For trusted job creation: multi-line script content. Requires trusted=true."
            });
        }
        if job_props["trusted"].is_null() {
            job_props["trusted"] = serde_json::json!({
                "type": "boolean",
                "description": "For trusted job creation: must be true when script_text is provided."
            });
        }
    }

    // Add trusted_result to CommandRequestOpResponse
    {
        let cr_resp_props =
            &mut spec["components"]["schemas"]["CommandRequestOpResponse"]["properties"];
        if cr_resp_props["trusted_result"].is_null() {
            cr_resp_props["trusted_result"] = serde_json::json!({
                "type": "object",
                "description": "For create_trusted_raw / create_trusted_raw_and_approve: structured execution result.",
                "properties": {
                    "exit_code": { "type": "integer" },
                    "duration_ms": { "type": "integer" },
                    "cwd": { "type": "string" },
                    "stdout_tail": { "type": "string" },
                    "stderr_tail": { "type": "string" },
                    "stdout_truncated": { "type": "boolean" },
                    "stderr_truncated": { "type": "boolean" },
                    "audit_log_path": { "type": "string" },
                    "blocked_by_denylist": { "type": "boolean" }
                }
            });
        }
    }
}

#[handler]
pub async fn codex_openapi_json(res: &mut Response) {
    let mut spec =
        match serde_json::from_str::<serde_json::Value>(include_str!("../data/openapi.json")) {
            Ok(spec) => spec,
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                ));
                return;
            }
        };
    spec["openapi"] = serde_json::json!("3.1.0");
    spec["servers"] = serde_json::json!([{ "url": public_url(), "description": "Public server" }]);
    spec["info"] = serde_json::json!({"title":"Private Drop Codex API","version":env!("CARGO_PKG_VERSION"),"description":"Codex-only project API. Message, file, and channel APIs are excluded."});
    spec["paths"] = serde_json::json!({
        "/api/codex/context": spec["paths"]["/api/codex/context"].clone(),
        "/api/codex/projects": spec["paths"]["/api/codex/projects"].clone(),
        "/api/codex/context_batch": spec["paths"]["/api/codex/context_batch"].clone(),
        "/api/codex/apply_patch": spec["paths"]["/api/codex/apply_patch"].clone(),
        "/api/codex/edit": spec["paths"]["/api/codex/edit"].clone(),
        "/api/codex/artifact": spec["paths"]["/api/codex/artifact"].clone(),
        "/api/codex/git": spec["paths"]["/api/codex/git"].clone(),
        "/api/codex/command": spec["paths"]["/api/codex/command"].clone(),
        "/api/codex/command_request": spec["paths"]["/api/codex/command_request"].clone(),
        "/api/codex/command_request_op": spec["paths"]["/api/codex/command_request_op"].clone(),
        "/api/codex/job": spec["paths"]["/api/codex/job"].clone(),
        "/api/codex/command_request_raw": spec["paths"]["/api/codex/command_request_raw"].clone(),
        "/api/codex/command_requests": spec["paths"]["/api/codex/command_requests"].clone(),
        "/api/codex/command_request_batch": spec["paths"]["/api/codex/command_request_batch"].clone(),
        "/api/codex/command_approve": spec["paths"]["/api/codex/command_approve"].clone(),
        "/api/codex/command_reject": spec["paths"]["/api/codex/command_reject"].clone(),
        "/api/codex/check": spec["paths"]["/api/codex/check"].clone(),
        "/api/codex/report": spec["paths"]["/api/codex/report"].clone()
    });
    spec["components"]["schemas"] = serde_json::json!({
        "ContextRequest": spec["components"]["schemas"]["ContextRequest"].clone(),
        "ContextResponse": spec["components"]["schemas"]["ContextResponse"].clone(),
        "ContextBatchItem": spec["components"]["schemas"]["ContextBatchItem"].clone(),
        "ContextBatchRequest": spec["components"]["schemas"]["ContextBatchRequest"].clone(),
        "ContextBatchResponse": spec["components"]["schemas"]["ContextBatchResponse"].clone(),
        "PatchRequest": spec["components"]["schemas"]["PatchRequest"].clone(),
        "PatchResponse": spec["components"]["schemas"]["PatchResponse"].clone(),
        "ReplaceTextEdit": spec["components"]["schemas"]["ReplaceTextEdit"].clone(),
        "ReplaceRangeEdit": spec["components"]["schemas"]["ReplaceRangeEdit"].clone(),
        "AppendFileEdit": spec["components"]["schemas"]["AppendFileEdit"].clone(),
        "CreateFileEdit": spec["components"]["schemas"]["CreateFileEdit"].clone(),
        "WriteFileEdit": spec["components"]["schemas"]["WriteFileEdit"].clone(),
        "CreateBinaryFileEdit": spec["components"]["schemas"]["CreateBinaryFileEdit"].clone(),
        "WriteBinaryFileEdit": spec["components"]["schemas"]["WriteBinaryFileEdit"].clone(),
        "CreateBinaryArtifactEdit": spec["components"]["schemas"]["CreateBinaryArtifactEdit"].clone(),
        "WriteBinaryArtifactEdit": spec["components"]["schemas"]["WriteBinaryArtifactEdit"].clone(),
        "CreateBinaryFileFromUploadEdit": spec["components"]["schemas"]["CreateBinaryFileFromUploadEdit"].clone(),
        "WriteBinaryFileFromUploadEdit": spec["components"]["schemas"]["WriteBinaryFileFromUploadEdit"].clone(),
        "CreateBinaryFileFromUrlEdit": spec["components"]["schemas"]["CreateBinaryFileFromUrlEdit"].clone(),
        "WriteBinaryFileFromUrlEdit": spec["components"]["schemas"]["WriteBinaryFileFromUrlEdit"].clone(),
        "EditRequest": spec["components"]["schemas"]["EditRequest"].clone(),
        "EditResponse": spec["components"]["schemas"]["EditResponse"].clone(),
        "ArtifactRequest": spec["components"]["schemas"]["ArtifactRequest"].clone(),
        "ArtifactResponse": spec["components"]["schemas"]["ArtifactResponse"].clone(),
        "GitRequest": spec["components"]["schemas"]["GitRequest"].clone(),
        "GitResponse": spec["components"]["schemas"]["GitResponse"].clone(),
        "CommandRequest": spec["components"]["schemas"]["CommandRequest"].clone(),
        "CommandResponse": spec["components"]["schemas"]["CommandResponse"].clone(),
        "CommandRequestCreate": spec["components"]["schemas"]["CommandRequestCreate"].clone(),
        "RawCommandRequestCreate": spec["components"]["schemas"]["RawCommandRequestCreate"].clone(),
        "CommandRequestBatchItem": spec["components"]["schemas"]["CommandRequestBatchItem"].clone(),
        "CommandRequestBatchCreate": spec["components"]["schemas"]["CommandRequestBatchCreate"].clone(),
        "CommandRequestsListRequest": spec["components"]["schemas"]["CommandRequestsListRequest"].clone(),
        "CommandApproveRequest": spec["components"]["schemas"]["CommandApproveRequest"].clone(),
        "CommandRejectRequest": spec["components"]["schemas"]["CommandRejectRequest"].clone(),
        "CommandRequestOpRequest": spec["components"]["schemas"]["CommandRequestOpRequest"].clone(),
        "CommandRequestOpResponse": spec["components"]["schemas"]["CommandRequestOpResponse"].clone(),
        "JobOpRequest": spec["components"]["schemas"]["JobOpRequest"].clone(),
        "JobInfo": spec["components"]["schemas"]["JobInfo"].clone(),
        "JobOpResponse": spec["components"]["schemas"]["JobOpResponse"].clone(),
        "ProjectCapabilities": spec["components"]["schemas"]["ProjectCapabilities"].clone(),
        "ProjectCapabilityInfo": spec["components"]["schemas"]["ProjectCapabilityInfo"].clone(),
        "InstanceInfo": spec["components"]["schemas"]["InstanceInfo"].clone(),
        "ProjectsResponse": spec["components"]["schemas"]["ProjectsResponse"].clone(),
        "CommandRequestResponse": spec["components"]["schemas"]["CommandRequestResponse"].clone(),
        "CommandRequestsListResponse": spec["components"]["schemas"]["CommandRequestsListResponse"].clone(),
        "CommandRequestBatchResponse": spec["components"]["schemas"]["CommandRequestBatchResponse"].clone(),
        "CheckRequest": spec["components"]["schemas"]["CheckRequest"].clone(),
        "CheckResponse": spec["components"]["schemas"]["CheckResponse"].clone(),
        "ReportRequest": spec["components"]["schemas"]["ReportRequest"].clone(),
        "ReportResponse": spec["components"]["schemas"]["ReportResponse"].clone()
    });
    apply_project_description_to_schema(
        &mut spec,
        &[
            "ContextRequest",
            "ContextBatchRequest",
            "PatchRequest",
            "EditRequest",
            "ArtifactRequest",
            "GitRequest",
            "CommandRequest",
            "CommandRequestCreate",
            "RawCommandRequestCreate",
            "CommandRequestBatchCreate",
            "CommandRequestsListRequest",
            "CommandRequestOpRequest",
            "JobOpRequest",
            "CommandApproveRequest",
            "CommandRejectRequest",
            "CheckRequest",
            "ReportRequest",
        ],
    );
    apply_edit_timeout_guidance(&mut spec);
    apply_job_recovery_guidance(&mut spec);
    apply_context_batch_guidance(&mut spec);
    apply_trusted_command_guidance(&mut spec);
    spec["components"]["schemas"]["ReportRequest"]["properties"]["channel"]["description"] =
        serde_json::json!("Report channel; not the project field.");
    res.render(Json(spec));
}

#[cfg(test)]
mod tests {
    use super::{apply_edit_timeout_guidance, apply_trusted_command_guidance};

    #[test]
    fn apply_project_edit_description_stays_under_300_chars() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        let description = spec["paths"]["/api/codex/edit"]["post"]["description"]
            .as_str()
            .unwrap();
        assert!(description.len() <= 300, "{}", description.len());
    }

    #[test]
    fn compact_schema_prefers_summary_for_large_edits() {
        let mut spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        apply_edit_timeout_guidance(&mut spec);
        let description = spec["paths"]["/api/codex/edit"]["post"]["description"]
            .as_str()
            .unwrap();
        let response_mode_description = spec["components"]["schemas"]["EditRequest"]["properties"]
            ["response_mode"]["description"]
            .as_str()
            .unwrap();
        assert!(description.len() <= 300, "{}", description.len());
        assert!(description.contains("response_mode=summary"));
        assert!(description.contains("check git_status"));
        assert!(response_mode_description.contains("multi-file edits"));
        assert!(response_mode_description.contains("use summary"));
    }

    #[test]
    fn context_batch_description_mentions_split_and_ssh() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        let description = spec["paths"]["/api/codex/context_batch"]["post"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("split") || description.contains("Split"),
            "context_batch description should mention split: {}",
            description
        );
        assert!(
            description.contains("SSH"),
            "context_batch description should mention SSH: {}",
            description
        );
        assert!(
            description.contains("preflight_rejected"),
            "context_batch description should mention preflight_rejected: {}",
            description
        );
    }

    #[test]
    fn context_batch_max_total_chars_description_mentions_limit() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        let description = spec["components"]["schemas"]["ContextBatchRequest"]["properties"]
            ["max_total_chars"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("80000"),
            "max_total_chars description should mention 80000: {}",
            description
        );
    }

    #[test]
    fn context_batch_response_has_preflight_fields() {
        let spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        let props = &spec["components"]["schemas"]["ContextBatchResponse"]["properties"];
        assert!(
            !props["preflight_rejected"].is_null(),
            "ContextBatchResponse should have preflight_rejected"
        );
        assert!(
            !props["estimated_chars"].is_null(),
            "ContextBatchResponse should have estimated_chars"
        );
        assert!(
            !props["suggestion"].is_null(),
            "ContextBatchResponse should have suggestion"
        );
        assert!(
            !props["warnings"].is_null(),
            "ContextBatchResponse should have warnings"
        );
    }

    #[test]
    fn trusted_command_guidance_adds_fields() {
        let mut spec: serde_json::Value =
            serde_json::from_str(include_str!("../data/openapi.json")).unwrap();
        apply_trusted_command_guidance(&mut spec);
        let cr_desc = spec["paths"]["/api/codex/command_request_op"]["post"]["description"]
            .as_str()
            .unwrap();
        assert!(
            cr_desc.contains("create_trusted_raw"),
            "command_request_op description should mention create_trusted_raw: {}",
            cr_desc
        );
        let cr_props = &spec["components"]["schemas"]["CommandRequestOpRequest"]["properties"];
        assert!(
            !cr_props["script_text"].is_null(),
            "script_text should be added"
        );
        assert!(
            !cr_props["timeout_secs"].is_null(),
            "timeout_secs should be added"
        );
        assert!(
            !cr_props["response_mode"].is_null(),
            "response_mode should be added"
        );
        let job_props = &spec["components"]["schemas"]["JobOpRequest"]["properties"];
        assert!(
            !job_props["script_text"].is_null(),
            "JobOpRequest script_text should be added"
        );
        assert!(
            !job_props["trusted"].is_null(),
            "JobOpRequest trusted should be added"
        );
        let resp_props = &spec["components"]["schemas"]["CommandRequestOpResponse"]["properties"];
        assert!(
            !resp_props["trusted_result"].is_null(),
            "CommandRequestOpResponse trusted_result should be added"
        );
    }
}

#[handler]
pub async fn codex_openapi_compact_json(res: &mut Response) {
    let mut spec =
        match serde_json::from_str::<serde_json::Value>(include_str!("../data/openapi.json")) {
            Ok(spec) => spec,
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                ));
                return;
            }
        };
    spec["openapi"] = serde_json::json!("3.1.0");
    spec["servers"] = serde_json::json!([{ "url": public_url(), "description": "Public server" }]);
    spec["info"] = serde_json::json!({"title":"Private Drop Compact Codex API","version":env!("CARGO_PKG_VERSION"),"description":"Compact Codex project API for GPT Actions. Uses aggregate endpoints to reduce action count."});
    spec["paths"] = serde_json::json!({
        "/api/codex/projects": spec["paths"]["/api/codex/projects"].clone(),
        "/api/codex/context_batch": spec["paths"]["/api/codex/context_batch"].clone(),
        "/api/codex/edit": spec["paths"]["/api/codex/edit"].clone(),
        "/api/codex/artifact": spec["paths"]["/api/codex/artifact"].clone(),
        "/api/codex/git": spec["paths"]["/api/codex/git"].clone(),
        "/api/codex/command": spec["paths"]["/api/codex/command"].clone(),
        "/api/codex/command_request_op": spec["paths"]["/api/codex/command_request_op"].clone(),
        "/api/codex/job": spec["paths"]["/api/codex/job"].clone(),
        "/api/codex/check": spec["paths"]["/api/codex/check"].clone(),
        "/api/codex/report": spec["paths"]["/api/codex/report"].clone(),
        "/api/desktop/task_op": spec["paths"]["/api/desktop/task_op"].clone()
    });
    spec["components"]["schemas"] = serde_json::json!({
        "ContextResponse": spec["components"]["schemas"]["ContextResponse"].clone(),
        "ContextBatchItem": spec["components"]["schemas"]["ContextBatchItem"].clone(),
        "ContextBatchRequest": spec["components"]["schemas"]["ContextBatchRequest"].clone(),
        "ContextBatchResponse": spec["components"]["schemas"]["ContextBatchResponse"].clone(),
        "ReplaceTextEdit": spec["components"]["schemas"]["ReplaceTextEdit"].clone(),
        "ReplaceRangeEdit": spec["components"]["schemas"]["ReplaceRangeEdit"].clone(),
        "AppendFileEdit": spec["components"]["schemas"]["AppendFileEdit"].clone(),
        "CreateFileEdit": spec["components"]["schemas"]["CreateFileEdit"].clone(),
        "WriteFileEdit": spec["components"]["schemas"]["WriteFileEdit"].clone(),
        "CreateBinaryFileEdit": spec["components"]["schemas"]["CreateBinaryFileEdit"].clone(),
        "WriteBinaryFileEdit": spec["components"]["schemas"]["WriteBinaryFileEdit"].clone(),
        "CreateBinaryArtifactEdit": spec["components"]["schemas"]["CreateBinaryArtifactEdit"].clone(),
        "WriteBinaryArtifactEdit": spec["components"]["schemas"]["WriteBinaryArtifactEdit"].clone(),
        "CreateBinaryFileFromUploadEdit": spec["components"]["schemas"]["CreateBinaryFileFromUploadEdit"].clone(),
        "WriteBinaryFileFromUploadEdit": spec["components"]["schemas"]["WriteBinaryFileFromUploadEdit"].clone(),
        "CreateBinaryFileFromUrlEdit": spec["components"]["schemas"]["CreateBinaryFileFromUrlEdit"].clone(),
        "WriteBinaryFileFromUrlEdit": spec["components"]["schemas"]["WriteBinaryFileFromUrlEdit"].clone(),
        "EditRequest": spec["components"]["schemas"]["EditRequest"].clone(),
        "EditResponse": spec["components"]["schemas"]["EditResponse"].clone(),
        "ArtifactRequest": spec["components"]["schemas"]["ArtifactRequest"].clone(),
        "ArtifactResponse": spec["components"]["schemas"]["ArtifactResponse"].clone(),
        "GitRequest": spec["components"]["schemas"]["GitRequest"].clone(),
        "GitResponse": spec["components"]["schemas"]["GitResponse"].clone(),
        "CommandRequest": spec["components"]["schemas"]["CommandRequest"].clone(),
        "CommandResponse": spec["components"]["schemas"]["CommandResponse"].clone(),
        "CommandRequestBatchItem": spec["components"]["schemas"]["CommandRequestBatchItem"].clone(),
        "CommandRequestOpRequest": spec["components"]["schemas"]["CommandRequestOpRequest"].clone(),
        "CommandRequestOpResponse": spec["components"]["schemas"]["CommandRequestOpResponse"].clone(),
        "JobOpRequest": spec["components"]["schemas"]["JobOpRequest"].clone(),
        "JobInfo": spec["components"]["schemas"]["JobInfo"].clone(),
        "JobOpResponse": spec["components"]["schemas"]["JobOpResponse"].clone(),
        "ProjectCapabilities": spec["components"]["schemas"]["ProjectCapabilities"].clone(),
        "ProjectCapabilityInfo": spec["components"]["schemas"]["ProjectCapabilityInfo"].clone(),
        "InstanceInfo": spec["components"]["schemas"]["InstanceInfo"].clone(),
        "ProjectsResponse": spec["components"]["schemas"]["ProjectsResponse"].clone(),
        "CheckRequest": spec["components"]["schemas"]["CheckRequest"].clone(),
        "CheckResponse": spec["components"]["schemas"]["CheckResponse"].clone(),
        "ReportRequest": spec["components"]["schemas"]["ReportRequest"].clone(),
        "ReportResponse": spec["components"]["schemas"]["ReportResponse"].clone(),
        "DesktopTask": spec["components"]["schemas"]["DesktopTask"].clone(),
        "DesktopTaskEvent": spec["components"]["schemas"]["DesktopTaskEvent"].clone(),
        "DesktopTaskOpRequest": spec["components"]["schemas"]["DesktopTaskOpRequest"].clone(),
        "DesktopTaskOpResponse": spec["components"]["schemas"]["DesktopTaskOpResponse"].clone()
    });
    apply_project_description_to_schema(
        &mut spec,
        &[
            "ContextBatchRequest",
            "EditRequest",
            "ArtifactRequest",
            "GitRequest",
            "CommandRequest",
            "CommandRequestOpRequest",
            "JobOpRequest",
            "CheckRequest",
            "ReportRequest",
        ],
    );
    apply_edit_timeout_guidance(&mut spec);
    apply_job_recovery_guidance(&mut spec);
    apply_context_batch_guidance(&mut spec);
    apply_trusted_command_guidance(&mut spec);
    spec["components"]["schemas"]["ReportRequest"]["properties"]["channel"]["description"] =
        serde_json::json!("Report channel; not the project field.");
    res.render(Json(spec));
}
