// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test setup helpers: build a Flex+httpmock TestComposite with a configurable
//! policy config, plus helpers to mock the upstream A2A agent's responses for
//! both the JSON-RPC and HTTP+JSON bindings.

use httpmock::MockServer;
use pdk_test::port::Port;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::TestComposite;

use super::{COMMON_CONFIG_DIR, POLICY_DIR, POLICY_NAME};

pub const FLEX_PORT: Port = 8081;

/// Build a `PolicyConfig` from a JSON value containing the gcl.yaml-shaped
/// configuration. The caller fully controls the schema so tests can exercise
/// any combination of bindings and continuation modes.
pub fn build_policy_config(config: serde_json::Value) -> PolicyConfig {
    PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(config)
        .build()
}

/// Spin up a Flex Gateway with the given policy configuration plus an httpmock
/// acting as the upstream A2A agent. Returns the composite (keep it alive), the
/// public Flex URL to target, and a connected `MockServer`.
pub async fn setup_test(
    policy_config: serde_json::Value,
) -> anyhow::Result<(TestComposite, String, MockServer)> {
    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = build_policy_config(policy_config);

    let api_config = ApiConfig::builder()
        .name("restToA2aApi")
        .upstream(&httpmock_config)
        .path("/")
        .port(FLEX_PORT)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.12.1")
        .hostname("local-flex")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let flex_url = flex.external_url(FLEX_PORT).unwrap();

    let httpmock: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(httpmock.socket()).await;

    Ok((composite, flex_url, mock_server))
}

/// Mock the upstream A2A agent (JSON-RPC binding) to return a JSON-RPC 2.0
/// success envelope wrapping the given `result` (a Task or Message).
pub async fn mock_jsonrpc_result(mock_server: &MockServer, result: serde_json::Value) {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": result,
    })
    .to_string();
    mock_server
        .mock_async(|when, then| {
            when.method(httpmock::Method::POST);
            then.status(200)
                .header("content-type", "application/json")
                .body(body);
        })
        .await;
}

/// Mock the upstream A2A agent (HTTP+JSON binding) to return a bare Task or
/// Message payload with the given HTTP status.
pub async fn mock_httpjson_result(
    mock_server: &MockServer,
    status: u16,
    result: serde_json::Value,
) {
    mock_server
        .mock_async(move |when, then| {
            when.method(httpmock::Method::POST);
            then.status(status)
                .header("content-type", "application/json")
                .body(result.to_string());
        })
        .await;
}
