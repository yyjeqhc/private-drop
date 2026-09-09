use super::*;
use crate::tool_runtime::TOOL_CALL_WRAPPER_FIELDS;

fn runtime_accepted_flattened_action_fields() -> std::collections::BTreeSet<String> {
    let mut fields = std::collections::BTreeSet::new();
    for spec in registered_tool_specs() {
        fields.extend(generic_tool_call_flattened_args_for_spec(&spec));
    }
    fields
}

fn flattened_schema_alternatives(schema: &Value) -> Vec<&Value> {
    schema["anyOf"].as_array().map_or_else(
        || vec![schema],
        |alternatives| alternatives.iter().collect(),
    )
}

#[test]
fn openapi_hidden_start_only_fields_do_not_enter_model_facing_flattened_schema() {
    let spec = build_openapi_spec();
    let properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "temporary_project_name",
        "deny_write_tools",
        "deny_shell_tools",
        "detail",
        "resume_session_id",
        "bind_current",
        "new_session",
    ] {
        assert!(
            !properties.contains_key(field),
            "hidden start-only flattened field {field} must not enter GPT Actions ToolCallRequest"
        );
    }
    assert!(
        properties.contains_key("execution_context"),
        "execution_context stays model-facing because update_session_context uses it"
    );
    assert!(properties.contains_key("mode"));
    assert!(properties.contains_key("base_ref"));
    assert_eq!(properties["mode"]["enum"], json!(["checkout", "worktree"]));
}

#[test]
fn openapi_flattened_execution_context_is_strongly_typed() {
    let spec = build_openapi_spec();
    let execution_context =
        &spec["components"]["schemas"]["ToolCallRequest"]["properties"]["execution_context"];
    assert_eq!(execution_context["type"], "object");
    assert_eq!(execution_context["additionalProperties"], false);
    assert_eq!(
        execution_context["properties"]["default_shell"]["enum"],
        json!(["sh", "bash"])
    );
    assert_eq!(
        execution_context["properties"]["default_cwd"]["maxLength"],
        4096
    );
}

#[test]
fn openapi_tool_call_request_does_not_advertise_hidden_start_bootstrap() {
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let wrapper_description = tool_call["description"].as_str().unwrap();
    let tool_description = tool_call["properties"][TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    assert!(!wrapper_description.contains("start_coding_task"));
    assert!(!tool_description.contains("start_coding_task"));
    assert!(tool_description.contains("work_on_project"));
    assert!(wrapper_description.contains("model-visible runtime tools"));
}

/// Recursively collect every `$ref` string found anywhere in a JSON value.
fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "$ref" {
                    if let Some(s) = v.as_str() {
                        out.push(s.to_string());
                    }
                }
                collect_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Resolve a local `#/components/schemas/<Name>` ref against the spec.
fn resolve_local_ref<'a>(spec: &'a Value, reference: &str) -> Option<&'a Value> {
    let rest = reference.strip_prefix("#/")?;
    let mut current = spec;
    for segment in rest.split('/') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Collect all operation ids in the spec (sorted, deduplicated).
fn operation_ids(spec: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for methods in spec["paths"].as_object().unwrap().values() {
        for op in methods.as_object().unwrap().values() {
            ids.push(op["operationId"].as_str().unwrap().to_string());
        }
    }
    ids.sort();
    ids
}

#[test]
fn openapi_operation_ids_are_minimal() {
    let spec = build_openapi_spec();
    let ids = operation_ids(&spec);
    let mut expected = GPT_ACTION_OPS
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(ids, expected);
}

#[test]
fn openapi_consequential_flags_match_operation_risk() {
    let spec = build_openapi_spec();
    let mut flags = std::collections::BTreeMap::new();
    for methods in spec["paths"].as_object().unwrap().values() {
        for op in methods.as_object().unwrap().values() {
            let operation_id = op["operationId"].as_str().unwrap().to_string();
            let consequential = op["x-openai-isConsequential"].as_bool().unwrap();
            flags.insert(operation_id, consequential);
        }
    }
    let readonly = [
        "listRuntimeTools",
        "listProjects",
        "getRuntimeStatus",
        "readProjectFile",
        "listProjectFiles",
        "searchProjectText",
        "getProjectGitStatus",
        "getProjectGitDiff",
        "getProjectGitDiffSummary",
        "getRuntimeJobStatus",
        "getRuntimeJobLog",
        "getRuntimeJobTail",
        "listRuntimeJobs",
        "registerProject",
        "createProject",
    ];
    let consequential = [
        "applyUnifiedDiff",
        "runProjectShellCommand",
        "startProjectShellJob",
        "gitRestorePaths",
        "discardUntrackedFiles",
        "callRuntimeTool",
    ];
    for id in readonly {
        assert_eq!(
            flags.get(id),
            Some(&false),
            "{} should be non-consequential",
            id
        );
    }
    for id in consequential {
        assert_eq!(flags.get(id), Some(&true), "{} should be consequential", id);
    }
    assert_eq!(flags.len(), 22);
}

#[test]
fn openapi_does_not_expose_any_legacy_or_non_gpt_action_paths() {
    let spec = build_openapi_spec();
    let paths = spec["paths"].as_object().unwrap();
    for legacy in LEGACY_FORBIDDEN_PATHS {
        assert!(
            !paths.contains_key(*legacy),
            "legacy/non-GPT-Actions path '{}' must not appear in openapi.json",
            legacy
        );
    }
}

#[test]
fn openapi_route_visibility_matches_canonical_metadata() {
    use std::collections::BTreeSet;

    let spec = build_openapi_spec();
    let actual = spec["paths"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let expected = crate::route_metadata::iter_routes()
        .filter(|route| {
            matches!(
                route.openapi_projection,
                crate::route_metadata::RouteOpenApiProjection::PublicAction(_)
            )
        })
        .map(|route| route.path.to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);

    for route in crate::route_metadata::iter_routes().filter(|route| {
        route.openapi_projection == crate::route_metadata::RouteOpenApiProjection::Hidden
    }) {
        assert!(
            !actual.contains(route.path),
            "hidden route leaked: {}",
            route.path
        );
    }
}

#[test]
fn openapi_exposes_only_canonical_unified_diff_action() {
    let spec = build_openapi_spec();
    let paths = spec["paths"].as_object().unwrap();
    assert!(paths.contains_key("/api/projects/apply_unified_diff"));
    for removed in [
        "/api/projects/apply_patch",
        "/api/projects/apply_patch_checked",
        "/api/projects/validate_patch",
    ] {
        assert!(
            !paths.contains_key(removed),
            "retired patch route leaked: {removed}"
        );
    }
    let operation = &spec["paths"]["/api/projects/apply_unified_diff"]["post"];
    assert_eq!(operation["operationId"], "applyUnifiedDiff");
    assert_eq!(
        operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ApplyUnifiedDiffToolResult"
    );
    let request = &spec["components"]["schemas"]["ApplyUnifiedDiffRequest"];
    assert_eq!(request["required"], json!(["project", "diff"]));
    assert!(request["properties"].get("diff").is_some());
    assert!(request["properties"].get("patch").is_none());
    let registry = registered_tool_specs()
        .into_iter()
        .find(|tool| tool.name == "apply_unified_diff")
        .expect("apply_unified_diff ToolSpec");
    assert_eq!(
        spec["components"]["schemas"]["ApplyUnifiedDiffToolResult"],
        registry.output_schema
    );
}

#[test]
fn openapi_uses_bearer_auth() {
    let spec = build_openapi_spec();
    assert_eq!(
        spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    let description = spec["components"]["securitySchemes"]["bearerAuth"]["description"]
        .as_str()
        .expect("bearerAuth description");
    assert!(
        description.contains("shared key"),
        "bearerAuth description must mention shared-key quick start: {description}"
    );
    assert!(
        description.contains("quick start"),
        "bearerAuth description must mention quick start: {description}"
    );
    assert!(
        description.contains("wc_pat"),
        "bearerAuth description must mention wc_pat managed tokens: {description}"
    );
    assert!(
        description.contains("managed mode"),
        "bearerAuth description must mention managed mode: {description}"
    );
    assert!(
        !description.contains("personal API token only") && !description.contains("wc_pat only"),
        "bearerAuth description must not regress to PAT-only guidance: {description}"
    );
}

#[test]
fn openapi_info_description_mentions_bearer_host_auth_modes() {
    let spec = build_openapi_spec();
    let description = spec["info"]["description"]
        .as_str()
        .expect("info description");
    for expected in [
        "static bearer/API-key hosts",
        "shared key",
        "quick start",
        "wc_pat",
        "managed mode",
    ] {
        assert!(
            description.contains(expected),
            "OpenAPI info.description must mention {expected:?}: {description}"
        );
    }
    assert!(
        !description.contains("personal API token only") && !description.contains("wc_pat only"),
        "OpenAPI info.description must not regress to PAT-only guidance: {description}"
    );
}

#[test]
fn openapi_top_level_security_uses_bearer() {
    let spec = build_openapi_spec();
    let security = spec["security"].as_array().expect("security array");
    assert!(!security.is_empty());
    assert!(security[0]["bearerAuth"].is_array());
}

#[test]
fn openapi_all_local_refs_resolve() {
    let spec = build_openapi_spec();
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    assert!(!refs.is_empty(), "expected at least one $ref in the spec");
    for reference in &refs {
        assert!(
            reference.starts_with("#/"),
            "only local refs are allowed, found: {}",
            reference
        );
        let resolved = resolve_local_ref(&spec, reference)
            .unwrap_or_else(|| panic!("unresolved $ref target: {}", reference));
        assert!(
            resolved.is_object(),
            "$ref target '{}' should resolve to a schema object",
            reference
        );
    }
}

#[test]
fn openapi_schemas_define_all_referenced_names() {
    let spec = build_openapi_spec();
    let schemas = spec["components"]["schemas"]
        .as_object()
        .expect("schemas object");
    // Every referenced schema name must exist as a key.
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    for reference in &refs {
        if let Some(name) = reference.strip_prefix("#/components/schemas/") {
            assert!(
                schemas.contains_key(name),
                "referenced schema '{}' is not defined in components/schemas",
                name
            );
        }
    }
}

#[test]
fn openapi_paths_only_use_post_method() {
    // GPT Actions surface is POST-only. /openapi.json itself is served by
    // a separate GET route and must NOT appear inside the schema paths.
    let spec = build_openapi_spec();
    for (path, methods) in spec["paths"].as_object().unwrap() {
        let method_keys: Vec<&String> = methods.as_object().unwrap().keys().collect();
        assert_eq!(
            method_keys,
            vec!["post"],
            "path '{}' should only expose POST, got {:?}",
            path,
            method_keys
        );
    }
}

#[test]
fn openapi_has_no_duplicate_operation_ids() {
    let spec = build_openapi_spec();
    let mut ids = Vec::new();
    for methods in spec["paths"].as_object().unwrap().values() {
        for op in methods.as_object().unwrap().values() {
            ids.push(op["operationId"].as_str().unwrap().to_string());
        }
    }
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        ids.len(),
        sorted.len(),
        "duplicate operation ids detected: {:?}",
        ids
    );
}

#[test]
fn openapi_operation_descriptions_fit_model_budget() {
    let spec = build_openapi_spec();
    for (path, methods) in spec["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            let operation_id = op["operationId"].as_str().unwrap_or("<missing>");
            let desc = op["description"].as_str().unwrap_or("");
            assert!(
                desc.chars().count() <= crate::tool_runtime::MODEL_TOOL_DESCRIPTION_MAX_CHARS,
                "{} {} operationId {} description has length {} (hard budget {})",
                method,
                path,
                operation_id,
                desc.chars().count(),
                crate::tool_runtime::MODEL_TOOL_DESCRIPTION_MAX_CHARS
            );
        }
    }
}

#[test]
fn openapi_rejects_legacy_codex_paths_from_model_facing_spec() {
    let spec = build_openapi_spec();
    assert!(
        spec["paths"].get("/api/codex/run").is_none(),
        "legacy /api/codex/run must not be exposed as a GPT Action path"
    );
    let serialized = serde_json::to_string(&spec).unwrap();
    assert!(
        !serialized.contains("runCodexTask"),
        "legacy runCodexTask operation id must stay absent from OpenAPI"
    );
    assert!(
        !serialized.contains("CodexRunRequest"),
        "legacy CodexRunRequest schema must stay absent from OpenAPI"
    );
    // callRuntimeTool is the GPT Actions route for both canonical guarded edits
    // and the contextual patch alternative; the description must preserve that ordering.
    let call_tool = &spec["paths"]["/api/tools/call"]["post"]["description"]
        .as_str()
        .unwrap();
    assert!(
        call_tool.contains("ordinary model-generated file edits")
            && call_tool.contains("tool=apply_text_edits")
            && call_tool.contains("current expected_sha256")
            && call_tool.contains("tool=apply_patch")
            && call_tool.contains("contextual or large patch-shaped change"),
        "callRuntimeTool description should prefer guarded text edits before contextual patching: {call_tool}"
    );
    // getRuntimeJobStatus / getRuntimeJobLog should mention job_id polling.
    let status_desc = &spec["paths"]["/api/jobs/status"]["post"]["description"]
        .as_str()
        .unwrap();
    assert!(status_desc.contains("job_id"));
    let log_desc = &spec["paths"]["/api/jobs/log"]["post"]["description"]
        .as_str()
        .unwrap();
    assert!(log_desc.contains("job_id"));
}

#[test]
fn openapi_call_runtime_tool_lists_accepted_tool_names() {
    use crate::tool_runtime::tool_definition::model_visible_tool_definitions;

    let spec = build_openapi_spec();
    let tool_desc = &spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        [TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    for definition in model_visible_tool_definitions() {
        let name = definition.name;
        assert!(
            tool_desc.contains(name),
            "ToolCallRequest.tool description should list accepted tool name '{}'",
            name
        );
    }
    assert!(tool_desc.contains("document_diagnostics"));
    assert!(tool_desc.contains("hover"));
    assert!(tool_desc.contains("workspace_symbols"));
    for hidden_extension in ["skill_list", "memory_search", "read_tool_trace"] {
        assert!(
            !tool_desc.contains(hidden_extension),
            "generic OpenAPI callRuntimeTool must not advertise Stateless operator extension {hidden_extension}"
        );
    }
    let properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    for field in ["project", "path", "limit", "session_id"] {
        assert!(
            properties.contains_key(field),
            "callRuntimeTool must expose flattened document_diagnostics field {field}"
        );
    }
    for field in ["line", "column", "query"] {
        assert!(
            properties.contains_key(field),
            "callRuntimeTool must expose flattened LSP field {field}"
        );
    }
    let operation_ids = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|path| {
            path.as_object()
                .into_iter()
                .flat_map(|methods| methods.values())
        })
        .filter_map(|operation| operation["operationId"].as_str())
        .collect::<Vec<_>>();
    assert!(!operation_ids.contains(&"hover"));
    assert!(!operation_ids.contains(&"workspaceSymbols"));
    assert_eq!(operation_ids.len(), 22);
}

#[test]
fn openapi_read_files_is_available_through_strict_flattened_runtime_fields() {
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let description = tool_call["properties"][TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("read_file"));
    assert!(description.contains("read_files"));
    assert!(description.contains("observe_jobs"));

    let items = &tool_call["properties"]["items"];
    let alternatives = flattened_schema_alternatives(items);
    let read_files = alternatives
        .iter()
        .copied()
        .find(|alternative| alternative["items"]["required"] == json!(["path"]))
        .expect("flattened items must retain the read_files array shape");
    let observe_jobs = alternatives
        .iter()
        .copied()
        .find(|alternative| alternative["items"]["required"] == json!(["job_id"]))
        .expect("flattened items must retain the observe_jobs array shape");
    for alternative in [read_files, observe_jobs] {
        assert_eq!(alternative["type"], "array");
        assert_eq!(alternative["minItems"], 1);
        assert_eq!(alternative["maxItems"], 8);
        assert_eq!(alternative["items"]["type"], "object");
        assert_eq!(alternative["items"]["additionalProperties"], false);
    }
    assert!(read_files["items"]["properties"].get("path").is_some());
    assert!(observe_jobs["items"]["properties"].get("job_id").is_some());
    assert_eq!(tool_call["additionalProperties"], false);

    let examples = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"];
    assert_eq!(examples["readFiles"]["value"]["tool"], "read_files");
    assert_eq!(
        examples["readFiles"]["value"]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn openapi_dynamic_flattened_arg_collisions_are_independent_of_tool_order() {
    let mut forward = json!({"ToolCallRequest": {"properties": {}}});
    let mut reverse = json!({"ToolCallRequest": {"properties": {}}});
    let specs = registered_tool_specs();
    let mut reversed_specs = specs.clone();
    reversed_specs.reverse();

    insert_tool_call_request_flattened_arg_properties_for_specs(&mut forward, specs);
    insert_tool_call_request_flattened_arg_properties_for_specs(&mut reverse, reversed_specs);

    assert_eq!(
        forward["ToolCallRequest"]["properties"],
        reverse["ToolCallRequest"]["properties"]
    );
}

#[test]
fn openapi_search_project_texts_is_available_through_strict_flattened_runtime_fields() {
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let description = tool_call["properties"][TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("search_project_text"));
    assert!(description.contains("search_project_texts"));

    let queries = &tool_call["properties"]["queries"];
    assert_eq!(queries["type"], "array");
    assert_eq!(queries["minItems"], 1);
    assert_eq!(queries["maxItems"], 8);
    assert_eq!(queries["items"]["additionalProperties"], false);
    assert_eq!(queries["items"]["required"], json!(["pattern"]));
    assert_eq!(
        queries["items"]["properties"]["result_mode"]["enum"],
        json!(["matches", "files_with_matches", "count"])
    );
    assert_eq!(tool_call["additionalProperties"], false);

    let examples = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"];
    assert_eq!(
        examples["searchProjectTexts"]["value"]["tool"],
        "search_project_texts"
    );
    assert_eq!(
        examples["searchProjectTexts"]["value"]["queries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn openapi_key_actions_have_examples() {
    let spec = build_openapi_spec();
    for (path, label) in [
        ("/api/jobs/status", "getRuntimeJobStatus"),
        ("/api/jobs/log", "getRuntimeJobLog"),
        ("/api/projects/read_file", "readProjectFile"),
        ("/api/projects/git_status", "getProjectGitStatus"),
        ("/api/projects/git_diff", "getProjectGitDiff"),
        ("/api/projects/git_diff_summary", "getProjectGitDiffSummary"),
        ("/api/projects/list_files", "listProjectFiles"),
        ("/api/projects/search_text", "searchProjectText"),
        ("/api/projects/apply_unified_diff", "applyUnifiedDiff"),
        ("/api/projects/run_shell", "runProjectShellCommand"),
        ("/api/projects/git_restore_paths", "gitRestorePaths"),
        ("/api/projects/discard_untracked", "discardUntrackedFiles"),
        ("/api/projects/run_job", "startProjectShellJob"),
        ("/api/projects/register", "registerProject"),
        ("/api/projects/create", "createProject"),
        ("/api/jobs/list", "listRuntimeJobs"),
        ("/api/jobs/tail", "getRuntimeJobTail"),
        ("/api/tools/call", "callRuntimeTool"),
    ] {
        let examples =
            &spec["paths"][path]["post"]["requestBody"]["content"]["application/json"]["examples"];
        assert!(
            examples.is_object(),
            "{} request should declare examples",
            label
        );
        assert!(
            !examples.as_object().unwrap().is_empty(),
            "{} request should declare at least one example",
            label
        );
    }
}

#[test]
fn openapi_dedicated_actions_have_expected_routes_and_operation_ids() {
    let spec = build_openapi_spec();
    let expected = [
        ("/api/tools/list", "listRuntimeTools"),
        ("/api/projects/list", "listProjects"),
        ("/api/projects/register", "registerProject"),
        ("/api/projects/create", "createProject"),
        ("/api/runtime/status", "getRuntimeStatus"),
        ("/api/jobs/status", "getRuntimeJobStatus"),
        ("/api/jobs/log", "getRuntimeJobLog"),
        ("/api/jobs/list", "listRuntimeJobs"),
        ("/api/jobs/tail", "getRuntimeJobTail"),
        ("/api/projects/read_file", "readProjectFile"),
        ("/api/projects/git_status", "getProjectGitStatus"),
        ("/api/projects/git_diff", "getProjectGitDiff"),
        ("/api/projects/git_diff_summary", "getProjectGitDiffSummary"),
        ("/api/projects/list_files", "listProjectFiles"),
        ("/api/projects/search_text", "searchProjectText"),
        ("/api/projects/apply_unified_diff", "applyUnifiedDiff"),
        ("/api/projects/run_shell", "runProjectShellCommand"),
        ("/api/projects/git_restore_paths", "gitRestorePaths"),
        ("/api/projects/discard_untracked", "discardUntrackedFiles"),
        ("/api/artifacts/import", "importConversationFilesToProject"),
        ("/api/projects/run_job", "startProjectShellJob"),
        ("/api/tools/call", "callRuntimeTool"),
    ];
    assert_eq!(expected.len(), GPT_ACTION_OPS.len());
    for (path, operation_id) in expected {
        assert_eq!(spec["paths"][path]["post"]["operationId"], operation_id);
    }
    let serialized = serde_json::to_string(&spec).unwrap();
    assert!(serialized.contains("Runner-registered"));
    assert!(serialized.contains("list_runners"));
    for retired_runner_term in [
        "agent-registered",
        "owning agent",
        "selected agent",
        "agent shell capability",
    ] {
        assert!(
            !serialized.contains(retired_runner_term),
            "OpenAPI must not teach retired Runner term {retired_runner_term:?}"
        );
    }
}

#[test]
fn openapi_import_preserves_reserved_gpt_actions_file_reference_contract() {
    let spec = build_openapi_spec();
    let request = &spec["components"]["schemas"]["ImportConversationFilesRequest"];
    assert_eq!(request["required"], json!(["openaiFileIdRefs", "project"]));
    let refs = &request["properties"]["openaiFileIdRefs"];
    assert_eq!(refs["type"], "array");
    assert_eq!(refs["maxItems"], 10);
    assert_eq!(
        refs["items"]["$ref"],
        "#/components/schemas/OpenAiFileIdRef"
    );

    let file_ref = &spec["components"]["schemas"]["OpenAiFileIdRef"];
    assert_eq!(file_ref["required"], json!(["download_link"]));
    assert_eq!(file_ref["additionalProperties"], false);
    for property in ["name", "id", "mime_type", "download_link"] {
        assert_eq!(file_ref["properties"][property]["type"], "string");
    }
    assert!(file_ref["properties"].get("download_url").is_none());
    assert!(file_ref["properties"].get("file_id").is_none());
}

#[test]
fn openapi_mutation_actions_describe_execution_risk_and_auth() {
    let spec = build_openapi_spec();
    for path in [
        "/api/projects/apply_unified_diff",
        "/api/projects/run_shell",
        "/api/projects/git_restore_paths",
        "/api/projects/discard_untracked",
        "/api/projects/run_job",
        "/api/projects/register",
        "/api/projects/create",
    ] {
        let desc = spec["paths"][path]["post"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(
            desc.to_lowercase().contains("side effect"),
            "{path}: {desc}"
        );
        assert!(
            desc.to_lowercase().contains("bearer auth"),
            "{path}: {desc}"
        );
    }
    for path in [
        "/api/projects/apply_unified_diff",
        "/api/projects/run_shell",
    ] {
        let desc = spec["paths"][path]["post"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(
            desc.to_lowercase().contains("runner shell capability"),
            "{path}: {desc}"
        );
    }
    for path in [
        "/api/projects/git_restore_paths",
        "/api/projects/discard_untracked",
    ] {
        let desc = spec["paths"][path]["post"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(desc.contains("structured_process_argv"), "{path}: {desc}");
        assert!(
            !desc.to_lowercase().contains("runner shell capability"),
            "{path}: {desc}"
        );
    }
    let desc = spec["paths"]["/api/projects/run_job"]["post"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(desc.to_lowercase().contains("async shell job"));
}

#[test]
fn openapi_readonly_actions_describe_readonly() {
    // Every read-only dedicated action must mark itself read-only (or
    // "never writes") in its description so GPT callers can tell them
    // apart from mutations.
    // callRuntimeTool is excluded because it is a generic escape hatch
    // that can dispatch either read-only or mutating tools.
    let spec = build_openapi_spec();
    for path in [
        "/api/tools/list",
        "/api/projects/list",
        "/api/runtime/status",
        "/api/jobs/status",
        "/api/jobs/log",
        "/api/jobs/list",
        "/api/jobs/tail",
        "/api/projects/read_file",
        "/api/projects/git_status",
        "/api/projects/git_diff",
        "/api/projects/git_diff_summary",
        "/api/projects/list_files",
        "/api/projects/search_text",
    ] {
        let desc = spec["paths"][path]["post"]["description"]
            .as_str()
            .unwrap_or("");
        let lower = desc.to_lowercase();
        assert!(
            lower.contains("read-only") || lower.contains("never writes"),
            "{} description should be marked read-only or never writes, got: {}",
            path,
            desc
        );
    }
}

#[test]
fn openapi_request_body_schemas_have_additional_properties_false() {
    // Every requestBody schema referenced by an operation must declare
    // `additionalProperties: false` at the top level so GPT Actions
    // rejects unknown fields rather than silently dropping them. Inner
    // properties (e.g. ToolCallRequest.params) may still allow arbitrary
    // keys; this guard only pins the top-level request object.
    let spec = build_openapi_spec();
    let schemas = spec["components"]["schemas"]
        .as_object()
        .expect("schemas object");
    for (path, methods) in spec["paths"].as_object().unwrap() {
        for (method, op) in methods.as_object().unwrap() {
            let request_schema_ref =
                op["requestBody"]["content"]["application/json"]["schema"]["$ref"].as_str();
            let schema_name = match request_schema_ref {
                Some(r) => r.strip_prefix("#/components/schemas/").unwrap_or(r),
                None => continue,
            };
            let schema = schemas.get(schema_name).unwrap_or_else(|| {
                panic!(
                    "{} {} references unknown schema '{}'",
                    method, path, schema_name
                )
            });
            assert_eq!(
                schema["additionalProperties"],
                Value::Bool(false),
                "{} {} requestBody schema '{}' must have additionalProperties=false",
                method,
                path,
                schema_name
            );
        }
    }
}

#[test]
fn openapi_file_search_shell_schemas_include_ergonomics_fields() {
    let spec = build_openapi_spec();
    let schemas = &spec["components"]["schemas"];
    let read_props = schemas["ReadProjectFileRequest"]["properties"]
        .as_object()
        .unwrap();
    assert!(read_props.contains_key("with_line_numbers"));

    let search_props = schemas["SearchProjectTextRequest"]["properties"]
        .as_object()
        .unwrap();
    assert!(search_props.contains_key("context_before"));
    assert!(search_props.contains_key("context_after"));
    assert!(search_props.contains_key("include_globs"));
    assert!(search_props.contains_key("exclude_globs"));
    assert!(search_props.contains_key("result_mode"));
    assert!(search_props.contains_key("pattern_mode"));
    assert!(search_props.contains_key("timeout_secs"));
    assert_eq!(search_props["include_globs"]["maxItems"], 32);
    assert_eq!(search_props["include_globs"]["items"]["maxLength"], 256);
    assert_eq!(
        search_props["result_mode"]["enum"],
        json!(["matches", "files_with_matches", "count"])
    );
    assert_eq!(
        search_props["pattern_mode"]["enum"],
        json!(["regex", "literal"])
    );
    assert_eq!(search_props["pattern_mode"]["default"], "regex");
    let flattened_props = schemas["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    assert_eq!(flattened_props["include_globs"]["maxItems"], 32);
    assert_eq!(flattened_props["include_globs"]["items"]["maxLength"], 256);
    assert_eq!(
        flattened_props["result_mode"]["enum"],
        json!(["matches", "files_with_matches", "count"])
    );
    assert_eq!(
        flattened_props["pattern_mode"]["enum"],
        json!(["regex", "literal"])
    );
    assert!(
        flattened_schema_alternatives(&flattened_props["timeout_secs"])
            .iter()
            .all(|schema| schema["type"] == "integer")
    );
    // Search timeout is server-clamped; the dedicated SearchProjectTextRequest
    // schema must not reject out-of-range integers with minimum/maximum.
    // ToolCallRequest.timeout_secs is a shared flattened field also used by
    // cargo_*/run_shell (which declare 1..120); do not require it to omit bounds.
    assert!(search_props["timeout_secs"].get("minimum").is_none());
    assert!(search_props["timeout_secs"].get("maximum").is_none());
    let search_timeout_desc = search_props["timeout_secs"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(
        search_timeout_desc.to_ascii_lowercase().contains("clamp"),
        "SearchProjectTextRequest.timeout_secs should document clamp: {search_timeout_desc}"
    );

    let run_shell_description = schemas["RunShellRequest"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(run_shell_description.contains("shell command"));
    let op_description = spec["paths"]["/api/projects/run_shell"]["post"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(op_description.contains("failure_kind"));
    assert!(op_description.contains("tool_failure"));
}

#[test]
fn openapi_omits_retired_write_project_file_dedicated_schema() {
    let spec = build_openapi_spec();
    assert!(
        spec["components"]["schemas"]
            .get("WriteProjectFileRequest")
            .is_none(),
        "the removed /api/projects/write_file action must not leave an unreferenced schema"
    );
}

#[test]
fn openapi_dedicated_project_action_schemas_include_optional_session_id() {
    let spec = build_openapi_spec();
    let schemas = &spec["components"]["schemas"];
    for name in [
        "ReadProjectFileRequest",
        "RunShellRequest",
        "ProjectIdRequest",
        "ProjectGitDiffRequest",
        "SearchProjectTextRequest",
        "ApplyUnifiedDiffRequest",
        "GitRestorePathsRequest",
        "DiscardUntrackedRequest",
        "StartProjectShellJobRequest",
        "ListProjectFilesRequest",
    ] {
        let schema = &schemas[name];
        assert!(
            schema["properties"].get("session_id").is_some(),
            "{name} missing optional session_id property"
        );
        assert_eq!(
            schema["properties"]["session_id"]["description"], SESSION_ID_FIELD_DESCRIPTION,
            "{name} session_id description should match dedicated action guidance"
        );
        let required = schema["required"].as_array().unwrap();
        assert!(
            !required.iter().any(|field| field == "session_id"),
            "{name} must not require session_id"
        );
    }
}

#[test]
fn openapi_call_runtime_tool_params_is_explicit_object() {
    // callRuntimeTool's ToolCallRequest must declare `params` as a property
    // that is an OpenAPI 3.1 object accepting arbitrary tool arguments.
    // GPT Actions sometimes mishandles free-form object params, which is
    // why dedicated typed actions are preferred; this test pins the schema
    // so `params` stays present and object-typed for advanced callers.
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let properties = tool_call["properties"].as_object().unwrap();
    assert!(
        properties.contains_key(TOOL_CALL_PARAMS_FIELD),
        "ToolCallRequest must declare a `params` property"
    );
    let params = &properties[TOOL_CALL_PARAMS_FIELD];
    assert_eq!(params["type"], "object", "params must be type object");
    assert_eq!(params["nullable"], true, "params must allow null");
    assert_eq!(
        params["additionalProperties"], true,
        "params must allow arbitrary object properties"
    );
    let description = tool_call["description"].as_str().unwrap_or("");
    assert!(
            description.contains(TOOL_CALL_RECORDING_SESSION_ID_FIELD)
                && description.contains("flattened top-level fields"),
            "ToolCallRequest should document GPT Action flattened fields and recorder metadata: {description}"
        );
    for phrase in [
        "record this wrapper call in an explicitly selected existing Workflow Session",
        "Omitted Session identifiers never infer a Workflow Session",
    ] {
        assert!(
            description.contains(phrase),
            "ToolCallRequest should document {phrase}: {description}"
        );
    }
    let properties = tool_call["properties"].as_object().unwrap();
    let recording_desc = properties[TOOL_CALL_RECORDING_SESSION_ID_FIELD]["description"]
        .as_str()
        .unwrap_or("");
    assert!(
        recording_desc.contains("exact Session ledger"),
        "recording_session_id should mention exact Session ledger: {recording_desc}"
    );
    assert!(recording_desc.contains("never supplies business Session guards or execution_context"));
    let session_desc = properties["session_id"]["description"]
        .as_str()
        .unwrap_or("");
    assert!(
        session_desc.contains("explicitly selects the Workflow Session")
            && session_desc.contains("Omission leaves ordinary project calls unrecorded"),
        "session_id should describe explicit Session selection: {session_desc}"
    );
    let work_example = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"]["workOnAbsolutePath"]["value"];
    assert_eq!(work_example["tool"], "work_on_project");
    assert_eq!(work_example["client_id"], "special");
    assert_eq!(work_example["instruction"], "Complete the development task");
    let managed_example = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"]["workOnManagedWorktree"]["value"];
    assert_eq!(managed_example["tool"], "work_on_project");
    assert_eq!(managed_example["mode"], "worktree");
    assert_eq!(managed_example["base_ref"], "origin/main");
    // `tool` remains required; `params` is optional (advanced callers may
    // omit it for argument-less tools).
    let required = tool_call["required"].as_array().unwrap();
    assert!(required
        .iter()
        .any(|v| v.as_str() == Some(TOOL_CALL_TOOL_FIELD)));
}

#[test]
fn openapi_call_runtime_tool_exposes_only_canonical_params_envelope() {
    let spec = build_openapi_spec();
    let properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    assert!(properties.contains_key(TOOL_CALL_PARAMS_FIELD));
    assert!(
        !properties.contains_key("arguments"),
        "retired arguments alias must not remain in ToolCallRequest"
    );
    let params = &properties[TOOL_CALL_PARAMS_FIELD];
    assert_eq!(params["type"], "object");
    assert_eq!(params["nullable"], true);
    assert_eq!(params["additionalProperties"], true);
}

#[test]
fn openapi_generic_runtime_exposes_plugin_gateway_and_preserves_canonical_nested_schema() {
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let properties = tool_call["properties"].as_object().unwrap();
    let selector_description = properties[TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    assert!(selector_description.contains("plugin_tool"));
    assert!(properties.contains_key("action"));
    assert!(properties.contains_key("runner"));
    assert!(properties.contains_key("plugin"));
    assert!(properties.contains_key("binding"));
    // `tool` is already the outer runtime selector and `arguments` is a retired
    // wrapper name, so the complete Plugin describe/call contract intentionally
    // uses the canonical non-null `params` object rather than aliases.
    assert_eq!(properties[TOOL_CALL_PARAMS_FIELD]["type"], "object");
    assert_eq!(
        properties[TOOL_CALL_PARAMS_FIELD]["additionalProperties"],
        true
    );

    let plugin = registered_tool_specs()
        .into_iter()
        .find(|candidate| candidate.name == "plugin_tool")
        .expect("plugin_tool canonical ToolSpec");
    assert_eq!(plugin.input_schema["properties"]["tool"]["type"], "string");
    assert_eq!(
        plugin.input_schema["properties"]["arguments"]["type"],
        "object"
    );
}

#[test]
fn openapi_call_runtime_tool_declares_flattened_action_fields() {
    // `tool_name` is a tool_manifest argument, distinct from the outer `tool` selector.
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let properties = tool_call["properties"].as_object().unwrap();
    let accepted_fields = runtime_accepted_flattened_action_fields();

    for field in &accepted_fields {
        assert!(
            properties.contains_key(field),
            "ToolCallRequest.properties.{field} must exist for flattened GPT Action calls"
        );
    }
    for field in properties.keys() {
        if TOOL_CALL_WRAPPER_FIELDS.contains(&field.as_str()) {
            continue;
        }
        assert!(
            accepted_fields.contains(field),
            "ToolCallRequest.properties.{field} is not accepted by any runtime ToolSpec"
        );
    }

    assert!(properties.contains_key(TOOL_CALL_PARAMS_FIELD));
    assert!(!properties.contains_key("arguments"));
    assert_eq!(properties["tool_name"]["type"], "string");
    assert!(
        !properties.contains_key("allow_cross_project_session"),
        "ToolCallRequest must not publish the cross-project debug escape as a flattened Action field"
    );
    for field in [
        "expected_failure",
        "expected_failure_kind",
        "assertion_name",
        "ack_session_context_revision",
        "context_request",
    ] {
        assert!(
            !properties.contains_key(field),
            "ToolCallRequest must not publish recorder metadata field {field}"
        );
    }
    let required = tool_call["required"].as_array().unwrap();
    assert_eq!(required, &vec![json!(TOOL_CALL_TOOL_FIELD)]);
    assert_eq!(tool_call["additionalProperties"], false);

    let desc_blob = serde_json::to_string(tool_call).unwrap();
    assert!(
        desc_blob.contains("top-level fields")
            && desc_blob.contains("canonical direct/non-Action argument envelope")
            && desc_blob.contains("retired `arguments` wrapper is rejected"),
        "ToolCallRequest must document flattened GPT Action compatibility and the canonical params envelope"
    );
}

#[test]
fn openapi_flattened_arg_table_does_not_overwrite_existing_properties() {
    let mut schemas = json!({
        "ToolCallRequest": {
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Existing project-specific schema."
                },
                "args": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Existing args-specific schema."
                }
            }
        }
    });

    insert_tool_call_request_flattened_arg_properties(&mut schemas);

    let properties = schemas["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    assert_eq!(
        properties["project"]["description"],
        "Existing project-specific schema."
    );
    assert_eq!(properties["args"]["items"]["type"], "integer");
    assert_eq!(
        properties["command"]["description"],
        FLATTENED_TOOL_ARG_DESCRIPTION
    );
    assert_eq!(
        properties["paths"]["description"],
        FLATTENED_TOOL_ARG_DESCRIPTION
    );
}

#[test]
fn openapi_tool_call_request_exposes_canonical_closeout_and_visible_runtime_fields() {
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let properties = tool_call["properties"].as_object().unwrap();

    for field in [
        "include_validation",
        "include_workspace",
        "include_checkpoints",
        "compact",
        "include_recommended_flows",
        "include_risk_summary",
    ] {
        assert!(
            properties.contains_key(field),
            "ToolCallRequest.properties.{field} must exist for flattened GPT Action calls"
        );
    }

    assert_eq!(properties["include_validation"]["type"], "boolean");
    assert_eq!(properties["include_workspace"]["type"], "boolean");
    assert_eq!(properties["include_checkpoints"]["type"], "boolean");
    assert_eq!(properties["compact"]["type"], "boolean");
    assert_eq!(properties["include_recommended_flows"]["type"], "boolean");
    assert_eq!(properties["include_risk_summary"]["type"], "boolean");
    assert!(
        !properties.contains_key("detail"),
        "hidden start-only detail field must not be model-facing"
    );
    for removed_startup_field in [
        "compact_startup",
        "include_tool_manifest",
        "tool_manifest_intent",
        "tool_manifest_categories",
        "tool_manifest_limit",
    ] {
        assert!(
            !properties.contains_key(removed_startup_field),
            "removed startup compatibility field {removed_startup_field} must stay absent"
        );
    }
    assert_eq!(
        tool_call["additionalProperties"], false,
        "ToolCallRequest must keep explicit flattened fields with additionalProperties=false"
    );

    let count = operation_ids(&spec).len();
    assert_eq!(count, 22, "GPT Actions operation count must stay 22");
}

#[test]
fn openapi_flattened_sync_wait_keeps_shared_bounded_contract() {
    let spec = build_openapi_spec();
    let properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    let sync_wait = properties
        .get("sync_wait_secs")
        .expect("shared flattened sync_wait_secs");
    let alternatives = flattened_schema_alternatives(sync_wait);
    assert!(!alternatives.is_empty());
    assert!(alternatives
        .iter()
        .all(|schema| schema["type"] == "integer"));
    assert!(alternatives.iter().all(|schema| schema["minimum"] == 1));
    assert!(alternatives.iter().all(|schema| schema["maximum"] == 60));
}

#[test]
fn openapi_call_runtime_tool_declares_checkpoint_flattened_fields() {
    // Regression: GPT Action wrapper rejected checkpoint note,
    // include_untracked, checkpoint_id, confirm, and include_diff_stat
    // because ToolCallRequest.properties did not declare them while
    // additionalProperties stayed false. Each flattened field must be
    // an explicit top-level property so GPT Actions accept it.
    let spec = build_openapi_spec();
    let properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "note",
        "include_untracked",
        "checkpoint_id",
        "confirm",
        "include_diff_stat",
    ] {
        assert!(
                properties.contains_key(field),
                "ToolCallRequest.properties.{field} must exist for flattened checkpoint GPT Action calls"
            );
    }
    assert_eq!(properties["note"]["type"], "string");
    assert_eq!(properties["include_untracked"]["type"], "boolean");
    assert_eq!(properties["checkpoint_id"]["type"], "string");
    assert_eq!(properties["confirm"]["type"], "boolean");
    assert_eq!(properties["include_diff_stat"]["type"], "boolean");
    assert_eq!(
        spec["components"]["schemas"]["ToolCallRequest"]["additionalProperties"],
        false
    );
    let count: usize = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|m| m.as_object().unwrap().len())
        .sum();
    assert_eq!(count, 22, "operation count must stay 22");
}

#[test]
fn openapi_call_runtime_tool_declares_apply_text_edits_flattened_fields() {
    // Regression: GPT Action wrapper rejected apply_text_edits changes and
    // dry_run because ToolCallRequest.properties did not declare them.
    // `changes` must mirror the runtime input schema
    // (bounded array of typed items), not a bare free-form object.
    let spec = build_openapi_spec();
    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let properties = tool_call["properties"].as_object().unwrap();
    for field in ["changes", "dry_run"] {
        assert!(
                properties.contains_key(field),
                "ToolCallRequest.properties.{field} must exist for flattened apply_text_edits GPT Action calls"
            );
    }
    assert_eq!(properties["dry_run"]["type"], "boolean");
    let changes = &properties["changes"];
    assert_eq!(
        changes["type"], "array",
        "changes must be an array, not a bare object"
    );
    assert_eq!(changes["minItems"], 1);
    assert_eq!(changes["maxItems"], 16);
    // This flattened GPT Action projection deliberately stays composition-free;
    // the direct MCP/local-coding ToolSpec carries the strict per-kind oneOf.
    let items = &changes["items"];
    assert_eq!(items["type"], "object");
    assert_eq!(items["additionalProperties"], false);
    assert!(items.get("oneOf").is_none());
    let kind_enum = &items["properties"]["kind"]["enum"]
        .as_array()
        .expect("changes.items.kind must be an enum");
    for variant in ["edit", "create", "delete", "rename"] {
        assert!(
            kind_enum.iter().any(|v| v == variant),
            "changes.items.kind enum must include {variant}"
        );
    }
    assert_eq!(items["properties"]["path"]["minLength"], 1);
    assert_eq!(items["properties"]["to_path"]["minLength"], 1);
    let description = changes["description"].as_str().unwrap();
    for contract in [
        "kind=edit requires path, expected_sha256, and edits and forbids to_path/content",
        "create requires path/content and forbids to_path/expected_sha256/edits",
        "delete requires path/expected_sha256 and forbids to_path/content/edits",
        "rename requires path/to_path/expected_sha256 and forbids content/edits",
    ] {
        assert!(
            description.contains(contract),
            "missing {contract}: {description}"
        );
    }
    let edits = &items["properties"]["edits"];
    let edit_description = edits["description"].as_str().unwrap();
    for contract in [
        "replace_exact requires non-empty old_text",
        "delete_exact requires non-empty old_text",
        "insert_before/insert_after require non-empty anchor_text and new_text",
    ] {
        assert!(
            edit_description.contains(contract),
            "missing {contract}: {edit_description}"
        );
    }
    assert_eq!(edits["items"]["properties"]["old_text"]["minLength"], 1);
    assert_eq!(edits["items"]["properties"]["anchor_text"]["minLength"], 1);
    assert!(edits["items"]["properties"]["new_text"]
        .get("minLength")
        .is_none());
    assert_eq!(
        tool_call["additionalProperties"], false,
        "additionalProperties must stay false"
    );
    let count: usize = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|m| m.as_object().unwrap().len())
        .sum();
    assert_eq!(count, 22, "operation count must stay 22");
}

#[test]
fn openapi_tools_list_response_includes_names_count_categories_flows() {
    // Phase 2: ToolsListResponse must declare names/count (required) and
    // categories/recommended_flows (optional), while keeping `tools` for
    // backward compatibility.
    let spec = build_openapi_spec();
    let resp = &spec["components"]["schemas"]["ToolsListResponse"];
    let required = resp["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "tools"));
    assert!(required.iter().any(|v| v == "names"));
    assert!(required.iter().any(|v| v == "count"));
    let props = resp["properties"].as_object().unwrap();
    assert!(props.contains_key("tools"));
    assert!(props.contains_key("names"));
    assert!(props.contains_key("count"));
    assert!(props.contains_key("categories"));
    assert!(props.contains_key("recommended_flows"));
    assert!(props.contains_key("total_count"));
    assert!(props.contains_key("filtered_count"));
    assert!(props.contains_key("truncated"));
    assert!(props.contains_key("hint"));
    assert!(props.contains_key("recommended_next"));
    assert_eq!(
        spec["paths"]["/api/tools/list"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/ToolsListRequest"
    );
    let req_props = spec["components"]["schemas"]["ToolsListRequest"]["properties"]
        .as_object()
        .unwrap();
    for field in ["category", "features", "summary_only", "limit"] {
        assert!(
            req_props.contains_key(field),
            "ToolsListRequest must expose bounded field {field}"
        );
    }
    assert_eq!(
        spec["components"]["schemas"]["ToolsListRequest"]["properties"]["limit"]["maximum"],
        100
    );
}

#[test]
fn openapi_tool_spec_includes_output_schema() {
    let spec = build_openapi_spec();
    let tool_spec = &spec["components"]["schemas"]["ToolSpec"];
    let required = tool_spec["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "inputSchema"));
    assert!(required.iter().any(|v| v == "outputSchema"));
    assert!(required.iter().any(|v| v == "annotations"));
    let props = tool_spec["properties"].as_object().unwrap();
    assert!(props["inputSchema"].is_object());
    assert!(props["outputSchema"].is_object());
    assert!(props["annotations"].is_object());
    assert_eq!(props["annotations"]["additionalProperties"], true);
}

#[test]
fn openapi_runtime_only_tools_do_not_get_dedicated_paths() {
    let spec = build_openapi_spec();
    let paths = spec["paths"].as_object().unwrap();
    for forbidden in [
        "/api/projects/cargo_fmt",
        "/api/projects/cargo_check",
        "/api/projects/cargo_test",
        "/api/projects/go_test",
        "/api/projects/git_diff_hunks",
        "/api/projects/show_changes",
        "/api/projects/workspace_checkpoint_create",
        "/api/projects/workspace_checkpoint_list",
        "/api/projects/workspace_checkpoint_show",
        "/api/projects/workspace_checkpoint_restore",
        "/api/projects/workspace_checkpoint_delete",
        "/api/projects/project_overview",
        "/api/projects/write_file",
    ] {
        assert!(
            !paths.contains_key(forbidden),
            "{} must remain runtime-only via callRuntimeTool",
            forbidden
        );
    }
}

#[test]
fn openapi_artifact_upload_tools_remain_generic_and_under_action_limit() {
    let spec = build_openapi_spec();
    let paths = spec["paths"].as_object().unwrap();
    for path in [
        "/api/projects/artifact_upload_begin",
        "/api/projects/artifact_upload_chunk",
        "/api/projects/artifact_upload_finish",
        "/api/projects/artifact_upload_abort",
    ] {
        assert!(
            !paths.contains_key(path),
            "{path} must remain runtime-only via callRuntimeTool"
        );
    }

    let ids = operation_ids(&spec);
    for id in &ids {
        assert!(
            !id.contains("ArtifactUpload") && !id.contains("artifactUpload"),
            "artifact upload must not be promoted to a dedicated GPT Action: {id}"
        );
    }
    let count = ids.len();
    assert_eq!(count, 22, "GPT Actions operation count must stay 22");
    assert!(count <= 30, "GPT Actions operation count must stay <= 30");

    let tool_call = &spec["components"]["schemas"]["ToolCallRequest"];
    let tool_desc = tool_call["properties"][TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    for tool in [
        "artifact_upload_begin",
        "artifact_upload_chunk",
        "artifact_upload_finish",
        "artifact_upload_abort",
    ] {
        assert!(
            tool_desc.contains(tool),
            "callRuntimeTool must document runtime tool {tool}"
        );
    }
    let properties = tool_call["properties"].as_object().unwrap();
    for field in [
        "project",
        "path",
        "content_base64",
        "upload_id",
        "offset",
        "expected_bytes",
        "expected_sha256",
        "mime_type",
        "overwrite",
        "allow_missing",
    ] {
        assert!(
            properties.contains_key(field),
            "ToolCallRequest.properties.{field} must exist for flattened artifact upload calls"
        );
    }
    let path_desc = properties["path"]["description"].as_str().unwrap();
    assert!(
        path_desc.contains("must exactly match the path used by artifact_upload_begin")
            && path_desc.contains("bind upload_id"),
        "path flattened description must explain upload path binding: {path_desc}"
    );
    let upload_id_desc = properties["upload_id"]["description"].as_str().unwrap();
    assert!(
        upload_id_desc.contains("same path from artifact_upload_begin is also required"),
        "upload_id flattened description must mention repeated path: {upload_id_desc}"
    );
    for field in [
        "category",
        "intent",
        "features",
        "summary_only",
        "limit",
        "include_recommended_flows",
        "include_risk_summary",
    ] {
        assert!(
                properties.contains_key(field),
                "ToolCallRequest.properties.{field} must exist for flattened list_tools/tool_manifest calls"
            );
    }
}

#[test]
fn openapi_retained_edit_tools_remain_runtime_only() {
    // Retained edit tools (write_project_file) remain reachable via
    // callRuntimeTool / MCP tools/call, but should not be promoted to
    // dedicated GPT Actions. The removed legacy edit tools are absent.
    let spec = build_openapi_spec();
    let paths = spec["paths"].as_object().unwrap();
    assert!(
        !paths.contains_key("/api/projects/write_file"),
        "write_file must remain runtime-only through callRuntimeTool"
    );
    assert!(
        !paths.contains_key("/api/projects/replace_in_file"),
        "replace_in_file must not have a dedicated path"
    );
    assert!(
        paths.contains_key("/api/projects/run_job"),
        "run_job remains a dedicated execution action"
    );
    assert_eq!(
        spec["paths"]["/api/projects/run_job"]["post"]["operationId"],
        "startProjectShellJob"
    );
    assert!(
        LEGACY_FORBIDDEN_PATHS.contains(&"/api/projects/write_file"),
        "write_file must stay in the forbidden guard"
    );
    assert!(
        !LEGACY_FORBIDDEN_PATHS.contains(&"/api/projects/run_job"),
        "run_job must not be in the forbidden guard now that it is a dedicated action"
    );
    let tool_desc = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        [TOOL_CALL_TOOL_FIELD]["description"]
        .as_str()
        .unwrap();
    for tool in ["write_project_file", "apply_text_edits", "apply_patch"] {
        assert!(
            tool_desc.contains(tool),
            "callRuntimeTool must document runtime tool {tool}"
        );
    }
    // `replace_in_file` was removed entirely, so it is neither documented in
    // the model-facing ToolCallRequest description nor a known tool.
    assert!(
        !tool_desc.contains("replace_in_file"),
        "callRuntimeTool must not document the removed replace_in_file tool"
    );
}

#[test]
fn openapi_work_on_project_example_keeps_first_use_projection_defaults() {
    let spec = build_openapi_spec();
    let request_properties = spec["components"]["schemas"]["ToolCallRequest"]["properties"]
        .as_object()
        .expect("ToolCallRequest properties");
    assert!(
        request_properties.contains_key("include_workflow_guidance"),
        "flattened OpenAPI ToolCallRequest must expose include_workflow_guidance"
    );
    let example = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"]["workOnAbsolutePath"]["value"];
    assert_eq!(example["tool"], "work_on_project");
    assert_eq!(example["instruction"], "Complete the development task");
    assert!(
        example.get("include_project_instructions").is_none(),
        "the generic first-use work_on_project example must preserve the default instruction-body projection"
    );
    assert!(
        example.get("include_workflow_guidance").is_none(),
        "the generic first-use work_on_project example must preserve the default workflow-guidance projection"
    );
}

#[test]
fn openapi_call_runtime_tool_examples_cover_params_and_no_params_without_retired_alias() {
    let spec = build_openapi_spec();
    let examples = &spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"];
    let values = examples
        .as_object()
        .unwrap()
        .values()
        .map(|example| &example["value"])
        .collect::<Vec<_>>();
    assert!(
        values.iter().all(|value| value.get("arguments").is_none()),
        "callRuntimeTool examples must not advertise the retired arguments alias"
    );
    assert!(
        values
            .iter()
            .any(|value| value.get("params").is_some_and(|params| params.is_object())),
        "callRuntimeTool examples should include the canonical params envelope"
    );
    assert!(
        values.iter().any(|value| {
            value["tool"].as_str() == Some("list_tools") && value.get("params").is_none()
        }),
        "callRuntimeTool examples should include an argument-less flattened variant"
    );
    assert!(
        values.iter().any(|value| {
            value["tool"].as_str() == Some("apply_text_edits")
                && value["project"].as_str() == Some("webcodex")
                && value["changes"]
                    .as_array()
                    .is_some_and(|changes| !changes.is_empty())
        }),
        "callRuntimeTool examples should document the default SHA-guarded apply_text_edits route"
    );
    assert!(
        values.iter().any(|value| {
            value["tool"].as_str() == Some("apply_patch")
                && value["project"].as_str() == Some("webcodex")
                && value["patch"]
                    .as_str()
                    .is_some_and(|patch| patch.contains("*** Begin Patch"))
        }),
        "callRuntimeTool examples should retain the contextual apply_patch route"
    );
}

#[test]
fn openapi_edit_routes_prefer_guarded_text_edits_for_model_generated_changes() {
    let spec = build_openapi_spec();
    let unified = spec["paths"]["/api/projects/apply_unified_diff"]["post"]["description"]
        .as_str()
        .expect("applyUnifiedDiff description");
    assert!(unified.contains("External/raw unified-diff mutation only"));
    assert!(unified.contains("tool=apply_text_edits"));
    assert!(unified.contains("tool=apply_patch"));
    assert!(unified.contains("contextual or large patch-shaped changes"));
    assert!(!unified.contains("Canonical complex or multi-file"));

    let call = spec["paths"]["/api/tools/call"]["post"]["description"]
        .as_str()
        .expect("callRuntimeTool description");
    assert!(call.contains("ordinary model-generated file edits"));
    assert!(call.contains("tool=apply_text_edits"));
    assert!(call.contains("current expected_sha256"));
    assert!(call.contains("tool=apply_patch"));
    assert!(call.contains("contextual or large patch-shaped change"));

    assert!(
        spec["paths"].get("/api/projects/apply_patch").is_none(),
        "apply_patch must remain runtime-only through callRuntimeTool"
    );
}

#[test]
fn openapi_call_runtime_tool_examples_only_advertise_model_visible_tools() {
    let spec = build_openapi_spec();
    let examples = spec["paths"]["/api/tools/call"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"]
        .as_object()
        .unwrap();
    for (example_name, example) in examples {
        let Some(tool) = example["value"]["tool"].as_str() else {
            continue;
        };
        assert!(
            crate::tool_runtime::tool_definition::is_model_visible_tool_name(tool),
            "generic Action example {example_name} advertises hidden runtime tool {tool}"
        );
    }
}

#[test]
fn openapi_targeted_inventory_request_schemas_are_bounded() {
    let spec = build_openapi_spec();
    let schemas = &spec["components"]["schemas"];

    let projects = &schemas["ListProjectsRequest"]["properties"];
    assert_eq!(projects["client_id"]["maxLength"], 128);
    assert_eq!(projects["project"]["maxLength"], 512);
    assert_eq!(projects["query"]["maxLength"], 200);
    assert_eq!(projects["limit"]["maximum"], 100);
    assert!(projects.get("summary_only").is_some());
    assert_eq!(
        spec["paths"]["/api/projects/list"]["post"]["requestBody"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/ListProjectsRequest"
    );

    let runtime = &schemas["RuntimeStatusRequest"]["properties"];
    assert_eq!(runtime["client_id"]["maxLength"], 128);
    assert!(runtime.get("compact").is_some());
    assert!(runtime.get("summary_only").is_some());

    let jobs = &schemas["ListJobsRequest"]["properties"];
    assert_eq!(jobs["limit"]["maximum"], 100);
    assert_eq!(jobs["project"]["maxLength"], 512);
    assert_eq!(jobs["session_id"]["maxLength"], 128);
}

#[test]
fn openapi_spec_serializes_as_valid_json() {
    // Building the spec must not panic and must produce a JSON object with
    // the top-level OpenAPI 3.1 keys ChatGPT expects.
    let spec = build_openapi_spec();
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["info"]["title"].is_string());
    assert!(spec["info"]["version"].is_string());
    assert!(spec["servers"].is_array());
    assert!(spec["paths"].is_object());
    assert!(spec["components"]["schemas"].is_object());
    assert!(spec["security"].is_array());
}

#[test]
fn openapi_exposes_get_runtime_status_action() {
    let spec = build_openapi_spec();
    assert_eq!(
        spec["paths"]["/api/runtime/status"]["post"]["operationId"],
        "getRuntimeStatus"
    );
    let description = spec["paths"]["/api/runtime/status"]["post"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("observability"));
    assert!(description.contains("stale_count"));
    assert!(!description.contains("offline_count"));
}
