// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Maps the raw generated [`Config`] (deserialized from the GCL schema) into a
//! validated [`PolicyConfig`] the filters consume.
//!
//! The generated struct carries strings for the two enums (`upstreamBinding`,
//! `continuationMode`) and a loosely-typed `a2aConfiguration`. This module
//! parses those into typed forms once, at `configure` time, so the hot path
//! never re-parses or re-validates.

use std::str::FromStr;

use pdk::script::Script;

use crate::a2a::SendConfiguration;
use crate::binding::{UnknownBinding, UpstreamBinding};
use crate::generated::config::{Config, ResponseField};

/// How the policy preserves multi-turn task continuity. Mutually exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationMode {
    /// Server-side: hash a conversation key, cache `contextId`+`taskId`,
    /// inject on the next turn, evict on terminal state.
    Cache,
    /// Client-supplied: `taskIdSelector` / `contextIdSelector` provide the ids;
    /// the cache is never touched.
    Explicit,
    /// Single-shot: no continuation.
    None,
}

/// Error building a [`PolicyConfig`] from the raw schema config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Binding(#[from] UnknownBinding),
    #[error("unknown continuationMode '{0}' (expected 'cache', 'explicit', or 'none')")]
    ContinuationMode(String),
}

impl FromStr for ContinuationMode {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cache" => Ok(Self::Cache),
            "explicit" => Ok(Self::Explicit),
            "none" => Ok(Self::None),
            other => Err(ConfigError::ContinuationMode(other.to_string())),
        }
    }
}

/// Validated, typed policy configuration consumed by the filters.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub binding: UpstreamBinding,
    pub continuation_mode: ContinuationMode,
    pub prompt_selector: Script,
    pub context_key_selector: Script,
    pub task_id_selector: Script,
    pub context_id_selector: Script,
    /// Master switch for response shaping. `false` (default) returns the raw
    /// upstream A2A body verbatim (no parse, no reshape) — a byte-faithful echo.
    /// `true` shapes the response: `response_fields` (if non-empty) takes
    /// precedence, otherwise `response_mapping`.
    pub custom_response: bool,
    pub response_mapping: Script,
    /// Dotted-path field list. When `custom_response` is set and this is
    /// non-empty it takes precedence over `response_mapping`: the policy resolves
    /// each path against the raw A2A result and assembles the REST response
    /// object in Rust (the runtime can't construct objects in a DataWeave
    /// property, nor compile DataWeave nested inside array items — see
    /// `response_build.rs` and `select.rs`).
    pub response_fields: Vec<ResponseField>,
    pub send_configuration: Option<SendConfiguration>,
    pub distributed: bool,
    pub conversation_ttl_seconds: u32,
    pub request_error_status: u32,
}

impl PolicyConfig {
    /// Build from the generated schema config, parsing the enum strings and
    /// nested objects. Fails only on an unknown binding / continuation-mode
    /// string (caught at `configure` time, before any request is served).
    pub fn from_generated(config: Config) -> Result<Self, ConfigError> {
        let binding = UpstreamBinding::from_str(&config.upstream_binding)?;
        // The `enableTaskContinuation` master switch overrides the mode: when off,
        // continuation is forced to `None` regardless of the configured
        // `continuationMode` — no cache is built and no ids are carried. The enum
        // value is still parsed first so a malformed string is reported even when
        // continuation is disabled.
        let parsed_mode = ContinuationMode::from_str(&config.continuation_mode)?;
        let continuation_mode = if config.enable_task_continuation {
            parsed_mode
        } else {
            ContinuationMode::None
        };

        let send_configuration = config.a2a_configuration.map(|a2a| SendConfiguration {
            accepted_output_modes: a2a.accepted_output_modes,
            blocking: a2a.blocking,
        });

        Ok(Self {
            binding,
            continuation_mode,
            prompt_selector: config.prompt_selector,
            context_key_selector: config.context_key_selector,
            task_id_selector: config.task_id_selector,
            context_id_selector: config.context_id_selector,
            custom_response: config.custom_response,
            response_mapping: config.response_mapping,
            response_fields: config.response_fields,
            send_configuration,
            distributed: config.distributed,
            // Schema constrains these ranges (ttl 60..=86400, status 400..=599);
            // clamp defensively to the same bounds in case the schema is ever
            // loosened — a sub-minute TTL would thrash the conversation cache.
            conversation_ttl_seconds: config.conversation_ttl_seconds.clamp(60, 86400) as u32,
            request_error_status: config.request_error_status.clamp(400, 599) as u32,
        })
    }

    /// Whether server-side caching is active (cache mode). Drives whether
    /// `configure` builds a `DataStorage` backend at all.
    pub fn uses_cache(&self) -> bool {
        self.continuation_mode == ContinuationMode::Cache
    }

    /// Whether the raw upstream A2A body is returned verbatim (no parse, no
    /// reshape). True when response shaping is off — the default posture.
    pub fn uses_raw_response(&self) -> bool {
        !self.custom_response
    }

    /// Whether the response is assembled from `responseFields` by dotted-path
    /// selection in Rust rather than via the `responseMapping` DataWeave
    /// selector. Requires shaping on (`custom_response`) AND a non-empty field
    /// list; when both hold it takes precedence over `responseMapping`.
    pub fn uses_response_fields(&self) -> bool {
        self.custom_response && !self.response_fields.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_mode_parsing() {
        assert_eq!("cache".parse(), Ok(ContinuationMode::Cache));
        assert_eq!("EXPLICIT".parse(), Ok(ContinuationMode::Explicit));
        assert_eq!("none".parse(), Ok(ContinuationMode::None));
        assert!("bogus".parse::<ContinuationMode>().is_err());
    }
}
