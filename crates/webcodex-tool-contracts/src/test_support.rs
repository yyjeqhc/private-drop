//! Test-only support for validating instances against the bounded ToolSpec schema subset.
//!
//! This deliberately implements only the schema vocabulary authored by WebCodex contracts. It is
//! not a general JSON Schema engine and is compiled only for crate tests or the root test-support
//! feature.

use serde_json::Value;

pub fn validate_schema_instance(instance: &Value, schema: &Value) -> Result<(), String> {
    validate_schema_instance_at(instance, schema, "$")
}

fn validate_schema_instance_at(instance: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(schemas) = schema.get("allOf").and_then(Value::as_array) {
        for child in schemas {
            validate_schema_instance_at(instance, child, path)?;
        }
    }
    if let Some(condition) = schema.get("if") {
        let branch = if validate_schema_instance_at(instance, condition, path).is_ok() {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            validate_schema_instance_at(instance, branch, path)?;
        }
    }
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
        let results = variants
            .iter()
            .map(|variant| validate_schema_instance_at(instance, variant, path))
            .collect::<Vec<_>>();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        return (successes == 1).then_some(()).ok_or_else(|| {
            let errors = results
                .into_iter()
                .enumerate()
                .filter_map(|(index, result)| {
                    result
                        .err()
                        .map(|error| format!("variant {index}: {error}"))
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!("{path}: expected exactly one matching schema, got {successes}; {errors}")
        });
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        return variants
            .iter()
            .find_map(|variant| {
                validate_schema_instance_at(instance, variant, path)
                    .ok()
                    .map(|_| ())
            })
            .ok_or_else(|| format!("{path}: no anyOf variant matched"));
    }
    if let Some(expected) = schema.get("const") {
        if instance != expected {
            return Err(format!("{path}: const mismatch"));
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.iter().any(|value| value == instance) {
            return Err(format!("{path}: value is outside the declared enum"));
        }
    }
    if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
        let matches = match expected_type {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "boolean" => instance.is_boolean(),
            "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
            "number" => instance.is_number(),
            "null" => instance.is_null(),
            _ => true,
        };
        if !matches {
            return Err(format!("{path}: expected {expected_type}"));
        }
    }
    if let Some(object) = instance.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    return Err(format!("{path}: missing required field {field}"));
                }
            }
        }
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            let properties = properties
                .ok_or_else(|| format!("{path}: strict object schema is missing properties"))?;
            for field in object.keys() {
                if !properties.contains_key(field) {
                    return Err(format!("{path}: unknown field {field}"));
                }
            }
        }
        if let Some(properties) = properties {
            for (field, value) in object {
                if let Some(child_schema) = properties.get(field) {
                    validate_schema_instance_at(value, child_schema, &format!("{path}.{field}"))?;
                }
            }
        }
    }
    if let Some(array) = instance.as_array() {
        if let Some(min_items) = schema.get("minItems").and_then(Value::as_u64) {
            if array.len() < min_items as usize {
                return Err(format!("{path}: below minItems"));
            }
        }
        if let Some(max_items) = schema.get("maxItems").and_then(Value::as_u64) {
            if array.len() > max_items as usize {
                return Err(format!("{path}: maxItems exceeded"));
            }
        }
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            for (index, item) in array.iter().enumerate() {
                if array[..index].iter().any(|earlier| earlier == item) {
                    return Err(format!("{path}: duplicate array item"));
                }
            }
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                validate_schema_instance_at(item, item_schema, &format!("{path}[{index}]"))?;
            }
        }
    }
    if let Some(value) = instance.as_str() {
        if schema
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| value.chars().count() > maximum as usize)
        {
            return Err(format!("{path}: maxLength exceeded"));
        }
    }
    if let Some(number) = instance.as_i64() {
        if schema
            .get("minimum")
            .and_then(Value::as_i64)
            .is_some_and(|minimum| number < minimum)
        {
            return Err(format!("{path}: below minimum"));
        }
        if schema
            .get("maximum")
            .and_then(Value::as_i64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err(format!("{path}: above maximum"));
        }
    } else if let Some(number) = instance.as_u64() {
        if schema
            .get("maximum")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err(format!("{path}: above maximum"));
        }
    }
    if let (Some(value), Some(pattern)) = (
        instance.as_str(),
        schema.get("pattern").and_then(Value::as_str),
    ) {
        let matches = match pattern {
            "^wc_host_binding_[0-9a-f]{32}$" => {
                value.strip_prefix("wc_host_binding_").is_some_and(|tail| {
                    tail.len() == 32
                        && tail
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            }
            "^wc_sess_[A-Za-z0-9_]+$" => value.strip_prefix("wc_sess_").is_some_and(|tail| {
                !tail.is_empty()
                    && tail
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            }),
            "^[0-9a-f]{64}$" => {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }
            "^repository:v1:[0-9a-f]{64}$" => {
                value.strip_prefix("repository:v1:").is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            }
            _ => true,
        };
        if !matches {
            return Err(format!("{path}: pattern mismatch"));
        }
    }
    Ok(())
}
