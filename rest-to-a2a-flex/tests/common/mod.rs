// Copyright 2026 Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common test utilities shared across all integration test files.
//!
//! Each integration test file is compiled as its own crate, and only uses a
//! subset of these helpers. Suppress the dead-code warnings that result.

#![allow(dead_code)]

pub mod setup;

// Directory where the policy implementation wasm + GCL are placed by `make build`.
pub const POLICY_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/wasm32-wasip1/release");

// Directory containing logging.yaml and (locally generated) registration.yaml.
pub const COMMON_CONFIG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/config");

// Policy implementation reference name. Override after a major version bump
// (read it from `target/policy-ref-name.txt` after `make build`).
pub const POLICY_NAME: &str = "rest-to-a2a-flex-v1-0";
