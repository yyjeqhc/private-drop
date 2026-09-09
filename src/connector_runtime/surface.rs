//! Root HTTP/OpenAPI projection of the canonical Connector capability registry.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub(crate) use webcodex_connector_runtime::surface::{capability_specs, CAPABILITY_NAMES};

#[cfg(test)]
pub(crate) fn route_for(name: &str) -> Option<&'static str> {
    crate::route_metadata::iter_routes().find_map(|route| match route.openapi_projection {
        crate::route_metadata::RouteOpenApiProjection::ConnectorCapability(capability_name)
            if capability_name == name =>
        {
            Some(route.path)
        }
        _ => None,
    })
}

pub(crate) fn build_openapi_spec(public_url: String) -> Value {
    let mut specs_by_name = BTreeMap::new();
    for spec in capability_specs() {
        let name = spec.name.clone();
        assert!(
            specs_by_name.insert(name.clone(), spec).is_none(),
            "duplicate canonical Connector capability: {name}"
        );
    }

    let mut paths = Map::new();
    for route in crate::route_metadata::iter_routes() {
        let crate::route_metadata::RouteOpenApiProjection::ConnectorCapability(capability_name) =
            route.openapi_projection
        else {
            continue;
        };
        let spec = specs_by_name.remove(capability_name).unwrap_or_else(|| {
            panic!("Connector route binding references unknown capability {capability_name}")
        });
        let consequential = spec
            .annotations
            .get("readOnlyHint")
            .and_then(Value::as_bool)
            != Some(true);
        let path_item = paths
            .entry(route.path.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let methods = path_item
            .as_object_mut()
            .expect("Connector OpenAPI path item must be an object");
        assert!(
            methods
                .insert(
                    route.method.openapi_key().to_string(),
                    json!({
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
                    }),
                )
                .is_none(),
            "duplicate Connector OpenAPI projection for {:?} {}",
            route.method,
            route.path
        );
    }
    assert!(
        specs_by_name.is_empty(),
        "canonical Connector capabilities missing RouteSpec bindings: {:?}",
        specs_by_name.keys().collect::<Vec<_>>()
    );

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
    fn route_bindings_are_bijective_with_the_canonical_connector_registry() {
        let bindings = crate::route_metadata::iter_routes()
            .filter_map(|route| match route.openapi_projection {
                crate::route_metadata::RouteOpenApiProjection::ConnectorCapability(name) => {
                    Some(name.to_string())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let capabilities = capability_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(bindings, capabilities);
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
        for route in crate::route_metadata::iter_routes() {
            let crate::route_metadata::RouteOpenApiProjection::ConnectorCapability(name) =
                route.openapi_projection
            else {
                continue;
            };
            assert_eq!(
                spec["paths"][route.path][route.method.openapi_key()]["operationId"],
                name,
                "Connector OpenAPI method/path must come from RouteSpec"
            );
        }
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
