//! Shared helpers for the schema-synchronization tests.
//!
//! `schemas/*.json` are hand-written and must agree with the types in this
//! crate. Every schema-sync test module compares them the same way, so the
//! comparison lives here once rather than being re-derived per module.

use std::collections::BTreeSet;

use serde_json::Value;

/// Returns the wire string a serialized enum variant produces.
pub fn wire_string(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("serialization should succeed")
        .as_str()
        .expect("enums should serialize to strings")
        .to_owned()
}

/// Collects a JSON array of strings, such as a schema's `required` list.
pub fn string_set(values: &Value) -> BTreeSet<String> {
    values
        .as_array()
        .expect("schema field should be an array")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("schema entries should be strings")
                .to_owned()
        })
        .collect()
}

/// Collects the property names an object schema declares.
pub fn properties(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .expect("schema should declare properties")
        .keys()
        .cloned()
        .collect()
}

/// Collects the field names a value serializes to.
pub fn serialized_fields(value: impl serde::Serialize) -> BTreeSet<String> {
    serde_json::to_value(value)
        .expect("value should serialize")
        .as_object()
        .expect("value should serialize to an object")
        .keys()
        .cloned()
        .collect()
}

/// Asserts that `generated` and `checked_in` declare the same object shape.
///
/// Property names, the `required` list, and `additionalProperties` are compared;
/// a definition that drifts in any of the three fails the calling test. Neither
/// property types nor bounds are compared, so a `type` that disagrees passes
/// here just as a dropped bound does. A field whose checked-in schema constrains
/// it beyond a bare `type` (a `minLength`, a `pattern`, a numeric bound) or that
/// carries a `deserialize_with` validator requires its own dedicated test
/// instead, one that binds each checked-in schema carrying that constraint
/// rather than merely one of them; for a `deserialize_with` validator, which no
/// checked-in schema can state, the counterpart to bind is that field's
/// declared `type` in each schema publishing it. That rule covers the narrower
/// subset, so a field declared with nothing but a `type` stays uncovered by
/// both.
pub fn assert_same_shape(generated: &schemars::Schema, checked_in: &Value) {
    let generated = serde_json::to_value(generated).expect("generated schema should serialize");
    assert_eq!(properties(&generated), properties(checked_in));
    assert_eq!(
        string_set(&generated["required"]),
        string_set(&checked_in["required"])
    );
    assert_eq!(
        generated["additionalProperties"],
        checked_in["additionalProperties"]
    );
}
