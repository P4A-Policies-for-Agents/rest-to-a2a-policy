# How To Use the REST → A2A Bridge Policy

A practical, scenario-driven guide to configuring the `rest-to-a2a` policy. It
covers the request and response sides, multi-turn continuation, and how to reach
for an operator-chained DataWeave policy when the in-policy response shaping is
not powerful enough.

For the protocol coverage map and runtime constraints, see
[`spec.md`](spec.md). For a fully worked OpenAPI + selector example, see
[`rest-api-example.md`](rest-api-example.md). For the property table, see the
[README](../README.md).

---

## 1. What the policy does

The policy sits on an API instance whose **upstream is an external A2A agent
(v1.0)**. On each inbound REST request it:

1. **Extracts the prompt** from the REST body with `promptSelector` (DataWeave).
   A null/empty/error result **fails closed** (`requestErrorStatus`, default 400)
   — the upstream is never called.
2. **Resolves continuation** (if enabled) to obtain a `taskId`/`contextId` to
   carry forward.
3. **Builds an A2A `SendMessage`** and rewrites the outbound body **in-band**,
   framed for the configured `upstreamBinding`.
4. On the response, **parses the A2A `Task`/`Message`**, updates the conversation
   cache, and **shapes the REST-facing body** (or returns it raw).

Streaming (SSE) is **not supported** — a `text/event-stream` upstream response is
passed through untouched with a warning.

---

## 2. Pick an upstream binding

| Binding | Operator sets `destinationPath` to | Policy sends |
|---|---|---|
| `jsonrpc` (default) | the A2A JSON-RPC endpoint | `{ "jsonrpc": "2.0", "id", "method": "SendMessage", "params": {…} }` |
| `httpjson` | `/message:send` | the bare A2A `params` payload, `content-type: application/json` |

The policy never rewrites `:path`; the upstream route is operator-owned. Choose
the binding your A2A agent speaks.

---

## 3. Configure the request side (prompt)

`promptSelector` is a DataWeave expression run against the inbound REST request.
It must return the **prompt string**.

```yaml
# Prompt lives at body.question
promptSelector: "#[payload.question]"

# Prompt lives in a nested field
promptSelector: "#[payload.input.text]"

# Prompt from a header
promptSelector: "#[attributes.headers['x-prompt']]"
```

A null/empty/error result rejects the request with `requestErrorStatus` before the
upstream is called (fail-closed). Bindings available to every selector: JSON
`payload` + request `attributes` (no `authentication`, no `vars`).

---

## 4. Configure multi-turn continuation

A2A tasks are multi-turn: an `input-required` reply must be answered with the same
`taskId`+`contextId`. The policy preserves that continuity via the `continuationMode`
setting.

### 4.1 Continuation modes

| Mode | Who supplies the ids | Cache used |
|---|---|---|
| `cache` (default) | the policy, keyed by `contextKeySelector` | yes |
| `explicit` | the client, via `taskIdSelector`/`contextIdSelector` | no |
| `none` | nobody (single-shot) | no |

**Cache mode** — the most common. A conversation key is hashed (SHA-256) into the
cache key; the raw value is never stored.

```yaml
continuationMode: cache
contextKeySelector: "#[payload.sessionId]"        # body field
# or from a header set by the channel:
# contextKeySelector: "#[attributes.headers['x-conversation-id']]"
distributed: false            # true only on a multi-replica gateway (see spec.md)
conversationTtlSeconds: 3600
```

Same `sessionId` resumes an `input-required` task; the caller never sees the A2A
`taskId`/`contextId`. The API Manager UI shows `contextKeySelector`, `distributed`,
and `conversationTtlSeconds` only when `continuationMode = cache`.

**Explicit mode** — the client manages the ids itself.

```yaml
continuationMode: explicit
taskIdSelector: "#[payload.taskId]"
contextIdSelector: "#[payload.contextId]"
```

The API Manager UI shows `taskIdSelector` and `contextIdSelector` only when
`continuationMode = explicit`. Both are required fields with default `#[null]`.
Return null (the default) for a fresh task/context.

**None mode** — stateless, one-shot calls.

```yaml
continuationMode: none
```

No cache, no ids carried, every call is independent.

---

## 5. Configure the response side

`responseType` (enum, default `raw`) controls how the response is shaped.

### 5.1 Raw passthrough (default)

```yaml
responseType: raw   # or simply omit it
```

The upstream A2A body is returned to the caller **verbatim** — byte-faithful, no
parse, no reshape, upstream headers preserved. This is the simplest option and the
only one that preserves JSON numbers exactly (e.g. an error `code` of `-32602`
stays an integer). Use it when the caller is happy to consume the raw A2A
`Task`/`Message`/error envelope.

### 5.2 Select a sub-tree with `responseMapping`

```yaml
responseType: mapping
responseMapping: "#[payload.task]"     # return just the Task object
```

`responseMapping` runs against the **raw A2A result object**. It is
**selection-only**: the gateway's embedded DataWeave rejects object construction
(`#[{ id: payload.task.id }]` and `%dw 2.0 … --- {…}` both fail and fall back to
raw passthrough). So you can pick a sub-tree (`#[payload.task]`,
`#[payload.task.status.state]`) but you cannot build a new shape here. On error
the raw body passes through (non-fatal). API Manager shows `responseMapping` only
when `responseType = mapping`.

### 5.3 Build a custom envelope with `responseFields`

To return a **bespoke flat/nested** body, set `responseType: fields` and list the
output fields. Each is an output `name` plus a dotted-path `selector` into the raw
A2A result; the policy assembles the object in Rust. API Manager shows
`responseFields` only when `responseType = fields`.

```yaml
responseType: fields
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

For an A2A `input-required` task this yields:

```json
{
  "conversationId": "ctx-7",
  "taskRef": "task-42",
  "status": "TASK_STATE_INPUT_REQUIRED",
  "reply": "Sure — what is your order number?"
}
```

Notes:
- A dotted `name` nests: `data.taskRef` → `{ "data": { "taskRef": … } }`.
- A leading `payload.` on a selector is ignored (so paths read like
  `responseMapping`).
- A path that resolves to nothing (missing key, out-of-range index) yields a
  `null` field — other fields are unaffected.
- The `selector` is a **plain JSON path, NOT DataWeave**. The gateway does not
  compile DataWeave nested inside array items, so a `#[...]` here would make the
  policy fail to load (HTTP 503). See [`spec.md`](spec.md).
- Conditionals and computed values are out of scope here — selectors are pure
  selection. For those, use a chained DataWeave policy (next section).

### 5.4 Decision guide

| You want | Use |
|---|---|
| The raw A2A body, byte-for-byte | `responseType: raw` (default) |
| A single sub-tree of the result | `responseType: mapping` + `responseMapping` |
| A flat/nested envelope from selected fields | `responseType: fields` + `responseFields` |
| Full DataWeave (conditionals, `map`, computed) | chained DataWeave policy → §6 |

---

## 6. Scenario: full DataWeave transformation of the response

When you need real DataWeave power on the response — conditionals, `map`/`filter`,
computed fields, building arbitrary objects/arrays — the in-policy options in §5
are not enough (the embedded DataWeave is selection-only, and `responseFields`
does pure selection). The answer is to **chain MuleSoft's built-in DataWeave Body
Transformation policy after `rest-to-a2a`** on the same API instance.

### 6.1 Why a separate policy

The **DataWeave Body Transformation** policy is a **native Java-backed filter
shipped inside the gateway runtime** (`filterName: java:dw-body-transformation`,
`extends: native-library-filter-v1-0-0`). It is **not** a PDK wasm policy — it has
no Rust source and **cannot be embedded inside `rest-to-a2a`**. The operator
attaches it as its own policy in the API instance's policy chain. It supports the
full DataWeave engine (not the restricted embedded evaluator used for policy
`dataweave`-format properties), so object construction works.

Reference:
<https://docs.mulesoft.com/gateway/latest/policies-included-dataweave-body-transformation>

### 6.2 How to wire it

1. On `rest-to-a2a`, set `responseType: raw` (the default) — or a light
   `responseMapping` selection with `responseType: mapping` — so the downstream
   policy receives the A2A result (or sub-tree) to transform. Letting it pass raw
   is usually cleanest: the transformation policy then sees the full upstream JSON.
2. Attach the **DataWeave Body Transformation** policy to the **same API
   instance**, **ordered after** `rest-to-a2a`, with `requestFlow: onResponse` so
   it runs on the response leg.
3. Author the transformation in that policy's DataWeave field.

Direction control: one instance handles one direction. `requestFlow: onRequest`
transforms the request; `requestFlow: onResponse` transforms the response. Attach
it twice if you need to transform both legs.

### 6.3 Example transformation (operator-authored, in the chained policy)

Given the raw A2A result this policy surfaces:

```json
{ "jsonrpc": "2.0", "id": 1, "result": { "task": {
    "id": "task-42", "contextId": "ctx-7",
    "status": { "state": "TASK_STATE_INPUT_REQUIRED",
                "update": { "role": "ROLE_AGENT",
                            "parts": [{ "text": "What is your order number?" }] } } } } }
```

a DataWeave Body Transformation (`requestFlow: onResponse`) can build a fully
custom response with logic the in-policy options cannot express:

```dataweave
%dw 2.0
output application/json
---
{
  conversationId: payload.result.task.contextId,
  done: payload.result.task.status.state as String
          startsWith "TASK_STATE_COMPLETED",
  needsInput: payload.result.task.status.state == "TASK_STATE_INPUT_REQUIRED",
  messages: payload.result.task.status.update.parts map ((p) -> p.text),
  // conditional / computed fields, defaults, etc. all work here
  reply: payload.result.task.status.update.parts[0].text default "(no reply)"
}
```

### 6.4 Caveats

- It runs in **streaming mode** and rewrites `Content-Type` / `Content-Length`.
- **Ordering matters**: place it after `rest-to-a2a` so it sees the body this
  policy surfaces. If `rest-to-a2a` already shaped the body (via `responseType:
  fields` or `responseType: mapping`), the DataWeave policy transforms *that*
  shape, not the raw A2A result.
- It is a separate policy the operator manages; it is not configured through
  `rest-to-a2a`'s schema.

---

## 7. End-to-end configuration examples

### 7.1 Stateful agent action, custom flat response (recommended default)

```yaml
upstreamBinding: jsonrpc
continuationMode: cache
contextKeySelector: "#[payload.sessionId]"
distributed: false
conversationTtlSeconds: 3600
promptSelector: "#[payload.question]"
metadataSelector: "#[{userId: payload.userId, sessionId: payload.sessionId}]"
responseType: fields
responseFields:
  - name: conversationId
    selector: task.contextId
  - name: taskRef
    selector: task.id
  - name: status
    selector: task.status.state
  - name: reply
    selector: task.status.update.parts[0].text
requestErrorStatus: 400
```

### 7.2 Stateless one-shot, raw A2A response

```yaml
upstreamBinding: jsonrpc
continuationMode: none
promptSelector: "#[payload.prompt]"
responseType: raw
requestErrorStatus: 400
```

### 7.3 HTTP+JSON binding, select the Task sub-tree

```yaml
upstreamBinding: httpjson          # operator sets destinationPath: /message:send
continuationMode: cache
contextKeySelector: "#[attributes.headers['x-conversation-id']]"
promptSelector: "#[payload.prompt]"
responseType: mapping
responseMapping: "#[payload.task]"
requestErrorStatus: 400
```

### 7.4 Client-managed continuation (explicit), raw response, then DataWeave-chain

```yaml
# rest-to-a2a: pass the raw A2A result downstream for full DataWeave transformation.
upstreamBinding: jsonrpc
continuationMode: explicit
taskIdSelector: "#[payload.taskId]"
contextIdSelector: "#[payload.contextId]"
promptSelector: "#[payload.prompt]"
responseType: raw
requestErrorStatus: 400
# Then attach DataWeave Body Transformation (requestFlow: onResponse) AFTER this
# policy on the same API instance — see §6.
```

---

## 8. Build, run, test

Use `make` (never raw `cargo`):

```bash
make build        # compile the wasm module
make run          # local Docker Omni Gateway playground (needs registration.yaml)
make test         # unit + integration tests
make test-unit    # unit only (no Docker)
```

See the [README](../README.md) for the full Make reference and publishing.
