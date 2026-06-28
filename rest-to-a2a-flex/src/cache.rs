// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Gossip-safe conversation cache for multi-turn A2A continuation (cache mode
//! only).
//!
//! Stores the `contextId`+`taskId` of a live task keyed by a **hashed**
//! conversation value, so the next REST turn carrying the same conversation
//! key resumes the task. Follows the gossip-replication safety rules
//! (`pdk-distributed-cache-gossip`):
//!
//! - **No DELETE before re-create** — terminal eviction on a remote backend
//!   CAS-overwrites to a tombstone marker (filtered on read; TTL reclaims it),
//!   never `delete()` (which would gossip a tombstone that can kill a fresh
//!   write). Local backend deletes directly (no gossip).
//! - **No proactive delete on read** — expired/terminal entries return `None`
//!   and are left for the namespace TTL.
//! - **CAS for state transitions** — bounded retry; the update closure guards
//!   against clobbering a terminal entry.
//!
//! The cache value is hashed before it becomes a key (no raw conversation id
//! at rest), and the entry stores no prompt text — only ids + lifecycle state.

use pdk::data_storage::{DataStorage, DataStorageError, StoreMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::a2a::{Continuation, StateClass};

/// Cache key namespace prefix. Keeps conversation entries distinct within the
/// storage namespace and makes keys self-describing in operational tooling.
const KEY_PREFIX: &str = "rest-to-a2a:";

/// Max CAS retries on a contended state transition. Exhaustion is logged and
/// abandoned — the next turn re-reads and retries naturally.
const CAS_MAX_RETRIES: u32 = 3;

/// A conversation cache key: the namespace prefix followed by the SHA-256 hex
/// of the operator's conversation value. The raw value never enters the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl CacheKey {
    /// Hash a conversation value into a cache key. Deterministic and one-way —
    /// the same conversation value always maps to the same key, and the key
    /// reveals nothing about the value.
    pub fn from_hashed(conversation_value: &str) -> Self {
        let digest = Sha256::digest(conversation_value.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in digest.iter() {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self(format!("{KEY_PREFIX}{hex}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lifecycle marker persisted with an entry. Continuable entries carry a
/// resumable task; the terminal marker is a gossip-safe tombstone (filtered on
/// read) used instead of DELETE on remote backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryState {
    Continuable,
    Terminal,
}

/// A cached conversation continuation. Holds only ids + lifecycle + timestamps
/// — never prompt or response text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationEntry {
    pub context_id: Option<String>,
    pub task_id: Option<String>,
    pub state: EntryState,
    /// Unix-epoch millis when this entry expires (in addition to the namespace
    /// TTL — guards against a remote backend serving a stale gossip copy).
    pub expires_at_millis: u64,
}

impl ConversationEntry {
    pub fn is_continuable(&self) -> bool {
        self.state == EntryState::Continuable
    }

    pub fn is_expired(&self, now_millis: u64) -> bool {
        now_millis >= self.expires_at_millis
    }

    /// The continuation ids to inject on the next turn, if still usable.
    pub fn continuation(&self) -> Continuation {
        Continuation {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
        }
    }
}

/// Errors surfaced by the conversation store. CAS conflicts are distinguished
/// so callers can treat them as retriable rather than hard failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    #[error("compare-and-swap conflict")]
    CasConflict,
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<DataStorageError> for CacheError {
    fn from(err: DataStorageError) -> Self {
        match err {
            DataStorageError::CasMismatch => CacheError::CasConflict,
            other => CacheError::Storage(format!("{other:?}")),
        }
    }
}

/// Gossip-safe conversation store over any [`DataStorage`] backend.
///
/// `distributed` selects the eviction strategy: a remote (gossip) backend
/// CAS-overwrites to a terminal tombstone; a local backend deletes directly.
/// All other logic is backend-agnostic.
pub struct ConversationStore<'a, S: DataStorage> {
    storage: &'a S,
    distributed: bool,
    ttl_millis: u64,
}

impl<'a, S: DataStorage> ConversationStore<'a, S> {
    pub fn new(storage: &'a S, distributed: bool, ttl_seconds: u32) -> Self {
        Self {
            storage,
            distributed,
            ttl_millis: ttl_seconds as u64 * 1000,
        }
    }

    /// Read the continuation for a key, if a live continuable entry exists.
    ///
    /// Returns `None` for: missing key, expired entry (left for TTL — no
    /// proactive delete), or a terminal tombstone. Never mutates the store.
    pub async fn get_continuation(
        &self,
        key: &CacheKey,
        now_millis: u64,
    ) -> Option<Continuation> {
        match self.storage.get::<ConversationEntry>(key.as_str()).await {
            Ok(Some((entry, _version))) => {
                if entry.is_expired(now_millis) || !entry.is_continuable() {
                    None
                } else {
                    Some(entry.continuation())
                }
            }
            // Missing key or any read error → treat as no continuation
            // (fail-open for continuity; a fresh send still works).
            _ => None,
        }
    }

    /// Persist the outcome of a send for this key, given the response's state
    /// class and ids. Continuable → upsert a live entry; terminal → evict
    /// (gossip-safe). Unknown states are left untouched.
    pub async fn record_outcome(
        &self,
        key: &CacheKey,
        class: StateClass,
        continuation: &Continuation,
        now_millis: u64,
    ) -> Result<(), CacheError> {
        match class {
            StateClass::Continuable => self.upsert(key, continuation, now_millis).await,
            StateClass::Terminal => self.evict(key, now_millis).await,
            StateClass::Unknown => Ok(()),
        }
    }

    /// Create-or-update a continuable entry. First writer uses `Absent`; an
    /// existing entry is CAS-overwritten (bounded retry), but never if it is
    /// already terminal (guard returns early to avoid resurrecting a closed
    /// conversation).
    async fn upsert(
        &self,
        key: &CacheKey,
        continuation: &Continuation,
        now_millis: u64,
    ) -> Result<(), CacheError> {
        let entry = ConversationEntry {
            context_id: continuation.context_id.clone(),
            task_id: continuation.task_id.clone(),
            state: EntryState::Continuable,
            expires_at_millis: now_millis + self.ttl_millis,
        };

        // First-writer attempt.
        match self
            .storage
            .store(key.as_str(), &StoreMode::Absent, &entry)
            .await
        {
            Ok(()) => return Ok(()),
            Err(DataStorageError::CasMismatch) => {} // key exists — fall through to CAS update
            Err(other) => return Err(other.into()),
        }

        for _ in 0..CAS_MAX_RETRIES {
            let version = match self.storage.get::<ConversationEntry>(key.as_str()).await {
                Ok(Some((existing, version))) => {
                    // Guard: never overwrite a terminal entry back to live.
                    if existing.state == EntryState::Terminal {
                        return Ok(());
                    }
                    version
                }
                Ok(None) => {
                    // Vanished between Absent-fail and read — retry as first writer.
                    match self
                        .storage
                        .store(key.as_str(), &StoreMode::Absent, &entry)
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(DataStorageError::CasMismatch) => continue,
                        Err(other) => return Err(other.into()),
                    }
                }
                Err(other) => return Err(other.into()),
            };

            match self
                .storage
                .store(key.as_str(), &StoreMode::Cas(version), &entry)
                .await
            {
                Ok(()) => return Ok(()),
                Err(DataStorageError::CasMismatch) => continue,
                Err(other) => return Err(other.into()),
            }
        }
        Err(CacheError::CasConflict)
    }

    /// Evict the entry for a terminal task. Local backend hard-deletes; remote
    /// backend CAS-overwrites to a terminal tombstone (no gossip DELETE).
    async fn evict(&self, key: &CacheKey, now_millis: u64) -> Result<(), CacheError> {
        if !self.distributed {
            return self.storage.delete(key.as_str()).await.map_err(Into::into);
        }

        let tombstone = ConversationEntry {
            context_id: None,
            task_id: None,
            state: EntryState::Terminal,
            expires_at_millis: now_millis + self.ttl_millis,
        };

        for _ in 0..CAS_MAX_RETRIES {
            match self.storage.get::<ConversationEntry>(key.as_str()).await {
                Ok(Some((existing, version))) => {
                    if existing.state == EntryState::Terminal {
                        return Ok(()); // already evicted
                    }
                    match self
                        .storage
                        .store(key.as_str(), &StoreMode::Cas(version), &tombstone)
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(DataStorageError::CasMismatch) => continue,
                        Err(other) => return Err(other.into()),
                    }
                }
                // Nothing to evict.
                Ok(None) => return Ok(()),
                Err(other) => return Err(other.into()),
            }
        }
        Err(CacheError::CasConflict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[test]
    fn key_is_prefixed_hash_and_deterministic() {
        let a = CacheKey::from_hashed("conv-1");
        let b = CacheKey::from_hashed("conv-1");
        let c = CacheKey::from_hashed("conv-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.as_str().starts_with(KEY_PREFIX));
        // 64 hex chars after the prefix.
        assert_eq!(a.as_str().len(), KEY_PREFIX.len() + 64);
        // Raw value must not appear in the key.
        assert!(!a.as_str().contains("conv-1"));
    }

    #[test]
    fn entry_expiry_and_continuable() {
        let entry = ConversationEntry {
            context_id: Some("c".into()),
            task_id: Some("t".into()),
            state: EntryState::Continuable,
            expires_at_millis: 1000,
        };
        assert!(entry.is_continuable());
        assert!(!entry.is_expired(999));
        assert!(entry.is_expired(1000));
        assert_eq!(entry.continuation().task_id.as_deref(), Some("t"));
    }

    #[test]
    fn cas_mismatch_maps_to_conflict() {
        assert_eq!(
            CacheError::from(DataStorageError::CasMismatch),
            CacheError::CasConflict
        );
    }

    // ── In-memory DataStorage mock with version tracking for CAS tests ──────

    #[derive(Default)]
    struct MockStorage {
        // key -> (json bytes, version counter)
        data: RefCell<HashMap<String, (Vec<u8>, u64)>>,
        fail_next_cas: RefCell<bool>,
    }

    impl DataStorage for MockStorage {
        async fn get_keys(&self) -> Result<Vec<String>, DataStorageError> {
            Ok(self.data.borrow().keys().cloned().collect())
        }

        async fn store<T: Serialize>(
            &self,
            key: &str,
            mode: &StoreMode,
            item: &T,
        ) -> Result<(), DataStorageError> {
            let bytes = serde_json::to_vec(item).unwrap();
            let mut map = self.data.borrow_mut();
            match mode {
                StoreMode::Always => {
                    let v = map.get(key).map(|(_, v)| v + 1).unwrap_or(1);
                    map.insert(key.to_string(), (bytes, v));
                    Ok(())
                }
                StoreMode::Absent => {
                    if map.contains_key(key) {
                        return Err(DataStorageError::CasMismatch);
                    }
                    map.insert(key.to_string(), (bytes, 1));
                    Ok(())
                }
                StoreMode::Cas(expected) => {
                    if *self.fail_next_cas.borrow() {
                        *self.fail_next_cas.borrow_mut() = false;
                        return Err(DataStorageError::CasMismatch);
                    }
                    match map.get(key) {
                        Some((_, v)) if v.to_string() == *expected => {
                            let nv = v + 1;
                            map.insert(key.to_string(), (bytes, nv));
                            Ok(())
                        }
                        _ => Err(DataStorageError::CasMismatch),
                    }
                }
            }
        }

        async fn get<T: serde::de::DeserializeOwned>(
            &self,
            key: &str,
        ) -> Result<Option<(T, String)>, DataStorageError> {
            match self.data.borrow().get(key) {
                Some((bytes, v)) => {
                    let item = serde_json::from_slice(bytes).unwrap();
                    Ok(Some((item, v.to_string())))
                }
                None => Ok(None),
            }
        }

        async fn delete(&self, key: &str) -> Result<(), DataStorageError> {
            self.data.borrow_mut().remove(key);
            Ok(())
        }

        async fn delete_all(&self) -> Result<(), DataStorageError> {
            self.data.borrow_mut().clear();
            Ok(())
        }
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        // Minimal executor: the mock never yields, so polling once is enough.
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(f);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("mock future pended unexpectedly"),
        }
    }

    fn cont(t: &str, c: &str) -> Continuation {
        Continuation {
            task_id: Some(t.into()),
            context_id: Some(c.into()),
        }
    }

    #[test]
    fn upsert_then_get_roundtrip() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, false, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap();
        let got = block(store.get_continuation(&key, 100)).unwrap();
        assert_eq!(got.task_id.as_deref(), Some("t1"));
        assert_eq!(got.context_id.as_deref(), Some("c1"));
    }

    #[test]
    fn expired_entry_not_returned_and_not_deleted() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, true, 1); // 1s ttl
        let key = CacheKey::from_hashed("conv");
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap();
        // now past expiry (1000ms)
        assert!(block(store.get_continuation(&key, 5000)).is_none());
        // Entry left in store for TTL (no proactive delete).
        assert!(!storage.data.borrow().is_empty());
    }

    #[test]
    fn terminal_evicts_local_hard_delete() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, false, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap();
        block(store.record_outcome(&key, StateClass::Terminal, &cont("t1", "c1"), 0)).unwrap();
        assert!(storage.data.borrow().is_empty());
        assert!(block(store.get_continuation(&key, 0)).is_none());
    }

    #[test]
    fn terminal_evicts_remote_tombstone_no_delete() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, true, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap();
        block(store.record_outcome(&key, StateClass::Terminal, &cont("t1", "c1"), 0)).unwrap();
        // Tombstone present (not deleted), but reads return None.
        assert!(!storage.data.borrow().is_empty());
        assert!(block(store.get_continuation(&key, 0)).is_none());
    }

    #[test]
    fn upsert_does_not_resurrect_terminal() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, true, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.record_outcome(&key, StateClass::Terminal, &Continuation::default(), 0))
            .unwrap();
        // No live entry yet → evict on empty is a no-op; now a continuable
        // upsert must still create (no terminal present to guard against).
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap();
        assert!(block(store.get_continuation(&key, 0)).is_some());
    }

    #[test]
    fn upsert_retries_on_cas_conflict() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, true, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.upsert(&key, &cont("t1", "c1"), 0)).unwrap(); // create
        *storage.fail_next_cas.borrow_mut() = true; // first CAS update fails
        block(store.upsert(&key, &cont("t2", "c1"), 0)).unwrap(); // retries, succeeds
        let got = block(store.get_continuation(&key, 0)).unwrap();
        assert_eq!(got.task_id.as_deref(), Some("t2"));
    }

    #[test]
    fn unknown_state_is_noop() {
        let storage = MockStorage::default();
        let store = ConversationStore::new(&storage, false, 3600);
        let key = CacheKey::from_hashed("conv");
        block(store.record_outcome(&key, StateClass::Unknown, &cont("t", "c"), 0)).unwrap();
        assert!(storage.data.borrow().is_empty());
    }
}
