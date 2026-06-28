// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal, self-contained JSON-RPC 2.0 envelope for the A2A `SendMessage`
//! call. This policy only ever issues a single request method
//! (`SendMessage`) and reads a single response, so the surface is small:
//! build a request envelope around a pre-built `params` value, and unwrap a
//! response envelope into its `result` / `error` halves.
//!
//! No dependency on any shared A2A crate — only `serde_json`.

use serde_json::{json, Value};

/// JSON-RPC protocol version string. A2A v1.0 JSON-RPC binding pins `"2.0"`.
pub const JSONRPC_VERSION: &str = "2.0";

/// Build a JSON-RPC 2.0 request envelope wrapping `params`.
///
/// `id` is the correlation id echoed by the upstream in its response. We use
/// a deterministic per-request id derived from the same seed as the A2A
/// `messageId` (see [`crate::a2a::generate_message_id`]); JSON-RPC only
/// requires it be unique within the connection and echoed back.
pub fn build_request(id: &str, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// The two mutually-exclusive halves of a JSON-RPC response, plus the echoed
/// `id`. Exactly one of `result` / `error` is `Some` for a well-formed
/// response; a malformed body yields both `None`.
#[derive(Debug, Clone, Default)]
pub struct ResponseEnvelope {
    pub result: Option<Value>,
    pub error: Option<Value>,
}

impl ResponseEnvelope {
    /// Parse a JSON-RPC response body, splitting it into `result` / `error`.
    /// Returns `None` if the body is not valid JSON (caller decides whether
    /// that is fatal). A valid-JSON body that lacks both members yields an
    /// envelope with both fields `None`.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(body).ok()?;
        Some(Self {
            result: value.get("result").cloned(),
            error: value.get("error").cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_shape() {
        let env = build_request("abc", "SendMessage", json!({"message": {"x": 1}}));
        assert_eq!(env["jsonrpc"], "2.0");
        assert_eq!(env["id"], "abc");
        assert_eq!(env["method"], "SendMessage");
        assert_eq!(env["params"]["message"]["x"], 1);
    }

    #[test]
    fn parse_result_envelope() {
        let body = br#"{"jsonrpc":"2.0","id":"abc","result":{"task":{"id":"t1"}}}"#;
        let env = ResponseEnvelope::parse(body).unwrap();
        assert!(env.error.is_none());
        assert_eq!(env.result.unwrap()["task"]["id"], "t1");
    }

    #[test]
    fn parse_error_envelope() {
        let body = br#"{"jsonrpc":"2.0","id":"abc","error":{"code":-32602,"message":"bad"}}"#;
        let env = ResponseEnvelope::parse(body).unwrap();
        assert!(env.result.is_none());
        assert_eq!(env.error.unwrap()["code"], -32602);
    }

    #[test]
    fn parse_non_json_is_none() {
        assert!(ResponseEnvelope::parse(b"not json").is_none());
    }

    #[test]
    fn parse_empty_object_both_none() {
        let env = ResponseEnvelope::parse(b"{}").unwrap();
        assert!(env.result.is_none() && env.error.is_none());
    }
}
