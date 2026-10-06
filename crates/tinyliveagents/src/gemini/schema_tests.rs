//! Tests for Gemini schema cleaning.

use super::*;
use serde_json::json;

#[test]
fn drops_unsupported_keywords_recursively() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "q": { "type": "string", "examples": ["x"], "description": "query" },
            "n": { "type": "integer", "minimum": 1, "exclusiveMaximum": 10 }
        },
        "required": ["q"]
    });
    assert_eq!(
        clean_schema(&schema),
        json!({
            "type": "object",
            "properties": {
                "q": { "type": "string", "description": "query" },
                "n": { "type": "integer", "minimum": 1 }
            },
            "required": ["q"]
        })
    );
}

#[test]
fn inlines_refs_and_handles_missing_or_recursive_ones() {
    let schema = json!({
        "type": "object",
        "$defs": {
            "Point": { "type": "object", "properties": { "x": { "type": "number" } } },
            "Loop": { "$ref": "#/$defs/Loop" }
        },
        "properties": {
            "p": { "$ref": "#/$defs/Point" },
            "missing": { "$ref": "#/$defs/Nope" },
            "loop": { "$ref": "#/$defs/Loop" }
        }
    });
    let cleaned = clean_schema(&schema);
    assert_eq!(
        cleaned["properties"]["p"],
        json!({ "type": "object", "properties": { "x": { "type": "number" } } })
    );
    assert_eq!(
        cleaned["properties"]["missing"],
        json!({ "type": "object" })
    );
    assert_eq!(cleaned["properties"]["loop"], json!({ "type": "object" }));
    assert!(cleaned.get("$defs").is_none());
}

#[test]
fn rewrites_const_one_of_nullable_types_and_enums() {
    let schema = json!({
        "type": "object",
        "properties": {
            "kind": { "const": "a" },
            "maybe": { "type": ["string", "null"] },
            "either": { "oneOf": [{ "type": "string" }, { "type": "integer" }] },
            "level": { "type": "integer", "enum": [1, 2] },
            "items": { "type": "array", "items": { "type": "string", "foo": 1 } }
        }
    });
    let cleaned = clean_schema(&schema);
    let props = &cleaned["properties"];
    assert_eq!(props["kind"], json!({ "enum": ["a"], "type": "string" }));
    assert_eq!(props["level"]["type"], "string");
    assert_eq!(
        props["maybe"],
        json!({ "type": "string", "nullable": true })
    );
    assert_eq!(
        props["either"],
        json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }] })
    );
    assert_eq!(props["level"]["enum"], json!(["1", "2"]));
    assert_eq!(props["items"]["items"], json!({ "type": "string" }));
}

#[test]
fn empty_or_non_object_schemas_become_empty_objects() {
    let empty = json!({ "type": "object", "properties": {} });
    assert_eq!(clean_schema(&json!(null)), empty);
    assert_eq!(clean_schema(&json!({})), empty);
    assert_eq!(
        clean_schema(&json!({ "additionalProperties": true })),
        empty
    );
}

#[test]
fn malformed_containers_are_dropped() {
    let cleaned = clean_schema(&json!({
        "type": "object",
        "properties": "nope",
        "anyOf": "nope",
        "enum": "nope"
    }));
    assert_eq!(cleaned, json!({ "type": "object" }));
}

#[test]
fn enum_and_const_types_stay_consistent_with_their_values() {
    let cleaned = clean_schema(&json!({
        "type": "object",
        "properties": {
            "level": { "type": "integer", "enum": [1, 2] },
            "one": { "const": 1 },
            "flag": { "const": true, "type": "boolean" },
            "mixed": { "type": ["integer", "string"], "enum": [1, "a"] }
        }
    }));
    let props = &cleaned["properties"];
    assert_eq!(
        props["level"],
        json!({ "type": "string", "enum": ["1", "2"] })
    );
    assert_eq!(props["one"], json!({ "type": "string", "enum": ["1"] }));
    assert_eq!(props["flag"], json!({ "type": "string", "enum": ["true"] }));
    assert_eq!(
        props["mixed"],
        json!({ "type": "string", "enum": ["1", "a"] })
    );
}

#[test]
fn type_unions_keep_every_alternative() {
    let cleaned = clean_schema(&json!({
        "type": "object",
        "properties": {
            "either": { "type": ["string", "integer"], "description": "d" },
            "maybe_either": { "type": ["string", "integer", "null"] },
            "only_null": { "type": ["null"] }
        }
    }));
    let props = &cleaned["properties"];
    assert_eq!(
        props["either"],
        json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }], "description": "d" })
    );
    assert_eq!(
        props["maybe_either"],
        json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }], "nullable": true })
    );
    assert_eq!(props["only_null"], json!({ "nullable": true }));
}
