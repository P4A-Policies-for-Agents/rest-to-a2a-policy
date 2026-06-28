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
| `enableTaskContinuation` | boolean | `true` | Master switch for multi-turn continuation. `true`: continuity per `continuationMode`. `false`: forces single-shot — no cache, no `taskId`/`contextId` carried; the continuation settings below are ignored. |
| `continuationMode` | enum `cache`\|`explicit`\|`none` | `cache` | Only when `enableTaskContinuation` is true. See **Continuation modes** below. |
| `contextKeySelector` | DataWeave | `#[null]` | Cache mode only. Conversation value → SHA-256 → cache key. Null = single-shot. |
| `taskIdSelector` | DataWeave | `#[null]` | Explicit mode only. Client-supplied `taskId`. |
| `contextIdSelector` | DataWeave | `#[null]` | Explicit mode only. Client-supplied `contextId`. |
| `customResponse` | boolean | `false` | Master switch for response shaping. `false`: return the raw A2A result **verbatim** (byte-faithful — no parse/reshape). `true`: shape the response — `responseFields` (if non-empty) wins, else `responseMapping`. The two fields below are ignored when this is off. |
| `responseMapping` | DataWeave | `#[payload]` | Only when `customResponse` is true and `responseFields` is empty. Runs against the raw A2A result object. **Selection-only** (e.g. `#[payload.task]`) — object/array construction is rejected by the runtime and falls back to raw passthrough (see `docs/spec.md`). Non-fatal on error. |
| `responseFields` | array of `{name, selector}` | `[]` | Only when `customResponse` is true. Assemble a flat/nested REST response from dotted JSON-path selections of the raw A2A result. **Overrides `responseMapping`** when non-empty. `selector` is a plain path (e.g. `task.status.update.parts[0].text`), NOT DataWeave — the gateway does not compile DataWeave nested inside array items. `name` may be dotted (e.g. `data.taskRef`) to nest. A path that resolves to nothing yields a `null` field. See **Building a custom response** below. |
| `a2aConfiguration` | object | — | Optional SendMessage `configuration`: `acceptedOutputModes[]`, `blocking` (default true). |
| `distributed` | boolean | `false` | Cache mode only. `true` = gossip-replicated remote store shared across replicas; `false` = local per-replica store. **`true` needs a multi-replica gateway** — in single-replica Local Mode the remote store does not persist across requests (see `docs/spec.md`). |
| `conversationTtlSeconds` | integer | `3600` | Cache mode only. Entry lifetime + remote namespace TTL (min 60, max 86400). |
| `requestErrorStatus` | integer | `400` | Caller-facing status on prompt-extraction failure (min 400, max 599). |

### Continuation modes

Continuation is governed first by the `enableTaskContinuation` master switch
(default `true`). Set it to `false` to disable multi-turn continuation entirely —
the policy then runs single-shot regardless of `continuationMode`, builds no
cache, carries no `taskId`/`contextId`, and ignores the continuation settings
below. When `true`, `continuationMode` selects how continuity works:

- **`cache`** — `contextKeySelector` yields a conversation value, hashed (SHA-256) into the cache key. A live continuable entry injects `taskId`+`contextId` on the next turn; continuable responses upsert the entry, terminal responses evict it. Gossip-safe (no DELETE-before-recreate; TTL eviction on remote). The raw conversation value is never stored.
- **`explicit`** — the client supplies the ids via `taskIdSelector`/`contextIdSelector`; the cache is never touched.
- **`none`** — single-shot; no continuation, no storage.

### Building a custom response

By default (`customResponse: false`) the policy returns the raw A2A result
**verbatim** — a byte-faithful echo with no parse or reshape. This is the simplest
posture and, unlike `responseMapping: "#[payload]"`, preserves JSON numbers exactly
(identity mapping re-serializes `-32602` as `-32602.0`). Set `customResponse: true`
to shape the response.

When shaping, `responseMapping` can only *select* a sub-tree of the raw A2A result —
the gateway's embedded DataWeave rejects object construction (see `docs/spec.md`).
To return a **custom-shaped** REST body, use `responseFields` instead: list the
output fields, each a `name` plus a dotted-path `selector` into the raw result. The
policy resolves every path and assembles the object in Rust. With `customResponse`
true, a non-empty `responseFields` takes precedence over `responseMapping`.

```yaml
customResponse: true
responseFields:
  - name: conversationId
    selector: task.contextId
  - name: taskRef
    selector: task.id
  - name: status
    selector: task.status.state
  - name: reply
    selector: task.status.update.parts[0].text   # array indices supported
```

Given an A2A `input-required` task, this yields:

```json
{ "conversationId": "ctx-7", "taskRef": "task-42",
  "status": "TASK_STATE_INPUT_REQUIRED", "reply": "Sure — what is your order number?" }
```

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

There is no dedicated "return raw" flag. The verbatim upstream body is already
returned when `responseMapping` evaluation fails (non-fatal passthrough). To
deliberately surface the raw A2A result, set `responseMapping: "#[payload]"` —
but identity mapping re-serializes JSON numbers as doubles (`-32602` →
`-32602.0`), so it is not byte-faithful. A true byte-for-byte echo would need a
new config option (not currently implemented). See [`docs/spec.md`](docs/spec.md).

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
