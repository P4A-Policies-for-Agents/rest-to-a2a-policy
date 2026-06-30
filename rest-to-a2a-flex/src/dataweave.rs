// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpers for normalizing DataWeave (`pdk::script`) evaluation results.
//!
//! Binding of payload/attributes and the evaluation call happen filter-side
//! (they need the live header/body handlers). This module owns the pure,
//! testable post-processing: collapsing a `Value` into the `Option<String>`
//! the policy needs for prompts / conversation keys / ids, and converting a
//! `Value` into a `serde_json::Value` for the response-mapping body.

use pdk::script::Value;

/// Normalize a DataWeave result into an optional non-empty string.
///
/// - `String` → `Some` unless empty/whitespace-only.
/// - `Number` / `Bool` → stringified (so a numeric conversation id works).
/// - `Null` / `Array` / `Object` → `None` (not a scalar identifier/prompt).
///
/// Used for `promptSelector`, `contextKeySelector`, `taskIdSelector`,
/// `contextIdSelector`. A `None` from the prompt selector is the fail-closed
/// trigger; a `None` from an id/key selector means "no continuation".
pub fn value_to_string(value: Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                // Return the trimmed form: surrounding whitespace in a
                // conversation key or A2A id would otherwise hash/compare
                // differently and silently miss a continuation.
                Some(trimmed.to_string())
            }
        }
        Value::Number(n) => Some(format_number(n)),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// Convert a DataWeave result into a `serde_json::Value` for use as the
/// response body. `pdk::script::Value` carries a serde `into` bridge to
/// `serde_json::Value`, so this is a direct conversion.
pub fn value_to_json(value: Value) -> serde_json::Value {
    value.into()
}

/// Render a DataWeave number without a trailing `.0` when it is integral, so a
/// conversation id like `42` stringifies to `"42"` rather than `"42.0"`.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn string_trimmed_to_option() {
        assert_eq!(
            value_to_string(Value::String("hi".into())),
            Some("hi".into())
        );
        assert_eq!(value_to_string(Value::String("   ".into())), None);
        assert_eq!(value_to_string(Value::String("".into())), None);
        // Surrounding whitespace is stripped so keys/ids compare cleanly.
        assert_eq!(
            value_to_string(Value::String("  conv-1\n".into())),
            Some("conv-1".into())
        );
    }

    #[test]
    fn integral_number_has_no_decimal() {
        assert_eq!(value_to_string(Value::Number(42.0)), Some("42".into()));
        assert_eq!(value_to_string(Value::Number(3.5)), Some("3.5".into()));
    }

    #[test]
    fn bool_stringified() {
        assert_eq!(value_to_string(Value::Bool(true)), Some("true".into()));
    }

    #[test]
    fn null_and_containers_are_none() {
        assert_eq!(value_to_string(Value::Null), None);
        assert_eq!(value_to_string(Value::Array(vec![])), None);
        assert_eq!(value_to_string(Value::Object(HashMap::new())), None);
    }

    #[test]
    fn value_to_json_roundtrip() {
        let mut map = HashMap::new();
        map.insert("k".to_string(), Value::String("v".into()));
        let json = value_to_json(Value::Object(map));
        assert_eq!(json["k"], "v");
    }
}
