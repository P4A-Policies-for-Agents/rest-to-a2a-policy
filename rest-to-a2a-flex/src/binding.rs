// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Upstream wire-binding selection and request-body framing.
//!
//! Both bindings forward **in-band** (`Flow::Continue` + body rewrite); the
//! upstream path is operator-owned via the route `destinationPath` (see
//! `docs/spec.md`). This module never mutates `:path` — it only decides how to
//! frame the A2A `SendMessage` `params` on the wire and what `content-type` to
//! set:
//!
//! - [`UpstreamBinding::JsonRpc`] — wrap `params` in a JSON-RPC 2.0 envelope.
//! - [`UpstreamBinding::HttpJson`] — emit the bare `params` payload.

use std::str::FromStr;

use serde_json::{json, Value};

use crate::a2a::SEND_MESSAGE_METHOD;
use crate::jsonrpc;

/// The wire binding used to talk to the upstream A2A agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamBinding {
    /// JSON-RPC 2.0 over HTTP POST. The A2A operation is named by the
    /// `method` field (`SendMessage`); the result/error lives in-band in the
    /// response envelope.
    JsonRpc,
    /// A2A v1.0 HTTP+JSON binding. The operation is named by the route
    /// (`POST /message:send`, set by the operator via `destinationPath`); the
    /// body is the bare `params` payload and errors use native HTTP status.
    HttpJson,
}

/// Error returned when an unknown `upstreamBinding` string is configured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown upstream binding '{0}' (expected 'jsonrpc' or 'httpjson')")]
pub struct UnknownBinding(String);

impl FromStr for UpstreamBinding {
    type Err = UnknownBinding;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "jsonrpc" | "json-rpc" | "json_rpc" => Ok(Self::JsonRpc),
            "httpjson" | "http+json" | "http-json" | "http_json" => Ok(Self::HttpJson),
            other => Err(UnknownBinding(other.to_string())),
        }
    }
}

impl UpstreamBinding {
    /// Frame the A2A `SendMessage` `params` into the outbound request body for
    /// this binding. `request_id` is the JSON-RPC correlation id (ignored by
    /// the HTTP+JSON binding).
    ///
    /// Returns the serialized body bytes. The caller sets `content-type:
    /// application/json` and removes `content-length` before writing, for both
    /// bindings.
    pub fn frame_request(&self, request_id: &str, params: Value) -> Vec<u8> {
        let body = match self {
            Self::JsonRpc => jsonrpc::build_request(request_id, SEND_MESSAGE_METHOD, params),
            Self::HttpJson => params,
        };
        // serde_json on an owned in-memory Value cannot fail.
        serde_json::to_vec(&body).unwrap_or_default()
    }

    /// Extract the raw A2A `SendMessageResult` value from an upstream response
    /// body for this binding, along with any upstream error.
    ///
    /// - JSON-RPC: split the envelope into `result` / `error`.
    /// - HTTP+JSON: the body *is* the result; a `google.rpc.Status`-shaped
    ///   `{"error": {...}}` body is surfaced as the error.
    ///
    /// `None` for both fields means the body was not valid JSON (caller treats
    /// the response as opaque / passes it through).
    pub fn parse_response(&self, body: &[u8]) -> ResponseParts {
        match self {
            Self::JsonRpc => match jsonrpc::ResponseEnvelope::parse(body) {
                Some(env) => ResponseParts {
                    result: env.result,
                    error: env.error,
                },
                None => ResponseParts::default(),
            },
            Self::HttpJson => match serde_json::from_slice::<Value>(body) {
                Ok(value) => {
                    let error = value.get("error").cloned();
                    // On a successful HTTP+JSON reply the whole body is the
                    // SendMessageResult oneof; on an error reply we still hand
                    // the body back as `result` so response DataWeave can shape
                    // it if desired.
                    ResponseParts {
                        result: Some(value),
                        error,
                    }
                }
                Err(_) => ResponseParts::default(),
            },
        }
    }
}

/// The two halves of a parsed upstream response. `result` is the raw A2A
/// `SendMessageResult` value that response DataWeave runs against; `error` is
/// any binding-native error envelope (present alongside `result` for
/// HTTP+JSON, exclusive of it for JSON-RPC).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseParts {
    pub result: Option<Value>,
    pub error: Option<Value>,
}

/// Build a REST-facing JSON error response body for a fail-closed request
/// rejection (missing/invalid prompt). Returned as bytes for
/// `Response::with_body`.
pub fn caller_error_body(message: &str) -> Vec<u8> {
    let body = json!({
        "error": {
            "message": message,
            "source": "rest-to-a2a",
        }
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_accepts_aliases() {
        assert_eq!("jsonrpc".parse(), Ok(UpstreamBinding::JsonRpc));
        assert_eq!("JSON-RPC".parse(), Ok(UpstreamBinding::JsonRpc));
        assert_eq!("httpjson".parse(), Ok(UpstreamBinding::HttpJson));
        assert_eq!("http+json".parse(), Ok(UpstreamBinding::HttpJson));
    }

    #[test]
    fn from_str_rejects_unknown() {
        let err = "grpc".parse::<UpstreamBinding>().unwrap_err();
        assert_eq!(err, UnknownBinding("grpc".to_string()));
    }

    #[test]
    fn jsonrpc_frames_envelope() {
        let params = json!({ "message": { "messageId": "m1" } });
        let bytes = UpstreamBinding::JsonRpc.frame_request("rid", params);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "rid");
        assert_eq!(v["method"], "SendMessage");
        assert_eq!(v["params"]["message"]["messageId"], "m1");
    }

    #[test]
    fn httpjson_frames_bare_params() {
        let params = json!({ "message": { "messageId": "m1" } });
        let bytes = UpstreamBinding::HttpJson.frame_request("ignored", params);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        // No envelope — the body is the params object itself.
        assert!(v.get("jsonrpc").is_none());
        assert_eq!(v["message"]["messageId"], "m1");
    }

    #[test]
    fn jsonrpc_parses_result_and_error() {
        let ok = UpstreamBinding::JsonRpc
            .parse_response(br#"{"jsonrpc":"2.0","id":"1","result":{"task":{"id":"t"}}}"#);
        assert_eq!(ok.result.unwrap()["task"]["id"], "t");
        assert!(ok.error.is_none());

        let err = UpstreamBinding::JsonRpc
            .parse_response(br#"{"jsonrpc":"2.0","id":"1","error":{"code":-32602}}"#);
        assert!(err.result.is_none());
        assert_eq!(err.error.unwrap()["code"], -32602);
    }

    #[test]
    fn httpjson_body_is_result() {
        let parts =
            UpstreamBinding::HttpJson.parse_response(br#"{"task":{"id":"t","contextId":"c"}}"#);
        assert_eq!(parts.result.unwrap()["task"]["contextId"], "c");
        assert!(parts.error.is_none());
    }

    #[test]
    fn httpjson_surfaces_google_rpc_status() {
        let parts = UpstreamBinding::HttpJson
            .parse_response(br#"{"error":{"code":404,"message":"not found"}}"#);
        assert_eq!(parts.error.unwrap()["code"], 404);
        // Body still available as result for response DataWeave.
        assert!(parts.result.is_some());
    }

    #[test]
    fn non_json_response_is_empty_parts() {
        assert_eq!(
            UpstreamBinding::JsonRpc.parse_response(b"<html>"),
            ResponseParts::default()
        );
        assert_eq!(
            UpstreamBinding::HttpJson.parse_response(b"<html>"),
            ResponseParts::default()
        );
    }

    #[test]
    fn caller_error_body_shape() {
        let bytes = caller_error_body("missing prompt");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["message"], "missing prompt");
        assert_eq!(v["error"]["source"], "rest-to-a2a");
    }
}
