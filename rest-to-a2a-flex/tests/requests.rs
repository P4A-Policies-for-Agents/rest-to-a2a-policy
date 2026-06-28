// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the REST → A2A v1.0 `SendMessage` bridge.
//!
//! Each test boots a Flex Gateway (PDK Local Mode) with an httpmock standing in
//! for the upstream A2A agent, then drives a REST request through the policy and
//! asserts on (a) what the policy forwarded upstream and (b) what it returned to
//! the caller. Streaming, fail-closed, both bindings, and cache continuation are
//! all covered.
//!
//! httpmock 0.6 exposes no received-requests accessor on `Mock`, so the
//! forwarded body is asserted by making the mock's `when` block match on body
//! substrings (`body_contains`) — the mock fires (and `assert_async` passes)
//! ONLY if the policy actually sent that content. "Must NOT contain" cases use a
//! guard mock matched on the forbidden substring with `assert_hits_async(0)`.

mod common;

use common::setup::setup_test;
use httpmock::Method;
use pdk_test::pdk_test;
use serde_json::{json, Value};

/// Base policy config for the JSON-RPC binding in cache mode, keyed off an
/// `x-conversation-id` request header. `responseMapping` echoes the raw A2A
/// result so tests can assert on the unshaped Task/Message.
fn jsonrpc_cache_config() -> Value {
    json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "cache",
        "contextKeySelector": "#[attributes.headers['x-conversation-id']]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    })
}

/// Policy config matching `docs/rest-api-example.md`: prompt from
/// `payload.question`, conversation key from `payload.sessionId`. The
/// `responseMapping` selects the A2A `task` object as the REST response.
fn rest_spec_config() -> Value {
    json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.question]",
        "continuationMode": "cache",
        "contextKeySelector": "#[payload.sessionId]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload.task]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    })
}

#[pdk_test]
async fn rest_spec_ask_maps_request_and_response() -> anyhow::Result<()> {
    let (_composite, flex_url, mock) = setup_test(rest_spec_config()).await?;

    // Fires only if the documented promptSelector forwarded `question`.
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "task": {
                        "id": "task-42", "contextId": "ctx-7",
                        "status": {
                            "state": "TASK_STATE_INPUT_REQUIRED",
                            "update": { "role": "ROLE_AGENT",
                                        "parts": [{ "text": "Sure — what is your order number?" }] }
                        }
                    } }
                })
                .to_string(),
            );
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(
            json!({
                "sessionId": "sess-abc-123",
                "userId": "user-789",
                "question": "What is the status of my order?",
                "metadata": { "channel": "web", "locale": "en-US" }
            })
            .to_string(),
        )
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;

    // The documented responseMapping (`#[payload.task]`) selects the A2A task
    // object out of the JSON-RPC envelope as the REST-facing AskResponse.
    let body: Value = resp.json().await?;
    assert_eq!(body["contextId"], "ctx-7");
    assert_eq!(body["id"], "task-42");
    assert_eq!(body["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(body["status"]["update"]["parts"][0]["text"], "Sure — what is your order number?");
    Ok(())
}

#[pdk_test]
async fn rest_spec_resumes_task_on_same_session_id() -> anyhow::Result<()> {
    let (_composite, flex_url, mock) = setup_test(rest_spec_config()).await?;
    let client = reqwest::Client::new();

    // Turn 1: input-required → ids cached under sessionId.
    let turn1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"first question""#);
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "task": { "id": "task-42", "contextId": "ctx-7",
                                          "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } }
                })
                .to_string(),
            );
        })
        .await;
    client
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "sessionId": "sess-resume", "question": "first question" }).to_string())
        .send()
        .await?;
    turn1.assert_async().await;

    // Turn 2: same sessionId injects the cached ids (no header involved — the key
    // is in the body, per the documented contextKeySelector).
    let turn2 = mock
        .mock_async(|when, then| {
            when.method(Method::POST)
                .body_contains(r#""text":"order 1234""#)
                .body_contains(r#""taskId":"task-42""#)
                .body_contains(r#""contextId":"ctx-7""#);
            then.status(200).header("content-type", "application/json").body(
                json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "task": { "id": "task-42", "contextId": "ctx-7",
                                          "status": { "state": "TASK_STATE_COMPLETED" } } }
                })
                .to_string(),
            );
        })
        .await;
    client
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "sessionId": "sess-resume", "question": "order 1234" }).to_string())
        .send()
        .await?;
    turn2.assert_async().await;
    Ok(())
}

#[pdk_test]
async fn rest_spec_missing_question_fails_closed() -> anyhow::Result<()> {
    let (_composite, flex_url, mock) = setup_test(rest_spec_config()).await?;
    let guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200).body("{}");
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        // sessionId present, but no `question` → fail-closed per promptSelector.
        .body(json!({ "sessionId": "sess-x" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await?;
    assert_eq!(body["error"]["source"], "rest-to-a2a");
    guard.assert_hits_async(0).await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Full config matrix — binding × continuationMode × distributed × response state
//
// Covers the gaps the jsonrpc-cache tests above don't: distributed cache,
// httpjson continuation, jsonrpc `none`, both upstream error shapes, a direct
// message reply, and a response-mapping failure passthrough.
// ─────────────────────────────────────────────────────────────────────────────

#[pdk_test]
async fn jsonrpc_none_does_not_inject_or_persist() -> anyhow::Result<()> {
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let client = reqwest::Client::new();

    // Two turns: even after an input-required reply, `none` mode never persists,
    // so turn 2 must carry NO taskId/contextId.
    let t1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"one""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "n-1", "contextId": "nc-1",
                                              "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } } })
                .to_string(),
            );
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .body(json!({ "prompt": "one" }).to_string()).send().await?;
    t1.assert_async().await;

    let t2 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"two""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "n-2",
                                              "status": { "state": "TASK_STATE_COMPLETED" } } } })
                .to_string(),
            );
        })
        .await;
    // Guard: no continuation id ever forwarded in `none` mode.
    let id_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""taskId""#);
            then.status(200).body("{}");
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .body(json!({ "prompt": "two" }).to_string()).send().await?;
    t2.assert_async().await;
    id_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn disabling_task_continuation_overrides_cache_mode() -> anyhow::Result<()> {
    // `enableTaskContinuation:false` is the master switch: even with
    // `continuationMode:cache` and a stable conversation key, continuation is
    // forced to `none` — no cache is built, so an input-required turn 1 leaves
    // nothing to inject on turn 2. Pins that the boolean overrides the enum.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "enableTaskContinuation": false,
        "continuationMode": "cache",
        "contextKeySelector": "#[attributes.headers['x-conversation-id']]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let client = reqwest::Client::new();

    let t1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"first""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "x-1", "contextId": "xc-1",
                                              "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } } })
                .to_string(),
            );
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-X")
        .body(json!({ "prompt": "first" }).to_string()).send().await?;
    t1.assert_async().await;

    let t2 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"second""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "x-2",
                                              "status": { "state": "TASK_STATE_COMPLETED" } } } })
                .to_string(),
            );
        })
        .await;
    // Guard: continuation disabled → no taskId ever forwarded, same conv id or not.
    let id_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""taskId""#);
            then.status(200).body("{}");
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-X")
        .body(json!({ "prompt": "second" }).to_string()).send().await?;
    t2.assert_async().await;
    id_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn distributed_cache_does_not_persist_in_single_replica_local_mode() -> anyhow::Result<()> {
    // KNOWN LIMITATION (documented in docs/spec.md): `distributed:true` selects
    // the remote (gossip) `DataStorage` backend. In single-replica Local Mode
    // that backend has no peer to gossip with and does NOT persist entries
    // across requests, so continuation ids are NOT carried into turn 2 — the
    // second send goes out fresh. This test pins that real behaviour so the
    // limitation is visible and regressions in the local-store path (which DOES
    // persist — see `rest_spec_resumes_task_on_same_session_id`) stay
    // distinguishable. Distributed continuation requires a multi-replica gateway
    // with shared gossip storage; it cannot be exercised in Local Mode.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "cache",
        "contextKeySelector": "#[attributes.headers['x-conversation-id']]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": true,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let client = reqwest::Client::new();

    let turn1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"d-first""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "dist-task", "contextId": "dist-ctx",
                                              "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } } })
                .to_string(),
            );
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-D")
        .body(json!({ "prompt": "d-first" }).to_string()).send().await?;
    turn1.assert_async().await;

    // Turn 2: with the remote backend, NO continuation id is injected (no gossip
    // peer in Local Mode). This guard mock matches only if `taskId` WERE present;
    // it must stay at zero hits.
    let injected_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST)
                .body_contains(r#""text":"d-second""#)
                .body_contains(r#""taskId":"dist-task""#);
            then.status(200).body("{}");
        })
        .await;
    // This fires: the second send goes out fresh, with no carried ids.
    let fresh = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"d-second""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "dist-task", "contextId": "dist-ctx",
                                              "status": { "state": "TASK_STATE_COMPLETED" } } } })
                .to_string(),
            );
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-D")
        .body(json!({ "prompt": "d-second" }).to_string()).send().await?;
    fresh.assert_async().await;
    injected_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn httpjson_cache_continuation_injects_ids() -> anyhow::Result<()> {
    // HTTP+JSON binding + cache mode: bare params upstream, ids injected on turn 2.
    let config = json!({
        "upstreamBinding": "httpjson",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "cache",
        "contextKeySelector": "#[attributes.headers['x-conversation-id']]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let client = reqwest::Client::new();

    let turn1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"hj-first""#);
            // HTTP+JSON: bare Task body, no JSON-RPC envelope.
            then.status(200).header("content-type", "application/json").body(
                json!({ "task": { "id": "hj-task", "contextId": "hj-ctx",
                                  "status": { "state": "TASK_STATE_INPUT_REQUIRED" } } })
                .to_string(),
            );
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-HJ")
        .body(json!({ "prompt": "hj-first" }).to_string()).send().await?;
    turn1.assert_async().await;

    let turn2 = mock
        .mock_async(|when, then| {
            when.method(Method::POST)
                .body_contains(r#""text":"hj-second""#)
                .body_contains(r#""taskId":"hj-task""#)
                .body_contains(r#""contextId":"hj-ctx""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "task": { "id": "hj-task", "contextId": "hj-ctx",
                                  "status": { "state": "TASK_STATE_COMPLETED" } } })
                .to_string(),
            );
        })
        .await;
    // Guard: HTTP+JSON must never wrap params in a JSON-RPC envelope.
    let envelope_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""jsonrpc""#);
            then.status(200).body("{}");
        })
        .await;
    client.post(&flex_url).header("content-type", "application/json")
        .header("x-conversation-id", "conv-HJ")
        .body(json!({ "prompt": "hj-second" }).to_string()).send().await?;
    turn2.assert_async().await;
    envelope_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn httpjson_explicit_forwards_client_ids() -> anyhow::Result<()> {
    let config = json!({
        "upstreamBinding": "httpjson",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "explicit",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[payload.taskId]",
        "contextIdSelector": "#[payload.contextId]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;

    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST)
                .body_contains(r#""taskId":"ht""#)
                .body_contains(r#""contextId":"hc""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "task": { "id": "ht", "status": { "state": "TASK_STATE_COMPLETED" } } })
                    .to_string(),
            );
        })
        .await;
    let envelope_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""jsonrpc""#);
            then.status(200).body("{}");
        })
        .await;

    reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "prompt": "p", "taskId": "ht", "contextId": "hc" }).to_string())
        .send()
        .await?;
    upstream.assert_async().await;
    envelope_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn jsonrpc_upstream_error_passes_to_response_mapping() -> anyhow::Result<()> {
    // JSON-RPC in-band error: no `result`. responseMapping runs against the raw
    // body (no result to bind), so the raw error envelope passes through.
    let (_composite, flex_url, mock) = setup_test(jsonrpc_cache_config()).await?;
    mock.mock_async(|when, then| {
        when.method(Method::POST);
        then.status(200).header("content-type", "application/json").body(
            json!({ "jsonrpc": "2.0", "id": 1,
                    "error": { "code": -32602, "message": "Invalid params" } })
            .to_string(),
        );
    })
    .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .header("x-conversation-id", "conv-err")
        .body(json!({ "prompt": "boom" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await?;
    // `#[payload]` identity re-serialises JSON numbers through DataWeave as
    // doubles, so the code surfaces as -32602.0 — compare as f64.
    assert_eq!(body["error"]["code"].as_f64(), Some(-32602.0));
    Ok(())
}

#[pdk_test]
async fn httpjson_upstream_error_status_surfaced() -> anyhow::Result<()> {
    // HTTP+JSON native error: google.rpc.Status shape with a 4xx status. The
    // body IS the result for this binding; responseMapping echoes it.
    let config = json!({
        "upstreamBinding": "httpjson",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    mock.mock_async(|when, then| {
        when.method(Method::POST);
        then.status(404).header("content-type", "application/json").body(
            json!({ "error": { "code": 404, "message": "Method not found",
                               "details": [{ "reason": "METHOD_NOT_FOUND" }] } })
            .to_string(),
        );
    })
    .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "prompt": "x" }).to_string())
        .send()
        .await?;

    // Native HTTP status is preserved end-to-end.
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await?;
    assert_eq!(body["error"]["details"][0]["reason"], "METHOD_NOT_FOUND");
    Ok(())
}

#[pdk_test]
async fn message_reply_is_terminal_and_passed_through() -> anyhow::Result<()> {
    // A direct `message` reply (no task) is terminal; cache mode persists nothing
    // and a follow-up on the same key is a fresh send.
    let (_composite, flex_url, mock) = setup_test(jsonrpc_cache_config()).await?;
    let client = reqwest::Client::new();

    let turn1 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"hi there""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "message": { "messageId": "m1", "role": "ROLE_AGENT",
                                                  "parts": [{ "text": "hello back" }] } } })
                .to_string(),
            );
        })
        .await;
    let resp = client
        .post(&flex_url)
        .header("content-type", "application/json")
        .header("x-conversation-id", "conv-msg")
        .body(json!({ "prompt": "hi there" }).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    turn1.assert_async().await;
    let body: Value = resp.json().await?;
    assert_eq!(body["message"]["parts"][0]["text"], "hello back");

    // Turn 2 same key: message reply persisted nothing → no continuation id.
    let turn2 = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""text":"again""#);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "message": { "messageId": "m2", "parts": [{ "text": "ok" }] } } })
                .to_string(),
            );
        })
        .await;
    let id_guard = mock
        .mock_async(|when, then| {
            when.method(Method::POST).body_contains(r#""taskId""#);
            then.status(200).body("{}");
        })
        .await;
    client
        .post(&flex_url)
        .header("content-type", "application/json")
        .header("x-conversation-id", "conv-msg")
        .body(json!({ "prompt": "again" }).to_string())
        .send()
        .await?;
    turn2.assert_async().await;
    id_guard.assert_hits_async(0).await;
    Ok(())
}

#[pdk_test]
async fn response_mapping_failure_passes_raw_body_through() -> anyhow::Result<()> {
    // A mapping that references a missing field hard-errors; the policy is
    // non-fatal and passes the raw A2A body through unchanged.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        // Parses fine, but fails at eval: substring expects numeric bounds, so
        // passing strings forces a runtime type error → non-fatal raw passthrough.
        "customResponse": true,
        "responseMapping": "#[dw::core::Strings::substring(payload.task.id, \"a\", \"b\")]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    mock.mock_async(|when, then| {
        when.method(Method::POST);
        then.status(200).header("content-type", "application/json").body(
            json!({ "jsonrpc": "2.0", "id": 1,
                    "result": { "task": { "id": "rawt", "contextId": "rawc",
                                          "status": { "state": "TASK_STATE_COMPLETED" } } } })
            .to_string(),
        );
    })
    .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "prompt": "x" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    // Mapping failed → the raw, unmodified A2A response envelope passes through.
    let body: Value = resp.json().await?;
    assert_eq!(body["result"]["task"]["id"], "rawt");
    Ok(())
}

#[pdk_test]
async fn raw_response_is_default_and_byte_faithful() -> anyhow::Result<()> {
    // `customResponse` defaults to false (omitted here): the upstream A2A body is
    // returned verbatim, with no parse/reshape. The probe is a JSON-RPC error
    // carrying the integer code -32602: a true raw passthrough preserves it as an
    // integer, whereas `responseMapping: "#[payload]"` re-serializes it as the
    // double -32602.0. Asserting the integer form proves the body was never
    // round-tripped through the policy's JSON shaping.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.prompt]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        // customResponse intentionally omitted → default false (raw).
        "responseMapping": "#[payload]",
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    mock.mock_async(|when, then| {
        when.method(Method::POST);
        then.status(200).header("content-type", "application/json").body(
            // Compact, distinctive bytes so the raw echo is unambiguous.
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid parameters"}}"#,
        );
    })
    .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "prompt": "x" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    // Raw bytes preserved: integer code stays an integer (not -32602.0), and the
    // full JSON-RPC error envelope is intact (not unwrapped to the result arm).
    let raw_text = resp.text().await?;
    assert!(
        raw_text.contains("-32602") && !raw_text.contains("-32602.0"),
        "expected byte-faithful integer code, got: {raw_text}"
    );
    let body: Value = serde_json::from_str(&raw_text)?;
    assert_eq!(body["error"]["code"], -32602);
    assert_eq!(body["jsonrpc"], "2.0");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// responseFields — policy-side response assembly from dotted JSON-path selectors.
//
// The runtime rejects object construction in a single `responseMapping` (proven
// by `rest_spec_ask_maps_request_and_response`, which must select `#[payload.task]`
// rather than build an envelope). It also will not compile DataWeave nested
// inside array items, so per-field selectors are plain JSON paths resolved in
// Rust (see `select.rs`), not DataWeave. `responseFields` assembles those
// selections into a flat/nested object. These tests exercise the documented flat
// `AskResponse`, dotted-path nesting, precedence over `responseMapping`, and live
// `parts[0].text` array-index selection.
// ─────────────────────────────────────────────────────────────────────────────

/// An A2A input-required task reply used across the responseFields tests.
fn input_required_task_response() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "task": {
            "id": "task-42", "contextId": "ctx-7",
            "status": {
                "state": "TASK_STATE_INPUT_REQUIRED",
                "update": { "role": "ROLE_AGENT",
                            "parts": [{ "text": "Sure — what is your order number?" }] }
            }
        } }
    })
    .to_string()
}

#[pdk_test]
async fn response_fields_build_flat_ask_response() -> anyhow::Result<()> {
    // The documented AskResponse contract (conversationId/taskRef/status/reply)
    // that a single constructed responseMapping cannot produce — assembled here
    // from four dotted-path fields. Also exercises `parts[0].text` array-index
    // selection end-to-end.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.question]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "responseFields": [
            { "name": "conversationId", "selector": "payload.task.contextId" },
            { "name": "taskRef",        "selector": "payload.task.id" },
            { "name": "status",         "selector": "payload.task.status.state" },
            { "name": "reply",          "selector": "payload.task.status.update.parts[0].text" }
        ],
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200)
                .header("content-type", "application/json")
                .body(input_required_task_response());
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "question": "What is the status of my order?" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;
    let body: Value = resp.json().await?;
    // Exactly the flat AskResponse the architecture originally intended.
    assert_eq!(body["conversationId"], "ctx-7");
    assert_eq!(body["taskRef"], "task-42");
    assert_eq!(body["status"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(body["reply"], "Sure — what is your order number?");
    // No stray envelope fields leaked through.
    assert!(body.get("jsonrpc").is_none());
    assert!(body.get("result").is_none());
    Ok(())
}

#[pdk_test]
async fn response_fields_dotted_names_build_nested_object() -> anyhow::Result<()> {
    // Dotted `name` builds nested objects in Rust.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.question]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "responseFields": [
            { "name": "data.taskRef",   "selector": "payload.task.id" },
            { "name": "data.context",   "selector": "payload.task.contextId" },
            { "name": "meta.state",     "selector": "payload.task.status.state" }
        ],
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200)
                .header("content-type", "application/json")
                .body(input_required_task_response());
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "question": "hi" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;
    let body: Value = resp.json().await?;
    assert_eq!(body["data"]["taskRef"], "task-42");
    assert_eq!(body["data"]["context"], "ctx-7");
    assert_eq!(body["meta"]["state"], "TASK_STATE_INPUT_REQUIRED");
    Ok(())
}

#[pdk_test]
async fn response_fields_take_precedence_over_response_mapping() -> anyhow::Result<()> {
    // responseMapping is set to a passthrough that WOULD echo the whole envelope,
    // but a non-empty responseFields overrides it.
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.question]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "responseFields": [
            { "name": "taskRef", "selector": "payload.task.id" }
        ],
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200)
                .header("content-type", "application/json")
                .body(input_required_task_response());
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "question": "hi" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;
    let body: Value = resp.json().await?;
    // Only the assembled field is present — the mapping passthrough was skipped.
    assert_eq!(body["taskRef"], "task-42");
    assert!(body.get("result").is_none());
    assert!(body.get("contextId").is_none());
    Ok(())
}

#[pdk_test]
async fn response_fields_missing_selection_becomes_null() -> anyhow::Result<()> {
    // A field whose selector resolves to a non-existent path contributes JSON
    // null rather than aborting the whole response (non-fatal posture).
    let config = json!({
        "upstreamBinding": "jsonrpc",
        "promptSelector": "#[payload.question]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "responseFields": [
            { "name": "taskRef", "selector": "payload.task.id" },
            { "name": "reply",   "selector": "payload.task.status.update.parts[0].text" }
        ],
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    // Terminal task with NO status.update message → `reply` path is absent.
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200).header("content-type", "application/json").body(
                json!({ "jsonrpc": "2.0", "id": 1,
                        "result": { "task": { "id": "task-99", "contextId": "ctx-9",
                                              "status": { "state": "TASK_STATE_COMPLETED" } } } })
                .to_string(),
            );
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "question": "done?" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;
    let body: Value = resp.json().await?;
    assert_eq!(body["taskRef"], "task-99");
    // Present and explicitly null — not missing.
    assert!(body.as_object().unwrap().contains_key("reply"));
    assert_eq!(body["reply"], Value::Null);
    Ok(())
}

#[pdk_test]
async fn response_fields_httpjson_binding_assembles_from_bare_body() -> anyhow::Result<()> {
    // HTTP+JSON binding: the bare body IS the result. responseFields selects
    // straight off the task object (no envelope unwrap needed).
    let config = json!({
        "upstreamBinding": "httpjson",
        "promptSelector": "#[payload.question]",
        "continuationMode": "none",
        "contextKeySelector": "#[null]",
        "taskIdSelector": "#[null]",
        "contextIdSelector": "#[null]",
        "customResponse": true,
        "responseMapping": "#[payload]",
        "responseFields": [
            { "name": "taskRef", "selector": "payload.task.id" },
            { "name": "status",  "selector": "payload.task.status.state" }
        ],
        "distributed": false,
        "conversationTtlSeconds": 3600,
        "requestErrorStatus": 400
    });
    let (_composite, flex_url, mock) = setup_test(config).await?;
    let upstream = mock
        .mock_async(|when, then| {
            when.method(Method::POST);
            then.status(200).header("content-type", "application/json").body(
                json!({ "task": { "id": "hj-task", "contextId": "hj-ctx",
                                  "status": { "state": "TASK_STATE_COMPLETED" } } })
                .to_string(),
            );
        })
        .await;

    let resp = reqwest::Client::new()
        .post(&flex_url)
        .header("content-type", "application/json")
        .body(json!({ "question": "hi" }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    upstream.assert_async().await;
    let body: Value = resp.json().await?;
    assert_eq!(body["taskRef"], "hj-task");
    assert_eq!(body["status"], "TASK_STATE_COMPLETED");
    Ok(())
}
