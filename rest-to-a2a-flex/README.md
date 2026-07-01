# "rest-to-a2a" Policy

This is the policy-implementation half of the **REST to A2A Bridge** policy. It compiles to a `wasm32-wasip1` module loaded by Omni Gateway at runtime.

The policy was created with the Omni Gateway Policy Development Kit (PDK). For the complete PDK documentation, see [PDK Overview](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview).

For a step-by-step configuration guide with scenarios (including chaining a DataWeave policy to transform the response), see [`docs/how-to.md`](docs/how-to.md).

For the A2A v1.0 protocol coverage map and limitations (including why streaming is unsupported), see [`docs/spec.md`](docs/spec.md).

## What it does

The policy sits on an API instance whose upstream is an external A2A agent (v1.0). On each inbound REST request it:

1. Extracts the prompt from the REST body via the `promptSelector` DataWeave expression. A null/empty/error result **fails closed** (`requestErrorStatus`, default 400) — the upstream is never called.
2. Resolves multi-turn continuation per `continuationMode` (see below) to obtain a `taskId`/`contextId` to carry forward.
3. Builds an A2A v1.0 `SendMessage` and rewrites the outbound body **in-band** (`Flow::Continue`), framed for the configured `upstreamBinding`. The upstream path is operator-owned via the route `destinationPath`; the policy never rewrites `:path`.
4. On the response, parses the raw A2A `Task`/`Message`, updates the conversation cache (cache mode), and shapes the REST-facing body via `responseMapping`. A mapping error is **non-fatal** — the raw A2A body passes through and a warning is logged.

Streaming (SSE) is **not supported**: a `text/event-stream` upstream response is passed through untouched with a warning. See [`docs/spec.md`](docs/spec.md) for the full A2A coverage map.

## Configuration reference

| Property | Type | Default | Notes |
|---|---|---|---|
| `upstreamBinding` | enum `jsonrpc`\|`httpjson` | `jsonrpc` | `jsonrpc`: JSON-RPC 2.0 envelope, method `SendMessage`. `httpjson`: bare A2A payload to `POST /message:send` (operator sets `destinationPath`). |
| `promptSelector` | DataWeave | `#[payload.prompt]` | Prompt string from the REST request. Fail-closed on null/empty/error. |
| `continuationMode` | enum `cache`\|`explicit`\|`none` | `cache` | How multi-turn continuation is handled. `cache`: gateway derives cache key from `contextKeySelector` and persists/injects taskId+contextId. `explicit`: client supplies ids via selectors. `none`: single-shot, no continuation. See **Continuation modes** below. Mode-specific fields are grouped into `cacheConfig` / `explicitConfig` objects (see below); each applies only in its mode. |
| `contextKeySelector` | DataWeave | — | Under `cacheConfig`. Cache mode only. Conversation value → SHA-256 → cache key. Empty = single-shot. |
| `taskIdSelector` | DataWeave | — | Under `explicitConfig`. Used when `continuationMode = explicit`. DataWeave expression returning the A2A `taskId` to continue. Leave empty for a fresh task. |
| `contextIdSelector` | DataWeave | — | Under `explicitConfig`. Used when `continuationMode = explicit`. DataWeave expression returning the A2A `contextId` to continue. Leave empty for a fresh context. |
| `responseType` | enum `raw`\|`mapping`\|`fields` | `raw` | How the upstream A2A response is returned. `raw` (default): byte-faithful passthrough. `mapping`: shape via `responseMapping`. `fields`: assemble via `responseFields`. Mode is explicit. |
| `responseMapping` | DataWeave | — | Under `mappingConfig`. Used only when `responseType = mapping`. Runs against the raw A2A result object (the Task/Message itself — paths are relative, no `task`/`message` prefix). **Selection-only** (e.g. `#[payload.artifacts[0].parts[0].text]`) — object/array construction is rejected by the runtime and falls back to raw passthrough (see `docs/spec.md`). Non-fatal on error. |
| `responseFields` | array of `{name, selector}` | `[]` | Under `fieldsConfig`. Used only when `responseType = fields`. Assemble a flat/nested REST response from dotted JSON-path selections of the raw A2A result. `selector` is a plain path (e.g. `artifacts[0].parts[0].text`), NOT DataWeave — the gateway does not compile DataWeave nested inside array items. `name` may be dotted (e.g. `data.taskRef`) to nest. A path that resolves to nothing yields a `null` field. See **Building a custom response** below. |
| `a2aConfiguration` | object | — | Optional SendMessage `configuration`: `acceptedOutputModes[]`, `blocking` (default true). |
| `metadataSelector` | DataWeave | `#[null]` | Optional. DataWeave expression returning an object of key/value pairs attached to the A2A message as `metadata`, e.g. `#[{tenant: attributes.headers['x-tenant'], traceId: payload.traceId}]`. Each value may be any DataWeave expression. A null/non-object result attaches no metadata. Leave as `#[null]` to attach none (an object-literal default is rejected by the runtime). |
| `distributed` | boolean | `false` | Cache mode only. `true` = gossip-replicated remote store shared across replicas; `false` = local per-replica store. **`true` needs a multi-replica gateway** — in single-replica Local Mode the remote store does not persist across requests (see `docs/spec.md`). |
| `conversationTtlSeconds` | integer | `3600` | Cache mode only. Entry lifetime + remote namespace TTL (min 60, max 86400). |
| `requestErrorStatus` | integer | `400` | Caller-facing status on prompt-extraction failure (min 400, max 599). |

### Continuation modes

`continuationMode` controls how multi-turn A2A task continuation is handled:

- **`cache` (default)** — `contextKeySelector` yields a conversation value, hashed (SHA-256) into the cache key. A live continuable entry injects `taskId`+`contextId` on the next turn; continuable responses upsert the entry, terminal responses evict it. Gossip-safe (no DELETE-before-recreate; TTL eviction on remote). The raw conversation value is never stored. `contextKeySelector`, `distributed`, and `conversationTtlSeconds` live in the `cacheConfig` object.
- **`explicit`** — the client supplies the ids via `taskIdSelector`/`contextIdSelector` (in the `explicitConfig` object); the cache is never touched.
- **`none`** — single-shot; no continuation, no cache, no ids carried forward. Every call is independent.

### Building a custom response

By default (`responseType: raw`) the policy returns the raw A2A result **verbatim** —
a byte-faithful echo with no parse or reshape. This is the simplest posture and,
unlike `responseMapping: "#[payload]"`, preserves JSON numbers exactly (identity
mapping re-serializes `-32602` as `-32602.0`). Set `responseType` to shape the
response.

When shaping with `responseType: mapping`, `responseMapping` can only *select* a
sub-tree of the raw A2A result — the gateway's embedded DataWeave rejects object
construction (see `docs/spec.md`). To return a **custom-shaped** REST body, set
`responseType: fields` and use `responseFields`: list the output fields, each a
`name` plus a dotted-path `selector` into the raw result. The policy resolves every
path and assembles the object in Rust.

```yaml
responseType: fields
fieldsConfig:
  responseFields:
    - name: conversationId
      selector: contextId
    - name: taskRef
      selector: id
    - name: status
      selector: status.state
    - name: reply
      selector: status.message.parts[0].text   # array indices supported
```

The selector root is the raw A2A result object itself (the Task/Message), so
paths are relative to it. Given an A2A `input-required` task, this yields:

```json
{ "conversationId": "ctx-7", "taskRef": "task-42",
  "status": "TASK_STATE_INPUT_REQUIRED", "reply": "Sure — what is your order number?" }
```

For a completed task the agent's answer is in an artifact instead:
`selector: artifacts[0].parts[0].text`.

Notes:
- A dotted `name` nests: `data.taskRef` → `{ "data": { "taskRef": ... } }`.
- A leading `payload.` on a selector is ignored (so paths read like `responseMapping`).
- A path that resolves to nothing (missing key, out-of-range index) yields a `null` field.
- Conflicting names (a leaf vs. a nested object at the same key) — first field wins; the conflict is logged.
- Conditionals and computed values are out of scope; selectors are pure selection.

#### Need full DataWeave on the response? Chain a transformation policy

`responseFields` only *selects*. For full DataWeave construction on the upstream
response (conditionals, `map`/`filter`, computed fields, object building), attach
MuleSoft's built-in **DataWeave Body Transformation** policy *after* `rest-to-a2a`
on the same API instance with `requestFlow: onResponse`. It is a native
Java-backed filter shipped in the gateway runtime — **not** a PDK policy, so it
cannot be embedded in this policy; the operator adds it to the API's policy chain.
One instance handles one direction (`onRequest` vs `onResponse`); it runs in
streaming mode and rewrites `Content-Type`/`Content-Length`. Place it after this
policy so it sees the A2A body this policy surfaces. See
[`docs/spec.md`](docs/spec.md) ("Response-shaping escape hatches") and the
[policy reference](https://docs.mulesoft.com/gateway/latest/policies-included-dataweave-body-transformation).

#### Exposing the raw upstream response

Set `responseType: raw` (the default) for a byte-faithful verbatim passthrough of the
upstream A2A result. No other configuration is needed. Note that `responseMapping:
"#[payload]"` is not byte-faithful — identity mapping re-serializes JSON numbers as
doubles (`-32602` → `-32602.0`). Use `responseType: raw` for a true byte-for-byte
echo. See [`docs/spec.md`](docs/spec.md).

The three modes are mutually exclusive. A continuable upstream reply that cannot be resumed (no cache key and no explicit ids) is still returned, with a warning logged.

## Make command reference

This project has a Makefile with the goals used during the policy development lifecycle.

### Setup
`make setup` installs the PDK build dependencies (`cargo-anypoint`).

### Build asset files
`make build-asset-files` fetches the latest definition from Exchange and verifies the config struct is present.

> **Note:** `src/generated/config.rs` is hand-maintained for this policy because the GCL schema declares DataWeave selectors and a nested `a2aConfiguration` object that the current `cargo anypoint config-gen` cannot prettify. The hand-rolled struct is checked in. The `build-asset-files` target only fetches the definition and verifies presence — it does not regenerate the file. Run `cargo anypoint config-gen` manually only when adding top-level scalar properties; do **not** regenerate over the nested object.

### Build
`make build` compiles the WebAssembly binary. Run `make build-asset-files` at least once before compiling so the source stays in sync with the definition.

### Run
`make run` executes the current build in a Docker containerized Omni Gateway. The `playground/config` directory must contain:

- A `registration.yaml` produced by an Omni Gateway registration in Local Mode (gitignored). Generate one via Runtime Manager → Omni Gateway → Add Gateway → Docker, replacing `--connected=true` with `--connected=false`, and run it in `playground/config`.
- An `api.yaml` with the desired policy configuration. The repo ships an example using the `jsonrpc` binding and a header-based conversation cache key.

### Test
`make test` runs unit + integration tests. Integration tests require a `tests/config/registration.yaml` produced via `anypoint-cli-v4 registration create-local` (gitignored).

Convenience targets:
- `make test-unit` — unit tests only (no Docker required).
- `make test-one TEST=<name>` — a single test by name.

### Publish / Release
`make publish` publishes a development (`-DEV`) asset; `make release` publishes a production asset. Both require the matching `definition_asset_id.version` in `Cargo.toml`.

*For more information, see [Uploading Custom Policies to Exchange](https://docs.mulesoft.com/pdk/latest/policies-pdk-publish-policies).*
