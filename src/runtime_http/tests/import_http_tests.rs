use super::super::import_http::set_import_test_download_base_url;
use crate::tool_runtime::files::{
    MAX_PROJECT_ARTIFACT_UPLOAD_BYTES, MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES,
};
use base64::{engine::general_purpose, Engine as _};
use salvo::test::{ResponseExt, TestClient};
use salvo::Service;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const IMPORT_TEST_AGENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// The global import-network override is shared with MCP and DNS import fixtures.
// Budget for queued full-suite contention while still failing a genuinely stuck wait.
const IMPORT_TEST_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const IMPORT_TEST_SERVER_IO_TIMEOUT: Duration = Duration::from_secs(30);

fn run_import_http_in_large_stack_test_thread<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    // The host-ref artifact import fixture retains the HTTP/import runtime and
    // upload protocol state across several awaits. Keep that integration stack
    // local to the test instead of requiring a suite-wide RUST_MIN_STACK.
    let result = std::thread::Builder::new()
        .name("runtime-http-import-test".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build isolated runtime HTTP import test runtime")
                .block_on(test());
        })
        .expect("spawn isolated runtime HTTP import test thread")
        .join();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn lock_import_http_test() -> tokio::sync::MutexGuard<'static, ()> {
    tokio::time::timeout(
        IMPORT_TEST_LOCK_TIMEOUT,
        crate::tool_runtime::conversation_import::lock_import_test_network(),
    )
    .await
    .expect("timed out waiting for import test network lock")
}

struct ImportDownloadBaseUrlGuard;

impl ImportDownloadBaseUrlGuard {
    fn set(base_url: String) -> Self {
        set_import_test_download_base_url(Some(base_url));
        Self
    }
}

impl Drop for ImportDownloadBaseUrlGuard {
    fn drop(&mut self) {
        set_import_test_download_base_url(None);
    }
}

struct MockHttpServer {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_mock_http_server(responses: Vec<Vec<u8>>) -> MockHttpServer {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut responses = std::collections::VecDeque::from(responses);
        while let Some(response) = responses.pop_front() {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(IMPORT_TEST_SERVER_IO_TIMEOUT, listener.accept()).await
            else {
                return;
            };
            let mut buf = [0_u8; 4096];
            if tokio::time::timeout(IMPORT_TEST_SERVER_IO_TIMEOUT, stream.read(&mut buf))
                .await
                .is_err()
            {
                return;
            }
            if tokio::time::timeout(IMPORT_TEST_SERVER_IO_TIMEOUT, stream.write_all(&response))
                .await
                .is_err()
            {
                return;
            }
            let _ = tokio::time::timeout(IMPORT_TEST_SERVER_IO_TIMEOUT, stream.shutdown()).await;
        }
    });
    MockHttpServer {
        base_url: format!("http://{}", addr),
        handle,
    }
}

fn http_response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {}\r\n", status).into_bytes();
    for (name, value) in headers {
        response.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
    }
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(body);
    response
}

fn import_body(download_link: &str, mime_type: &str, name: &str) -> Value {
    json!({"project":"agent:importer:demo","output_dir":"docs/assets","openaiFileIdRefs":[{"name":name,"id":"file_mock","mime_type":mime_type,"download_link":download_link}]})
}

async fn import_test_service_with_local_runtime() -> Service {
    let config = super::test_config(Some("secret"));
    let (_tmp, db) = super::test_db();
    let tmp_proj = tempfile::tempdir().unwrap();
    let runtime = Arc::new(super::runtime_with_local_project(tmp_proj.path(), "demo"));
    Service::new(super::build_projects_router(config, db, runtime))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImportUploadFixtureOutcome {
    chunk_count: usize,
    aborted: bool,
    finished: bool,
}

async fn next_import_agent_request(
    registry: &crate::runner_http::RunnerRegistry,
) -> crate::runner_protocol::RunnerRequest {
    use crate::runner_protocol::RunnerPollRequest;

    tokio::time::timeout(IMPORT_TEST_AGENT_REQUEST_TIMEOUT, async {
        loop {
            if let Some(request) = registry
                .poll(RunnerPollRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                })
                .await
                .unwrap()
            {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for import agent request")
}

async fn complete_import_artifact_uploads(
    registry: Arc<crate::runner_http::RunnerRegistry>,
    upload_count: usize,
) -> Vec<ImportUploadFixtureOutcome> {
    use crate::runner_protocol::RunnerResultRequest;
    use sha2::{Digest, Sha256};

    let mut outcomes = Vec::with_capacity(upload_count);
    for index in 0..upload_count {
        let request = next_import_agent_request(&registry).await;
        assert_eq!(request.kind, "file_artifact_upload_begin");
        let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
        assert!(payload.get("download_url").is_none());
        assert!(payload.get("download_link").is_none());
        assert!(payload.get("openaiFileIdRefs").is_none());
        assert_eq!(payload["max_bytes"], MAX_PROJECT_ARTIFACT_UPLOAD_BYTES);
        let path = payload["path"].as_str().unwrap().to_string();
        let mime_type = payload["mime_type"].as_str().unwrap().to_string();
        let expected_bytes = payload["expected_bytes"].clone();
        let full_path = std::path::Path::new(request.cwd.as_deref().unwrap()).join(&path);
        if full_path.exists() && payload["overwrite"] == false {
            registry
                .complete(RunnerResultRequest {
                    client_id: "importer".to_string(),
                    runner_instance_id: "inst-import".to_string(),
                    request_id: request.request_id,
                    exit_code: Some(0),
                    stdout: Some(
                        json!({"path":path,"error":"file exists and overwrite is false"})
                            .to_string(),
                    ),
                    stderr: None,
                    duration_ms: Some(1),
                    error: None,
                })
                .await
                .unwrap();
            outcomes.push(ImportUploadFixtureOutcome {
                chunk_count: 0,
                aborted: false,
                finished: false,
            });
            continue;
        }

        let upload_id = format!("wc_upload_import_fixture_{index}");
        registry
            .complete(RunnerResultRequest {
                client_id: "importer".to_string(),
                runner_instance_id: "inst-import".to_string(),
                request_id: request.request_id,
                exit_code: Some(0),
                stdout: Some(
                    json!({
                        "path": path,
                        "upload_id": upload_id,
                        "received_bytes": 0,
                        "next_offset": 0,
                        "expected_bytes": expected_bytes,
                        "expected_sha256": null,
                        "max_bytes": MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                        "mime_type": mime_type,
                        "committed": false
                    })
                    .to_string(),
                ),
                stderr: None,
                duration_ms: Some(1),
                error: None,
            })
            .await
            .unwrap();

        let mut bytes = Vec::new();
        let mut chunk_count = 0usize;
        loop {
            let request = next_import_agent_request(&registry).await;
            let payload: Value = serde_json::from_str(request.content.as_deref().unwrap()).unwrap();
            assert_eq!(payload["path"], path);
            assert_eq!(payload["upload_id"], upload_id);
            match request.kind.as_str() {
                "file_artifact_upload_chunk" => {
                    assert_eq!(
                        payload["max_chunk_bytes"],
                        MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES
                    );
                    assert_eq!(payload["offset"], bytes.len());
                    let chunk = general_purpose::STANDARD
                        .decode(payload["content_base64"].as_str().unwrap())
                        .unwrap();
                    assert!(!chunk.is_empty());
                    assert!(chunk.len() <= MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES);
                    bytes.extend_from_slice(&chunk);
                    chunk_count += 1;
                    registry
                        .complete(RunnerResultRequest {
                            client_id: "importer".to_string(),
                            runner_instance_id: "inst-import".to_string(),
                            request_id: request.request_id,
                            exit_code: Some(0),
                            stdout: Some(
                                json!({
                                    "path": path,
                                    "upload_id": upload_id,
                                    "received_bytes": bytes.len(),
                                    "next_offset": bytes.len(),
                                    "expected_bytes": expected_bytes,
                                    "expected_sha256": null,
                                    "max_bytes": MAX_PROJECT_ARTIFACT_UPLOAD_BYTES,
                                    "mime_type": mime_type,
                                    "committed": false
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
                "file_artifact_upload_finish" => {
                    std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
                    std::fs::write(&full_path, &bytes).unwrap();
                    let sha256 = format!("{:x}", Sha256::digest(&bytes));
                    registry
                        .complete(RunnerResultRequest {
                            client_id: "importer".to_string(),
                            runner_instance_id: "inst-import".to_string(),
                            request_id: request.request_id,
                            exit_code: Some(0),
                            stdout: Some(
                                json!({
                                    "path": path,
                                    "upload_id": upload_id,
                                    "bytes": bytes.len(),
                                    "received_bytes": bytes.len(),
                                    "expected_bytes": expected_bytes,
                                    "expected_sha256": null,
                                    "sha256": sha256,
                                    "mime_type": mime_type,
                                    "committed": true
                                })
                                .to_string(),
                            ),
                            stderr: None,
                            duration_ms: Some(1),
                            error: None,
                        })
                        .await
                        .unwrap();
                    outcomes.push(ImportUploadFixtureOutcome {
                        chunk_count,
                        aborted: false,
                        finished: true,
                    });
                    break;
                }
                "file_artifact_upload_abort" => {
                    registry
                        .complete(RunnerResultRequest {
                            client_id: "importer".to_string(),
                            runner_instance_id: "inst-import".to_string(),
                            request_id: request.request_id,
                            exit_code: Some(0),
                            stdout: Some(
                                json!({
                                    "path": path,
                                    "upload_id": upload_id,
                                    "received_bytes": bytes.len(),
                                    "temp_file_removed": true,
                                    "sidecar_removed": true,
                                    "final_file_exists": full_path.exists(),
                                    "committed": false
                                })
                                .to_string(),
                            ),
                            stderr: None,
                            duration_ms: Some(1),
                            error: None,
                        })
                        .await
                        .unwrap();
                    outcomes.push(ImportUploadFixtureOutcome {
                        chunk_count,
                        aborted: true,
                        finished: false,
                    });
                    break;
                }
                other => panic!("unexpected import artifact request kind: {other}"),
            }
        }
    }
    outcomes
}

#[tokio::test]
async fn import_http_accepts_office_mime_and_extension_policy() {
    let cases = [
        (
            "report.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "report.pptx",
        ),
        (
            "deck.pptx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "deck.xlsx",
        ),
        (
            "book.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "book.docx",
        ),
    ];

    for (path, mime, mismatched_path) in cases {
        let service = import_test_service_with_local_runtime().await;
        let mut accepted = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&import_body("https://example.com/file", mime, path))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&accepted),
            salvo::http::StatusCode::BAD_REQUEST
        );
        let body: Value = accepted.take_json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("OpenAI file host"),
            "matching Office MIME/path should pass import MIME policy: {body:?}"
        );

        let mut octet = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&import_body(
                "https://example.com/file",
                "application/octet-stream",
                path,
            ))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&octet),
            salvo::http::StatusCode::BAD_REQUEST
        );
        let body: Value = octet.take_json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("OpenAI file host"));

        let mut mismatched = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&import_body(
                "https://files.oaiusercontent.com/file",
                mime,
                mismatched_path,
            ))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&mismatched),
            salvo::http::StatusCode::BAD_REQUEST
        );
        let body: Value = mismatched.take_json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("unsupported MIME"));
    }

    for path in ["payload.dat", "payload.artifact"] {
        let service = import_test_service_with_local_runtime().await;
        let mut rejected = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&import_body(
                "https://files.oaiusercontent.com/file",
                "application/octet-stream",
                path,
            ))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&rejected),
            salvo::http::StatusCode::BAD_REQUEST
        );
        let body: Value = rejected.take_json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("unsupported MIME"),
            "artifact-only octet-stream suffix must remain rejected by conversation import: {path}: {body:?}"
        );
    }
}

async fn runtime_conversation_import_host_ref_saves_pptx_through_artifact_path_body() {
    use crate::auth::{AuthContext, AuthKind};
    use crate::runner_protocol::RunnerCapabilities;
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolTransport,
    };
    use sha2::{Digest, Sha256};

    let _guard = lock_import_http_test().await;
    let pptx = b"pptx-conversation-attachment".to_vec();
    let expected_sha256 = format!("{:x}", Sha256::digest(&pptx));
    let server = start_mock_http_server(vec![http_response(
        "200 OK",
        &[("Content-Length", pptx.len().to_string())],
        &pptx,
    )])
    .await;
    let _download_base = ImportDownloadBaseUrlGuard::set(server.base_url.clone());
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = super::register_import_agent_with_capabilities(
        tmp.path(),
        Some(RunnerCapabilities {
            file_write: true,
            ..Default::default()
        }),
    )
    .await;
    let agent = tokio::spawn(complete_import_artifact_uploads(registry, 1));
    let arguments = json!({
        "project": "agent:importer:demo",
        "openaiFileIdRefs": [{
            "download_url": "https://8.8.8.8/import-test.pptx",
            "file_id": "file_host_pptx",
            "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "file_name": "source.pptx"
        }],
        "output_dir": "paper/export",
        "targets": ["import-test.pptx"],
        "overwrite": false
    });
    let auth = AuthContext {
        kind: AuthKind::Bootstrap,
        user_id: None,
        username: None,
        api_key_id: None,
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        token_kind: None,
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.call_tool_with_context(
            ToolCallRequest {
                tool_name: "import_conversation_files_to_project".to_string(),
                arguments,
            },
            ToolCallContext {
                transport: ToolTransport::Mcp,
                session_id: None,
                auth: Some(&auth),
                window: None,
                record_oauth_scope_denials: false,
                host_file_import_trust: HostFileImportTrust::TrustedOAuthClient,
            },
        ),
    )
    .await
    .expect("conversation import dispatch timed out");
    let result = outcome.result.expect("conversation import result");
    if !result.success {
        agent.abort();
        panic!("conversation import failed: {:?}", result.error);
    }
    tokio::time::timeout(Duration::from_secs(5), agent)
        .await
        .expect("save_project_artifact fixture timed out")
        .unwrap();

    assert_eq!(result.output["count"], 1);
    let imported = &result.output["imported"][0];
    assert_eq!(imported["path"], "paper/export/import-test.pptx");
    assert_eq!(imported["source_name"], "source.pptx");
    assert_eq!(imported["bytes_written"], pptx.len());
    assert_eq!(imported["sha256"], expected_sha256);
    assert_eq!(
        imported["mime_type"],
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("paper/export/import-test.pptx")).unwrap(),
        pptx
    );
}

#[test]
fn runtime_conversation_import_host_ref_saves_pptx_through_artifact_path() {
    run_import_http_in_large_stack_test_thread(|| {
        runtime_conversation_import_host_ref_saves_pptx_through_artifact_path_body()
    });
}

#[tokio::test]
async fn runtime_conversation_import_rejects_non_mcp_transport() {
    use crate::auth::{AuthContext, AuthKind};
    use crate::runner_protocol::RunnerCapabilities;
    use crate::tool_runtime::ToolCall;

    let tmp = tempfile::tempdir().unwrap();
    let (runtime, _registry) = super::register_import_agent_with_capabilities(
        tmp.path(),
        Some(RunnerCapabilities {
            file_write: true,
            ..Default::default()
        }),
    )
    .await;
    let auth = AuthContext {
        kind: AuthKind::Bootstrap,
        user_id: None,
        username: None,
        api_key_id: None,
        role: Some("admin".to_string()),
        scopes: vec!["admin".to_string()],
        is_bootstrap: true,
        token_kind: None,
        allowed_client_id: None,
        shared_key_hash: None,
        project_grant_id: None,
    };
    let call = ToolCall::from_tool_name(
        "import_conversation_files_to_project",
        json!({
            "project": "agent:importer:demo",
            "openaiFileIdRefs": [{
                "download_url": "https://files.oaiusercontent.com/should-not-download.pptx",
                "file_id": "file_host_pptx",
                "mime_type": "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "file_name": "source.pptx"
            }],
            "targets": ["import-test.pptx"]
        }),
    )
    .unwrap();
    let result = runtime.dispatch_with_auth(call, Some(&auth)).await;
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("MCP host file-reference mechanism")));
}

#[tokio::test]
async fn import_http_existing_mime_policy_still_passes_before_host_validation() {
    for (path, mime) in [
        ("image.png", "image/png"),
        ("paper.pdf", "application/pdf"),
        ("bundle.zip", "application/zip"),
        ("notes.txt", "text/plain"),
    ] {
        let service = import_test_service_with_local_runtime().await;
        let mut resp = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&import_body("https://example.com/file", mime, path))
            .send(&service)
            .await;
        assert_eq!(
            super::effective_status(&resp),
            salvo::http::StatusCode::BAD_REQUEST
        );
        let body: Value = resp.take_json().await.unwrap();
        assert!(
            body["error"].as_str().unwrap().contains("OpenAI file host"),
            "existing MIME/path should pass import MIME policy before host validation: {path}: {body:?}"
        );
    }

    let service = import_test_service_with_local_runtime().await;
    let mut unsupported = TestClient::post("http://localhost/api/artifacts/import")
        .bearer_auth("secret")
        .json(&import_body(
            "https://files.oaiusercontent.com/file",
            "application/x-msdownload",
            "payload.bin",
        ))
        .send(&service)
        .await;
    assert_eq!(
        super::effective_status(&unsupported),
        salvo::http::StatusCode::BAD_REQUEST
    );
    let body: Value = unsupported.take_json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("unsupported MIME"));
}

#[tokio::test]
async fn import_http_existing_png_pdf_zip_text_formats_still_import() {
    let _guard = lock_import_http_test().await;
    let cases = [
        ("image.png", "image/png", b"png-bytes".as_slice()),
        ("paper.pdf", "application/pdf", b"pdf-bytes".as_slice()),
        ("bundle.zip", "application/zip", b"zip-bytes".as_slice()),
        ("notes.txt", "text/plain", b"text-bytes".as_slice()),
    ];
    let responses = cases
        .iter()
        .map(|(_, _, bytes)| {
            http_response(
                "200 OK",
                &[("Content-Length", bytes.len().to_string())],
                bytes,
            )
        })
        .collect();
    let server = start_mock_http_server(responses).await;
    let _download_base = ImportDownloadBaseUrlGuard::set(server.base_url.clone());
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = super::register_import_agent_with_capabilities(
        tmp.path(),
        Some(crate::runner_protocol::RunnerCapabilities {
            file_write: true,
            ..Default::default()
        }),
    )
    .await;
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let service = Service::new(super::build_projects_router(config, db, runtime));

    for (path, mime, expected_bytes) in cases {
        let agent = tokio::spawn(complete_import_artifact_uploads(registry.clone(), 1));
        let mut resp = TestClient::post("http://localhost/api/artifacts/import")
            .bearer_auth("secret")
            .json(&json!({
                "project":"agent:importer:demo",
                "output_dir":"docs/assets",
                "targets":[path],
                "openaiFileIdRefs":[{
                    "name":path,
                    "id":"file_existing_format",
                    "mime_type":mime,
                    "download_link":format!("https://files.oaiusercontent.com/{path}")
                }]
            }))
            .send(&service)
            .await;
        tokio::time::timeout(Duration::from_secs(5), agent)
            .await
            .expect("existing-format upload fixture timed out")
            .unwrap();
        assert_eq!(super::effective_status(&resp), salvo::http::StatusCode::OK);
        let body: Value = resp.take_json().await.unwrap();
        assert_eq!(body["output"]["count"], 1);
        assert_eq!(
            body["output"]["imported"][0]["path"],
            format!("docs/assets/{path}")
        );
        assert_eq!(
            body["output"]["imported"][0]["bytes_written"],
            expected_bytes.len()
        );
        assert_eq!(body["output"]["imported"][0]["mime_type"], mime);
        assert_eq!(
            std::fs::read(tmp.path().join("docs/assets").join(path)).unwrap(),
            expected_bytes
        );
    }
}

#[tokio::test]
async fn import_http_streams_download_in_bounded_upload_chunks() {
    let _guard = lock_import_http_test().await;
    let bytes = vec![b'x'; MAX_PROJECT_ARTIFACT_UPLOAD_CHUNK_BYTES + 17];
    let server = start_mock_http_server(vec![http_response(
        "200 OK",
        &[("Content-Length", bytes.len().to_string())],
        &bytes,
    )])
    .await;
    let _download_base = ImportDownloadBaseUrlGuard::set(server.base_url.clone());
    let tmp = tempfile::tempdir().unwrap();
    let (runtime, registry) = super::register_import_agent_with_capabilities(
        tmp.path(),
        Some(crate::runner_protocol::RunnerCapabilities {
            file_write: true,
            ..Default::default()
        }),
    )
    .await;
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let service = Service::new(super::build_projects_router(config, db, runtime));
    let agent = tokio::spawn(complete_import_artifact_uploads(registry, 1));

    let mut resp = TestClient::post("http://localhost/api/artifacts/import")
        .bearer_auth("secret")
        .json(&json!({
            "project":"agent:importer:demo",
            "output_dir":"docs/assets",
            "targets":["streamed.zip"],
            "openaiFileIdRefs":[{
                "name":"streamed.zip",
                "id":"file_streamed",
                "mime_type":"application/zip",
                "download_link":"https://files.oaiusercontent.com/streamed.zip"
            }]
        }))
        .send(&service)
        .await;
    let outcomes = tokio::time::timeout(Duration::from_secs(10), agent)
        .await
        .expect("streaming upload fixture timed out")
        .unwrap();

    assert_eq!(super::effective_status(&resp), salvo::http::StatusCode::OK);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].chunk_count, 2);
    assert!(outcomes[0].finished);
    assert!(!outcomes[0].aborted);
    let body: Value = resp.take_json().await.unwrap();
    assert_eq!(body["output"]["imported"][0]["bytes_written"], bytes.len());
    assert_eq!(
        std::fs::read(tmp.path().join("docs/assets/streamed.zip")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn import_http_preserves_overwrite_false_protection() {
    let _guard = lock_import_http_test().await;
    let replacement = b"replacement".to_vec();
    let server = start_mock_http_server(vec![http_response(
        "200 OK",
        &[("Content-Length", replacement.len().to_string())],
        &replacement,
    )])
    .await;
    let _download_base = ImportDownloadBaseUrlGuard::set(server.base_url.clone());
    let tmp = tempfile::tempdir().unwrap();
    let existing = tmp.path().join("docs/assets/existing.png");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, b"original").unwrap();
    let (runtime, registry) = super::register_import_agent_with_capabilities(
        tmp.path(),
        Some(crate::runner_protocol::RunnerCapabilities {
            file_write: true,
            ..Default::default()
        }),
    )
    .await;
    let config = super::test_config(Some("secret"));
    let (_db_tmp, db) = super::test_db();
    let service = Service::new(super::build_projects_router(config, db, runtime));
    let agent = tokio::spawn(complete_import_artifact_uploads(registry, 1));
    let mut resp = TestClient::post("http://localhost/api/artifacts/import")
        .bearer_auth("secret")
        .json(&json!({
            "project":"agent:importer:demo",
            "output_dir":"docs/assets",
            "targets":["existing.png"],
            "overwrite":false,
            "openaiFileIdRefs":[{
                "name":"replacement.png",
                "id":"file_png",
                "mime_type":"image/png",
                "download_link":"https://files.oaiusercontent.com/replacement.png"
            }]
        }))
        .send(&service)
        .await;
    tokio::time::timeout(Duration::from_secs(5), agent)
        .await
        .expect("overwrite fixture timed out")
        .unwrap();
    assert_eq!(
        super::effective_status(&resp),
        salvo::http::StatusCode::BAD_REQUEST
    );
    let body: Value = resp.take_json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("overwrite is false"));
    assert_eq!(std::fs::read(existing).unwrap(), b"original");
}

#[tokio::test]
async fn import_http_rejects_http_download_link() {
    let service = import_test_service_with_local_runtime().await;
    let mut resp = TestClient::post("http://localhost/api/artifacts/import")
        .bearer_auth("secret")
        .json(&import_body(
            "http://files.oaiusercontent.com/a.png",
            "image/png",
            "a.png",
        ))
        .send(&service)
        .await;
    assert_eq!(
        super::effective_status(&resp),
        salvo::http::StatusCode::BAD_REQUEST
    );
    let body: Value = resp.take_json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("https"));
}

#[tokio::test]
async fn import_http_rejects_non_openai_file_host() {
    let service = import_test_service_with_local_runtime().await;
    let mut resp = TestClient::post("http://localhost/api/artifacts/import")
        .bearer_auth("secret")
        .json(&import_body(
            "https://example.com/a.png",
            "image/png",
            "a.png",
        ))
        .send(&service)
        .await;
    assert_eq!(
        super::effective_status(&resp),
        salvo::http::StatusCode::BAD_REQUEST
    );
    let body: Value = resp.take_json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("OpenAI file host"));
}
