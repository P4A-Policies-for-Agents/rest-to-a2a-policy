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

    #[serde(alias = "taskIdSelector", deserialize_with = "de_selector")]
    pub task_id_selector: pdk::script::Script,

    #[serde(alias = "contextIdSelector", deserialize_with = "de_selector")]
    pub context_id_selector: pdk::script::Script,

    #[serde(alias = "responseMapping", deserialize_with = "de_selector")]
    pub response_mapping: pdk::script::Script,

    #[serde(alias = "a2aConfiguration")]
    pub a2a_configuration: Option<A2aConfiguration>,

    #[serde(alias = "distributed", default = "default_distributed")]
    pub distributed: bool,

    #[serde(alias = "conversationTtlSeconds", default = "default_conversation_ttl_seconds")]
    pub conversation_ttl_seconds: i64,

    #[serde(alias = "requestErrorStatus", default = "default_request_error_status")]
    pub request_error_status: i64,
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
