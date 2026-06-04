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
    spec["components"]["schemas"]["EditRequest"]["properties"]["response_mode"]["description"] =
        serde_json::json!(
            "Response detail. For larger or multi-file edits, use summary to reduce timeout risk."
        );
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
    spec["components"]["schemas"]["ReportRequest"]["properties"]["channel"]["description"] =
        serde_json::json!("Report channel; not the project field.");
    res.render(Json(spec));
}

#[cfg(test)]
mod tests {
    use super::apply_edit_timeout_guidance;

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
    spec["components"]["schemas"]["ReportRequest"]["properties"]["channel"]["description"] =
        serde_json::json!("Report channel; not the project field.");
    res.render(Json(spec));
}
