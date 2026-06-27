# REST to A2A Bridge Policy

A MuleSoft Omni Gateway custom policy that converts an inbound **REST** API call into an outbound **A2A protocol v1.0 `SendMessage`** to an external A2A agent. Built with the [Policy Development Kit (PDK)](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview) **1.9.0** as a standalone, split-model project.

## Use case

An Agentforce agent exposes a **REST agent action** that needs to call an **external A2A agent v1.0**. This policy sits on the API instance whose upstream *is* the A2A agent. It:

1. Intercepts the inbound REST request.
2. Extracts the prompt (and conversation identity) via operator-supplied **DataWeave**.
3. Builds an A2A v1.0 `SendMessage` and forwards it upstream over the configured binding (`jsonrpc` or `httpjson`).
4. Shapes the raw A2A Task/Message response back to REST via **DataWeave**.

A2A tasks are multi-turn: a `SendMessage` can return `input-required`, needing the same `taskId`+`contextId` on the next turn. Continuity is preserved either by a **gossip-safe cache** keyed on a conversation value, or by having the client pass the ids **explicitly** (mutually exclusive modes).

> **Streaming is out of scope.** This policy issues a unary `SendMessage` only. See [`rest-to-a2a-flex/docs/spec.md`](rest-to-a2a-flex/docs/spec.md) for the full A2A v1.0 coverage map and limitations.

## Project layout

```
rest-to-a2a-policy/
├── rest-to-a2a-definition/   # GCL schema + Exchange metadata
│   ├── exchange.json
│   ├── gcl.yaml
│   └── Makefile
└── rest-to-a2a-flex/         # Rust implementation (compiles to wasm32-wasip1)
    ├── Cargo.toml
    ├── Makefile
    ├── docs/spec.md          # A2A v1.0 coverage map + cache lifecycle
    ├── src/
    │   ├── lib.rs            # entrypoint + filter wiring
    │   ├── a2a.rs            # A2A v1.0 method names, SendMessage builder, TaskState, response parse
    │   ├── jsonrpc.rs        # minimal JSON-RPC 2.0 envelope types
    │   ├── binding.rs        # UpstreamBinding {JsonRpc, HttpJson}, request rewrite, error responses
    │   ├── continuation.rs   # ContinuationMode {Cache, Explicit, None}: resolve + persist
    │   ├── cache.rs          # gossip-safe ConversationStore (CAS / put_absent / no-delete)
    │   ├── dataweave.rs      # selector evaluation helpers
    │   ├── config_map.rs     # generated Config → PolicyConfig
    │   └── generated/        # config struct from gcl.yaml (hand-maintained)
    ├── playground/           # Docker-based local Omni Gateway for `make run`
    └── tests/                # integration tests (pdk-test)
```

See the per-half `README.md` files for the `make` workflows, and `rest-to-a2a-flex/docs/spec.md` for protocol coverage.
