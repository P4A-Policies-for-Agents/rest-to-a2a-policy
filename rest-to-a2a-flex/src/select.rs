// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Dotted JSON-path selection over a `serde_json::Value`.
//!
//! `responseFields` selectors are evaluated here, in Rust, rather than as
//! embedded DataWeave. The gateway's `dw2pel` config transform compiles only
//! **top-level** `format: dataweave` properties — it does not recurse into
//! `format: dataweave` declared inside array-item properties, so a nested
//! DataWeave selector reaches the policy as a raw `#[...]` string that fails to
//! deserialize into a PEL `Expression` (verified against Flex 1.12.1). A plain
//! path selector sidesteps that entirely and is strictly more capable: array
//! indices work and JSON numbers are preserved verbatim (no double coercion).
//!
//! ## Path grammar
//!
//! - Segments are separated by `.` — e.g. `task.status.state`.
//! - A segment may carry one or more array indices — e.g. `parts[0]`,
//!   `matrix[1][2]`. A bare `[0]` segment (leading index) is also accepted.
//! - An optional leading `payload.` (or a lone `payload`) is stripped, so the
//!   selector reads the same way the bound `payload` does in `responseMapping`.
//! - An empty path (or just `payload`) selects the whole root.
//!
//! Anything the path cannot resolve (missing key, out-of-range index, indexing
//! a non-array, keying a non-object, malformed token) yields `None`. The caller
//! maps `None` to JSON `null` — consistent with the policy's non-fatal posture.

use serde_json::Value;

/// Resolve `path` against `root`. Returns the referenced sub-value, or `None`
/// if any segment fails to resolve. See the module docs for the grammar.
pub fn select<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let trimmed = path.trim();

    // Strip an optional leading `payload.` / lone `payload` for familiarity
    // with the `responseMapping` mental model (where the result is bound as
    // `payload`). The result object IS the selection root here.
    let rest = if let Some(stripped) = trimmed.strip_prefix("payload.") {
        stripped
    } else if trimmed == "payload" {
        ""
    } else {
        trimmed
    };

    if rest.is_empty() {
        return Some(root);
    }

    let mut cur = root;
    for token in rest.split('.') {
        cur = resolve_token(cur, token)?;
    }
    Some(cur)
}

/// Resolve one dot-delimited token: an optional object key followed by zero or
/// more `[index]` array steps (e.g. `parts[0][1]`, `task`, `[2]`).
fn resolve_token<'a>(value: &'a Value, token: &str) -> Option<&'a Value> {
    // The key part runs up to the first '[' (or the whole token if no index).
    let key_end = token.find('[').unwrap_or(token.len());
    let key = &token[..key_end];

    let mut cur = if key.is_empty() {
        // A leading-index token like `[0]` indexes the current value directly.
        value
    } else {
        value.get(key)?
    };

    let mut bracket = &token[key_end..];
    while let Some(stripped) = bracket.strip_prefix('[') {
        let close = stripped.find(']')?;
        let index: usize = stripped[..close].parse().ok()?;
        cur = cur.get(index)?;
        bracket = &stripped[close + 1..];
    }

    // Trailing garbage after the last ']' (e.g. `parts[0]x`) is malformed.
    if bracket.is_empty() {
        Some(cur)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "task": {
                "id": "task-42",
                "contextId": "ctx-7",
                "status": {
                    "state": "TASK_STATE_INPUT_REQUIRED",
                    "update": { "parts": [{ "text": "hello" }, { "text": "world" }] }
                },
                "count": 3,
                "flag": true
            }
        })
    }

    #[test]
    fn selects_nested_scalar() {
        assert_eq!(select(&sample(), "task.id"), Some(&json!("task-42")));
        assert_eq!(
            select(&sample(), "task.status.state"),
            Some(&json!("TASK_STATE_INPUT_REQUIRED"))
        );
    }

    #[test]
    fn selects_through_array_index() {
        assert_eq!(
            select(&sample(), "task.status.update.parts[0].text"),
            Some(&json!("hello"))
        );
        assert_eq!(
            select(&sample(), "task.status.update.parts[1].text"),
            Some(&json!("world"))
        );
    }

    #[test]
    fn preserves_non_string_types() {
        assert_eq!(select(&sample(), "task.count"), Some(&json!(3)));
        assert_eq!(select(&sample(), "task.flag"), Some(&json!(true)));
    }

    #[test]
    fn strips_optional_payload_prefix() {
        assert_eq!(
            select(&sample(), "payload.task.id"),
            Some(&json!("task-42"))
        );
        assert!(select(&sample(), "payload").unwrap().is_object());
    }

    #[test]
    fn empty_path_selects_root() {
        assert!(select(&sample(), "").unwrap().is_object());
        assert!(select(&sample(), "  ").unwrap().is_object());
    }

    #[test]
    fn missing_paths_yield_none() {
        assert_eq!(select(&sample(), "task.missing"), None);
        assert_eq!(select(&sample(), "task.status.update.parts[9].text"), None);
        assert_eq!(select(&sample(), "task.id.deeper"), None); // key into a scalar
        assert_eq!(select(&sample(), "task[0]"), None); // index into an object
    }

    #[test]
    fn malformed_tokens_yield_none() {
        assert_eq!(select(&sample(), "task.status.update.parts[0]x"), None);
        assert_eq!(select(&sample(), "task.status.update.parts[abc]"), None);
        assert_eq!(select(&sample(), "task.status.update.parts["), None);
    }

    #[test]
    fn leading_index_token_indexes_root() {
        let arr = json!(["a", "b", "c"]);
        assert_eq!(select(&arr, "[1]"), Some(&json!("b")));
    }
}
