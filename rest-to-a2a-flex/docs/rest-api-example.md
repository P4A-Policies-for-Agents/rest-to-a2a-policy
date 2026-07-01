# Example REST API & DataWeave Wiring

A worked example of putting the **REST → A2A v1.0 bridge** policy in front of a
REST agent action. It shows a concrete inbound REST contract, the A2A agent it
fronts, and the exact DataWeave selectors that map between them. The selectors
here are the ones exercised by the integration tests in
[`tests/requests.rs`](../tests/requests.rs) (`rest_spec_*`).

> The policy reads **nothing** from the API specification at runtime (see
> `docs/spec.md`). This document is a design contract for the operator — the
> selectors below are what the operator configures; the OpenAPI is what the
> caller (an Agentforce REST agent action) is coded against.

## Scenario

An Agentforce agent exposes a REST agent action `POST /v1/agent/ask`. The API
instance's upstream is an external A2A agent (v1.0). The policy turns each REST
call into an A2A `SendMessage` and shapes the A2A `Task`/`Message` reply back
into the REST response the agent action expects.

Multi-turn continuity is keyed on the caller-supplied `sessionId` (cache mode):
the same `sessionId` resumes an `input-required` task without the caller ever
seeing A2A `taskId`/`contextId`.

## REST contract (OpenAPI 3.0, excerpt)

```yaml
openapi: 3.0.3
info:
  title: Agent Ask API
  version: 1.0.0
paths:
  /v1/agent/ask:
    post:
      summary: Ask the downstream A2A agent a question.
      operationId: ask
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/AskRequest'
      responses:
        '200':
          description: Agent reply (may ask for more input).
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/AskResponse'
        '400':
          description: Prompt missing/invalid — request rejected, upstream not called.
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/ErrorResponse'
components:
  schemas:
    AskRequest:
      type: object
      required: [sessionId, question]
      properties:
        sessionId:
          type: string
          description: Stable conversation key. Same value resumes a task.
          example: "sess-abc-123"
        question:
          type: string
          description: The user's prompt, forwarded to the A2A agent.
          example: "What is the status of my order?"
        userId:
          type: string
          example: "user-789"
        metadata:
          type: object
          additionalProperties: true
          example: { "channel": "web", "locale": "en-US" }
    AskResponse:
      # The response IS the A2A v1.0 Task object itself — the JSON-RPC `result`
      # unwrapped, returned as-is by `mappingConfig.responseMapping: #[payload]`
      # (or shaped with `fieldsConfig.responseFields`). Paths are relative to the
      # Task; there is no `task`/`message` wrapper. See the note under "DataWeave
      # selectors explained" on why the mapping is a *selection*, not a
      # constructed object.
      type: object
      properties:
        id:
          type: string
          description: A2A taskId of the live/last task.
          example: "task-42"
        contextId:
          type: string
          description: A2A contextId, surfaced so clients can correlate turns.
          example: "ctx-7"
        status:
          type: object
          description: A2A task status — carries the state and the agent's latest message.
          properties:
            state:
              type: string
              description: A2A task state (proto-JSON, e.g. TASK_STATE_INPUT_REQUIRED).
              example: "TASK_STATE_INPUT_REQUIRED"
            message:
              type: object
              description: The agent's latest message (role + parts) on an input-required task.
        artifacts:
          type: array
          description: Agent output on a completed task; text at artifacts[0].parts[0].text.
    ErrorResponse:
      type: object
      properties:
        error:
          type: object
          properties:
            message: { type: string }
            source: { type: string, example: "rest-to-a2a" }
```

### Example request

```http
POST /v1/agent/ask HTTP/1.1
Content-Type: application/json

{
  "sessionId": "sess-abc-123",
  "userId": "user-789",
  "question": "What is the status of my order?",
  "metadata": { "channel": "web", "locale": "en-US" }
}
```

### Example response (task asks for more input)

The body is the A2A Task object itself (the JSON-RPC `result` unwrapped):

```json
{
  "id": "task-42",
  "contextId": "ctx-7",
  "status": {
    "state": "TASK_STATE_INPUT_REQUIRED",
    "message": {
      "role": "ROLE_AGENT",
      "parts": [{ "text": "Sure — what is your order number?" }]
    }
  }
}
```

## Policy configuration (gcl.yaml values)

```yaml
upstreamBinding: jsonrpc
continuationMode: cache
cacheConfig:
  contextKeySelector: "#[payload.sessionId]"
  distributed: false
  conversationTtlSeconds: 3600
requestErrorStatus: 400
promptSelector: "#[payload.question]"
responseType: mapping
mappingConfig:
  responseMapping: "#[payload]"
metadataSelector: "#[{userId: payload.userId, channel: payload.metadata.channel}]"
```

### DataWeave selectors explained

| Selector | Expression | Runs against | Produces |
|---|---|---|---|
| `promptSelector` | `#[payload.question]` | inbound REST body | the prompt string for the A2A message part. Null/empty ⇒ **fail-closed 400**. |
| `contextKeySelector` (in `cacheConfig`) | `#[payload.sessionId]` | inbound REST body | the conversation value → SHA-256 → cache key. Same `sessionId` resumes the task. |
| `metadataSelector` | `#[{userId: payload.userId, channel: payload.metadata.channel}]` | inbound REST body | object of key/value pairs attached to the A2A message as `metadata`. A null/non-object result attaches nothing. |
| `responseMapping` (in `mappingConfig`) | `#[payload]` | **raw A2A result** (the Task itself) | the `AskResponse` body — the whole Task object. A sub-tree selection like `#[payload.artifacts[0].parts[0].text]` returns just that value. |

The response mapping binds `payload` to the raw A2A result — the Task (or
Message) itself, i.e. the JSON-RPC `result` unwrapped (no `task`/`message`
wrapper) — and **selects** a sub-tree as the REST response. A mapping error is
**non-fatal** — the raw A2A body passes through with a warning.

> **Selection only — object construction is not supported.** The Flex Gateway
> 1.12.1 embedded DataWeave used for policy `dataweave`-format properties
> evaluates **selectors** (`#[payload]`, `#[payload.artifacts[0].parts[0].text]`,
> `#[payload.status.state]`) but **rejects object/array construction**
> (`#[{ a: payload.x }]`, the full `%dw 2.0 … --- {…}` script form, and
> `default`/index-chain reshaping). A construction expression silently fails at
> eval and the policy falls back to non-fatal raw passthrough (the unmodified
> upstream body is returned). This was verified end-to-end against the live
> runtime (see the `rest_spec_*` and
> `response_mapping_failure_passes_raw_body_through` integration tests). To
> reshape into a flat custom envelope (`conversationId`/`taskRef`/`reply`), use
> `responseFields` (below) — `responseMapping` itself is restricted to selecting
> a sub-tree of the A2A result.

### Custom envelope with `responseFields`

To return a bespoke flat (or nested) shape instead of the raw Task, set
`responseType: fields` and use `responseFields` (in the `fieldsConfig` object).
Each entry is an output `name` plus a **dotted JSON-path** `selector` into the
raw A2A result (relative to the Task — no `task`/`message` prefix); the policy
assembles the object in Rust.

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
      selector: status.message.parts[0].text   # input-required follow-up
      # completed-task output lives under artifacts instead:
      # selector: artifacts[0].parts[0].text
```

For the same A2A `input-required` task this returns the flat `AskResponse`:

```json
{ "conversationId": "ctx-7", "taskRef": "task-42",
  "status": "TASK_STATE_INPUT_REQUIRED", "reply": "Sure — what is your order number?" }
```

> **Why a path, not DataWeave.** The per-field `selector` is a plain JSON path,
> NOT a `#[...]` DataWeave expression. The gateway's `dw2pel` config transform
> compiles only top-level `format: dataweave` properties; it does not recurse
> into DataWeave nested inside array items, so a `#[...]` here reaches the policy
> uncompiled and the whole policy fails to load (HTTP 503,
> `invalid type: string …, expected PEL Expression`). Paths support array
> indices (`parts[0]`), an optional leading `payload.` is ignored, and a path
> that resolves to nothing yields a `null` field. A dotted output `name`
> (`data.taskRef`) nests. Verified by the `response_fields_*` integration tests.

> **Header-keyed variant.** If the conversation key lives in a header instead of
> the body (e.g. an `x-conversation-id` set by the channel), use
> `contextKeySelector: "#[attributes.headers['x-conversation-id']]"`. The rest
> of the wiring is unchanged.

## A2A upstream payloads (for reference)

What the policy sends (JSON-RPC binding, fresh turn):

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "SendMessage",
  "params": {
    "message": {
      "messageId": "<sha256-prefix>",
      "role": "ROLE_USER",
      "parts": [{ "text": "What is the status of my order?" }]
    }
  }
}
```

On the next turn for the same `sessionId`, the policy injects the cached ids
into `params.message.taskId` / `params.message.contextId`.

What the upstream returns (input-required task). The A2A Task **is** the
JSON-RPC `result` — there is no `result.task` wrapper:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "id": "task-42",
    "contextId": "ctx-7",
    "status": {
      "state": "TASK_STATE_INPUT_REQUIRED",
      "message": { "role": "ROLE_AGENT", "parts": [{ "text": "Sure — what is your order number?" }] }
    }
  }
}
```
