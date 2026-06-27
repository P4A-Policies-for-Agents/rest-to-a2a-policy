# REST → A2A v1.0 Bridge — Protocol Coverage & Design

> Status: scaffold. This document is filled out alongside implementation. It is
> the authoritative coverage map; keep it in sync with the code.

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

- **`cache`** — `contextKeySelector` yields a conversation value → SHA-256 →
  cache key. On read, a continuable entry injects `contextId`+`taskId`. On
  response, continuable states upsert, terminal states evict. Gossip-safe
  (no DELETE-before-recreate; TTL eviction on remote).
- **`explicit`** — `taskIdSelector` / `contextIdSelector` provide the ids; the
  cache is never touched.
- **`none`** — single-shot.

## Request failure posture

- Prompt extraction null/empty/error → **fail-closed**: caller gets
  `requestErrorStatus` (default 400); upstream is not called.
- Response DataWeave error → **non-fatal**: the raw A2A body passes through and
  a warning is logged.
