use crate::projects::ProjectConfig;
use crate::runner_http::RunnerRegistry;
use crate::tool_runtime::{RuntimeInfo, ToolRuntime, ToolSpec};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub(in crate::tool_runtime::tests) const SAMPLE_PROJECT: &str = "agent:oe:private-drop";
pub(in crate::tool_runtime::tests) const UNIT_TOOL_FIXTURES: &[&str] = &[
    "list_tools",
    "list_projects",
    "list_runners",
    "runtime_status",
];

pub(in crate::tool_runtime::tests) fn test_runtime() -> ToolRuntime {
    ToolRuntime::new_for_tests()
}

pub(in crate::tool_runtime::tests) fn registered_tool_names() -> Vec<String> {
    crate::tool_runtime::registered_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

pub(in crate::tool_runtime::tests) fn sample_tool_args(name: &str) -> Value {
    let spec = crate::tool_runtime::registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("missing tool spec for {name}"));
    sample_tool_args_for_spec(&spec)
}

pub(in crate::tool_runtime::tests) fn sample_tool_args_for_spec(spec: &ToolSpec) -> Value {
    let required = spec.input_schema["required"]
        .as_array()
        .unwrap_or_else(|| panic!("{} schema should list required fields", spec.name));
    if required.is_empty() && UNIT_TOOL_FIXTURES.contains(&spec.name.as_str()) {
        return Value::Null;
    }

    let mut args: serde_json::Map<String, Value> = required
        .iter()
        .map(|field| {
            let field = field
                .as_str()
                .unwrap_or_else(|| panic!("{} required field should be a string", spec.name));
            (field.to_string(), sample_field_value(field))
        })
        .collect();
    // Conditional project-source schemas cannot express one representative
    // source through the top-level `required` array. Keep fixtures aligned with
    // each tool's metadata contract.
    match spec.name.as_str() {
        "work_on_project" => {
            args.insert("project".to_string(), json!(SAMPLE_PROJECT));
        }
        "observe_jobs" => {
            args.insert("items".to_string(), json!([{"job_id": "job_123"}]));
        }
        "plugin_tool" => {
            args.insert("action".to_string(), json!("list"));
        }
        _ => {}
    }
    Value::Object(args)
}

pub(in crate::tool_runtime::tests) fn sample_field_value(field: &str) -> Value {
    match field {
        "project" => json!(SAMPLE_PROJECT),
        "command" => json!("true"),
        "executable" => json!("git"),
        "language" => json!("sh"),
        "script" => json!("true"),
        "patch" => json!("diff --git a/a b/a\n"),
        "paths" => json!(["old.txt"]),
        "items" => json!([{"path": "src/lib.rs"}]),
        "queries" => json!([{"pattern": "fn main"}]),
        "path" => json!("src/lib.rs"),
        "old" | "old_text" => json!("a"),
        "new" | "new_text" => json!("b"),
        "pattern" => json!("fn main"),
        "text" => json!("// hi\n"),
        "content" => json!("fn main() {}\n"),
        "instruction" => json!("implement the requested change"),
        "title" => json!("Durable agent work"),
        "include_project_instructions" | "include_workflow_guidance" => json!(false),
        "content_base64" => json!("AA=="),
        "openaiFileIdRefs" => json!([{
            "download_url": "https://files.oaiusercontent.com/test",
            "file_id": "file_test"
        }]),
        "start_line" | "end_line" | "line" | "column" | "offset" => json!(1),
        "upload_id" => json!("wc_upload_test_1"),
        "edits" => json!([{"kind": "replace_exact", "old_text": "a", "new_text": "b"}]),
        "changes" => json!([{
            "kind": "edit",
            "path": "src/lib.rs",
            "expected_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "edits": [{"kind": "replace_exact", "old_text": "a", "new_text": "b"}]
        }]),
        "prompt" => json!("summarize"),
        "query" => json!("ToolRuntime"),
        "diff" => json!("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n"),
        "job_id" => json!("job_123"),
        "idempotency_key" => json!("sample-detached-key"),
        "handle" => json!("reviewer"),
        "display_name" => json!("Reviewer"),
        "agent_id" => json!(format!("wc_dagent_{}", "a".repeat(32))),
        "assignee_agent_id" => json!(format!("wc_dagent_{}", "a".repeat(32))),
        "task_id" => json!(format!("wc_agent_task_{}", "1".repeat(32))),
        "attempt_id" => json!(format!("wc_agent_task_attempt_{}", "2".repeat(32))),
        "attempt_fence" => json!(format!("wc_agent_task_fence_{}", "3".repeat(32))),
        "attempt_controller_generation" | "expected_generation" => json!(1),
        "outcome" => json!("succeeded"),
        "agent_ids" => json!([format!("wc_dagent_{}", "a".repeat(32))]),
        "endpoint_id" => json!(format!("wc_endpoint_{}", "b".repeat(32))),
        "conversation_id" => json!(format!("wc_conv_{}", "c".repeat(32))),
        "delivery_ids" => json!([format!("wc_delivery_{}", "d".repeat(32))]),
        "expected_profile_revision" => json!(1),
        "expected_controller_generation" => json!(1),
        "wake_id" => json!(format!("wc_wake_{}", "e".repeat(32))),
        "consume_token" => json!(format!("wc_wake_consume_{}", "f".repeat(32))),
        "host" => json!("ChatGPT"),
        "body" => json!("hello"),
        "provider_id" => json!("codex"),
        "run_id" => json!("wc_agent_run_sample_1234"),
        "shell_id" => json!("wc_shell_123"),
        "session_id" => json!("wc_sess_existing"),
        "checkpoint_id" => json!("wc_ckpt_1234"),
        "confirm" => json!(true),
        "client_id" => json!("oe"),
        "application_id" => json!(format!("application_{}", "a".repeat(32))),
        "display_id" => json!(format!("display_{}", "a".repeat(32))),
        "snapshot_generation" => json!(1),
        "x" | "y" => json!(0),
        "surface_id" => json!("surface_test"),
        "element_id" => json!("element_test"),
        "action" => json!("focus"),
        "key" => json!("tab"),
        "id" => json!("private-drop"),
        "base_commit" => json!("a".repeat(40)),
        "head_commit" => json!("b".repeat(40)),
        "expected_head" => json!("a".repeat(40)),
        "expected_revision" => json!(format!("sha256:{}", "a".repeat(64))),
        "name" => json!("Private Drop"),
        "kind" => json!("note"),
        "message" => json!("hello"),
        "answer" => json!("done"),
        "completion_key" => json!("sample-completion-key"),
        "expected_assignment_fence" => json!(format!("wsa1_{}", "A".repeat(43))),
        "message_id" => json!("wc_msg_0001"),
        "execution_context" => json!({}),
        other => panic!("missing sample value for required field {other}"),
    }
}

/// Helper: fetch a ToolSpec by name from the runtime.
pub(in crate::tool_runtime::tests) fn spec_named<'a>(
    specs: &'a [ToolSpec],
    name: &str,
) -> &'a ToolSpec {
    specs
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("tool '{}' missing from specs", name))
}

/// Helper: the `required` field of a tool's input schema, as Strings.
pub(in crate::tool_runtime::tests) fn required_fields(spec: &ToolSpec) -> Vec<String> {
    spec.input_schema["required"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(in crate::tool_runtime::tests) fn seed_model_facing_recovery_events(
    runtime: &ToolRuntime,
    session_id: &str,
    project: &str,
    count: usize,
) -> u64 {
    use crate::tool_runtime::sessions::{
        SessionContextRevisionAck, SessionTransport, ToolCallRecorderMetadata,
    };

    let mut revision = 0u64;
    for index in 0..count {
        let start = runtime.sessions.record_tool_call_started_with_metadata(
            Some(session_id),
            SessionTransport::Mcp,
            "run_process",
            &json!({"project": project, "executable": "true"}),
            Some(project.to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: SessionContextRevisionAck::Revision(revision),
                ..Default::default()
            },
            crate::tool_runtime::sessions::session_tool_contract("run_process"),
        );
        let evidence = format!("event-{index:02}-{}", "x".repeat(760));
        let recorded = runtime
            .sessions
            .record_model_facing_tool_call_finished(
                start,
                true,
                &json!({
                    "exit_code": 0,
                    "stdout_tail": evidence,
                    "stderr_tail": "y".repeat(760),
                    "stdout_truncated": false,
                    "stderr_truncated": false,
                    "stdout_lines": 1,
                    "stderr_lines": 1,
                    "purpose": "test",
                    "command_summary": "true",
                    "cwd": ".",
                    "executor": "agent",
                    "execution_state": "completed"
                }),
                None,
                None,
            )
            .expect("seeded model-facing recovery event");
        revision = recorded.context_revision;
    }
    revision
}

pub(in crate::tool_runtime::tests) fn seed_large_changed_path_recovery_events(
    runtime: &ToolRuntime,
    session_id: &str,
    project: &str,
    count: usize,
) -> u64 {
    use crate::tool_runtime::sessions::{
        SessionContextRevisionAck, SessionTransport, ToolCallRecorderMetadata,
    };

    let mut revision = runtime.sessions.context_revision(session_id).unwrap_or(0);
    for index in 0..count {
        let paths = (0..8)
            .map(|path_index| {
                format!(
                    "src/recovery-{index:02}-{path_index:02}-{}",
                    "p".repeat(450)
                )
            })
            .collect::<Vec<_>>();
        let start = runtime.sessions.record_tool_call_started_with_metadata(
            Some(session_id),
            SessionTransport::Mcp,
            "delete_project_files",
            &json!({"project": project, "paths": paths}),
            Some(project.to_string()),
            ToolCallRecorderMetadata {
                ack_session_context_revision: SessionContextRevisionAck::Revision(revision),
                ..Default::default()
            },
            crate::tool_runtime::sessions::session_tool_contract("delete_project_files"),
        );
        let recorded = runtime
            .sessions
            .record_model_facing_tool_call_finished(
                start,
                true,
                &json!({"deleted_count": 8}),
                None,
                None,
            )
            .expect("seeded large changed-path recovery event");
        revision = recorded.context_revision;
    }
    revision
}

pub(in crate::tool_runtime::tests) fn local_project_config(path: &str) -> ProjectConfig {
    ProjectConfig {
        path: path.to_string(),
        client_id: "local-unit-test".to_string(),
        allow_patch: true,
    }
}

pub(in crate::tool_runtime::tests) fn runtime_with_project(
    root: &Path,
    project_id: &str,
) -> ToolRuntime {
    let _ = (root, project_id);
    ToolRuntime::new(
        Arc::new(RunnerRegistry::default()),
        Arc::new(RuntimeInfo::default()),
    )
}

pub(in crate::tool_runtime::tests) fn runtime_with_info(info: RuntimeInfo) -> ToolRuntime {
    ToolRuntime::new(Arc::new(RunnerRegistry::default()), Arc::new(info))
}
