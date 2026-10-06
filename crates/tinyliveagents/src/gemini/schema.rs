//! Tool-parameter schema cleaning for Gemini function declarations.
//!
//! Gemini accepts an OpenAPI-flavoured subset of JSON Schema and rejects a
//! setup that uses anything else, which ends the whole session before it
//! starts. Hosts usually generate schemas for richer validators, so every
//! declaration is cleaned before it is sent: unsupported keywords are dropped,
//! `$ref`s are inlined from `$defs`/`definitions`, and the shapes Gemini
//! cannot express are rewritten into ones it can:
//!
//! - Gemini only accepts string enums, so `enum` values (and a `const`, which
//!   becomes a one-value `enum`) are converted to strings and the schema's
//!   `type` becomes `string`, keeping the type and the values consistent.
//! - A `type` array drops `"null"` in favour of `nullable: true`; a single
//!   remaining type becomes `type`, several become an `anyOf` of one schema
//!   per type, so no alternative is lost.

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
                out.insert("enum".into(), Value::Array(vec![string_value(inner)]));
            }
            "type" => match inner {
                Value::Array(types) => {
                    let non_null: Vec<&Value> = types
                        .iter()
                        .filter(|t| t.as_str() != Some("null"))
                        .collect();
                    match non_null.as_slice() {
                        [] => {}
                        [only] => {
                            out.insert("type".into(), (*only).clone());
                        }
                        several => {
                            let options = several
                                .iter()
                                .map(|t| serde_json::json!({ "type": t }))
                                .collect();
                            out.insert("anyOf".into(), Value::Array(options));
                        }
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
                if let Value::Array(values) = inner {
                    let values = values.iter().map(string_value).collect();
                    out.insert(key.clone(), Value::Array(values));
                }
            }
            other if ALLOWED.contains(&other) => {
                out.insert(key.clone(), inner.clone());
            }
            _ => {}
        }
    }
    if out.contains_key("enum") {
        // Enum values are strings now, so the type must say so; an `anyOf`
        // built from a type array would contradict them.
        out.insert("type".into(), Value::String("string".into()));
        if out.get("anyOf").is_some() && !map.contains_key("anyOf") && !map.contains_key("oneOf") {
            out.remove("anyOf");
        }
    }
    Value::Object(out)
}

/// A JSON value as Gemini's string-only enum members accept it.
fn string_value(value: &Value) -> Value {
    match value {
        Value::String(_) => value.clone(),
        other => Value::String(other.to_string()),
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
