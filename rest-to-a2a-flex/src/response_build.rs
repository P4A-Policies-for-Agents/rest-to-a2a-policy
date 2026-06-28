// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build a flat (optionally nested) REST response object from a list of
//! selection-only DataWeave results.
//!
//! ## Why this exists
//!
//! The Flex Gateway 1.12.1 embedded DataWeave used for `dataweave`-format policy
//! properties evaluates **selectors** (`#[payload.task.id]`) but **rejects
//! object/array construction** (`#[{ a: payload.x }]`, the full `%dw 2.0 … ---`
//! script form). A construction expression silently fails at eval and the policy
//! falls back to raw passthrough — so an operator cannot reshape the A2A result
//! into a bespoke flat envelope using a single `responseMapping`.
//!
//! `responseFields` works around that: each field is a pure **selection**
//! expression evaluated filter-side; the *construction* the runtime can't do is
//! performed here, in Rust. The list is ordered; a `name` may be dotted
//! (`data.taskRef`) to build nested objects.
//!
//! Out of scope (runtime-limited, documented in `docs/spec.md`): arrays,
//! conditionals, and computed/concatenated values. Each value is whatever the
//! selector returned, placed verbatim at its (possibly nested) path.

use serde_json::{Map, Value};

/// One assembled field: its (possibly dotted) output path and the already
/// evaluated, JSON-converted selector result.
pub struct BuiltField {
    pub name: String,
    pub value: Value,
}

/// Outcome of [`assemble`]: the built object plus any field names that were
/// skipped due to a path collision (the caller logs them).
pub struct Assembled {
    pub object: Value,
    /// Field names skipped because their path conflicts with an already-placed
    /// field (e.g. `a.b` after `a` was set to a scalar, or a duplicate leaf).
    pub collisions: Vec<String>,
}

/// Assemble an ordered list of `(dotted-name, value)` pairs into a nested JSON
/// object. Earlier fields win: a later field whose path collides with an
/// already-placed value is skipped and reported in `collisions`.
///
/// An empty `name`, or one with an empty path segment (`a..b`, `.x`, `x.`), is
/// treated as a collision (invalid) and skipped — never silently dropped.
pub fn assemble(fields: Vec<BuiltField>) -> Assembled {
    let mut root = Map::new();
    let mut collisions = Vec::new();

    'fields: for field in fields {
        let segments: Vec<&str> = field.name.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            collisions.push(field.name);
            continue;
        }

        // Walk/create intermediate objects; the last segment is the leaf.
        let mut cursor = &mut root;
        for seg in &segments[..segments.len() - 1] {
            // Descend, creating an empty object if absent. If the slot already
            // holds a non-object, the paths conflict → skip this whole field.
            let entry = cursor
                .entry((*seg).to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            match entry {
                Value::Object(_) => {}
                _ => {
                    collisions.push(field.name);
                    continue 'fields;
                }
            }
            // Re-borrow as the map for the next descent.
            cursor = match cursor.get_mut(*seg) {
                Some(Value::Object(m)) => m,
                _ => unreachable!("entry just ensured an object"),
            };
        }

        let leaf = segments[segments.len() - 1];
        if cursor.contains_key(leaf) {
            // Duplicate leaf, or a leaf landing on a prefix already used as an
            // object — earlier field wins.
            collisions.push(field.name);
            continue;
        }
        cursor.insert(leaf.to_string(), field.value);
    }

    Assembled {
        object: Value::Object(root),
        collisions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(name: &str, value: Value) -> BuiltField {
        BuiltField {
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn flat_fields_build_single_level_object() {
        let out = assemble(vec![
            f("conversationId", json!("ctx-7")),
            f("taskRef", json!("task-42")),
            f("status", json!("TASK_STATE_INPUT_REQUIRED")),
            f("reply", json!("hi")),
        ]);
        assert!(out.collisions.is_empty());
        assert_eq!(
            out.object,
            json!({
                "conversationId": "ctx-7",
                "taskRef": "task-42",
                "status": "TASK_STATE_INPUT_REQUIRED",
                "reply": "hi"
            })
        );
    }

    #[test]
    fn dotted_names_build_nested_objects() {
        let out = assemble(vec![
            f("data.taskRef", json!("task-42")),
            f("data.context", json!("ctx-7")),
            f("meta.state", json!("done")),
        ]);
        assert!(out.collisions.is_empty());
        assert_eq!(
            out.object,
            json!({
                "data": { "taskRef": "task-42", "context": "ctx-7" },
                "meta": { "state": "done" }
            })
        );
    }

    #[test]
    fn deeply_nested_path() {
        let out = assemble(vec![f("a.b.c.d", json!(1))]);
        assert!(out.collisions.is_empty());
        assert_eq!(out.object, json!({ "a": { "b": { "c": { "d": 1 } } } }));
    }

    #[test]
    fn non_string_values_are_preserved_verbatim() {
        let out = assemble(vec![
            f("n", json!(42)),
            f("b", json!(true)),
            f("nul", Value::Null),
            f("arr", json!([1, 2])),
            f("obj", json!({ "k": "v" })),
        ]);
        assert!(out.collisions.is_empty());
        assert_eq!(out.object["n"], json!(42));
        assert_eq!(out.object["b"], json!(true));
        assert_eq!(out.object["nul"], Value::Null);
        assert_eq!(out.object["arr"], json!([1, 2]));
        assert_eq!(out.object["obj"], json!({ "k": "v" }));
    }

    #[test]
    fn duplicate_leaf_first_wins_and_is_reported() {
        let out = assemble(vec![f("x", json!("first")), f("x", json!("second"))]);
        assert_eq!(out.object, json!({ "x": "first" }));
        assert_eq!(out.collisions, vec!["x".to_string()]);
    }

    #[test]
    fn scalar_then_nested_under_it_collides() {
        // `a` is a scalar; `a.b` cannot nest under it.
        let out = assemble(vec![f("a", json!("scalar")), f("a.b", json!("nested"))]);
        assert_eq!(out.object, json!({ "a": "scalar" }));
        assert_eq!(out.collisions, vec!["a.b".to_string()]);
    }

    #[test]
    fn nested_then_scalar_on_prefix_collides() {
        // `a.b` makes `a` an object; a later bare `a` leaf conflicts.
        let out = assemble(vec![f("a.b", json!("nested")), f("a", json!("scalar"))]);
        assert_eq!(out.object, json!({ "a": { "b": "nested" } }));
        assert_eq!(out.collisions, vec!["a".to_string()]);
    }

    #[test]
    fn empty_segments_are_invalid_and_reported() {
        let out = assemble(vec![
            f("", json!(1)),
            f(".x", json!(2)),
            f("x.", json!(3)),
            f("a..b", json!(4)),
        ]);
        assert_eq!(out.object, json!({}));
        assert_eq!(out.collisions.len(), 4);
    }

    #[test]
    fn empty_list_yields_empty_object() {
        let out = assemble(vec![]);
        assert_eq!(out.object, json!({}));
        assert!(out.collisions.is_empty());
    }
}
