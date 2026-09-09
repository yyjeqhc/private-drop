use super::*;

async fn mcp_export_runtime(
    root: &std::path::Path,
    owner: Option<&str>,
) -> (Arc<ToolRuntime>, Arc<crate::runner_http::RunnerRegistry>) {
    use crate::runner_protocol::{RunnerCapabilities, RunnerProjectSummary, RunnerRegisterRequest};
    let registry = Arc::new(crate::runner_http::RunnerRegistry::default());
    registry
        .register(crate::test_support::current_runner_registration(
            RunnerRegisterRequest {
                process_started_at: None,
                build: None,
                job_concurrency_limit: None,
                job_inventory: None,
                coding_agent_providers: None,
                coding_agent_inventory: None,
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
                runner_protocol_generation: crate::runner_protocol::RUNNER_PROTOCOL_GENERATION_V2,
                display_name: None,
                owner: owner.map(str::to_string),
                hostname: None,
                host_context: None,
                capabilities: RunnerCapabilities::default(),
                policy: None,
            },
        ))
        .await
        .unwrap();
    crate::test_support::apply_project_inventory_snapshot(
        &registry,
        "exporter",
        "inst-export",
        vec![RunnerProjectSummary {
            id: "demo".to_string(),
            name: Some("demo".to_string()),
            path: root.to_string_lossy().into_owned(),
            allow_patch: true,
            kind: None,
            registration_source: None,
            description: None,
            hooks: Vec::new(),
            disabled: false,
            revision: None,
            git_branch: None,
            git_head: None,
            git_dirty: None,
            updated_at: chrono::Utc::now().timestamp(),
            shell_profile: None,
        }],
    )
    .await;
    let runtime = Arc::new(
        ToolRuntime::new_for_tests_with_runner_registry(registry.clone())
            .with_model_surface(ModelSurface::FullOperatorRuntime),
    );
    (runtime, registry)
}

async fn poll_mcp_export_request(
    registry: &Arc<crate::runner_http::RunnerRegistry>,
) -> crate::runner_protocol::RunnerRequest {
    use crate::runner_protocol::RunnerPollRequest;
    loop {
        if let Some(request) = registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
        {
            return request;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn complete_mcp_export_request(
    registry: &Arc<crate::runner_http::RunnerRegistry>,
    request: crate::runner_protocol::RunnerRequest,
    stdout: Value,
) {
    use crate::runner_protocol::RunnerResultRequest;
    registry
        .complete(RunnerResultRequest {
            client_id: "exporter".to_string(),
            runner_instance_id: "inst-export".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some(stdout.to_string()),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
}

fn mcp_export_optimized_chunk_range(
    request: &crate::runner_protocol::RunnerRequest,
    path: &str,
    file_bytes: usize,
) -> (usize, usize) {
    assert_eq!(request.kind, "file_read_project_artifact_export_chunk");
    let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert_eq!(payload["path"], path);
    assert_eq!(payload["expected_file_bytes"], file_bytes);
    let offset = payload["offset"].as_u64().unwrap() as usize;
    let length = payload["length"].as_u64().unwrap() as usize;
    assert!(length <= MAX_READ_PROJECT_ARTIFACT_LENGTH);
    let end = offset.saturating_add(length).min(file_bytes);
    (offset, end)
}

async fn complete_mcp_export_optimized_chunk(
    registry: &Arc<crate::runner_http::RunnerRegistry>,
    request: crate::runner_protocol::RunnerRequest,
    path: &str,
    bytes: &[u8],
) -> usize {
    let (offset, end) = mcp_export_optimized_chunk_range(&request, path, bytes.len());
    complete_mcp_export_request(
        registry,
        request,
        json!({
            "path": path,
            "file_bytes": bytes.len(),
            "offset": offset,
            "bytes_returned": end - offset,
            "content_base64": general_purpose::STANDARD.encode(&bytes[offset..end]),
            "next_offset": end,
            "truncated": end < bytes.len(),
            "eof": end == bytes.len(),
        }),
    )
    .await;
    offset
}

async fn complete_mcp_export_metadata_with_max(
    registry: Arc<crate::runner_http::RunnerRegistry>,
    path: &str,
    bytes: usize,
    sha256: &str,
    mime_type: &str,
    max_bytes: usize,
) {
    use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
    let request = loop {
        if let Some(request) = registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
        {
            break request;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert_eq!(request.kind, "file_read_project_artifact_metadata");
    let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
    assert_eq!(payload["path"], path);
    assert_eq!(payload["max_bytes"], max_bytes);
    assert_eq!(payload["allow_missing"], false);
    registry
        .complete(RunnerResultRequest {
            client_id: "exporter".to_string(),
            runner_instance_id: "inst-export".to_string(),
            request_id: request.request_id,
            exit_code: Some(0),
            stdout: Some(
                json!({
                    "path": path,
                    "bytes": bytes,
                    "sha256": sha256,
                    "mime_type": mime_type,
                })
                .to_string(),
            ),
            stderr: None,
            duration_ms: Some(1),
            error: None,
        })
        .await
        .unwrap();
}

async fn complete_mcp_export_metadata(
    registry: Arc<crate::runner_http::RunnerRegistry>,
    path: &str,
    bytes: usize,
    sha256: &str,
    mime_type: &str,
) {
    complete_mcp_export_metadata_with_max(
        registry,
        path,
        bytes,
        sha256,
        mime_type,
        MAX_PROJECT_ARTIFACT_EXPORT_BYTES,
    )
    .await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpExportChunkFault {
    None,
    InvalidBase64,
    Offset,
    Eof,
    MutateFirstChunk,
    MutateLaterChunk,
}

async fn complete_mcp_export_resource_read(
    registry: Arc<crate::runner_http::RunnerRegistry>,
    path: &str,
    bytes: Vec<u8>,
    mime_type: &str,
    sha256: &str,
    fault: McpExportChunkFault,
) {
    use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
    complete_mcp_export_metadata(registry.clone(), path, bytes.len(), sha256, mime_type).await;
    let mut expected_offset = 0usize;
    while expected_offset < bytes.len() {
        let request = loop {
            if let Some(request) = registry
                .poll(RunnerPollRequest {
                    client_id: "exporter".to_string(),
                    runner_instance_id: "inst-export".to_string(),
                })
                .await
                .unwrap()
            {
                break request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        assert_eq!(
            request.kind, "file_read_project_artifact_export_chunk",
            "optimized-capable Runner must receive the internal export chunk request"
        );
        let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
        assert_eq!(payload["path"], path);
        assert_eq!(payload["expected_file_bytes"], bytes.len());
        assert!(payload.get("max_file_bytes").is_none());
        let offset = payload["offset"].as_u64().unwrap() as usize;
        let length = payload["length"].as_u64().unwrap() as usize;
        assert_eq!(offset, expected_offset);
        assert!(length <= MAX_READ_PROJECT_ARTIFACT_LENGTH);
        let end = offset.saturating_add(length).min(bytes.len());
        let mut chunk = bytes[offset..end].to_vec();
        if (fault == McpExportChunkFault::MutateFirstChunk && offset == 0)
            || (fault == McpExportChunkFault::MutateLaterChunk && offset > 0)
        {
            if let Some(first) = chunk.first_mut() {
                *first ^= 0xff;
            }
        }
        let eof = end == bytes.len();
        let reported_offset = if fault == McpExportChunkFault::Offset && offset == 0 {
            1
        } else {
            offset
        };
        let reported_eof = if fault == McpExportChunkFault::Eof && offset == 0 {
            !eof
        } else {
            eof
        };
        let content_base64 = if fault == McpExportChunkFault::InvalidBase64 && offset == 0 {
            "***not-base64***".to_string()
        } else {
            general_purpose::STANDARD.encode(&chunk)
        };
        let stdout = json!({
            "path": path,
            "file_bytes": bytes.len(),
            "offset": reported_offset,
            "bytes_returned": chunk.len(),
            "content_base64": content_base64,
            "next_offset": end,
            "truncated": !reported_eof,
            "eof": reported_eof,
        })
        .to_string();
        registry
            .complete(RunnerResultRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: Some(stdout),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .unwrap();
        if matches!(
            fault,
            McpExportChunkFault::InvalidBase64
                | McpExportChunkFault::Offset
                | McpExportChunkFault::Eof
        ) && offset == 0
        {
            return;
        }
        expected_offset = end;
    }
}

async fn issue_mcp_artifact_export_with_metadata_max(
    runtime: Arc<ToolRuntime>,
    registry: Arc<crate::runner_http::RunnerRegistry>,
    auth: crate::auth::AuthContext,
    path: &str,
    bytes: &[u8],
    mime_type: &str,
    max_bytes: usize,
) -> Value {
    use sha2::{Digest, Sha256};
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let call = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let path = path.to_string();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "tools/call",
                    Some(json!(3101)),
                    mcp_2026_params(json!({
                        "name": "export_project_artifact",
                        "arguments": {
                            "project": "agent:exporter:demo",
                            "path": path,
                        }
                    })),
                ),
                Some(&auth),
            )
            .await
        }
    });
    complete_mcp_export_metadata_with_max(
        registry,
        path,
        bytes.len(),
        &sha256,
        mime_type,
        max_bytes,
    )
    .await;
    let outcome = call.await.unwrap();
    let McpOutcome::Ok(value) = outcome else {
        panic!("artifact export must succeed, got {outcome:?}");
    };
    value
}

async fn issue_mcp_artifact_export(
    runtime: Arc<ToolRuntime>,
    registry: Arc<crate::runner_http::RunnerRegistry>,
    auth: crate::auth::AuthContext,
    path: &str,
    bytes: &[u8],
    mime_type: &str,
) -> Value {
    issue_mcp_artifact_export_with_metadata_max(
        runtime,
        registry,
        auth,
        path,
        bytes,
        mime_type,
        MAX_PROJECT_ARTIFACT_EXPORT_BYTES,
    )
    .await
}

#[tokio::test]
async fn mcp_artifact_export_surface_is_stateless_full_operator_only() {
    let legacy = mcp_tools_list_payload_with_compact(ModelSurface::FullOperatorRuntime, false);
    assert!(!legacy["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "export_project_artifact"));

    let stateless =
        mcp_tools_list_payload_with_compact_and_app(ModelSurface::FullOperatorRuntime, false, true);
    let spec = stateless["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "export_project_artifact")
        .expect("stateless full-operator tools/list must expose artifact export");
    assert_eq!(spec["inputSchema"]["required"], json!(["project", "path"]));
    assert!(spec["inputSchema"]["properties"]
        .get("session_id")
        .is_some());
    assert!(spec["inputSchema"]["properties"]
        .get("allow_cross_project_session")
        .is_none());

    let runtime = test_runtime_with_surface(ModelSurface::FullOperatorRuntime);
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap);
    auth.is_bootstrap = true;
    let legacy_call = handle_mcp_request(
        &runtime,
        rpc(
            "tools/call",
            Some(json!(3100)),
            json!({
                "name": "export_project_artifact",
                "arguments": {"project": "agent:any:any", "path": "report.pdf"}
            }),
        ),
        Some(&auth),
    )
    .await;
    match legacy_call {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert!(value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stateless-2026"));
        }
        other => panic!("legacy artifact export must fail closed, got {other:?}"),
    }
}

#[tokio::test]
async fn adaptive_artifact_export_direct_and_gateway_preserve_protocol_and_caller_gates() {
    let runtime = test_runtime_with_surface(ModelSurface::AdaptiveRuntime);
    let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap);
    auth.is_bootstrap = true;
    for via_gateway in [false, true] {
        let arguments = json!({"project": "agent:any:any", "path": "report.pdf"});
        let params = if via_gateway {
            json!({
                "name": crate::mcp::tools::ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME,
                "arguments": {"tool": "export_project_artifact", "arguments": arguments}
            })
        } else {
            json!({"name": "export_project_artifact", "arguments": arguments})
        };
        for (stateless, expected_error) in [
            (false, "stateless-2026"),
            (true, "authenticated caller identity is unavailable"),
        ] {
            let outcome = handle_mcp_request(
                &runtime,
                rpc(
                    "tools/call",
                    Some(json!(3101)),
                    if stateless {
                        mcp_2026_params(params.clone())
                    } else {
                        params.clone()
                    },
                ),
                if stateless { None } else { Some(&auth) },
            )
            .await;
            let McpOutcome::BadRequest(value) = outcome else {
                panic!(
                    "export must reject gateway={via_gateway}, stateless={stateless}: {outcome:?}"
                );
            };
            assert_eq!(value["error"]["code"], -32602);
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected_error),
                "gateway={via_gateway}, stateless={stateless}: {value}"
            );
        }
    }
}

#[test]
fn mcp_artifact_export_oauth_binding_survives_access_token_refresh() {
    let oauth = |access_token_id: &str, client_id: &str| {
        let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
        auth.user_id = Some("user-alice".to_string());
        auth.username = Some("alice".to_string());
        auth.api_key_id = Some(access_token_id.to_string());
        auth.token_kind = Some("oauth2".to_string());
        auth.allowed_client_id = Some(client_id.to_string());
        auth.scopes = vec![crate::auth::SCOPE_PROJECT_READ.to_string()];
        auth
    };
    let first =
        mcp_artifact_export_caller_binding(Some(&oauth("wc_oat_record_1", "client-a"))).unwrap();
    let refreshed =
        mcp_artifact_export_caller_binding(Some(&oauth("wc_oat_record_2", "client-a"))).unwrap();
    let other_client =
        mcp_artifact_export_caller_binding(Some(&oauth("wc_oat_record_3", "client-b"))).unwrap();
    assert_eq!(
        first, refreshed,
        "access-token refresh must retain export identity"
    );
    assert_ne!(
        first, other_client,
        "OAuth client identity remains part of the binding"
    );
}

#[tokio::test]
async fn mcp_artifact_export_oauth_resource_read_uses_project_read_and_stable_identity() {
    use crate::runner_protocol::RunnerPollRequest;
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let oauth = |access_token_id: &str, scopes: Vec<String>| {
        let mut auth = crate::auth::AuthContext::new(crate::auth::AuthKind::OAuth2Token);
        auth.user_id = Some("user-alice".to_string());
        auth.username = Some("alice".to_string());
        auth.api_key_id = Some(access_token_id.to_string());
        auth.token_kind = Some("oauth2".to_string());
        auth.allowed_client_id = Some("client-chatgpt".to_string());
        auth.scopes = scopes;
        auth
    };
    let creator = oauth(
        "wc_oat_record_1",
        vec![crate::auth::SCOPE_PROJECT_READ.to_string()],
    );
    let bytes = b"%PDF-1.7\noauth export\n%%EOF\n".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        creator,
        "paper/oauth.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();

    let refreshed = oauth(
        "wc_oat_record_2",
        vec![crate::auth::SCOPE_PROJECT_READ.to_string()],
    );
    let read = tokio::spawn({
        let runtime = runtime.clone();
        let uri = uri.clone();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "resources/read",
                    Some(json!(3110)),
                    mcp_2026_params(json!({"uri": uri})),
                ),
                Some(&refreshed),
            )
            .await
        }
    });
    complete_mcp_export_resource_read(
        registry.clone(),
        "paper/oauth.pdf",
        bytes.clone(),
        "application/pdf",
        &sha256,
        McpExportChunkFault::None,
    )
    .await;
    let outcome = read.await.unwrap();
    let McpOutcome::Ok(value) = outcome else {
        panic!("refreshed OAuth caller should retain export identity, got {outcome:?}");
    };
    let decoded = general_purpose::STANDARD
        .decode(value["result"]["contents"][0]["blob"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, bytes);

    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        oauth(
            "wc_oat_record_3",
            vec![crate::auth::SCOPE_PROJECT_READ.to_string()],
        ),
        "paper/oauth.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let missing_scope = oauth("wc_oat_record_4", vec![]);
    let denied = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3111)),
            mcp_2026_params(json!({"uri": uri})),
        ),
        Some(&missing_scope),
    )
    .await;
    match denied {
        McpOutcome::Forbidden {
            body,
            required_scope,
        } => {
            assert_eq!(required_scope, Some(crate::auth::SCOPE_PROJECT_READ));
            assert_eq!(body["error"], "insufficient_scope");
            assert!(body["error_description"]
                .as_str()
                .unwrap_or("")
                .contains(crate::auth::SCOPE_PROJECT_READ));
        }
        other => panic!("OAuth export read without project:read must fail, got {other:?}"),
    }
    assert!(registry
        .poll(RunnerPollRequest {
            client_id: "exporter".to_string(),
            runner_instance_id: "inst-export".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn export_project_artifact_non_mcp_path_fails_before_runner_read() {
    use crate::runner_protocol::RunnerPollRequest;
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-export", "alice");
    let result = runtime
        .dispatch_with_auth(
            crate::tool_runtime::ToolCall::ExportProjectArtifact {
                project: "agent:exporter:demo".to_string(),
                path: "paper/report.pdf".to_string(),
                session_id: None,
            },
            Some(&auth),
        )
        .await;
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("MCP-only")));
    assert!(registry
        .poll(RunnerPollRequest {
            client_id: "exporter".to_string(),
            runner_instance_id: "inst-export".to_string(),
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn mcp_artifact_export_resource_link_and_binary_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-roundtrip", "alice");
    // Keep the PDF above one public read_project_artifact chunk so the export
    // resource path must exercise the bounded multi-read loop.
    let mut pdf = Vec::with_capacity(96 * 1024);
    pdf.extend_from_slice(b"%PDF-1.7\nWebCodex export fixture\n");
    while pdf.len() < 96 * 1024 - 6 {
        pdf.extend_from_slice(b"artifact export bounded chunk fixture\n");
    }
    pdf.truncate(96 * 1024 - 6);
    pdf.extend_from_slice(b"%%EOF\n");

    // Minimal real OOXML ZIP: [Content_Types].xml, package relationship, and
    // ppt/presentation.xml. The export path does not semantically parse Office
    // content, but this keeps the PPTX round-trip fixture structurally genuine.
    let pptx = general_purpose::STANDARD
        .decode("UEsDBBQAAAAAAPtWD10vICcR9gAAAPYAAAATAAAAW0NvbnRlbnRfVHlwZXNdLnhtbDw/eG1sIHZlcnNpb249IjEuMCI/PjxUeXBlcyB4bWxucz0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL3BhY2thZ2UvMjAwNi9jb250ZW50LXR5cGVzIj48T3ZlcnJpZGUgUGFydE5hbWU9Ii9wcHQvcHJlc2VudGF0aW9uLnhtbCIgQ29udGVudFR5cGU9ImFwcGxpY2F0aW9uL3ZuZC5vcGVueG1sZm9ybWF0cy1vZmZpY2Vkb2N1bWVudC5wcmVzZW50YXRpb25tbC5wcmVzZW50YXRpb24ubWFpbit4bWwiLz48L1R5cGVzPlBLAwQUAAAAAAD7Vg9dO8y/FQoBAAAKAQAACwAAAF9yZWxzLy5yZWxzPD94bWwgdmVyc2lvbj0iMS4wIj8+PFJlbGF0aW9uc2hpcHMgeG1sbnM9Imh0dHA6Ly9zY2hlbWFzLm9wZW54bWxmb3JtYXRzLm9yZy9wYWNrYWdlLzIwMDYvcmVsYXRpb25zaGlwcyI+PFJlbGF0aW9uc2hpcCBJZD0icklkMSIgVHlwZT0iaHR0cDovL3NjaGVtYXMub3BlbnhtbGZvcm1hdHMub3JnL29mZmljZURvY3VtZW50LzIwMDYvcmVsYXRpb25zaGlwcy9vZmZpY2VEb2N1bWVudCIgVGFyZ2V0PSJwcHQvcHJlc2VudGF0aW9uLnhtbCIvPjwvUmVsYXRpb25zaGlwcz5QSwMEFAAAAAAA+1YPXZD24kRrAAAAawAAABQAAABwcHQvcHJlc2VudGF0aW9uLnhtbDw/eG1sIHZlcnNpb249IjEuMCI/PjxwOnByZXNlbnRhdGlvbiB4bWxuczpwPSJodHRwOi8vc2NoZW1hcy5vcGVueG1sZm9ybWF0cy5vcmcvcHJlc2VudGF0aW9ubWwvMjAwNi9tYWluIi8+UEsBAhQDFAAAAAAA+1YPXS8gJxH2AAAA9gAAABMAAAAAAAAAAAAAAIABAAAAAFtDb250ZW50X1R5cGVzXS54bWxQSwECFAMUAAAAAAD7Vg9dO8y/FQoBAAAKAQAACwAAAAAAAAAAAAAAgAEnAQAAX3JlbHMvLnJlbHNQSwECFAMUAAAAAAD7Vg9dkPbiRGsAAABrAAAAFAAAAAAAAAAAAAAAgAFaAgAAcHB0L3ByZXNlbnRhdGlvbi54bWxQSwUGAAAAAAMAAwC8AAAA9wIAAAAA")
        .unwrap();
    let cases = vec![
        ("paper/report.pdf", "application/pdf", pdf),
        ("paper/deck.pptx", crate::artifact_policy::PPTX_MIME, pptx),
    ];

    for (path, mime_type, bytes) in cases {
        let export = issue_mcp_artifact_export(
            runtime.clone(),
            registry.clone(),
            auth.clone(),
            path,
            &bytes,
            mime_type,
        )
        .await;
        let result = &export["result"];
        assert_eq!(result["isError"], false, "export: {export:?}");
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        let link = &result["content"][0];
        assert_eq!(link["type"], "resource_link");
        assert_eq!(link["mimeType"], mime_type);
        assert_eq!(
            link["name"],
            std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        );
        let uri = link["uri"].as_str().unwrap().to_string();
        assert!(uri.starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX));
        let structured = serde_json::to_string(&result["structuredContent"]).unwrap();
        assert!(!structured.contains(MCP_ARTIFACT_EXPORT_URI_PREFIX));
        assert!(!structured.contains("content_base64"));
        assert!(!structured.contains("\"blob\""));

        let listed = handle_mcp_request(
            &runtime,
            rpc(
                "resources/list",
                Some(json!(3102)),
                mcp_2026_params(json!({})),
            ),
            Some(&auth),
        )
        .await;
        let McpOutcome::Ok(listed) = listed else {
            panic!("resources/list must succeed");
        };
        assert!(!serde_json::to_string(&listed).unwrap().contains(&uri));

        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let read = tokio::spawn({
            let runtime = runtime.clone();
            let auth = auth.clone();
            let uri = uri.clone();
            async move {
                handle_mcp_request(
                    &runtime,
                    rpc(
                        "resources/read",
                        Some(json!(3103)),
                        mcp_2026_params(json!({"uri": uri})),
                    ),
                    Some(&auth),
                )
                .await
            }
        });
        complete_mcp_export_resource_read(
            registry.clone(),
            path,
            bytes.clone(),
            mime_type,
            &sha256,
            McpExportChunkFault::None,
        )
        .await;
        let outcome = read.await.unwrap();
        let McpOutcome::Ok(value) = outcome else {
            panic!("resources/read must return embedded binary, got {outcome:?}");
        };
        let contents = &value["result"]["contents"][0];
        assert_eq!(contents["uri"], uri);
        assert_eq!(contents["mimeType"], mime_type);
        let decoded = general_purpose::STANDARD
            .decode(contents["blob"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, bytes);
        assert!(value["result"].get("structuredContent").is_none());
    }
}

#[tokio::test]
async fn mcp_artifact_export_resource_link_accepts_above_whole_payload_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-large-link", "alice");
    let bytes = vec![0x5a; MAX_PROJECT_ARTIFACT_BYTES + 1];
    let export = issue_mcp_artifact_export(
        runtime,
        registry,
        auth,
        "paper/above-whole-payload.dat",
        &bytes,
        "application/octet-stream",
    )
    .await;
    assert_eq!(export["result"]["isError"], false, "export: {export:?}");
    assert_eq!(
        export["result"]["content"][0]["mimeType"],
        "application/octet-stream"
    );
    assert!(export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX));
}

#[tokio::test]
async fn http_mcp_artifact_export_resources_read_streams_valid_json_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), None).await;
    let auth = crate::auth::AuthContext {
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        ..crate::auth::AuthContext::new(crate::auth::AuthKind::Bootstrap)
    };
    let mut bytes: Vec<u8> = (0..(96 * 1024 + 5))
        .map(|index| (index % 251) as u8)
        .collect();
    bytes[..5].copy_from_slice(b"%PDF-");
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let path = "paper/http-stream.pdf";
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth,
        path,
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();

    let config = test_config(Some("secret"));
    let (_db_tmp, db) = test_db();
    let service = Service::new(build_test_router(config, db, runtime));
    let params = mcp_2026_params(json!({"uri": uri.clone()}));
    let request = async {
        let mut response = TestClient::post("http://localhost/mcp")
            .bearer_auth("secret")
            .add_header(
                MCP_PROTOCOL_VERSION_HEADER,
                MCP_STATELESS_PROTOCOL_VERSION,
                true,
            )
            .add_header(MCP_METHOD_HEADER, "resources/read", true)
            .add_header(MCP_NAME_HEADER, uri.as_str(), true)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 3190,
                "method": "resources/read",
                "params": params
            }))
            .send(&service)
            .await;
        let status = effective_status(&response);
        let body: Value = response.take_json().await.unwrap();
        (status, body)
    };
    let complete = complete_mcp_export_resource_read(
        registry,
        path,
        bytes.clone(),
        "application/pdf",
        &sha256,
        McpExportChunkFault::None,
    );
    let ((status, body), _) = tokio::join!(request, complete);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 3190);
    assert_eq!(body["result"]["resultType"], "complete");
    assert!(body["result"].get("ttlMs").is_none());
    assert!(body["result"].get("cacheScope").is_none());
    assert_eq!(body["result"]["contents"][0]["uri"], uri);
    assert_eq!(body["result"]["contents"][0]["mimeType"], "application/pdf");
    let decoded = general_purpose::STANDARD
        .decode(body["result"]["contents"][0]["blob"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, bytes);
    assert_eq!(
        body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "webcodex"
    );
}

#[tokio::test]
async fn mcp_artifact_export_optimized_pipeline_is_four_way_bounded_and_offset_ordered() {
    use crate::runner_protocol::RunnerPollRequest;
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-pipeline", "alice");
    let path = "paper/pipeline.pdf";
    let size = MAX_READ_PROJECT_ARTIFACT_LENGTH * 9 + 123;
    let bytes: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        path,
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let read = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let uri = uri.clone();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "resources/read",
                    Some(json!(3130)),
                    mcp_2026_params(json!({"uri": uri})),
                ),
                Some(&auth),
            )
            .await
        }
    });

    complete_mcp_export_metadata(
        registry.clone(),
        path,
        bytes.len(),
        &sha256,
        "application/pdf",
    )
    .await;
    let first = poll_mcp_export_request(&registry).await;
    assert_eq!(
        mcp_export_optimized_chunk_range(&first, path, bytes.len()).0,
        0
    );
    complete_mcp_export_optimized_chunk(&registry, first, path, &bytes).await;

    let mut first_batch = Vec::new();
    for _ in 0..MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS {
        first_batch.push(poll_mcp_export_request(&registry).await);
    }
    first_batch
        .sort_by_key(|request| mcp_export_optimized_chunk_range(request, path, bytes.len()).0);
    let first_offsets: Vec<usize> = first_batch
        .iter()
        .map(|request| mcp_export_optimized_chunk_range(request, path, bytes.len()).0)
        .collect();
    assert_eq!(
        first_offsets,
        (1..=4)
            .map(|index| index * MAX_READ_PROJECT_ARTIFACT_LENGTH)
            .collect::<Vec<_>>()
    );
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "the optimized batch must not dispatch a fifth chunk"
    );

    let mut batch = first_batch.into_iter();
    let b0 = batch.next().unwrap();
    let b1 = batch.next().unwrap();
    let b2 = batch.next().unwrap();
    let b3 = batch.next().unwrap();
    for request in [b3, b1, b2] {
        complete_mcp_export_optimized_chunk(&registry, request, path, &bytes).await;
    }
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "the next batch must wait until every request in the current batch is drained"
    );
    complete_mcp_export_optimized_chunk(&registry, b0, path, &bytes).await;

    let mut second_batch = Vec::new();
    for _ in 0..MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS {
        second_batch.push(poll_mcp_export_request(&registry).await);
    }
    second_batch
        .sort_by_key(|request| mcp_export_optimized_chunk_range(request, path, bytes.len()).0);
    assert_eq!(
        second_batch
            .iter()
            .map(|request| mcp_export_optimized_chunk_range(request, path, bytes.len()).0)
            .collect::<Vec<_>>(),
        (5..=8)
            .map(|index| index * MAX_READ_PROJECT_ARTIFACT_LENGTH)
            .collect::<Vec<_>>()
    );
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "a later optimized batch must keep the same four-request bound"
    );
    while let Some(request) = second_batch.pop() {
        complete_mcp_export_optimized_chunk(&registry, request, path, &bytes).await;
    }

    let final_chunk = poll_mcp_export_request(&registry).await;
    assert_eq!(
        mcp_export_optimized_chunk_range(&final_chunk, path, bytes.len()).0,
        9 * MAX_READ_PROJECT_ARTIFACT_LENGTH
    );
    complete_mcp_export_optimized_chunk(&registry, final_chunk, path, &bytes).await;

    let McpOutcome::Ok(value) = read.await.unwrap() else {
        panic!("optimized pipelined resource read must succeed");
    };
    let decoded = general_purpose::STANDARD
        .decode(value["result"]["contents"][0]["blob"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, bytes);
    assert_eq!(format!("{:x}", Sha256::digest(&decoded)), sha256);
    assert_eq!(
        registry
            .get_runner_view("exporter")
            .await
            .unwrap()
            .pending_requests,
        0
    );
}

#[tokio::test]
async fn mcp_artifact_export_total_timeout_cleans_abandoned_pending_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-pipeline-timeout", "alice");
    let path = "paper/pipeline-timeout.pdf";
    let bytes: Vec<u8> = (0..MAX_READ_PROJECT_ARTIFACT_LENGTH * 5)
        .map(|index| (index % 233) as u8)
        .collect();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        path,
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let gate = Arc::new(Semaphore::new(1));
    let read = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let gate = gate.clone();
        async move {
            mcp_artifact_export_resource_read_with_gate_timeout(
                &runtime,
                &uri,
                Some(&auth),
                gate,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
        }
    });

    complete_mcp_export_metadata(
        registry.clone(),
        path,
        bytes.len(),
        &sha256,
        "application/pdf",
    )
    .await;
    let first = poll_mcp_export_request(&registry).await;
    complete_mcp_export_optimized_chunk(&registry, first, path, &bytes).await;

    let mut inflight = Vec::new();
    for _ in 0..MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS {
        inflight.push(poll_mcp_export_request(&registry).await);
    }
    assert!(matches!(
        read.await.unwrap(),
        Err(McpArtifactExportReadError::Timeout)
    ));
    assert_eq!(gate.available_permits(), 1);
    for request in inflight {
        assert!(
            !registry.cancel_request(&request.request_id).await,
            "resource timeout must remove every abandoned optimized chunk request"
        );
    }

    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        "paper/metadata-timeout.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let read = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        let gate = gate.clone();
        async move {
            mcp_artifact_export_resource_read_with_gate_timeout(
                &runtime,
                &uri,
                Some(&auth),
                gate,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
        }
    });
    let metadata = poll_mcp_export_request(&registry).await;
    assert_eq!(metadata.kind, "file_read_project_artifact_metadata");
    assert!(matches!(
        read.await.unwrap(),
        Err(McpArtifactExportReadError::Timeout)
    ));
    assert!(
        !registry.cancel_request(&metadata.request_id).await,
        "resource timeout must also remove an abandoned metadata recheck"
    );
}

#[tokio::test]
async fn mcp_artifact_export_optimized_batch_drains_before_offset_ordered_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-pipeline-error", "alice");
    let path = "paper/pipeline-error.pdf";
    let bytes: Vec<u8> = (0..MAX_READ_PROJECT_ARTIFACT_LENGTH * 5)
        .map(|index| (index % 239) as u8)
        .collect();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        path,
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let mut read = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move {
            handle_mcp_request(
                &runtime,
                rpc(
                    "resources/read",
                    Some(json!(3131)),
                    mcp_2026_params(json!({"uri": uri})),
                ),
                Some(&auth),
            )
            .await
        }
    });

    complete_mcp_export_metadata(
        registry.clone(),
        path,
        bytes.len(),
        &sha256,
        "application/pdf",
    )
    .await;
    let first = poll_mcp_export_request(&registry).await;
    complete_mcp_export_optimized_chunk(&registry, first, path, &bytes).await;

    let mut batch = Vec::new();
    for _ in 0..MAX_MCP_ARTIFACT_EXPORT_CHUNK_READS {
        batch.push(poll_mcp_export_request(&registry).await);
    }
    batch.sort_by_key(|request| mcp_export_optimized_chunk_range(request, path, bytes.len()).0);
    let mut batch = batch.into_iter();
    let earliest = batch.next().unwrap();
    let later_unsafe = batch.next().unwrap();
    let good_two = batch.next().unwrap();
    let good_three = batch.next().unwrap();

    let (unsafe_offset, unsafe_end) =
        mcp_export_optimized_chunk_range(&later_unsafe, path, bytes.len());
    complete_mcp_export_request(
        &registry,
        later_unsafe,
        json!({
            "path": path,
            "file_bytes": bytes.len(),
            "offset": unsafe_offset + 1,
            "bytes_returned": unsafe_end - unsafe_offset,
            "content_base64": general_purpose::STANDARD.encode(&bytes[unsafe_offset..unsafe_end]),
            "next_offset": unsafe_end,
            "truncated": unsafe_end < bytes.len(),
            "eof": unsafe_end == bytes.len(),
        }),
    )
    .await;
    complete_mcp_export_optimized_chunk(&registry, good_three, path, &bytes).await;
    complete_mcp_export_optimized_chunk(&registry, good_two, path, &bytes).await;

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut read)
            .await
            .is_err(),
        "one completed batch error must not short-circuit and drop another dispatched request"
    );

    complete_mcp_export_request(
        &registry,
        earliest,
        json!({
            "error_kind": "snapshot_changed",
            "error": Value::Null,
        }),
    )
    .await;

    match read.await.unwrap() {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert_eq!(
                value["error"]["message"],
                "Exported artifact no longer matches its snapshot"
            );
        }
        other => panic!("earliest requested-offset batch error must win, got {other:?}"),
    }
    assert_eq!(
        registry
            .get_runner_view("exporter")
            .await
            .unwrap()
            .pending_requests,
        0
    );
}

#[tokio::test]
async fn mcp_artifact_export_same_size_mutations_fail_final_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-mutation", "alice");
    let bytes = vec![0x5a; 70 * 1024];
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    for fault in [
        McpExportChunkFault::MutateFirstChunk,
        McpExportChunkFault::MutateLaterChunk,
    ] {
        let export = issue_mcp_artifact_export(
            runtime.clone(),
            registry.clone(),
            auth.clone(),
            "paper/mutation.pdf",
            &bytes,
            "application/pdf",
        )
        .await;
        let uri = export["result"]["content"][0]["uri"]
            .as_str()
            .unwrap()
            .to_string();
        let read = tokio::spawn({
            let runtime = runtime.clone();
            let auth = auth.clone();
            async move {
                handle_mcp_request(
                    &runtime,
                    rpc(
                        "resources/read",
                        Some(json!(3121)),
                        mcp_2026_params(json!({"uri": uri})),
                    ),
                    Some(&auth),
                )
                .await
            }
        });
        complete_mcp_export_resource_read(
            registry.clone(),
            "paper/mutation.pdf",
            bytes.clone(),
            "application/pdf",
            &sha256,
            fault,
        )
        .await;
        match read.await.unwrap() {
            McpOutcome::BadRequest(value) => {
                assert_eq!(value["error"]["code"], -32602, "fault: {fault:?}");
                assert_eq!(
                    value["error"]["message"], "Exported artifact no longer matches its snapshot",
                    "fault: {fault:?}"
                );
            }
            other => panic!("same-size mutation {fault:?} must fail closed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn mcp_artifact_export_backpressure_is_two_way_bounded_and_retryable() {
    use crate::runner_protocol::RunnerPollRequest;
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-gate", "alice");
    let bytes = b"%PDF-1.7\nconcurrent export\n%%EOF\n".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let stable = ToolResult::ok(json!({
        "project": "agent:exporter:demo",
        "path": "paper/gate.pdf",
        "bytes": bytes.len(),
        "sha256": sha256,
        "mime_type": "application/pdf",
        "name": "gate.pdf",
    }));
    let caller = mcp_artifact_export_caller_binding(Some(&auth)).unwrap();
    let (uri, _) = mcp_issue_artifact_export(caller, &stable).unwrap();
    let gate = Arc::new(Semaphore::new(2));

    let spawn_read = |id: i64| {
        let runtime = runtime.clone();
        let auth = auth.clone();
        let uri = uri.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            let _ = id;
            mcp_artifact_export_resource_read_with_gate(
                &runtime,
                &uri,
                Some(&auth),
                gate,
                Duration::from_secs(1),
            )
            .await
        })
    };
    let first = spawn_read(1);
    let second = spawn_read(2);
    let first_metadata = poll_mcp_export_request(&registry).await;
    let second_metadata = poll_mcp_export_request(&registry).await;
    assert_eq!(first_metadata.kind, "file_read_project_artifact_metadata");
    assert_eq!(second_metadata.kind, "file_read_project_artifact_metadata");
    assert_eq!(gate.available_permits(), 0);

    let busy = mcp_artifact_export_resource_read_with_gate(
        &runtime,
        &uri,
        Some(&auth),
        gate.clone(),
        Duration::from_millis(25),
    )
    .await;
    assert!(matches!(busy, Err(McpArtifactExportReadError::Busy)));
    assert!(
        registry
            .poll(RunnerPollRequest {
                client_id: "exporter".to_string(),
                runner_instance_id: "inst-export".to_string(),
            })
            .await
            .unwrap()
            .is_none(),
        "busy admission must not start a Runner read"
    );

    for request in [first_metadata, second_metadata] {
        complete_mcp_export_request(
            &registry,
            request,
            json!({
                "path": "paper/gate.pdf",
                "bytes": bytes.len(),
                "sha256": sha256,
                "mime_type": "application/pdf",
            }),
        )
        .await;
    }
    for _ in 0..2 {
        let request = poll_mcp_export_request(&registry).await;
        assert_eq!(request.kind, "file_read_project_artifact_export_chunk");
        let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
        let offset = payload["offset"].as_u64().unwrap() as usize;
        let length = payload["length"].as_u64().unwrap() as usize;
        let end = offset.saturating_add(length).min(bytes.len());
        complete_mcp_export_request(
            &registry,
            request,
            json!({
                "path": "paper/gate.pdf",
                "file_bytes": bytes.len(),
                "offset": offset,
                "bytes_returned": end - offset,
                "content_base64": general_purpose::STANDARD.encode(&bytes[offset..end]),
                "next_offset": end,
                "truncated": end < bytes.len(),
                "eof": end == bytes.len(),
            }),
        )
        .await;
    }
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(gate.available_permits(), 2);

    // The busy attempt did not consume the handle: the same authenticated
    // caller can retry it after capacity returns.
    let retry = spawn_read(3);
    let metadata = poll_mcp_export_request(&registry).await;
    complete_mcp_export_request(
        &registry,
        metadata,
        json!({
            "path": "paper/gate.pdf",
            "bytes": bytes.len(),
            "sha256": sha256,
            "mime_type": "application/pdf",
        }),
    )
    .await;
    let chunk = poll_mcp_export_request(&registry).await;
    let payload: Value = serde_json::from_str(chunk.content.as_deref().unwrap()).unwrap();
    let offset = payload["offset"].as_u64().unwrap() as usize;
    let length = payload["length"].as_u64().unwrap() as usize;
    let end = offset.saturating_add(length).min(bytes.len());
    complete_mcp_export_request(
        &registry,
        chunk,
        json!({
            "path": "paper/gate.pdf",
            "file_bytes": bytes.len(),
            "offset": offset,
            "bytes_returned": end - offset,
            "content_base64": general_purpose::STANDARD.encode(&bytes[offset..end]),
            "next_offset": end,
            "truncated": false,
            "eof": true,
        }),
    )
    .await;
    assert!(retry.await.unwrap().is_ok());
    assert_eq!(gate.available_permits(), 2);

    // A terminal snapshot failure also releases its RAII permit.
    let failed = spawn_read(4);
    let metadata = poll_mcp_export_request(&registry).await;
    complete_mcp_export_request(
        &registry,
        metadata,
        json!({
            "path": "paper/gate.pdf",
            "bytes": bytes.len(),
            "sha256": "b".repeat(64),
            "mime_type": "application/pdf",
        }),
    )
    .await;
    assert!(matches!(
        failed.await.unwrap(),
        Err(McpArtifactExportReadError::SnapshotChanged)
    ));
    assert_eq!(gate.available_permits(), 2);
}

#[tokio::test]
async fn mcp_artifact_export_resource_is_caller_bound_and_rechecks_project_authorization() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let creator = mcp_export_api_auth("key-owner", "alice");
    let bytes = b"%PDF-1.7\nowner bound\n%%EOF\n".to_vec();
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        creator.clone(),
        "private/report.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();

    let other_caller = mcp_export_api_auth("key-other", "alice");
    let stolen = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3104)),
            mcp_2026_params(json!({"uri": uri.clone()})),
        ),
        Some(&other_caller),
    )
    .await;
    match stolen {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert_eq!(
                value["error"]["message"],
                "Artifact export resource is unavailable"
            );
        }
        other => panic!("stolen URI must not transfer authority, got {other:?}"),
    }

    let same_stable_identity_wrong_project_owner = mcp_export_api_auth("key-owner", "bob");
    let denied = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3105)),
            mcp_2026_params(json!({"uri": uri})),
        ),
        Some(&same_stable_identity_wrong_project_owner),
    )
    .await;
    match denied {
        McpOutcome::BadRequest(value) => {
            assert_eq!(value["error"]["code"], -32602);
            assert_eq!(
                value["error"]["message"],
                "Artifact export resource is unavailable"
            );
        }
        other => panic!("project authorization must be rechecked, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_artifact_export_unknown_expired_and_changed_snapshots_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-snapshot", "alice");
    let unknown = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3106)),
            mcp_2026_params(json!({
                "uri": "webcodex-artifact://export/wc_export_0123456789abcdef0123456789abcdef"
            })),
        ),
        Some(&auth),
    )
    .await;
    assert!(matches!(unknown, McpOutcome::BadRequest(_)));

    let bytes = b"%PDF-1.7\nsnapshot\n%%EOF\n".to_vec();
    let original_sha = format!("{:x}", Sha256::digest(&bytes));
    let expired_export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        "paper/snapshot.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let expired_uri = expired_export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    mcp_expire_artifact_export_for_test(&expired_uri);
    let expired = handle_mcp_request(
        &runtime,
        rpc(
            "resources/read",
            Some(json!(3107)),
            mcp_2026_params(json!({"uri": expired_uri})),
        ),
        Some(&auth),
    )
    .await;
    assert!(matches!(expired, McpOutcome::BadRequest(_)));

    for (changed_bytes, changed_sha, changed_mime) in [
        (bytes.len() + 1, original_sha.clone(), "application/pdf"),
        (bytes.len(), "b".repeat(64), "application/pdf"),
        (bytes.len(), original_sha.clone(), "application/zip"),
    ] {
        let export = issue_mcp_artifact_export(
            runtime.clone(),
            registry.clone(),
            auth.clone(),
            "paper/snapshot.pdf",
            &bytes,
            "application/pdf",
        )
        .await;
        let uri = export["result"]["content"][0]["uri"]
            .as_str()
            .unwrap()
            .to_string();
        let read = tokio::spawn({
            let runtime = runtime.clone();
            let auth = auth.clone();
            let uri = uri.clone();
            async move {
                handle_mcp_request(
                    &runtime,
                    rpc(
                        "resources/read",
                        Some(json!(3108)),
                        mcp_2026_params(json!({"uri": uri})),
                    ),
                    Some(&auth),
                )
                .await
            }
        });
        complete_mcp_export_metadata(
            registry.clone(),
            "paper/snapshot.pdf",
            changed_bytes,
            &changed_sha,
            changed_mime,
        )
        .await;
        let outcome = read.await.unwrap();
        match outcome {
            McpOutcome::BadRequest(value) => {
                assert_eq!(value["error"]["code"], -32602);
                assert_eq!(
                    value["error"]["message"],
                    "Exported artifact no longer matches its snapshot"
                );
            }
            other => panic!("changed export snapshot must fail closed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn mcp_artifact_export_malformed_chunk_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), Some("alice")).await;
    let auth = mcp_export_api_auth("key-malformed", "alice");
    let bytes = vec![0x5a; 70 * 1024];
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let export = issue_mcp_artifact_export(
        runtime.clone(),
        registry.clone(),
        auth.clone(),
        "paper/malformed.pdf",
        &bytes,
        "application/pdf",
    )
    .await;
    let uri = export["result"]["content"][0]["uri"]
        .as_str()
        .unwrap()
        .to_string();
    for fault in [
        McpExportChunkFault::InvalidBase64,
        McpExportChunkFault::Offset,
        McpExportChunkFault::Eof,
    ] {
        let read = tokio::spawn({
            let runtime = runtime.clone();
            let auth = auth.clone();
            let uri = uri.clone();
            async move {
                handle_mcp_request(
                    &runtime,
                    rpc(
                        "resources/read",
                        Some(json!(3109)),
                        mcp_2026_params(json!({"uri": uri})),
                    ),
                    Some(&auth),
                )
                .await
            }
        });
        complete_mcp_export_resource_read(
            registry.clone(),
            "paper/malformed.pdf",
            bytes.clone(),
            "application/pdf",
            &sha256,
            fault,
        )
        .await;
        let outcome = read.await.unwrap();
        match outcome {
            McpOutcome::BadRequest(value) => {
                assert_eq!(value["error"]["code"], -32603, "fault: {fault:?}");
                assert_eq!(
                    value["error"]["message"],
                    "Artifact export resource failed bounded safety validation",
                    "fault: {fault:?}"
                );
            }
            other => panic!("malformed chunk {fault:?} must fail closed, got {other:?}"),
        }
    }
}

#[test]
fn mcp_artifact_export_preserves_streaming_bound_and_durable_projection_has_no_handle_or_blob() {
    let output = json!({
        "project": "agent:exporter:demo",
        "path": "paper/too-large.pdf",
        "bytes": MAX_PROJECT_ARTIFACT_EXPORT_BYTES + 1,
        "sha256": "a".repeat(64),
        "mime_type": "application/pdf",
        "name": "too-large.pdf",
    });
    let error =
        validate_project_artifact_export_snapshot("paper/too-large.pdf", &output).unwrap_err();
    assert!(error.contains("maximum"));
    let exact_max = json!({
        "project": "agent:exporter:demo",
        "path": "paper/exact-max.pdf",
        "bytes": MAX_PROJECT_ARTIFACT_EXPORT_BYTES,
        "sha256": "a".repeat(64),
        "mime_type": "application/pdf",
        "name": "exact-max.pdf",
    });
    let snapshot =
        validate_project_artifact_export_snapshot("paper/exact-max.pdf", &exact_max).unwrap();
    assert_eq!(snapshot.bytes, MAX_PROJECT_ARTIFACT_EXPORT_BYTES);
    assert_eq!(MAX_PROJECT_ARTIFACT_BYTES, 10 * 1024 * 1024);

    let stable = ToolResult::ok(json!({
        "project": "agent:exporter:demo",
        "path": "paper/report.pdf",
        "bytes": 11,
        "sha256": "a".repeat(64),
        "mime_type": "application/pdf",
        "name": "report.pdf",
    }));
    let durable =
        crate::tool_runtime::audit_safe_result_for_tool("export_project_artifact", &stable.output);
    let serialized = serde_json::to_string(&durable).unwrap();
    assert!(!serialized.contains(MCP_ARTIFACT_EXPORT_URI_PREFIX));
    assert!(!serialized.contains("content_base64"));
    assert!(!serialized.contains("\"blob\""));
}

#[test]
fn mcp_artifact_export_incremental_base64_matches_whole_encoding() {
    let bytes: Vec<u8> = (0..(2 * MAX_READ_PROJECT_ARTIFACT_LENGTH + 17))
        .map(|index| (index % 251) as u8)
        .collect();
    let mut encoder = McpArtifactExportBase64Encoder::default();
    let mut encoded = String::new();
    let mut offset = 0usize;
    for length in [1usize, 2, 7, MAX_READ_PROJECT_ARTIFACT_LENGTH, 11, 65531] {
        if offset >= bytes.len() {
            break;
        }
        let end = offset.saturating_add(length).min(bytes.len());
        encoded.push_str(&encoder.push(&bytes[offset..end]));
        offset = end;
    }
    if offset < bytes.len() {
        encoded.push_str(&encoder.push(&bytes[offset..]));
    }
    encoded.push_str(&encoder.finish());
    assert_eq!(encoded, general_purpose::STANDARD.encode(&bytes));
}

#[test]
fn mcp_artifact_export_registry_is_bounded_fair_and_cleans_expired_entries() {
    let snapshot = ProjectArtifactExportSnapshot {
        path: "paper/report.pdf".to_string(),
        bytes: 1,
        sha256: "a".repeat(64),
        mime_type: "application/pdf".to_string(),
        name: "report.pdf".to_string(),
    };
    let caller_a = McpArtifactExportCallerBinding::ApiToken {
        api_key_id: "key-registry-a".to_string(),
    };
    let caller_b = McpArtifactExportCallerBinding::ApiToken {
        api_key_id: "key-registry-b".to_string(),
    };
    let mut registry = McpArtifactExportRegistry::default();
    let expired_uri = registry.insert(McpArtifactExportRecord {
        caller: caller_a.clone(),
        project: "agent:exporter:demo".to_string(),
        snapshot: snapshot.clone(),
        expires_at: Instant::now(),
    });
    assert!(registry.get_for_caller(&expired_uri, &caller_a).is_none());
    assert!(registry.entries.is_empty());

    let b_uri = registry.insert(McpArtifactExportRecord {
        caller: caller_b.clone(),
        project: "agent:exporter:demo".to_string(),
        snapshot: snapshot.clone(),
        expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
    });
    let mut a_uris = Vec::new();
    for _ in 0..MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER {
        a_uris.push(registry.insert(McpArtifactExportRecord {
            caller: caller_a.clone(),
            project: "agent:exporter:demo".to_string(),
            snapshot: snapshot.clone(),
            expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
        }));
    }
    let a_oldest = a_uris[0].clone();
    let a_17th = registry.insert(McpArtifactExportRecord {
        caller: caller_a.clone(),
        project: "agent:exporter:demo".to_string(),
        snapshot: snapshot.clone(),
        expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
    });
    assert!(registry.get_for_caller(&a_oldest, &caller_a).is_none());
    assert!(registry.get_for_caller(&a_17th, &caller_a).is_some());
    assert!(
        registry.get_for_caller(&b_uri, &caller_b).is_some(),
        "caller A churn must not evict caller B while A is constrained by its own quota"
    );
    assert_eq!(
        registry
            .entries
            .values()
            .filter(|record| record.caller == caller_a)
            .count(),
        MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER
    );

    let mut global = McpArtifactExportRegistry::default();
    for caller_index in 0..(MAX_MCP_ARTIFACT_EXPORTS / MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER) {
        let caller = McpArtifactExportCallerBinding::ApiToken {
            api_key_id: format!("key-global-{caller_index}"),
        };
        for _ in 0..MAX_MCP_ARTIFACT_EXPORTS_PER_CALLER {
            global.insert(McpArtifactExportRecord {
                caller: caller.clone(),
                project: "agent:exporter:demo".to_string(),
                snapshot: snapshot.clone(),
                expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
            });
        }
    }
    assert_eq!(global.entries.len(), MAX_MCP_ARTIFACT_EXPORTS);
    assert_eq!(global.order.len(), MAX_MCP_ARTIFACT_EXPORTS);
    let extra_caller = McpArtifactExportCallerBinding::ApiToken {
        api_key_id: "key-global-extra".to_string(),
    };
    global.insert(McpArtifactExportRecord {
        caller: extra_caller,
        project: "agent:exporter:demo".to_string(),
        snapshot,
        expires_at: Instant::now() + MCP_ARTIFACT_EXPORT_TTL,
    });
    assert_eq!(global.entries.len(), MAX_MCP_ARTIFACT_EXPORTS);
    assert_eq!(global.order.len(), MAX_MCP_ARTIFACT_EXPORTS);
}

#[tokio::test]
async fn mcp_artifact_export_action_audit_does_not_persist_handle_or_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = mcp_export_runtime(tmp.path(), None).await;
    let (_db_tmp, db) = test_db();
    let service = Service::new(build_test_router(
        test_config(Some("secret")),
        db.clone(),
        runtime,
    ));
    let bytes = b"%PDF-1.7\naudit export\n%%EOF\n".to_vec();
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let agent = tokio::spawn({
        let sha256 = sha256.clone();
        let bytes_len = bytes.len();
        async move {
            complete_mcp_export_metadata(
                registry,
                "paper/audit.pdf",
                bytes_len,
                &sha256,
                "application/pdf",
            )
            .await;
        }
    });
    let mut response = TestClient::post("http://localhost/mcp")
        .bearer_auth("secret")
        .add_header(
            MCP_PROTOCOL_VERSION_HEADER,
            MCP_STATELESS_PROTOCOL_VERSION,
            true,
        )
        .add_header(MCP_METHOD_HEADER, "tools/call", true)
        .add_header(MCP_NAME_HEADER, "export_project_artifact", true)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3112,
            "method": "tools/call",
            "params": mcp_2026_params(json!({
                "name": "export_project_artifact",
                "arguments": {
                    "project": "agent:exporter:demo",
                    "path": "paper/audit.pdf"
                }
            }))
        }))
        .send(&service)
        .await;
    agent.await.unwrap();
    assert_eq!(effective_status(&response), StatusCode::OK);
    let body: Value = response.take_json().await.unwrap();
    let uri = body["result"]["content"][0]["uri"]
        .as_str()
        .expect("successful export must return resource link");
    assert!(uri.starts_with(MCP_ARTIFACT_EXPORT_URI_PREFIX));

    let (operation, summary, error): (String, String, String) = {
        let conn = db.conn_for_tests();
        conn.query_row(
            "SELECT operation, summary_json, COALESCE(error_summary, '') FROM action_events ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(operation, "export_project_artifact");
    for durable in [&summary, &error] {
        assert!(!durable.contains(MCP_ARTIFACT_EXPORT_URI_PREFIX));
        assert!(!durable.contains("wc_export_"));
        assert!(!durable.contains("content_base64"));
        assert!(!durable.contains("\"blob\""));
    }
}
