//! Focused contract and Runner-boundary tests for `read_files`.

use super::super::*;
use super::support::*;
use crate::runner_protocol::{RunnerPollRequest, RunnerResultRequest};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

fn item(path: &str, start_line: Option<usize>, limit: Option<usize>) -> ReadFilesItem {
    ReadFilesItem {
        path: path.to_string(),
        start_line,
        limit,
    }
}

async fn next_read_request(
    runtime: &ToolRuntime,
    client_id: &str,
) -> crate::runner_protocol::RunnerRequest {
    wait_for_patch_agent_request(runtime, client_id).await
}

async fn complete_read(
    runtime: &ToolRuntime,
    client_id: &str,
    request: &crate::runner_protocol::RunnerRequest,
    content: &str,
) {
    complete_agent_ranged_file_read_request(runtime, client_id, request, content).await;
}

#[test]
fn read_files_input_schema_enforces_batch_and_item_bounds() {
    let specs = registered_tool_specs();
    let read_files = spec_named(&specs, "read_files");
    let schema = &read_files.input_schema;
    let validates = |value: &Value| {
        crate::tool_runtime::startup_brief::validate_schema_instance_for_test(value, schema).is_ok()
    };

    assert!(validates(
        &json!({"project": "demo", "items": [{"path": "a.rs"}]})
    ));
    assert!(validates(&json!({
        "project": "demo",
        "items": (0..8).map(|index| json!({"path": format!("{index}.rs")})).collect::<Vec<_>>()
    })));
    assert_eq!(schema["properties"]["items"]["minItems"], 1);
    assert_eq!(schema["properties"]["items"]["maxItems"], 8);
    assert!(!validates(&json!({
        "project": "demo",
        "items": (0..9).map(|index| json!({"path": format!("{index}.rs")})).collect::<Vec<_>>()
    })));
    assert_eq!(
        schema["properties"]["items"]["items"]["properties"]["path"]["minLength"],
        1
    );
    assert_eq!(
        schema["properties"]["max_result_bytes"]["default"],
        64 * 1024
    );
    assert_eq!(
        schema["properties"]["max_result_bytes"]["maximum"],
        512 * 1024
    );
    assert!(schema["properties"]["max_result_bytes"]["description"]
        .as_str()
        .unwrap()
        .contains("protocol overlays"));
    assert!(validates(&json!({
        "project": "demo",
        "items": [{"path": "a.rs"}],
        "max_result_bytes": 128 * 1024
    })));
    assert!(!validates(&json!({
        "project": "demo",
        "items": [{"path": "a.rs"}],
        "max_result_bytes": 512 * 1024 + 1
    })));
    assert!(!validates(&json!({
        "project": "demo",
        "items": [{"path": "a.rs", "unexpected": true}]
    })));
    assert!(!validates(&json!({
        "project": "demo",
        "items": [{"path": "a.rs"}],
        "unexpected": true
    })));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["items"]["items"]["additionalProperties"],
        false
    );

    for count in [1, 8] {
        let parsed = ToolCall::from_tool_name(
            "read_files",
            json!({
                "project": "demo",
                "items": (0..count)
                    .map(|index| json!({"path": format!("{index}.rs")}))
                    .collect::<Vec<_>>(),
                "with_line_numbers": true
            }),
        )
        .unwrap();
        assert!(matches!(parsed, ToolCall::ReadFiles { items, .. } if items.len() == count));
    }

    for invalid in [
        json!({"project": "demo", "items": []}),
        json!({
            "project": "demo",
            "items": (0..9).map(|index| json!({"path": format!("{index}.rs")})).collect::<Vec<_>>()
        }),
        json!({"project": "demo", "items": [{"path": " "}]}),
        json!({"project": "demo", "items": [{"path": "a.rs", "unexpected": true}]}),
        json!({"project": "demo", "items": [{"path": "a.rs"}], "unexpected": true}),
    ] {
        assert!(ToolCall::from_tool_name("read_files", invalid).is_err());
    }
}

#[tokio::test]
async fn read_files_returns_ordered_normalized_successes_after_out_of_order_completion() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-success";
    let runtime_project =
        register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let first_content = "one\ntwo\nthree\nfour\n";
    let second_content = "main\n";

    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .read_files(
                    "demo".to_string(),
                    vec![
                        item("src/lib.rs", Some(2), Some(2)),
                        item("src/main.rs", None, Some(1)),
                    ],
                    Some(true),
                )
                .await
        }
    });
    let request_a = next_read_request(&runtime, client_id).await;
    let request_b = next_read_request(&runtime, client_id).await;
    assert_eq!(request_a.kind, "file_read");
    assert_eq!(request_b.kind, "file_read");

    for request in [&request_b, &request_a] {
        let content = match request.path.as_deref() {
            Some("src/lib.rs") => first_content,
            Some("src/main.rs") => second_content,
            other => panic!("unexpected read path: {other:?}"),
        };
        complete_read(&runtime, client_id, request, content).await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], runtime_project);
    assert_eq!(result.output["requested_count"], 2);
    assert_eq!(result.output["returned_count"], 2);
    assert_eq!(result.output["succeeded_count"], 2);
    assert_eq!(result.output["failed_count"], 0);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["index"], 0);
    assert_eq!(items[0]["path"], "src/lib.rs");
    assert_eq!(items[0]["output"]["text"], "2 | two\n3 | three");
    assert_eq!(items[0]["output"]["format"], "numbered");
    assert_eq!(items[0]["output"]["start_line"], 2);
    assert_eq!(items[0]["output"]["returned_lines"], 2);
    assert_eq!(items[0]["output"]["end_line"], 3);
    assert_eq!(items[0]["output"]["has_more"], true);
    assert_eq!(items[0]["output"]["next_start_line"], 4);
    assert_eq!(
        items[0]["output"]["sha256"],
        format!("{:x}", Sha256::digest(first_content.as_bytes()))
    );
    assert_eq!(items[1]["index"], 1);
    assert_eq!(items[1]["path"], "src/main.rs");
    assert_eq!(items[1]["output"]["text"], "1 | main");
    assert_eq!(result.output["output_truncated"], false);
    assert!(result.output["next_index"].is_null());

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap();
}

#[tokio::test]
async fn read_file_dispatch_complete_success_is_sparse_after_session_recording() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-sparse-single";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("sparse read".to_string()));
    let session_id = session.session_id.clone();
    let auth = auth_context(None, true);
    let content = "one\ntwo";

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "src/lib.rs".to_string(),
                            start_line: None,
                            limit: None,
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    assert_eq!(request.path.as_deref(), Some("src/lib.rs"));
    complete_read(&runtime, client_id, &request, content).await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let item = &result.output["items"][0];
    assert_eq!(item["output"]["text"], "one\ntwo");
    assert_eq!(item["path"], "src/lib.rs");
    assert_eq!(
        item["output"]["sha256"],
        format!("{:x}", Sha256::digest(content.as_bytes()))
    );
    assert_eq!(item["output"]["total_lines"], 2);
    for omitted in [
        "format",
        "start_line",
        "limit",
        "returned_lines",
        "end_line",
        "has_more",
        "next_start_line",
        "continuation",
    ] {
        assert!(
            item["output"].get(omitted).is_none(),
            "complete full-file read item field {omitted} should be omitted: {item}"
        );
    }
    let sparse_bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(
        sparse_bytes <= 600,
        "complete sparse one-item read_files regressed above model-facing budget: {sparse_bytes} bytes"
    );
    eprintln!("read_files_sparse_complete_one_item_bytes={sparse_bytes}");

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| {
            panic!("sparse one-item read_files success must match schema: {error}")
        });

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    let finished = summary
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "tool_call_finished" && event.tool_name == "read_files")
        .expect("recorded read_files completion");
    assert!(
        finished
            .observed_paths
            .iter()
            .any(|path| path == "src/lib.rs"),
        "Session observation extraction must see the full read result before sparsification"
    );
}

#[tokio::test]
async fn read_file_dispatch_partial_success_keeps_full_range_cursor() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-partial-visible";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("read continuation session".to_string()),
    );
    let session_id = session.session_id.clone();
    let auth = auth_context(None, true);
    let content = "one\ntwo\nthree";

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "src/lib.rs".to_string(),
                            start_line: Some(2),
                            limit: Some(1),
                        }],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    complete_read(&runtime, client_id, &request, content).await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let item = &result.output["items"][0];
    assert_eq!(item["output"]["text"], "two");
    assert_eq!(item["output"]["format"], "plain");
    assert_eq!(item["path"], "src/lib.rs");
    assert_eq!(item["output"]["start_line"], 2);
    assert_eq!(item["output"]["limit"], 1);
    assert_eq!(item["output"]["total_lines"], 3);
    assert_eq!(item["output"]["returned_lines"], 1);
    assert_eq!(item["output"]["end_line"], 2);
    assert_eq!(item["output"]["has_more"], true);
    assert_eq!(item["output"]["next_start_line"], 3);
    let continuation = &item["continuation"];
    assert_eq!(continuation["kind"], "read_range");
    assert_eq!(continuation["safe_cursor"], true);
    assert_eq!(continuation["snapshot_stable"], false);
    assert_eq!(
        continuation["source_sha256"],
        format!("{:x}", Sha256::digest(content.as_bytes()))
    );
    assert_eq!(continuation["suggested_call"]["tool"], "read_files");
    assert_eq!(
        continuation["suggested_call"]["arguments"]["session_id"],
        session_id
    );
    let next_call = ToolCall::from_tool_name(
        continuation["suggested_call"]["tool"].as_str().unwrap(),
        continuation["suggested_call"]["arguments"].clone(),
    )
    .expect("read_files continuation suggested_call must parse");
    assert!(matches!(
        next_call,
        ToolCall::ReadFiles {
            project: ref next_project,
            ref items,
            session_id: Some(ref next_session_id),
            with_line_numbers: None,
            max_result_bytes: None,
        } if next_project == &project
            && items.len() == 1
            && items[0].path == "src/lib.rs"
            && items[0].start_line == Some(3)
            && items[0].limit == Some(1)
            && next_session_id == &session_id
    ));

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| {
            panic!("partial one-item read_files success must match schema: {error}")
        });
}

#[tokio::test]
async fn read_files_continuation_is_positional_not_snapshot_stable() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-source-change";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let auth = auth_context(None, true);
    let first_content = "one\ntwo\nthree";

    let first = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "src/lib.rs".to_string(),
                            start_line: Some(1),
                            limit: Some(1),
                        }],
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    complete_read(&runtime, client_id, &request, first_content).await;
    let first = first.await.unwrap();
    let first_item = &first.output["items"][0];
    let first_sha = first_item["output"]["sha256"].as_str().unwrap().to_string();
    let suggested = &first_item["continuation"]["suggested_call"];
    let next_call = ToolCall::from_tool_name(
        suggested["tool"].as_str().unwrap(),
        suggested["arguments"].clone(),
    )
    .expect("positional continuation must parse");

    // Insert a line before the cursor between calls. The next read is still a
    // deterministic absolute line read, but it is intentionally not frozen to
    // the first file snapshot.
    let changed_content = "zero\none\ntwo\nthree";
    let second = tokio::spawn({
        let runtime = runtime.clone();
        let auth = auth.clone();
        async move { runtime.dispatch_with_auth(next_call, Some(&auth)).await }
    });
    let request = next_read_request(&runtime, client_id).await;
    assert_eq!(request.start_line, Some(2));
    complete_read(&runtime, client_id, &request, changed_content).await;
    let second = second.await.unwrap();
    assert!(second.success, "{:?}", second.error);
    let second_item = &second.output["items"][0];
    assert_eq!(second_item["output"]["text"], "one");
    assert_ne!(second_item["output"]["sha256"], first_sha);
    assert_eq!(first_item["continuation"]["source_sha256"], first_sha);
    assert_eq!(first_item["continuation"]["snapshot_stable"], false);
}

#[tokio::test]
async fn read_file_dispatch_complete_explicit_range_keeps_full_range_metadata() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-explicit-range-visible";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let auth = auth_context(None, true);
    let content = "one\ntwo";

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project: project,
                        items: vec![crate::tool_runtime::ReadFilesItem {
                            path: "src/lib.rs".to_string(),
                            start_line: Some(1),
                            limit: Some(2),
                        }],
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    complete_read(&runtime, client_id, &request, content).await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let item = &result.output["items"][0];
    assert_eq!(item["output"]["text"], "one\ntwo");
    assert_eq!(item["output"]["format"], "plain");
    assert_eq!(item["path"], "src/lib.rs");
    assert_eq!(item["output"]["start_line"], 1);
    assert_eq!(item["output"]["limit"], 2);
    assert_eq!(item["output"]["total_lines"], 2);
    assert_eq!(item["output"]["returned_lines"], 2);
    assert_eq!(item["output"]["end_line"], 2);
    assert_eq!(item["output"]["has_more"], false);
    assert!(item["output"]["next_start_line"].is_null());
    assert!(item.get("continuation").is_none());

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| {
            panic!("explicit-range one-item read_files success must match schema: {error}")
        });
}

#[tokio::test]
async fn read_files_dispatch_complete_batch_is_sparse_and_schema_valid() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-sparse-batch";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project,
                        items: vec![
                            item("src/lib.rs", None, None),
                            item("src/main.rs", None, None),
                        ],
                        session_id: None,
                        with_line_numbers: Some(true),
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let requests = [
        next_read_request(&runtime, client_id).await,
        next_read_request(&runtime, client_id).await,
    ];
    for request in &requests {
        let content = match request.path.as_deref() {
            Some("src/lib.rs") => "lib",
            Some("src/main.rs") => "main",
            other => panic!("unexpected batch read path: {other:?}"),
        };
        complete_read(&runtime, client_id, request, content).await;
    }

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    for omitted in [
        "project",
        "requested_count",
        "returned_count",
        "succeeded_count",
        "failed_count",
        "output_truncated",
        "next_index",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "complete read_files batch field {omitted} should be omitted: {}",
            result.output
        );
    }
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["success"], true);
        assert!(item["error"].is_null());
        assert_eq!(item["output"]["format"], "numbered");
        assert!(item["output"].get("path").is_none());
        assert!(item["output"]["sha256"].as_str().is_some());
        assert_eq!(item["output"]["total_lines"], 1);
        for omitted in [
            "start_line",
            "limit",
            "returned_lines",
            "end_line",
            "has_more",
            "next_start_line",
        ] {
            assert!(
                item["output"].get(omitted).is_none(),
                "complete batch item field {omitted} should be omitted: {item}"
            );
        }
    }
    let sparse_bytes = serde_json::to_vec(&result.output).unwrap().len();
    assert!(
        sparse_bytes <= 600,
        "complete two-file sparse batch regressed above model-facing budget: {sparse_bytes} bytes"
    );
    eprintln!("read_files_sparse_complete_bytes={sparse_bytes}");

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| panic!("sparse read_files batch must match schema: {error}"));
}

#[tokio::test]
async fn read_files_partial_item_has_actionable_item_continuation() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-batch-item-continuation";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("batch read continuation session".to_string()),
    );
    let session_id = session.session_id.clone();
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        let session_id = session_id.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project,
                        items: vec![
                            item("src/lib.rs", Some(2), Some(1)),
                            item("src/main.rs", None, None),
                        ],
                        session_id: Some(session_id),
                        with_line_numbers: Some(true),
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..2 {
        let request = next_read_request(&runtime, client_id).await;
        let content = match request.path.as_deref() {
            Some("src/lib.rs") => "one\ntwo\nthree",
            Some("src/main.rs") => "main",
            other => panic!("unexpected read path: {other:?}"),
        };
        complete_read(&runtime, client_id, &request, content).await;
    }

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["output_truncated"], false);
    assert!(result.output.get("continuation").is_none());
    let items = result.output["items"].as_array().unwrap();
    let continuation = &items[0]["continuation"];
    assert_eq!(continuation["kind"], "read_range");
    assert_eq!(continuation["safe_cursor"], true);
    assert_eq!(continuation["snapshot_stable"], false);
    let suggested = &continuation["suggested_call"];
    assert_eq!(suggested["arguments"]["session_id"], session_id);
    let next_call = ToolCall::from_tool_name(
        suggested["tool"].as_str().unwrap(),
        suggested["arguments"].clone(),
    )
    .expect("item continuation must parse");
    assert!(matches!(
        next_call,
        ToolCall::ReadFiles {
            project: ref next_project,
            ref items,
            session_id: Some(ref next_session_id),
            with_line_numbers: Some(true),
            max_result_bytes: None,
        } if next_project == &project
            && items.len() == 1
            && items[0].path == "src/lib.rs"
            && items[0].start_line == Some(3)
            && items[0].limit == Some(1)
            && next_session_id == &session_id
    ));
    assert!(items[1].get("continuation").is_none());

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| panic!("partial read_files item must match schema: {error}"));
}

#[tokio::test]
async fn read_files_dispatch_large_default_batch_uses_sparse_fit_before_budget() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-sparse-budget-order";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let auth = auth_context(None, true);
    let paths = (0..8)
        .map(|index| format!("src/{index}.rs"))
        .collect::<Vec<_>>();
    let expected_text = "x".repeat(7_400);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        let paths = paths.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project,
                        items: paths.iter().map(|path| item(path, None, None)).collect(),
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..8 {
        let request = next_read_request(&runtime, client_id).await;
        complete_read(&runtime, client_id, &request, &expected_text).await;
    }

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.output.get("output_truncated").is_none(),
        "{}",
        result.output
    );
    assert!(
        result.output.get("next_index").is_none(),
        "{}",
        result.output
    );
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items.len(), 8);
    for item in items {
        assert_eq!(item["output"]["text"], expected_text);
        assert!(item["output"].get("start_line").is_none());
        assert!(item["output"].get("next_start_line").is_none());
    }
}

#[tokio::test]
async fn read_files_dispatch_mixed_batch_keeps_outer_and_failure_semantics() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "read-sparse-mixed";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let expected_project = project.clone();
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project,
                        items: vec![item("good.txt", None, Some(1)), item(".env", None, None)],
                        session_id: None,
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    assert_eq!(request.path.as_deref(), Some("good.txt"));
    complete_read(&runtime, client_id, &request, "ok\nmore").await;

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["project"], expected_project);
    assert_eq!(result.output["requested_count"], 2);
    assert_eq!(result.output["returned_count"], 2);
    assert_eq!(result.output["succeeded_count"], 1);
    assert_eq!(result.output["failed_count"], 1);
    assert_eq!(result.output["output_truncated"], false);
    assert!(result.output["next_index"].is_null());

    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["success"], true);
    assert_eq!(items[0]["path"], "good.txt");
    assert_eq!(items[0]["output"]["text"], "ok");
    assert_eq!(items[0]["output"]["has_more"], true);
    assert_eq!(items[0]["output"]["next_start_line"], 2);
    assert_eq!(items[0]["continuation"]["kind"], "read_range");
    let suggested = &items[0]["continuation"]["suggested_call"];
    ToolCall::from_tool_name(
        suggested["tool"].as_str().unwrap(),
        suggested["arguments"].clone(),
    )
    .expect("successful mixed-batch item continuation must remain parseable");
    assert_eq!(items[1]["success"], false);
    assert_eq!(items[1]["path"], ".env");
    assert_eq!(items[1]["output"]["error_kind"], "read_file_failed");
    assert_eq!(items[1]["output"]["reason_code"], "sensitive_path");
    assert_eq!(items[1]["output"]["state_changed"], false);
    assert!(items[1]["error"].as_str().is_some());

    let schema = crate::tool_runtime::registry::output_schema_for_tool("read_files");
    let serialized = serde_json::to_value(&result).unwrap();
    crate::tool_runtime::startup_brief::validate_schema_instance_for_test(&serialized, &schema)
        .unwrap_or_else(|error| panic!("mixed sparse/full read batch must match schema: {error}"));
}

#[tokio::test]
async fn read_files_isolates_mixed_failures_without_leaking_absolute_paths() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-mixed";
    register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;

    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .read_files(
                    "demo".to_string(),
                    vec![
                        item("good.txt", None, None),
                        item("missing.txt", None, None),
                        item(".env", None, None),
                        item("invalid.bin", None, None),
                    ],
                    None,
                )
                .await
        }
    });

    for _ in 0..3 {
        let request = next_read_request(&runtime, client_id).await;
        match request.path.as_deref() {
            Some("good.txt") => complete_read(&runtime, client_id, &request, "ok\n").await,
            Some("missing.txt") => {
                complete_patch_agent_request(
                    &runtime,
                    client_id,
                    &request.request_id,
                    -1,
                    "",
                    "read_file failed: not_found",
                )
                .await;
            }
            Some("invalid.bin") => {
                complete_patch_agent_request(
                    &runtime,
                    client_id,
                    &request.request_id,
                    -1,
                    "",
                    "read_file failed: invalid_utf8",
                )
                .await;
            }
            other => panic!("sensitive or unexpected path reached Runner: {other:?}"),
        }
    }

    let result = task.await.unwrap();
    assert!(result.success);
    assert_eq!(result.output["succeeded_count"], 1);
    assert_eq!(result.output["failed_count"], 3);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["success"], true);
    assert_eq!(items[1]["output"]["reason_code"], "not_found");
    assert_eq!(items[2]["output"]["reason_code"], "sensitive_path");
    assert_eq!(items[3]["output"]["reason_code"], "invalid_utf8");
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains(&root.path().to_string_lossy().to_string()));
    assert!(!serialized.contains("os error"));
}

#[tokio::test]
async fn read_files_max_batch_can_enqueue_all_eight_independent_reads() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-concurrency";
    register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .read_files(
                    "demo".to_string(),
                    (0..8)
                        .map(|index| item(&format!("{index}.txt"), None, Some(1)))
                        .collect(),
                    None,
                )
                .await
        }
    });

    let mut active = Vec::new();
    for _ in 0..8 {
        active.push(next_read_request(&runtime, client_id).await);
    }
    assert_eq!(active.len(), 8);
    let extra_before_completion = runtime
        .runner_registry
        .poll(RunnerPollRequest {
            client_id: client_id.to_string(),
            runner_instance_id: "inst".to_string(),
        })
        .await
        .unwrap();
    assert!(
        extra_before_completion.is_none(),
        "max-size read batch enqueued work beyond its eight-item bound"
    );

    for request in active.into_iter().rev() {
        complete_read(&runtime, client_id, &request, "value\n").await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["succeeded_count"], 8);
    assert_eq!(result.output["items"].as_array().unwrap().len(), 8);
}

#[tokio::test]
async fn read_files_deadline_preserves_completed_results_and_cancels_unfinished_reads() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests().with_read_files_deadline(Duration::from_millis(75));
    let client_id = "batch-deadline";
    register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .read_files(
                    "demo".to_string(),
                    vec![
                        item("fast.txt", None, None),
                        item("slow-a.txt", None, None),
                        item("slow-b.txt", None, None),
                    ],
                    None,
                )
                .await
        }
    });
    let requests = [
        next_read_request(&runtime, client_id).await,
        next_read_request(&runtime, client_id).await,
        next_read_request(&runtime, client_id).await,
    ];
    let fast = requests
        .iter()
        .find(|request| request.path.as_deref() == Some("fast.txt"))
        .unwrap();
    complete_read(&runtime, client_id, fast, "ready\n").await;

    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("batch deadline should finish promptly")
        .unwrap();
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["success"], true);
    assert_eq!(items[0]["output"]["text"], "ready");
    assert_eq!(items[1]["output"]["reason_code"], "timeout");
    assert_eq!(items[2]["output"]["reason_code"], "timeout");

    for request in requests
        .iter()
        .filter(|request| request.path.as_deref() != Some("fast.txt"))
    {
        let late = runtime
            .runner_registry
            .complete(RunnerResultRequest {
                client_id: client_id.to_string(),
                runner_instance_id: "inst".to_string(),
                request_id: request.request_id.clone(),
                exit_code: Some(0),
                stdout: Some(canonical_agent_file_read_output("late\n", 1)),
                stderr: Some(String::new()),
                duration_ms: Some(100),
                error: None,
            })
            .await;
        assert!(late.is_err(), "timed-out Runner request was not cancelled");
    }
}

#[tokio::test]
async fn read_files_records_one_outer_session_event_and_keeps_metadata_outer_only() {
    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-session";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("batch read".to_string()));
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        async move {
            let auth = auth_context(None, true);
            runtime
                .dispatch_with_auth(
                    ToolCall::ReadFiles {
                        project,
                        items: vec![item("a.rs", None, None), item("b.rs", None, None)],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: None,
                    },
                    Some(&auth),
                )
                .await
        }
    });
    for _ in 0..2 {
        let request = next_read_request(&runtime, client_id).await;
        complete_read(&runtime, client_id, &request, "session-private-text\n").await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
    assert!(result.output.get("permission").is_none());
    for item in result.output["items"].as_array().unwrap() {
        let serialized = serde_json::to_string(item).unwrap();
        assert!(!serialized.contains("session_recorded"));
        assert!(!serialized.contains("session_event_id"));
        assert!(!serialized.contains("permission"));
    }

    let summary = runtime
        .sessions
        .summary(&session.session_id, Some(20))
        .unwrap();
    assert_eq!(summary.counts.tool_calls, 1);
    assert_eq!(summary.counts.read_like, 1);
    let event = finished_event(&summary, "read_files");
    assert_eq!(event.status.as_deref(), Some("succeeded"));
    assert_eq!(
        event.observed_paths,
        vec!["a.rs".to_string(), "b.rs".to_string()]
    );
    let ledger = serde_json::to_string(&summary.events).unwrap();
    assert!(!ledger.contains("session-private-text"));
}

#[tokio::test]
async fn read_files_direct_session_overlay_pressure_keeps_final_response_under_hard_cap() {
    use crate::tool_runtime::sessions::{
        SessionContextRevisionAck, SessionTransport, ToolCallRecorderMetadata,
    };
    use webcodex_core::runtime_contract::MODEL_INSPECTION_MAX_RESULT_BYTES as MAX_SERIALIZED_OUTPUT_BYTES;

    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-direct-final-cap";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("direct final cap".to_string()));
    assert_eq!(
        seed_model_facing_recovery_events(&runtime, &session.session_id, &project, 20),
        20
    );
    let auth = auth_context(None, true);

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let project = project.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .dispatch_with_auth_transport_options_and_metadata(
                    ToolCall::ReadFiles {
                        project,
                        items: vec![
                            item("a.rs", None, None),
                            item("b.rs", None, None),
                            item("c.rs", None, None),
                            item("d.rs", None, None),
                        ],
                        session_id: Some(session_id),
                        with_line_numbers: None,
                        max_result_bytes: Some(MAX_SERIALIZED_OUTPUT_BYTES),
                    },
                    Some(&auth),
                    SessionTransport::Mcp,
                    ToolCallRecorderMetadata {
                        ack_session_context_revision: SessionContextRevisionAck::Revision(0),
                        ..Default::default()
                    },
                )
                .await
        }
    });
    let content = "x".repeat(150 * 1024);
    for _ in 0..4 {
        let request = next_read_request(&runtime, client_id).await;
        complete_read(&runtime, client_id, &request, &content).await;
    }

    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_continuity").is_none());
    assert!(result.output.get("session_recovery").is_none());
    assert!(result.output.get("session_context_revision").is_none());
    let serialized_len = serde_json::to_vec(&result).unwrap().len();
    assert!(
        serialized_len <= MAX_SERIALIZED_OUTPUT_BYTES,
        "direct Session overlays pushed read_files final response above the 512 KiB inspection hard cap: {serialized_len} bytes"
    );
}

#[tokio::test]
async fn read_files_outer_recording_session_preserves_complete_sparse_shape() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
        ToolProtocolCapabilities, ToolTransport,
    };
    use crate::tool_runtime::sessions::SessionContextRevisionAck;

    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-outer-sparse";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime
        .sessions
        .start_session(Some(project.clone()), Some("outer sparse read".to_string()));
    let auth = auth_context(None, true);
    let arguments = json!({
        "project": project,
        "items": [{"path": "a.rs"}]
    });

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_invocation_metadata(
                    ToolCallRequest {
                        tool_name: "read_files".to_string(),
                        arguments,
                    },
                    ToolCallContext {
                        transport: ToolTransport::Mcp,
                        session_id: Some(&session_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: false,
                        host_file_import_trust: HostFileImportTrust::Untrusted,
                    },
                    ToolInvocationMetadata {
                        ack_session_context_revision: SessionContextRevisionAck::Revision(0),
                        ..Default::default()
                    },
                    ToolProtocolCapabilities {
                        context_continuity: true,
                        context_sidecar: true,
                        ..Default::default()
                    },
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    complete_read(&runtime, client_id, &request, "small\n").await;

    let result = task.await.unwrap().result.expect("model-facing result");
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_recorded").is_none());
    assert!(result.output.get("session_event_id").is_none());
    assert!(result.output.get("session_id").is_none());
    assert!(result.output.get("session_context_revision").is_none());
    assert!(result.output.get("session_continuity").is_none());
    assert!(result.output.get("session_recovery").is_none());
    for omitted in [
        "project",
        "requested_count",
        "returned_count",
        "succeeded_count",
        "failed_count",
        "output_truncated",
        "next_index",
    ] {
        assert!(
            result.output.get(omitted).is_none(),
            "outer recording changed complete sparse field {omitted}: {}",
            result.output
        );
    }
    assert_eq!(result.output["items"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["items"][0]["output"]["text"], "small");
    assert!(result.output["items"][0]["output"].get("path").is_none());
}

#[tokio::test]
async fn read_files_outer_recorder_observes_canonical_batch_before_primary_projection() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
        ToolProtocolCapabilities, ToolTransport,
    };

    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-canonical-before-projection";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let recording_session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("canonical batch evidence".to_string()),
    );
    let business_session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("inner business read".to_string()),
    );
    let auth = auth_context(None, true);
    let arguments = json!({
        "project": project,
        "items": [{"path": "a.rs"}, {"path": "b.rs"}],
        "session_id": business_session.session_id,
        "max_result_bytes": 8192
    });

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let session_id = recording_session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_invocation_metadata(
                    ToolCallRequest {
                        tool_name: "read_files".to_string(),
                        arguments,
                    },
                    ToolCallContext {
                        transport: ToolTransport::Mcp,
                        session_id: Some(&session_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: false,
                        host_file_import_trust: HostFileImportTrust::Untrusted,
                    },
                    ToolInvocationMetadata::default(),
                    ToolProtocolCapabilities::default(),
                )
                .await
        }
    });
    let content = format!("{}\n", "x".repeat(3_000));
    for _ in 0..2 {
        let request = next_read_request(&runtime, client_id).await;
        complete_read(&runtime, client_id, &request, &content).await;
    }

    let result = task.await.unwrap().result.expect("model-facing result");
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["output_truncated"], true);
    assert_eq!(result.output["truncation_reason"], "batch_response_budget");
    assert_eq!(result.output["returned_count"], 1);
    assert_eq!(result.output["next_index"], 1);
    assert_eq!(
        result.output["continuation"]["suggested_call"]["arguments"]["session_id"],
        business_session.session_id,
        "model continuation must preserve the concrete business Session rather than inherit the outer recorder"
    );

    let recording_summary = runtime
        .sessions
        .summary(&recording_session.session_id, Some(20))
        .unwrap();
    let recording_event = finished_event(&recording_summary, "read_files");
    assert_eq!(
        recording_event.observed_paths,
        vec!["a.rs".to_string(), "b.rs".to_string()],
        "outer recorder must consume canonical batch evidence before the terminal model budget"
    );
    let business_summary = runtime
        .sessions
        .summary(&business_session.session_id, Some(20))
        .unwrap();
    let business_event = finished_event(&business_summary, "read_files");
    assert_eq!(
        business_event.observed_paths,
        vec!["a.rs".to_string(), "b.rs".to_string()],
        "concrete business Session must remain independently recorded with canonical evidence"
    );
}

#[tokio::test]
async fn read_files_ignores_context_ack_and_preserves_bounded_attention() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
        ToolProtocolCapabilities, ToolTransport,
    };
    use crate::tool_runtime::sessions::{
        PostSessionMessageInput, SessionContextRevisionAck, SessionMessageKind,
        SessionMessagePriority,
    };
    use webcodex_workspace::file_read_range::MAX_SERIALIZED_OUTPUT_BYTES;

    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-overlay-bound";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("bounded recovery overlays".to_string()),
    );
    assert_eq!(
        seed_model_facing_recovery_events(&runtime, &session.session_id, &project, 50),
        50
    );
    assert_eq!(
        seed_large_changed_path_recovery_events(&runtime, &session.session_id, &project, 50,),
        100
    );
    for kind in [
        SessionMessageKind::Guidance,
        SessionMessageKind::Question,
        SessionMessageKind::Risk,
        SessionMessageKind::Todo,
    ] {
        for index in 0..5 {
            runtime
                .sessions
                .post_message_with_ack(
                    PostSessionMessageInput {
                        session_id: session.session_id.clone(),
                        kind,
                        message: format!("overlay-{kind:?}-{index}-{}", "m".repeat(7_900)),
                        tags: vec!["bounded-overlay".to_string()],
                        reply_to: None,
                        priority: SessionMessagePriority::High,
                    },
                    kind == SessionMessageKind::Guidance,
                )
                .unwrap();
        }
    }
    // Seed an explicit retained attempt boundary after the deliberately large
    // historical overlay. Current validation evidence must not infer a complete
    // attempt from a truncated Session tail.
    runtime
        .sessions
        .ensure_coding_session(crate::tool_runtime::sessions::CodingSessionRequest {
            project: project.clone(),
            authority_fingerprint:
                crate::tool_runtime::sessions::TEST_ONLY_PROJECT_SESSION_AUTHORITY_FINGERPRINT
                    .to_string(),
            resume_session_id: Some(session.session_id.clone()),
            instruction: Some("validate bounded recovery overlays".to_string()),
            mode: crate::tool_runtime::SessionMode::Normal,
            guards: crate::tool_runtime::sessions::SessionGuards::default(),
            execution_context: None,
            project_instructions: None,
            transport: crate::tool_runtime::sessions::SessionTransport::Api,
            context_refreshed: true,
            write_scope_verified: true,
        })
        .unwrap();
    let assertion_name = "recovery websocket validation";
    let validation_request = json!({
        "project": project,
        "executable": "cargo",
        "args": ["test", "before-recovery"],
        "cwd": ".",
        "purpose": "test",
        "assertion_name": assertion_name,
    });
    let (_, recorder_metadata) =
        ToolCall::from_tool_name_with_recorder_metadata("run_process", validation_request.clone())
            .unwrap();
    let audited = crate::tool_runtime::tool_audit::session_log_arguments_for_tool_request(
        "run_process",
        &validation_request,
    );
    let validation_start = runtime.sessions.record_tool_call_started_with_metadata(
        Some(&session.session_id),
        crate::tool_runtime::sessions::SessionTransport::Mcp,
        "run_process",
        &audited,
        Some(project.clone()),
        recorder_metadata,
        crate::tool_runtime::sessions::session_tool_contract("run_process"),
    );
    runtime.sessions.record_tool_call_finished(
        validation_start,
        false,
        &json!({
            "exit_code": 101,
            "purpose": "test",
            "execution_state": "completed",
            "stdout_tail": "validation failed\n",
            "stderr_tail": "",
            "stdout_truncated": false,
            "stderr_truncated": false,
        }),
        Some("validation failed"),
        None,
    );

    let auth = auth_context(None, true);
    let arguments = json!({
        "project": project,
        "items": [{"path": "a.rs"}]
    });

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_invocation_metadata(
                    ToolCallRequest {
                        tool_name: "read_files".to_string(),
                        arguments,
                    },
                    ToolCallContext {
                        transport: ToolTransport::Mcp,
                        session_id: Some(&session_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: false,
                        host_file_import_trust: HostFileImportTrust::Untrusted,
                    },
                    ToolInvocationMetadata {
                        context_request: vec!["webcodex.workflow".to_string()],
                        ack_session_context_revision: SessionContextRevisionAck::Revision(0),
                        ..Default::default()
                    },
                    ToolProtocolCapabilities {
                        context_continuity: true,
                        context_sidecar: true,
                        ..Default::default()
                    },
                )
                .await
        }
    });
    let request = next_read_request(&runtime, client_id).await;
    complete_read(&runtime, client_id, &request, "small\n").await;

    let result = task.await.unwrap().result.expect("model-facing result");
    assert!(result.success, "{:?}", result.error);
    for field in [
        "session_context_revision",
        "session_continuity",
        "session_recovery",
    ] {
        assert!(result.output.get(field).is_none(), "{field}");
    }
    assert_eq!(result.output["session_attention"]["requires_ack"], true);
    let attention_messages = result.output["session_attention"]["messages"]
        .as_array()
        .unwrap();
    assert_eq!(attention_messages.len(), 1);
    assert_eq!(attention_messages[0]["message_truncated"], true);
    assert_eq!(result.output["session_attention"]["truncated"], true);
    assert_eq!(result.output["session_attention"]["omitted_count"], 4);
    assert_eq!(result.output["context_projection"]["timing"], "post_tool");
    assert_eq!(
        result.output["context_projection"]["applies_to_current_effect"],
        false
    );
    assert_eq!(
        result.output["context_projection"]["materials"][0]["key"],
        "webcodex.workflow"
    );
    assert!(result.output.get("output_truncated").is_none());
    assert_eq!(result.output["items"].as_array().unwrap().len(), 1);
    let serialized_len = serde_json::to_vec(&result).unwrap().len();
    eprintln!("maximal_bounded_session_overlay_result_bytes={serialized_len}");
    assert!(
        serialized_len <= MAX_SERIALIZED_OUTPUT_BYTES,
        "bounded Session recovery/handoff/attention overlays alone exceeded the 256 KiB hard cap: {serialized_len} bytes"
    );
}

#[tokio::test]
async fn read_files_outer_recording_session_keeps_final_response_under_hard_cap() {
    use crate::tool_runtime::kernel::{
        HostFileImportTrust, ToolCallContext, ToolCallRequest, ToolInvocationMetadata,
        ToolProtocolCapabilities, ToolTransport,
    };
    use crate::tool_runtime::sessions::SessionContextRevisionAck;
    use webcodex_core::runtime_contract::MODEL_INSPECTION_MAX_RESULT_BYTES as MAX_SERIALIZED_OUTPUT_BYTES;

    let root = tempfile::tempdir().unwrap();
    let runtime = ToolRuntime::new_for_tests();
    let client_id = "batch-final-cap";
    let project = register_runner_project_at_path(&runtime, client_id, "demo", root.path()).await;
    let session = runtime.sessions.start_session(
        Some(project.clone()),
        Some("final response cap".to_string()),
    );
    assert_eq!(
        seed_model_facing_recovery_events(&runtime, &session.session_id, &project, 20),
        20
    );
    let auth = auth_context(None, true);
    let arguments = json!({
        "project": project,
        "items": [
            {"path": "a.rs"},
            {"path": "b.rs"},
            {"path": "c.rs"},
            {"path": "d.rs"}
        ],
        "max_result_bytes": MAX_SERIALIZED_OUTPUT_BYTES
    });

    let task = tokio::spawn({
        let runtime = runtime.clone();
        let session_id = session.session_id.clone();
        let auth = auth.clone();
        async move {
            runtime
                .call_tool_with_invocation_metadata(
                    ToolCallRequest {
                        tool_name: "read_files".to_string(),
                        arguments,
                    },
                    ToolCallContext {
                        transport: ToolTransport::Mcp,
                        session_id: Some(&session_id),
                        auth: Some(&auth),
                        window: None,
                        record_oauth_scope_denials: false,
                        host_file_import_trust: HostFileImportTrust::Untrusted,
                    },
                    ToolInvocationMetadata {
                        ack_session_context_revision: SessionContextRevisionAck::Revision(0),
                        ..Default::default()
                    },
                    ToolProtocolCapabilities {
                        context_continuity: true,
                        context_sidecar: true,
                        ..Default::default()
                    },
                )
                .await
        }
    });
    let content = "x".repeat(150 * 1024);
    for _ in 0..4 {
        let request = next_read_request(&runtime, client_id).await;
        complete_read(&runtime, client_id, &request, &content).await;
    }

    let outcome = task.await.unwrap();
    assert!(outcome.success);
    let result = outcome.result.expect("model-facing result");
    assert!(result.success, "{:?}", result.error);
    assert!(result.output.get("session_continuity").is_none());
    assert!(result.output.get("session_recovery").is_none());
    assert!(result.output.get("session_context_revision").is_none());

    assert_eq!(result.output["output_truncated"], true);
    assert_eq!(result.output["truncation_reason"], "hard_result_cap");
    let returned_count = result.output["returned_count"].as_u64().unwrap();
    let next_index = result.output["next_index"].as_u64().unwrap();
    assert!(returned_count < 4);
    assert_eq!(next_index, returned_count);
    let serialized_len = serde_json::to_vec(&result).unwrap().len();
    assert!(
        serialized_len <= MAX_SERIALIZED_OUTPUT_BYTES,
        "outer Session overlays pushed read_files final response above the 512 KiB inspection hard cap: {serialized_len} bytes"
    );
}
