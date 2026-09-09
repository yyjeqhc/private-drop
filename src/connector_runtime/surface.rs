//! Root HTTP/OpenAPI projection of the canonical Connector capability registry.

use serde_json::{json, Map, Value};

pub(crate) use webcodex_connector_runtime::surface::{capability_specs, CAPABILITY_NAMES};

pub(crate) fn route_for(name: &str) -> Option<&'static str> {
    use crate::route_metadata::RouteId;
    let id = match name {
        "task_start" => RouteId::ConnectorTaskStart,
        "task_list" => RouteId::ConnectorTaskList,
        "task_resume" => RouteId::ConnectorTaskResume,
        "files_list" => RouteId::ConnectorFilesList,
        "files_read" => RouteId::ConnectorFilesRead,
        "files_search" => RouteId::ConnectorFilesSearch,
        "code_navigate" => RouteId::ConnectorCodeNavigate,
        "edits_apply" => RouteId::ConnectorEditsApply,
        "checks_run" => RouteId::ConnectorChecksRun,
        "commands_run" => RouteId::ConnectorCommandsRun,
        "task_review" => RouteId::ConnectorTaskReview,
        "task_cancel" => RouteId::ConnectorTaskCancel,
        "task_finish" => RouteId::ConnectorTaskFinish,
        "code_impact" => RouteId::ConnectorCodeImpact,
        _ => return None,
    };
    Some(crate::route_metadata::path(id))
}

pub(crate) fn build_openapi_spec(public_url: String) -> Value {
    let mut paths = Map::new();
    for spec in capability_specs() {
        let route = route_for(&spec.name).expect("registered connector capability has a route");
        let consequential = spec
            .annotations
            .get("readOnlyHint")
            .and_then(Value::as_bool)
            != Some(true);
        paths.insert(
            route.to_string(),
            json!({
                "post": {
                    "operationId": spec.name,
                    "summary": spec.description,
                    "x-openai-isConsequential": consequential,
                    "security": [{ "bearerAuth": [] }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": { "schema": spec.input_schema }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Capability completed",
                            "content": { "application/json": { "schema": spec.output_schema } }
                        },
                        "400": { "description": "Invalid input or task operation failed" },
                        "403": { "description": "Authentication scope or task mode denied the capability" },
                        "404": { "description": "Task is not visible in this project and identity context" }
                    }
                }
            }),
        );
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "WebCodex Project Connector",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "A project-bound coding capability surface for hosted chat clients. Start a task, inspect, edit, validate, review, and finish. Project and executor routing are connector context and are never model input."
        },
        "servers": [{ "url": public_url, "description": "WebCodex connector" }],
        "paths": Value::Object(paths),
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn root_routes_cover_the_canonical_connector_registry() {
        for spec in capability_specs() {
            assert!(route_for(&spec.name).is_some(), "{}", spec.name);
        }
    }

    #[test]
    fn hosted_openapi_is_generated_from_the_canonical_capability_list() {
        let spec = build_openapi_spec("https://connector.example".to_string());
        let operations = spec["paths"]
            .as_object()
            .unwrap()
            .values()
            .map(|path| path["post"]["operationId"].as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>();
        let expected = CAPABILITY_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(operations, expected);
        assert_eq!(
            spec["paths"].as_object().unwrap().len(),
            CAPABILITY_NAMES.len()
        );
        let expected_paths = crate::route_metadata::iter_routes()
            .filter(|route| {
                matches!(
                    route.openapi_projection,
                    crate::route_metadata::RouteOpenApiProjection::ConnectorCapability(_)
                )
            })
            .map(|route| route.path.to_string())
            .collect::<BTreeSet<_>>();
        let actual_paths = spec["paths"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_paths, expected_paths);
    }
}
