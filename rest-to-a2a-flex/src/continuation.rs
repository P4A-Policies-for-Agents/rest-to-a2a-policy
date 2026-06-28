// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Continuation routing: turn the policy's continuation mode plus the
//! already-evaluated DataWeave selector values into the `taskId`/`contextId`
//! to send upstream, and persist the outcome after the reply.
//!
//! DataWeave evaluation happens filter-side (it needs the live body/header
//! handlers), so this module is deliberately **pure over string inputs** — it
//! receives the selector results (`Option<String>`) and decides what to do.
//! That keeps the mode state-machine fully unit-testable without a gateway.
//!
//! The three modes are mutually exclusive (`pdk-distributed-cache-gossip` rule:
//! storage is touched in `cache` mode only):
//!
//! - **Cache** — hash the conversation value into a [`CacheKey`], look up a live
//!   continuation on the request, persist/evict on the response. The key is
//!   threaded forward via [`RequestContinuation::cache_key`].
//! - **Explicit** — the client supplies `taskId`/`contextId` through their own
//!   selectors; storage is never consulted or written.
//! - **None** — single-shot; no ids, no storage.

use pdk::data_storage::DataStorage;

use crate::a2a::{Continuation, StateClass};
use crate::cache::{CacheError, CacheKey, ConversationStore};

/// The continuation resolved for an outbound request, plus the cache key to
/// carry into the response filter (cache mode only).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestContinuation {
    /// `taskId`/`contextId` to inject into the `SendMessage` (may be empty for a
    /// fresh conversation).
    pub continuation: Continuation,
    /// The cache key to use when persisting the response outcome. `Some` only in
    /// cache mode with a non-empty conversation value.
    pub cache_key: Option<CacheKey>,
    /// Whether a continuation id was actually carried into this send. Drives the
    /// "un-resumable input-required" warning when the upstream asks to continue
    /// a conversation the caller can't resume.
    pub had_continuation_id: bool,
}

impl RequestContinuation {
    /// Whether the upstream's continuable reply can actually be resumed on a
    /// later turn — true if we are caching under a key, or the caller is driving
    /// continuation explicitly with at least one id.
    pub fn is_resumable(&self) -> bool {
        self.cache_key.is_some() || self.had_continuation_id
    }
}

/// Resolve the request-side continuation for `cache` mode.
///
/// `conversation_value` is the evaluated `contextKeySelector` result. When
/// present it is hashed into a [`CacheKey`] and a live continuation is looked up
/// (gossip-safe; expired/terminal entries return nothing). The key is returned
/// even on a miss so the response filter can persist a freshly-created task
/// under it.
pub async fn resolve_cache<S: DataStorage>(
    store: &ConversationStore<'_, S>,
    conversation_value: Option<String>,
    now_millis: u64,
) -> RequestContinuation {
    let Some(value) = conversation_value else {
        // No conversation key this turn → single-shot, nothing to persist.
        return RequestContinuation::default();
    };

    let key = CacheKey::from_hashed(&value);
    let continuation = store
        .get_continuation(&key, now_millis)
        .await
        .unwrap_or_default();
    let had_continuation_id = !continuation.is_empty();

    RequestContinuation {
        continuation,
        cache_key: Some(key),
        had_continuation_id,
    }
}

/// Resolve the request-side continuation for `explicit` mode: use whatever the
/// client supplied via the `taskIdSelector` / `contextIdSelector`. Storage is
/// never touched (no cache key).
pub fn resolve_explicit(task_id: Option<String>, context_id: Option<String>) -> RequestContinuation {
    let continuation = Continuation {
        task_id,
        context_id,
    };
    let had_continuation_id = !continuation.is_empty();
    RequestContinuation {
        continuation,
        cache_key: None,
        had_continuation_id,
    }
}

/// Persist the response outcome under the request's cache key.
///
/// No-op unless a [`CacheKey`] was carried forward (i.e. cache mode with a
/// conversation value). Continuable → upsert; terminal → evict. Errors are
/// returned for the caller to log; they are non-fatal to the response.
pub async fn persist_outcome<S: DataStorage>(
    store: &ConversationStore<'_, S>,
    request: &RequestContinuation,
    class: StateClass,
    response_continuation: &Continuation,
    now_millis: u64,
) -> Result<(), CacheError> {
    let Some(key) = &request.cache_key else {
        return Ok(());
    };
    store
        .record_outcome(key, class, response_continuation, now_millis)
        .await
}

/// Whether the upstream's continuable reply is un-resumable and should warn:
/// the task wants to continue, but no cache key was set and the caller passed
/// no ids, so the next turn cannot pick it up.
pub fn is_unresumable_continuable(request: &RequestContinuation, class: StateClass) -> bool {
    class == StateClass::Continuable && !request.is_resumable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_map::ContinuationMode;

    #[test]
    fn explicit_carries_client_ids() {
        let r = resolve_explicit(Some("t".into()), Some("c".into()));
        assert_eq!(r.continuation.task_id.as_deref(), Some("t"));
        assert_eq!(r.continuation.context_id.as_deref(), Some("c"));
        assert!(r.cache_key.is_none());
        assert!(r.had_continuation_id);
        assert!(r.is_resumable());
    }

    #[test]
    fn explicit_with_no_ids_is_fresh() {
        let r = resolve_explicit(None, None);
        assert!(r.continuation.is_empty());
        assert!(!r.had_continuation_id);
        assert!(!r.is_resumable());
    }

    #[test]
    fn none_mode_default_is_unresumable_when_continuable() {
        // `none`/explicit-fresh both produce the default RequestContinuation.
        let r = RequestContinuation::default();
        assert!(is_unresumable_continuable(&r, StateClass::Continuable));
        assert!(!is_unresumable_continuable(&r, StateClass::Terminal));
    }

    #[test]
    fn cache_key_present_is_resumable() {
        let r = RequestContinuation {
            cache_key: Some(CacheKey::from_hashed("conv")),
            ..Default::default()
        };
        assert!(r.is_resumable());
        assert!(!is_unresumable_continuable(&r, StateClass::Continuable));
    }

    #[test]
    fn continuation_mode_is_routed_by_caller() {
        // Sanity: the enum the filter switches on is the one from config_map.
        let mode = ContinuationMode::Explicit;
        assert_eq!(mode, ContinuationMode::Explicit);
    }
}
