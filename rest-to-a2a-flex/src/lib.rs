// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! REST to A2A v1.0 `SendMessage` bridge policy for MuleSoft Omni Gateway.
//!
//! Converts an inbound REST API call into an outbound A2A protocol v1.0
//! `SendMessage` to an external A2A agent. The prompt and conversation identity
//! are extracted from the REST request via DataWeave; the raw A2A Task/Message
//! response is shaped back to REST via DataWeave. Supports the JSON-RPC 2.0 and
//! HTTP+JSON upstream bindings, with multi-turn continuation via a gossip-safe
//! conversation cache or client-supplied taskId/contextId. Streaming (SSE) is
//! NOT supported — see `docs/spec.md`.
//!
//! ## Upstream routing
//!
//! Both bindings forward **in-band** (`Flow::Continue` + body rewrite). The
//! upstream path is operator-owned via the route `destinationPath` — the policy
//! never rewrites `:path` (no reliable route-cache API in PDK 1.9.0). See
//! `binding.rs` and `docs/spec.md`.

mod a2a;
mod binding;
mod cache;
mod config_map;
mod continuation;
mod dataweave;
mod generated;
mod jsonrpc;
mod response_build;
mod select;

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use pdk::data_storage::{DataStorage, DataStorageBuilder, LocalDataStorage};
use pdk::hl::*;
use pdk::logger;
use pdk::script::HandlerAttributesBinding;

use crate::binding::caller_error_body;
use crate::cache::ConversationStore;
use crate::config_map::{ContinuationMode, PolicyConfig};
use crate::continuation::{
    is_unresumable_continuable, persist_outcome, resolve_cache, resolve_explicit,
    RequestContinuation,
};
use crate::dataweave::{value_to_json, value_to_string};
use crate::generated::config::Config;

/// HTTP header name for the body content type.
const CONTENT_TYPE: &str = "content-type";
/// HTTP header name for the (stale, post-rewrite) body length.
const CONTENT_LENGTH: &str = "content-length";
/// JSON media type set on every rewritten body.
const APPLICATION_JSON: &str = "application/json";

/// Per-request context threaded from the request filter to the response filter.
/// `None` (via `RequestData`) means the request was short-circuited before a
/// send was framed (fail-closed) — the response filter then does nothing.
#[derive(Debug, Clone, Default)]
struct RequestContext {
    continuation: RequestContinuation,
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    store_builder: DataStorageBuilder,
) -> Result<()> {
    let raw: Config = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!(
            "Failed to parse configuration '{}'. Cause: {}",
            String::from_utf8_lossy(&bytes),
            err
        )
    })?;
    let config = PolicyConfig::from_generated(raw).map_err(|err| anyhow!("{err}"))?;

    // Storage is built (and a backend chosen) ONLY in cache mode — gossip rule:
    // the local/remote branch lives in `configure`; everything downstream is
    // generic over `S: DataStorage`.
    if config.uses_cache() {
        if config.distributed {
            let ttl_ms = config.conversation_ttl_seconds.saturating_mul(1000);
            let storage = store_builder.remote("rest-to-a2a", ttl_ms);
            launch_policy(launcher, Rc::new(config), Some(Rc::new(storage))).await
        } else {
            let storage = store_builder.local("rest-to-a2a");
            launch_policy(launcher, Rc::new(config), Some(Rc::new(storage))).await
        }
    } else {
        // explicit / none: no storage built. Pin the generic to the local type;
        // the `None` means the cache code path is never entered.
        launch_policy::<LocalDataStorage>(launcher, Rc::new(config), None).await
    }
}

/// Launch the request/response filter pair, backend-agnostic over `S`.
/// `storage` is `Some` only in cache mode.
async fn launch_policy<S: DataStorage + 'static>(
    launcher: Launcher,
    config: Rc<PolicyConfig>,
    storage: Option<Rc<S>>,
) -> Result<()> {
    let filter = on_request({
        let config = config.clone();
        let storage = storage.clone();
        move |request, stream| {
            let config = config.clone();
            let storage = storage.clone();
            async move { request_filter(request, stream, config, storage).await }
        }
    })
    .on_response({
        let config = config.clone();
        let storage = storage.clone();
        move |response, request_data, stream| {
            let config = config.clone();
            let storage = storage.clone();
            async move { response_filter(response, request_data, stream, config, storage).await }
        }
    });

    launcher.launch(filter).await?;
    Ok(())
}

async fn request_filter<S: DataStorage>(
    state: RequestState,
    stream: StreamProperties,
    config: Rc<PolicyConfig>,
    storage: Option<Rc<S>>,
) -> Flow<RequestContext> {
    let headers_state = state.into_headers_state().await;

    // Seed for the deterministic messageId: prefer a request-id header so the
    // id correlates with gateway logs, else fall back to the path.
    let id_seed = headers_state
        .handler()
        .header("x-request-id")
        .unwrap_or_else(|| headers_state.path());

    // Build every selector evaluator and bind request attributes now, while the
    // headers handler is live. Attribute-only sub-expressions resolve here;
    // payload-dependent ones complete after the body is read.
    let attributes = HandlerAttributesBinding::new(headers_state.handler(), &stream);
    let mut prompt_eval = config.prompt_selector.evaluator();
    prompt_eval.bind_attributes(&attributes);

    // Continuation selectors depend on the mode.
    let mut key_eval = config.context_key_selector.evaluator();
    let mut task_eval = config.task_id_selector.evaluator();
    let mut ctx_eval = config.context_id_selector.evaluator();
    match config.continuation_mode {
        ContinuationMode::Cache => key_eval.bind_attributes(&attributes),
        ContinuationMode::Explicit => {
            task_eval.bind_attributes(&attributes);
            ctx_eval.bind_attributes(&attributes);
        }
        ContinuationMode::None => {}
    }
    drop(attributes);

    // Header mutations MUST happen in the headers phase — once the body is read
    // (event flow) headers can no longer change. The outbound body is rewritten
    // below for both bindings, so set JSON content-type and drop the stale
    // content-length now. Harmless if the request is later rejected (the Break
    // response is independent).
    headers_state.handler().remove_header(CONTENT_LENGTH);
    headers_state
        .handler()
        .set_header(CONTENT_TYPE, APPLICATION_JSON);

    // Transition to the body and bind the payload to each evaluator.
    let body_state = headers_state.into_body_state().await;
    let body = body_state.handler().body();
    let payload: &[u8] = &body;
    prompt_eval.bind_payload(&payload);
    match config.continuation_mode {
        ContinuationMode::Cache => key_eval.bind_payload(&payload),
        ContinuationMode::Explicit => {
            task_eval.bind_payload(&payload);
            ctx_eval.bind_payload(&payload);
        }
        ContinuationMode::None => {}
    }

    // Fail-closed: a missing/empty/non-scalar prompt rejects the request before
    // the upstream is ever called.
    let prompt = match prompt_eval.eval().ok().and_then(value_to_string) {
        Some(p) => p,
        None => {
            logger::warn!("rest-to-a2a: prompt selector yielded no value — rejecting request");
            return Flow::Break(
                Response::new(config.request_error_status)
                    .with_headers(vec![(
                        CONTENT_TYPE.to_string(),
                        APPLICATION_JSON.to_string(),
                    )])
                    .with_body(caller_error_body("missing or invalid prompt")),
            );
        }
    };

    // Resolve continuation per mode.
    let request_continuation = match config.continuation_mode {
        ContinuationMode::Cache => {
            let conversation_value = key_eval.eval().ok().and_then(value_to_string);
            match &storage {
                Some(storage) => {
                    let store = ConversationStore::new(
                        storage.as_ref(),
                        config.distributed,
                        config.conversation_ttl_seconds,
                    );
                    resolve_cache(&store, conversation_value, now_millis()).await
                }
                None => RequestContinuation::default(),
            }
        }
        ContinuationMode::Explicit => {
            let task_id = task_eval.eval().ok().and_then(value_to_string);
            let context_id = ctx_eval.eval().ok().and_then(value_to_string);
            resolve_explicit(task_id, context_id)
        }
        ContinuationMode::None => RequestContinuation::default(),
    };

    // Build the A2A SendMessage and frame it for the configured binding.
    let mut seed = id_seed.into_bytes();
    seed.extend_from_slice(prompt.as_bytes());
    let message_id = a2a::generate_message_id(&seed);
    let params = a2a::build_send_message(
        &prompt,
        &message_id,
        &request_continuation.continuation,
        config.send_configuration.as_ref(),
    );
    let framed = config.binding.frame_request(&message_id, params);

    // Rewrite the outbound body in-band (content-type/content-length were
    // already handled in the headers phase above).
    if let Err(err) = body_state.handler().set_body(&framed) {
        logger::error!("rest-to-a2a: failed to set request body: {err:?}");
        return Flow::Break(
            Response::new(config.request_error_status)
                .with_headers(vec![(
                    CONTENT_TYPE.to_string(),
                    APPLICATION_JSON.to_string(),
                )])
                .with_body(caller_error_body("failed to build A2A request")),
        );
    }

    Flow::Continue(RequestContext {
        continuation: request_continuation,
    })
}

async fn response_filter<S: DataStorage>(
    state: ResponseState,
    request_data: RequestData<RequestContext>,
    stream: StreamProperties,
    config: Rc<PolicyConfig>,
    storage: Option<Rc<S>>,
) {
    // Only act on requests that were forwarded (Continue). A fail-closed request
    // never reached the upstream.
    let ctx = match request_data {
        RequestData::Continue(ctx) => ctx,
        _ => return,
    };

    let headers_state = state.into_headers_state().await;

    // Streaming is out of scope: pass an SSE response through untouched.
    if let Some(content_type) = headers_state.handler().header(CONTENT_TYPE) {
        if content_type.contains("text/event-stream") {
            logger::warn!(
                "rest-to-a2a: upstream returned text/event-stream — streaming is not supported, \
                 passing through untouched"
            );
            return;
        }
    }

    // Three response-shaping paths, gated by `customResponse`:
    //  - raw (default, `customResponse=false`): the upstream A2A body is returned
    //    verbatim — no parse, no reshape, byte-faithful. Nothing is bound here.
    //  - `responseFields` (precedence when shaping): each field is a dotted
    //    JSON-path resolved in Rust against the parsed A2A result (see
    //    `select.rs`). No DataWeave — the gateway can't compile DataWeave nested
    //    inside array items.
    //  - `responseMapping` (shaping fallback): a single top-level DataWeave
    //    evaluator, whose attributes are bound here while the header handler is
    //    live.
    let use_raw = config.uses_raw_response();
    let use_fields = config.uses_response_fields();
    let mut mapping_eval = config.response_mapping.evaluator();
    if !use_raw && !use_fields {
        let attributes = HandlerAttributesBinding::new(headers_state.handler(), &stream);
        mapping_eval.bind_attributes(&attributes);
    }

    // Header mutations must happen in the headers phase. When shaping, the body
    // is rewritten below (shaped on success, or passed through raw on mapping
    // failure — the raw A2A body is itself JSON), so the JSON content-type holds
    // either way and the stale content-length must go. In raw mode the body is
    // left untouched, so the upstream headers (content-type, content-length)
    // pass through verbatim alongside it.
    if !use_raw {
        headers_state.handler().remove_header(CONTENT_LENGTH);
        headers_state
            .handler()
            .set_header(CONTENT_TYPE, APPLICATION_JSON);
    }

    let body_state = headers_state.into_body_state().await;
    let raw = body_state.handler().body();

    // Parse the binding-specific envelope into the raw A2A SendMessageResult.
    let parts = config.binding.parse_response(&raw);
    let result_value = parts.result.clone();

    // Surface a binding-native upstream error for observability. This is not
    // fatal: the body still flows back to the caller (shaped or raw) so the
    // operator's response logic can act on it, but a silent JSON-RPC in-band
    // error or HTTP+JSON `google.rpc.Status` would otherwise be invisible.
    if let Some(error) = &parts.error {
        logger::warn!("rest-to-a2a: upstream A2A returned an error envelope: {error}");
    }

    // Continuation persistence (cache mode only — `cache_key` present).
    if let (Some(result), Some(storage)) = (&result_value, &storage) {
        let parsed = a2a::parse_result(result);
        let class = parsed.class();
        let store = ConversationStore::new(
            storage.as_ref(),
            config.distributed,
            config.conversation_ttl_seconds,
        );
        if let Err(err) = persist_outcome(
            &store,
            &ctx.continuation,
            class,
            &parsed.continuation(),
            now_millis(),
        )
        .await
        {
            logger::warn!("rest-to-a2a: cache persist failed (non-fatal): {err}");
        }
        if is_unresumable_continuable(&ctx.continuation, class) {
            logger::warn!(
                "rest-to-a2a: upstream task is continuable but no continuation key/id was \
                 available — this conversation cannot be resumed"
            );
        }
    } else if let Some(result) = &result_value {
        // Explicit / none mode: no storage, but still warn on an un-resumable
        // continuable reply.
        let class = a2a::parse_result(result).class();
        if is_unresumable_continuable(&ctx.continuation, class) {
            logger::warn!(
                "rest-to-a2a: upstream task is continuable but no continuation id was supplied — \
                 this conversation cannot be resumed"
            );
        }
    }

    // Raw mode: return the upstream A2A body verbatim. The body was never
    // re-read or rewritten and the headers were left intact above, so there is
    // nothing to do — the original bytes (and their content-type/length) flow
    // through unchanged. This is byte-faithful, unlike `responseMapping:
    // "#[payload]"`, which re-serializes JSON numbers as doubles.
    if use_raw {
        return;
    }

    // Shape the REST-facing body. Both paths run against the raw A2A result
    // object bound as `payload`. Any failure is non-fatal: the raw upstream body
    // passes through unchanged.
    let payload_bytes = result_value
        .as_ref()
        .and_then(|v| serde_json::to_vec(v).ok())
        .unwrap_or_else(|| raw.clone());
    let payload: &[u8] = &payload_bytes;

    if use_fields {
        // Assemble the response object in Rust from the dotted-path fields. The
        // result object is the selection root; a path that resolves to nothing
        // (or a result that isn't valid JSON) contributes JSON `null`, matching
        // the policy's non-fatal posture rather than aborting the whole response.
        let root: serde_json::Value =
            serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null);
        let mut built = Vec::with_capacity(config.response_fields.len());
        for field in &config.response_fields {
            let value = match select::select(&root, &field.selector) {
                Some(v) => v.clone(),
                None => {
                    logger::warn!(
                        "rest-to-a2a: responseFields selector '{}' for '{}' resolved to nothing (field set to null)",
                        field.selector,
                        field.name
                    );
                    serde_json::Value::Null
                }
            };
            built.push(response_build::BuiltField {
                name: field.name.clone(),
                value,
            });
        }
        let assembled = response_build::assemble(built);
        for name in &assembled.collisions {
            logger::warn!(
                "rest-to-a2a: responseFields entry '{name}' collides with an earlier field — skipped"
            );
        }
        let shaped = serde_json::to_vec(&assembled.object).unwrap_or_default();
        if let Err(err) = body_state.handler().set_body(&shaped) {
            logger::warn!(
                "rest-to-a2a: failed to set assembled response body (passing raw through): {err:?}"
            );
        }
    } else {
        mapping_eval.bind_payload(&payload);
        match mapping_eval.eval() {
            Ok(value) => {
                let shaped = serde_json::to_vec(&value_to_json(value)).unwrap_or_default();
                if let Err(err) = body_state.handler().set_body(&shaped) {
                    logger::warn!(
                        "rest-to-a2a: failed to set mapped response body (passing raw through): {err:?}"
                    );
                }
            }
            Err(err) => {
                logger::warn!(
                    "rest-to-a2a: response mapping failed (passing raw A2A body through): {err:?}"
                );
            }
        }
    }
}

/// Current Unix-epoch milliseconds. PDK exposes no time module; `SystemTime`
/// works in the WASM runtime. A clock error degrades to `0` (entries then look
/// already-expired, which fails safe toward a fresh send).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
