// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration struct mapped from `../rest-to-a2a-definition/gcl.yaml`.
//!
//! Hand-maintained: the GCL schema declares DataWeave selectors (mapped to
//! `pdk::script::Script`) and a nested `a2aConfiguration` object that the
//! current `cargo anypoint config-gen` cannot prettify. Keep this struct in
//! sync with the schema by hand. Do NOT regenerate over the nested object.

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "upstreamBinding", default = "default_upstream_binding")]
    pub upstream_binding: String,

    #[serde(alias = "promptSelector", deserialize_with = "de_selector")]
    pub prompt_selector: pdk::script::Script,

    #[serde(alias = "continuationMode", default = "default_continuation_mode")]
    pub continuation_mode: String,

    #[serde(alias = "contextKeySelector", deserialize_with = "de_selector")]
    pub context_key_selector: pdk::script::Script,

    #[serde(
        alias = "taskIdSelector",
        default,
        deserialize_with = "de_optional_selector"
    )]
    pub task_id_selector: Option<pdk::script::Script>,

    #[serde(
        alias = "contextIdSelector",
        default,
        deserialize_with = "de_optional_selector"
    )]
    pub context_id_selector: Option<pdk::script::Script>,

    #[serde(alias = "responseType", default = "default_response_type")]
    pub response_type: String,

    #[serde(alias = "responseMapping", deserialize_with = "de_selector")]
    pub response_mapping: pdk::script::Script,

    #[serde(alias = "responseFields", default)]
    pub response_fields: Vec<ResponseField>,

    #[serde(alias = "a2aConfiguration")]
    pub a2a_configuration: Option<A2aConfiguration>,

    #[serde(
        alias = "metadataSelector",
        default,
        deserialize_with = "de_optional_selector"
    )]
    pub metadata_selector: Option<pdk::script::Script>,

    #[serde(alias = "distributed", default = "default_distributed")]
    pub distributed: bool,

    #[serde(
        alias = "conversationTtlSeconds",
        default = "default_conversation_ttl_seconds"
    )]
    pub conversation_ttl_seconds: i64,

    #[serde(alias = "requestErrorStatus", default = "default_request_error_status")]
    pub request_error_status: i64,
}

/// One entry of `responseFields`: a (possibly dotted) output `name` plus a
/// dotted JSON-path `selector` resolved against the raw A2A result. The policy
/// assembles these into the REST response in Rust — see `response_build.rs` for
/// why construction can't live in a single DataWeave property, and `select.rs`
/// for why the per-field selector is a plain path string rather than DataWeave
/// (the gateway's `dw2pel` transform does not compile `format: dataweave`
/// nested inside array items, so a `#[...]` here would reach the policy as an
/// uncompiled string and fail to parse).
#[derive(Deserialize, Clone, Debug)]
pub struct ResponseField {
    #[serde(alias = "name")]
    pub name: String,
    #[serde(alias = "selector")]
    pub selector: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct A2aConfiguration {
    #[serde(alias = "acceptedOutputModes")]
    pub accepted_output_modes: Option<Vec<String>>,
    #[serde(alias = "blocking", default = "default_blocking")]
    pub blocking: bool,
}

fn default_upstream_binding() -> String {
    "jsonrpc".to_string()
}

fn default_continuation_mode() -> String {
    "cache".to_string()
}

fn default_distributed() -> bool {
    false
}

fn default_response_type() -> String {
    "raw".to_string()
}

fn default_conversation_ttl_seconds() -> i64 {
    3600
}

fn default_request_error_status() -> i64 {
    400
}

fn default_blocking() -> bool {
    true
}

/// Deserialize a DataWeave selector into a compiled `Script`. All selectors in
/// this policy share the same bindings: JSON payload + request/response
/// attributes (no `authentication`, no `vars`). The schema declares the same
/// bindings; keep the two in sync.
fn de_selector<'de, D>(deserializer: D) -> Result<pdk::script::Script, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let exp: pdk::script::Expression = serde::de::Deserialize::deserialize(deserializer)?;
    pdk::script::ScriptingEngine::script(&exp)
        .input(pdk::script::Input::Attributes)
        .input(pdk::script::Input::Payload(pdk::script::Format::Json))
        .compile()
        .map_err(serde::de::Error::custom)
}

/// Deserialize an OPTIONAL DataWeave selector. The `taskIdSelector` and
/// `contextIdSelector` have no schema default, so a missing key (or an explicit
/// JSON `null`) yields `None` and the explicit-mode code treats that as "fresh
/// task / context". A present string compiles exactly like [`de_selector`].
fn de_optional_selector<'de, D>(deserializer: D) -> Result<Option<pdk::script::Script>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let exp: Option<pdk::script::Expression> = serde::de::Deserialize::deserialize(deserializer)?;
    match exp {
        Some(exp) => pdk::script::ScriptingEngine::script(&exp)
            .input(pdk::script::Input::Attributes)
            .input(pdk::script::Input::Payload(pdk::script::Format::Json))
            .compile()
            .map(Some)
            .map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}
