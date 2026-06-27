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
//! NOTE: this is the scaffold entrypoint. Filter logic (request/response
//! rewriting, continuation, cache) is implemented in the dedicated modules and
//! wired here in subsequent tasks.

mod generated;

use anyhow::{anyhow, Result};
use pdk::hl::*;

use crate::generated::config::Config;

#[entrypoint]
async fn configure(launcher: Launcher, Configuration(bytes): Configuration) -> Result<()> {
    let _config: Config = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!(
            "Failed to parse configuration '{}'. Cause: {}",
            String::from_utf8_lossy(&bytes),
            err
        )
    })?;

    launcher.launch(on_request(request_filter)).await?;
    Ok(())
}

async fn request_filter(_request_state: RequestState) -> Flow<()> {
    Flow::Continue(())
}
