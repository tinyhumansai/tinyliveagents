//! Tool-parameter schema cleaning for Gemini function declarations.
//!
//! Gemini accepts an OpenAPI-flavoured subset of JSON Schema and rejects a
//! setup that uses anything else, which ends the whole session before it
//! starts. Hosts usually generate schemas for richer validators, so every
//! declaration is cleaned before it is sent: unsupported keywords are dropped,
//! `$ref`s are inlined from `$defs`/`definitions`, `const` becomes a
//! one-value `enum`, and a `type` array such as `["string", "null"]` collapses
//! to its first non-null member with `nullable: true`.

use serde_json::{Map, Value};

/// Keywords Gemini understands. Everything else is removed.
const ALLOWED: &[&str] = &[
    "type",
    "format",
    "description",
    "nullable",
    "enum",
    "items",
    "properties",
    "required",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "anyOf",
    "propertyOrdering",
    "title",
    "default",
];

/// Maximum `$ref` inlining depth; deeper (recursive) references become an
/// untyped object rather than looping forever.
const MAX_REF_DEPTH: usize = 8;

/// Cleans one parameters schema. A missing or non-object schema becomes an
/// empty object schema, which Gemini accepts for argument-less functions.
#[must_use]
pub fn clean_schema(schema: &Value) -> Value {
    let definitions = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .cloned()
        .unwrap_or(Value::Null);
    let cleaned = clean(schema, &definitions, 0);
    match cleaned {
        Value::Object(map) if !map.is_empty() => Value::Object(map),
        _ => serde_json::json!({ "type": "object", "properties": {} }),
    }
}

fn clean(value: &Value, definitions: &Value, depth: usize) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        if depth >= MAX_REF_DEPTH {
            return serde_json::json!({ "type": "object" });
        }
        let name = reference.rsplit('/').next().unwrap_or_default();
        return match definitions.get(name) {
            Some(target) => clean(target, definitions, depth + 1),
            None => serde_json::json!({ "type": "object" }),
        };
    }
    let mut out = Map::new();
    for (key, inner) in map {
        match key.as_str() {
            "properties" => {
                if let Value::Object(props) = inner {
                    let props = props
                        .iter()
                        .map(|(name, schema)| (name.clone(), clean(schema, definitions, depth)))
                        .collect();
                    out.insert(key.clone(), Value::Object(props));
                }
            }
            "items" => {
                out.insert(key.clone(), clean(inner, definitions, depth));
            }
            "anyOf" | "oneOf" => {
                if let Value::Array(options) = inner {
                    let options = options
                        .iter()
                        .map(|schema| clean(schema, definitions, depth))
                        .collect();
                    out.insert("anyOf".into(), Value::Array(options));
                }
            }
            "const" => {
                out.insert("enum".into(), Value::Array(vec![inner.clone()]));
            }
            "type" => match inner {
                Value::Array(types) => {
                    let non_null: Vec<&Value> = types
                        .iter()
                        .filter(|t| t.as_str() != Some("null"))
                        .collect();
                    if let Some(first) = non_null.first() {
                        out.insert("type".into(), (*first).clone());
                    }
                    if non_null.len() < types.len() {
                        out.insert("nullable".into(), Value::Bool(true));
                    }
                }
                other => {
                    out.insert("type".into(), other.clone());
                }
            },
            "enum" => {
                // Gemini only accepts string enums.
                if let Value::Array(values) = inner {
                    let values = values
                        .iter()
                        .map(|v| match v {
                            Value::String(_) => v.clone(),
                            other => Value::String(other.to_string()),
                        })
                        .collect();
                    out.insert(key.clone(), Value::Array(values));
                }
            }
            other if ALLOWED.contains(&other) => {
                out.insert(key.clone(), inner.clone());
            }
            _ => {}
        }
    }
    if out.contains_key("enum") && !out.contains_key("type") {
        out.insert("type".into(), Value::String("string".into()));
    }
    Value::Object(out)
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
