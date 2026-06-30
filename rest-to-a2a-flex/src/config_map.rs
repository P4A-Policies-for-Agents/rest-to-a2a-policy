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

/// How the upstream A2A response is shaped before returning to the caller.
/// Mutually exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseType {
    /// Byte-faithful passthrough: the raw A2A result is returned unchanged.
    Raw,
    /// Shape via the `responseMapping` DataWeave expression.
    Mapping,
    /// Assemble the response in Rust from the `responseFields` path selectors.
    Fields,
}

/// Error building a [`PolicyConfig`] from the raw schema config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Binding(#[from] UnknownBinding),
    #[error("unknown continuationMode '{0}' (expected 'cache', 'explicit', or 'none')")]
    ContinuationMode(String),
    #[error("unknown responseType '{0}' (expected 'raw', 'mapping', or 'fields')")]
    ResponseType(String),
}

impl FromStr for ResponseType {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Ok(Self::Raw),
            "mapping" => Ok(Self::Mapping),
            "fields" => Ok(Self::Fields),
            other => Err(ConfigError::ResponseType(other.to_string())),
        }
    }
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
    /// Explicit-mode only. `None` when the operator left the box empty — the
    /// request filter then treats it as "fresh task" without evaluating a
    /// selector.
    pub task_id_selector: Option<Script>,
    /// Explicit-mode only. `None` when left empty — treated as "fresh context".
    pub context_id_selector: Option<Script>,
    /// How the upstream response is shaped. `Raw` (default) returns the raw
    /// upstream A2A body verbatim (no parse, no reshape) — a byte-faithful echo.
    /// `Mapping` applies `response_mapping`; `Fields` assembles from
    /// `response_fields`.
    pub response_type: ResponseType,
    pub response_mapping: Script,
    /// Dotted-path field list, used when `response_type` is `Fields`: the policy
    /// resolves each path against the raw A2A result and assembles the REST
    /// response object in Rust (the runtime can't construct objects in a
    /// DataWeave property, nor compile DataWeave nested inside array items — see
    /// `response_build.rs` and `select.rs`).
    pub response_fields: Vec<ResponseField>,
    /// Optional DataWeave expression yielding an object of A2A message metadata
    /// key/value pairs. `None` when the operator left it empty — no metadata is
    /// attached.
    pub metadata_selector: Option<Script>,
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
        // Continuation behavior is driven entirely by `continuationMode`. `none`
        // is the single-shot mode: no cache built, no ids carried.
        let continuation_mode = ContinuationMode::from_str(&config.continuation_mode)?;
        let response_type = ResponseType::from_str(&config.response_type)?;

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
            response_type,
            response_mapping: config.response_mapping,
            response_fields: config.response_fields,
            metadata_selector: config.metadata_selector,
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
    /// reshape). True for the default `Raw` response type.
    pub fn uses_raw_response(&self) -> bool {
        self.response_type == ResponseType::Raw
    }

    /// Whether the response is assembled from `responseFields` by dotted-path
    /// selection in Rust rather than via the `responseMapping` DataWeave
    /// selector.
    pub fn uses_response_fields(&self) -> bool {
        self.response_type == ResponseType::Fields
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

    #[test]
    fn response_type_parsing() {
        assert_eq!("raw".parse(), Ok(ResponseType::Raw));
        assert_eq!("MAPPING".parse(), Ok(ResponseType::Mapping));
        assert_eq!("fields".parse(), Ok(ResponseType::Fields));
        assert!("bogus".parse::<ResponseType>().is_err());
    }
}
