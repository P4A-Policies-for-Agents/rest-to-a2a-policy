# REST → A2A v1.0 Bridge — Protocol Coverage & Design

> Status: implemented. This is the authoritative coverage map; keep it in sync
> with the code.

## A2A v1.0 coverage

### Supported

- **Unary `SendMessage`** over both wire bindings:
  - **JSON-RPC 2.0** — `POST` of `{jsonrpc:"2.0", id, method:"SendMessage", params}`.
  - **HTTP+JSON** — `POST /message:send` with the bare `params` payload.
- **Message shape** — `{messageId, role:"user", parts:[{text}]}` plus optional
  `taskId` / `contextId` (continuation) and optional `configuration`
  (`acceptedOutputModes[]`, `blocking`).
- **Response handling** — Task or Message result; both A2A error shapes
  (JSON-RPC in-band error; HTTP+JSON `google.rpc.Status`) surfaced to the
  response DataWeave.
- **Multi-turn continuation** — `cache` (server-side, keyed on a hashed
  conversation value) or `explicit` (client supplies `taskId`/`contextId`).

### Out of scope (explicit)

- `SendStreamingMessage` / `message:stream` — **no SSE**. A `text/event-stream`
  response is passed through untouched with a warning.
- `SubscribeToTask` / `tasks:subscribe` / `tasks/resubscribe`.
- `GetTask` / `ListTasks` / `CancelTask`.
- Push-notification config methods.
- Agent-card methods / transcoding.
- gRPC binding.
- v1 ↔ v0.3 transcoding — the upstream must speak A2A v1.0.

## Task state classification

| State | Class | Cache action |
|---|---|---|
| `input-required`, `working`, `submitted` | continuable | upsert (persist contextId+taskId) |
| `completed`, `failed`, `canceled`, `rejected` | terminal | evict |

## Continuation modes

`enableTaskContinuation` (default `true`) is the master switch. When `false`, the
policy forces continuation to `none` regardless of `continuationMode`: no storage
is built, no ids are carried, every call is single-shot, and the continuation
properties (`continuationMode` and the selectors) are ignored. The override is
applied in `config_map` (the enum string is still
parsed first, so a malformed `continuationMode` is reported even when disabled).
When `true`, `continuationMode` selects:

- **`cache`** — `contextKeySelector` yields a conversation value → SHA-256 →
  cache key. On read, a continuable entry injects `contextId`+`taskId`. On
  response, continuable states upsert, terminal states evict. Gossip-safe
  (no DELETE-before-recreate; TTL eviction on remote).
- **`explicit`** — `taskIdSelector` / `contextIdSelector` provide the ids; the
  cache is never touched.
- **`none`** — single-shot.

## Upstream routing (in-band, no `:path` rewrite)

Both bindings forward **in-band** (`Flow::Continue` + body rewrite) so the API
instance's route, upstream, auth and any other attached policies still apply.
The upstream path is **operator-owned** via the route `destinationPath`; the
policy never mutates `:path`.

- `jsonrpc` — operator points `destinationPath` at the A2A JSON-RPC endpoint.
  Policy rewrites the body to the JSON-RPC envelope only.
- `httpjson` — operator sets `destinationPath: /message:send`. Policy rewrites
  the body to the bare `params` payload and sets `content-type: application/json`.

Rationale (PDK 1.9.0): the upstream path is an Envoy route action applied after
the wasm filter chain. `set_header(":path", …)` only mutates the header map;
re-routing on that mutation is host/version-dependent and PDK exposes no
route-cache API. An out-of-band `HttpClient` side-call was rejected — it would
bypass the operator's route/upstream/auth chain.

Header/body handling uses plain **event flow** (no `enable_stop_iteration`):
header ops (`content-type`, remove `content-length`) run in the headers phase
unconditionally (the body is always rewritten); DW eval + `set_body` run in the
body phase. The evaluator retains bound `attributes` across the transition.

## `responseMapping` is selection-only (runtime constraint)

The Flex Gateway 1.12.1 embedded DataWeave used for `dataweave`-format policy
properties evaluates **selectors** (`#[payload.task]`,
`#[payload.task.status.state]`, `#[payload]`) but **rejects object/array
construction** — `#[{ k: payload.x }]`, the full `%dw 2.0 … --- {…}` script
form, and `default`/index-chain reshaping all fail. A construction expression
fails at eval and triggers the non-fatal raw-passthrough fallback (the
unmodified upstream body is returned). Verified end-to-end against the live
runtime. Therefore `responseMapping` must select a sub-tree of the A2A result;
flattening into a bespoke envelope is done by `responseFields` (below) rather
than by `responseMapping`.

## `responseFields` — Rust-side response assembly

To return a custom-shaped REST body despite the construction constraint above,
`responseFields` moves the object construction into the policy. Each entry is an
output `name` plus a `selector`; the policy resolves every selector against the
raw A2A result and assembles the object in Rust (`src/response_build.rs`).
`responseFields` applies only when `customResponse` is `true`; when it is and the
list is non-empty, it **overrides** `responseMapping`.

The per-field `selector` is a **plain dotted JSON path**, not DataWeave. This is
forced by a second, distinct runtime constraint: the gateway's `dw2pel` config
transform compiles only **top-level** `format: dataweave` properties — it does
**not** recurse into `format: dataweave` declared inside array-item properties.
A DataWeave selector nested under `responseFields[].selector` therefore reaches
the policy as an uncompiled `#[...]` string and fails to deserialize into a PEL
`Expression`, so the whole policy fails to configure (HTTP 503). Verified against
Flex 1.12.1:
`invalid type: string "#[payload.task.id]", expected PEL Expression`.

The path grammar (`src/select.rs`): `.`-separated segments, optional `[index]`
array steps (`parts[0]`, `matrix[1][2]`), an optional leading `payload.` that is
stripped, and an empty path that selects the whole result. A dotted output `name`
nests (`data.taskRef`). Anything a path can't resolve yields a `null` field
(non-fatal, consistent with the mapping fallback). Plain-path selection is also
strictly more faithful than `#[payload]` identity mapping, which re-serializes
JSON numbers as doubles; path selection preserves the original token.

## Response-shaping escape hatches

Two distinct mechanisms reshape the upstream A2A response into the REST body the
caller expects. They are complementary; pick by how much transformation power you
need.

1. **`responseFields` — in-policy (this policy).** Assemble a flat/nested envelope
   from dotted JSON-path selections of the raw A2A result, resolved in Rust (see
   the section above). Pure selection: no conditionals, computed values, or
   expression logic. Overrides `responseMapping` when non-empty. Use this for the
   common case (flatten a sub-tree into `conversationId`/`taskRef`/`reply`).

2. **Operator-chained DataWeave Body Transformation — out-of-policy.** When you
   need full DataWeave construction on the upstream response (conditionals,
   `map`/`filter`, computed fields, object/array building), attach MuleSoft's
   built-in **DataWeave Body Transformation** policy *after* `rest-to-a2a` on the
   same API instance with `requestFlow: onResponse`. It runs on the response leg
   and can rewrite the body with arbitrary DataWeave. Notes:
   - It is a **native Java-backed filter shipped inside the gateway runtime**
     (`filterName: java:dw-body-transformation`, `extends:
     native-library-filter-v1-0-0`) — **not** a PDK wasm policy. It has no Rust
     source and **cannot be embedded inside this policy**; the operator attaches
     it as a separate policy in the API instance's policy chain.
   - One instance handles one direction. `requestFlow: onRequest` transforms the
     request; `requestFlow: onResponse` transforms the response. Attach twice to
     do both.
   - It runs in streaming mode and rewrites `Content-Type` / `Content-Length`.
   - Ordering matters: place it after `rest-to-a2a` so it sees the A2A result
     this policy surfaces (or the body `responseFields`/`responseMapping`
     produced).
   - Reference: <https://docs.mulesoft.com/gateway/latest/policies-included-dataweave-body-transformation>.

   This is the documented workaround for the construction constraint above — the
   in-policy DataWeave cannot build objects, but the operator-chained policy can.

### `customResponse` — raw vs shaped (master switch)

`customResponse` (default `false`) governs whether the response is shaped at all:

- **`false` (default) — raw passthrough.** The upstream A2A body is returned
  **verbatim**: the response body is never re-read or rewritten and the upstream
  headers (`content-type`, `content-length`) pass through unchanged. This is
  **byte-faithful** — unlike `responseMapping: "#[payload]"`, which round-trips
  through the policy's JSON shaping and re-serializes JSON numbers as doubles
  (`-32602` → `-32602.0`). Pinned by the
  `raw_response_is_default_and_byte_faithful` integration test. The continuation
  cache still runs in this mode (it reads the parsed result independently); only
  REST-body shaping is skipped.
- **`true` — shaped.** `responseFields` (if non-empty) takes precedence,
  otherwise `responseMapping` is evaluated. Both shaping properties are ignored
  when `customResponse` is off.

Implemented in `response_filter` via `PolicyConfig::uses_raw_response()` /
`uses_response_fields()`; the raw branch returns before any body read/rewrite.

## Distributed cache requires a multi-replica gateway

`distributed:true` selects the remote gossip `DataStorage` backend. In a
**single-replica Local Mode** gateway that backend has no peer to gossip with
and does not persist entries across requests, so cache continuation silently
does not carry ids forward — the next turn sends fresh. Distributed continuation
is only effective on a multi-replica deployment with shared gossip storage. The
default (`distributed:false`, local per-replica store) persists correctly in
Local Mode. Pinned by the
`distributed_cache_does_not_persist_in_single_replica_local_mode` integration
test.

## Request failure posture

- Prompt extraction null/empty/error → **fail-closed**: caller gets
  `requestErrorStatus` (default 400); upstream is not called.
- `customResponse:false` (default) → raw passthrough: the upstream body is
  returned verbatim; no shaping is attempted, so there is nothing to fail.
- Response DataWeave error (`responseMapping` path, shaping on) → **non-fatal**:
  the raw A2A body passes through and a warning is logged.
- `responseFields` selector that resolves to nothing → **non-fatal**: that field
  is set to `null`; other fields are unaffected.
