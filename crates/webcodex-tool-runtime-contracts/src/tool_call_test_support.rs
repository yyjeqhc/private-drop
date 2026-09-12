use serde_json::{json, Value};
use webcodex_tool_contracts::{registered_tool_specs, ToolSpec};

const SAMPLE_PROJECT: &str = "agent:oe:private-drop";
const UNIT_TOOL_FIXTURES: &[&str] = &[
    "list_tools",
    "list_projects",
    "list_runners",
    "runtime_status",
];

pub(super) fn sample_tool_args(name: &str) -> Value {
    let spec = registered_tool_specs()
        .into_iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("missing tool spec for {name}"));
    sample_tool_args_for_spec(&spec)
}

fn sample_tool_args_for_spec(spec: &ToolSpec) -> Value {
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
    match spec.name.as_str() {
        "work_on_project" => {
            args.insert("project".to_string(), json!(SAMPLE_PROJECT));
        }
        "update_goal" => {
            args.insert("expected_revision".to_string(), json!(1));
        }
        "observe_jobs" => {
            args.insert("items".to_string(), json!([{"job_id": "job_123"}]));
        }
        "plugin_tool" => {
            args.insert("action".to_string(), json!("list"));
        }
        "ssh_resource" => {
            args.insert("action".to_string(), json!("list"));
            args.insert("runner".to_string(), json!("runner-a"));
        }
        _ => {}
    }
    Value::Object(args)
}

fn sample_field_value(field: &str) -> Value {
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
        "objective" => json!("Preserve durable high-level intent without execution authority."),
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
        "goal_id" => json!(format!("wc_goal_{}", "0".repeat(32))),
        "attempt_id" => json!(format!("wc_agent_task_attempt_{}", "2".repeat(32))),
        "attempt_fence" => json!(format!("wc_agent_task_fence_{}", "3".repeat(32))),
        "attempt_controller_generation" => json!(1),
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
        "session_id" => json!(format!("wc_sess_{}", "1".repeat(32))),
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
        "expected_generation" => json!(1),
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

pub(super) fn sample_tool_args_with_session(name: &str) -> Value {
    let mut args = sample_tool_args(name);
    let obj = args
        .as_object_mut()
        .unwrap_or_else(|| panic!("{name} does not accept object arguments"));
    obj.insert(
        "session_id".to_string(),
        Value::String("wc_sess_accessor".to_string()),
    );
    args
}
