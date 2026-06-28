// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Self-contained A2A protocol v1.0 surface for the `SendMessage` bridge.
//!
//! This module owns the small slice of A2A v1.0 the policy actually emits and
//! reads: the `SendMessage` method name, the `Message` / `configuration`
//! request shape, the task-state classification that drives continuation, and
//! a parser that pulls `(state, taskId, contextId)` out of a raw
//! `SendMessageResult` regardless of upstream binding.
//!
//! **A2A v1.0 wire forms are canonical proto-JSON** (verified against the
//! reference v1 types): task states are `TASK_STATE_*`, message role is
//! `ROLE_USER`, and a send result is the externally-tagged oneof
//! `{"task": {...}}` | `{"message": {...}}`. Legacy lowercase v0.3 forms
//! (`"working"`, `"user"`) are NOT emitted or accepted — this policy is
//! v1.0-only (see `docs/spec.md`).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// A2A v1.0 JSON-RPC method name for a unary send. The HTTP+JSON binding names
/// the same operation by route (`POST /message:send`); see [`crate::binding`].
pub const SEND_MESSAGE_METHOD: &str = "SendMessage";

/// A2A v1.0 message author role for a caller-originated message.
pub const ROLE_USER: &str = "ROLE_USER";

/// Continuation identifiers carried across turns of a multi-turn task.
///
/// Either field may be absent: a fresh conversation has neither; a resumed one
/// has both (cache mode) or whatever the client supplied (explicit mode).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Continuation {
    pub task_id: Option<String>,
    pub context_id: Option<String>,
}

impl Continuation {
    /// True when no identifiers are present — i.e. this is a fresh send with no
    /// task to resume.
    pub fn is_empty(&self) -> bool {
        self.task_id.is_none() && self.context_id.is_none()
    }
}

/// The optional A2A `SendMessage` `configuration` block (operator-supplied via
/// the policy schema's `a2aConfiguration`).
#[derive(Debug, Clone, Default)]
pub struct SendConfiguration {
    pub accepted_output_modes: Option<Vec<String>>,
    /// A2A v1.0 proto-JSON spells the blocking flag `returnImmediately` (the
    /// inverse of v0.3's `blocking`). The policy schema exposes `blocking` for
    /// operator familiarity; we translate here: `returnImmediately = !blocking`.
    pub blocking: bool,
}

/// Build the `params` object for an A2A v1.0 `SendMessage` call.
///
/// Shape (proto-JSON, A2A v1.0):
/// ```json
/// {
///   "message": {
///     "messageId": "...",
///     "role": "ROLE_USER",
///     "parts": [{ "text": "<prompt>" }],
///     "taskId": "...",     // only when resuming
///     "contextId": "..."   // only when resuming
///   },
///   "configuration": { "acceptedOutputModes": [...], "returnImmediately": false }
/// }
/// ```
/// `configuration` is omitted entirely when no `a2aConfiguration` is set.
pub fn build_send_message(
    prompt: &str,
    message_id: &str,
    continuation: &Continuation,
    configuration: Option<&SendConfiguration>,
) -> Value {
    let mut message = Map::new();
    message.insert("messageId".to_string(), json!(message_id));
    message.insert("role".to_string(), json!(ROLE_USER));
    message.insert("parts".to_string(), json!([{ "text": prompt }]));
    if let Some(task_id) = &continuation.task_id {
        message.insert("taskId".to_string(), json!(task_id));
    }
    if let Some(context_id) = &continuation.context_id {
        message.insert("contextId".to_string(), json!(context_id));
    }

    let mut params = Map::new();
    params.insert("message".to_string(), Value::Object(message));

    if let Some(cfg) = configuration {
        let mut config = Map::new();
        if let Some(modes) = &cfg.accepted_output_modes {
            config.insert("acceptedOutputModes".to_string(), json!(modes));
        }
        // Always emit the (inverted) blocking flag so a configured block makes
        // the upstream wait for a terminal/continuable status before replying.
        config.insert("returnImmediately".to_string(), json!(!cfg.blocking));
        params.insert("configuration".to_string(), Value::Object(config));
    }

    Value::Object(params)
}

/// Classification of an A2A task state for continuation purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateClass {
    /// `submitted` / `working` / `input-required` (and `auth-required`): the
    /// task is alive; persist `taskId`+`contextId` to continue.
    Continuable,
    /// `completed` / `failed` / `canceled` / `rejected`: the task is done;
    /// evict any cached continuation.
    Terminal,
    /// `unspecified` or anything unrecognized: treat as non-resumable, do not
    /// persist. (Defensive — a well-behaved v1.0 upstream never sends this for
    /// a real reply.)
    Unknown,
}

/// Map an A2A v1.0 canonical task-state string to its continuation class.
///
/// Accepts the canonical proto form (`TASK_STATE_WORKING`). For resilience
/// against an upstream that leaks legacy lowercase forms, the v0.3 spellings
/// (`"working"`, `"input-required"`, …) are also recognized — they classify
/// identically and never reach the wire we emit.
pub fn classify_state(state: &str) -> StateClass {
    match state {
        "TASK_STATE_SUBMITTED"
        | "TASK_STATE_WORKING"
        | "TASK_STATE_INPUT_REQUIRED"
        | "TASK_STATE_AUTH_REQUIRED"
        | "submitted"
        | "working"
        | "input-required"
        | "auth-required" => StateClass::Continuable,
        "TASK_STATE_COMPLETED"
        | "TASK_STATE_FAILED"
        | "TASK_STATE_CANCELED"
        | "TASK_STATE_REJECTED"
        | "completed"
        | "failed"
        | "canceled"
        | "cancelled"
        | "rejected" => StateClass::Terminal,
        _ => StateClass::Unknown,
    }
}

/// Continuation-relevant facts extracted from a raw `SendMessageResult`.
///
/// The same shape feeds both bindings: the JSON-RPC binding hands us the
/// unwrapped `result`; the HTTP+JSON binding hands us the bare response body.
/// Both are the A2A v1.0 send-result oneof `{"task": {...}}` | `{"message":
/// {...}}`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedResult {
    /// The task lifecycle state string, if the result is a `task`. `None` for a
    /// direct `message` reply (which carries no task and is inherently
    /// single-shot / terminal).
    pub state: Option<String>,
    pub task_id: Option<String>,
    pub context_id: Option<String>,
}

impl ParsedResult {
    /// Continuation class implied by the parsed state. A direct `message` reply
    /// (no state) is `Terminal` — there is no task to resume.
    pub fn class(&self) -> StateClass {
        match &self.state {
            Some(s) => classify_state(s),
            None => StateClass::Terminal,
        }
    }

    /// The continuation identifiers to persist for a continuable task.
    pub fn continuation(&self) -> Continuation {
        Continuation {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
        }
    }
}

/// Extract continuation facts from a raw A2A v1.0 `SendMessageResult` value.
///
/// Handles both arms of the send-result oneof:
/// - `{"task": {"id", "contextId", "status": {"state"}}}` → state + ids.
/// - `{"message": {"taskId", "contextId"}}` → ids only, no state (terminal).
///
/// Tolerant of a bare `Task` root (`{"id","status":{...}}`) in case an upstream
/// omits the `task` wrapper. Unknown shapes yield an all-`None`
/// [`ParsedResult`] (classified `Terminal`).
pub fn parse_result(result: &Value) -> ParsedResult {
    if let Some(task) = result.get("task").or_else(|| {
        // Bare-Task fallback: a top-level object carrying `status.state`.
        result.get("status").is_some().then_some(result)
    }) {
        return ParsedResult {
            state: task
                .get("status")
                .and_then(|s| s.get("state"))
                .and_then(Value::as_str)
                .map(str::to_string),
            task_id: task.get("id").and_then(Value::as_str).map(str::to_string),
            context_id: task
                .get("contextId")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
    }

    if let Some(message) = result.get("message") {
        return ParsedResult {
            state: None,
            task_id: message
                .get("taskId")
                .and_then(Value::as_str)
                .map(str::to_string),
            context_id: message
                .get("contextId")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
    }

    ParsedResult::default()
}

/// Derive a deterministic, opaque message id from a per-request seed.
///
/// A2A requires a unique `messageId` per message; we avoid a `uuid` dependency
/// by hashing the seed (request id ++ prompt) with SHA-256 and hex-encoding a
/// prefix. Deterministic per (seed) — fine for correlation, and carries no PII
/// (one-way hash).
pub fn generate_message_id(seed: &[u8]) -> String {
    let digest = Sha256::digest(seed);
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_message_fresh_shape() {
        let params = build_send_message("hello", "mid1", &Continuation::default(), None);
        assert_eq!(params["message"]["messageId"], "mid1");
        assert_eq!(params["message"]["role"], "ROLE_USER");
        assert_eq!(params["message"]["parts"][0]["text"], "hello");
        // No continuation ids, no configuration on a fresh send.
        assert!(params["message"].get("taskId").is_none());
        assert!(params["message"].get("contextId").is_none());
        assert!(params.get("configuration").is_none());
    }

    #[test]
    fn send_message_injects_continuation() {
        let cont = Continuation {
            task_id: Some("t1".into()),
            context_id: Some("c1".into()),
        };
        let params = build_send_message("hi", "mid2", &cont, None);
        assert_eq!(params["message"]["taskId"], "t1");
        assert_eq!(params["message"]["contextId"], "c1");
    }

    #[test]
    fn send_message_configuration_inverts_blocking() {
        let cfg = SendConfiguration {
            accepted_output_modes: Some(vec!["text/plain".into()]),
            blocking: true,
        };
        let params = build_send_message("hi", "m", &Continuation::default(), Some(&cfg));
        assert_eq!(
            params["configuration"]["acceptedOutputModes"][0],
            "text/plain"
        );
        // blocking:true → returnImmediately:false
        assert_eq!(params["configuration"]["returnImmediately"], false);
    }

    #[test]
    fn classify_canonical_and_legacy() {
        assert_eq!(
            classify_state("TASK_STATE_WORKING"),
            StateClass::Continuable
        );
        assert_eq!(
            classify_state("TASK_STATE_INPUT_REQUIRED"),
            StateClass::Continuable
        );
        assert_eq!(classify_state("TASK_STATE_COMPLETED"), StateClass::Terminal);
        assert_eq!(classify_state("input-required"), StateClass::Continuable);
        assert_eq!(classify_state("failed"), StateClass::Terminal);
        assert_eq!(
            classify_state("TASK_STATE_UNSPECIFIED"),
            StateClass::Unknown
        );
        assert_eq!(classify_state("bogus"), StateClass::Unknown);
    }

    #[test]
    fn parse_task_result() {
        let result = json!({
            "task": {
                "id": "t9",
                "contextId": "c9",
                "status": { "state": "TASK_STATE_INPUT_REQUIRED" }
            }
        });
        let parsed = parse_result(&result);
        assert_eq!(parsed.task_id.as_deref(), Some("t9"));
        assert_eq!(parsed.context_id.as_deref(), Some("c9"));
        assert_eq!(parsed.class(), StateClass::Continuable);
        assert_eq!(parsed.continuation().task_id.as_deref(), Some("t9"));
    }

    #[test]
    fn parse_bare_task_fallback() {
        let result = json!({ "id": "t1", "status": { "state": "TASK_STATE_COMPLETED" } });
        let parsed = parse_result(&result);
        assert_eq!(parsed.task_id.as_deref(), Some("t1"));
        assert_eq!(parsed.class(), StateClass::Terminal);
    }

    #[test]
    fn parse_message_result_is_terminal() {
        let result = json!({ "message": { "messageId": "m1", "contextId": "c1" } });
        let parsed = parse_result(&result);
        assert_eq!(parsed.state, None);
        assert_eq!(parsed.context_id.as_deref(), Some("c1"));
        assert_eq!(parsed.class(), StateClass::Terminal);
    }

    #[test]
    fn parse_unknown_shape_default() {
        let parsed = parse_result(&json!({ "weird": true }));
        assert_eq!(parsed, ParsedResult::default());
        assert_eq!(parsed.class(), StateClass::Terminal);
    }

    #[test]
    fn message_id_deterministic_and_hex() {
        let a = generate_message_id(b"seed-1");
        let b = generate_message_id(b"seed-1");
        let c = generate_message_id(b"seed-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
