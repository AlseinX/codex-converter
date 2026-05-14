# codex-conv Design Document

**Date:** 2026-05-13
**Status:** Draft

## Overview

`codex-conv` is a reverse proxy that converts between OpenAI Responses API (used by Codex CLI) and Anthropic Messages API. It enables using any Anthropic-compatible backend with Codex CLI without modification.

### Core Principle: Perfect Forwarding

The proxy must act as a **transparent protocol converter** — only translating between two API formats. It must not:

- **Fabricate data:** Never inject values that don't come from upstream or downstream (no hardcoded token counts, no synthetic fields)
- **Drop data silently:** Every field from the downstream request must either be forwarded (mapped to Anthropic equivalent) or explicitly documented as unsupported with a verified rationale explaining why Anthropic has no equivalent
- **Override client intent:** Never replace a client-specified value with a proxy-decided value unless required by protocol format differences (e.g., `stream` forced to `true` because the proxy architecture requires SSE internally)
- **Invent behavior:** Never add custom logic beyond what either API defines (no custom threshold tables, no synthetic mappings, no heuristic decisions)

When a field exists in both APIs, forward it directly. When a field exists in only one API, document it with a verified explanation. When in doubt, forward.

## Architecture

### High-Level

```
Codex CLI  --[HTTP POST + SSE]-->  codex-conv  --[HTTP POST + SSE]-->  Anthropic API
           <--[SSE events]--                   <--[SSE events]--
```

- **Stateless across requests:** No session state, no `previous_response_id` handling
- **One-to-one request mapping:** Each Responses API request converts to exactly one Anthropic Messages API request
- **One task per connection:** Each request handled by a single tokio task
- **Fully parallel:** No per-session queues needed
- **Real-time SSE conversion:** Events are converted and forwarded immediately, not buffered

### Components

| Component | Responsibility |
|---|---|
| **URL Router** | Parse upstream base URL from request path |
| **Config Layer** | Three-way config injection (YAML / env vars / CLI flags), all with defaults |
| **Conversion Task** | Single-request state machine driving the entire conversion lifecycle |
| **TLS Layer** | Downstream configurable certs, upstream custom root certs + HTTP proxy |
| **Logging** | tokio tracing with async appender |

### Conversion Task State Machine

The core of the system. A single state machine per request:

```
Receive complete request
        │
        ▼
Parse Responses API request
        │
        ▼
Build namespace registry + ID maps
        │
        ▼
Convert to Anthropic Messages API request
        │
        ▼
Forward to upstream (stream: true)
        │
        ▼
SSE pipeline (event-by-event):
  Anthropic SSE event arrives
        │
        ▼ (real-time conversion)
  Convert to Responses API SSE event
        │
        ▼
  Immediately write to downstream
        │
        ▼
  Repeat until stream ends
        │
        ▼
Send response.completed
        │
        ▼
Task ends, all state released
```

In-flight state maintained per request:
- Delta accumulators (for done events)
- Tool call mappings: `content_block_index → (tool_use_id, call_id, name)`
- ID map: `toolu_xxx` ↔ `call_xxx` bidirectional
- Namespace registry: `flat_name → (namespace, tool_name)`
- Index mapping: Anthropic `content_block.index` → Responses API `output_index` + `content_index`

#### Thinking/Reasoning Round-Trip

The Anthropic API returns encrypted signatures for thinking blocks during streaming. The proxy handles the round-trip as follows:

1. **Streaming response (Anthropic → Responses API):** `signature_delta` events are consumed and discarded — the proxy does not accumulate or persist signature data. The `reasoning_text.done` event carries the accumulated reasoning text.

2. **Subsequent request (Responses API → Anthropic):** When a `reasoning` input item appears (from a previous response), it is converted to a `redacted_thinking` content block:
   ```json
   {"type": "redacted_thinking"}
   ```
   This is Anthropic's opaque marker type — it intentionally carries **no data field, no signature, no thinking text**. It simply signals to Anthropic that thinking occurred in a prior turn. This is the correct approach for a stateless proxy: the proxy does not need to store signatures between requests, and `redacted_thinking` does not require them.

## URL Routing

### Format

Codex base URL: `https://my.domain/https/api.anthropic.com`

Codex appends `responses`, sends to: `https://my.domain/https/api.anthropic.com/responses`

Protocol normalization (all accepted):
- `/https/api.anthropic.com/responses`
- `/https:/api.anthropic.com/responses`
- `/https://api.anthropic.com/responses`

Proxy extracts:
- Upstream base URL: `https://api.anthropic.com` (no `/v1`, follows Anthropic convention)
- Upstream endpoint: `/v1/messages` (proxy appends, not configurable)

### Hostname Validation (SSRF Protection)

The extracted upstream hostname is validated to prevent SSRF attacks. The following are rejected with HTTP 400:

- Private IPv4 ranges: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `127.0.0.0/8`
- Loopback IPv6: `::1`
- Hostname `localhost`
- Literal IP addresses in any of the above ranges

Validation occurs after URL parsing but before connecting upstream. Optionally bypassed via `server.allowed_upstreams` config (for internal testing):

| Path | Type | Default | Description |
|---|---|---|---|
| `server.allowed_upstreams` | string[] | `[]` | Hostnames exempt from SSRF validation |

### Key/Model Passthrough

- **API Key Extraction:** From downstream `Authorization` header. Strip `Bearer ` prefix if present; use raw value if no prefix. Return HTTP 401 if header is missing or empty
- **API Key Forwarding:** Sent as upstream `x-api-key` header (and `anthropic-version` header, see Upstream HTTP Headers)
- **Model ID:** Request body `model` field passed through unchanged

### Rationale for no `/v1` in base URL

All Anthropic-compatible providers use base URLs without `/v1`:
- Anthropic official: `https://api.anthropic.com`
- Zhipu: `https://open.bigmodel.cn/api/anthropic`
- OpenRouter: `https://openrouter.ai/api`

The Anthropic SDK appends `/v1/messages` to whatever base URL is provided.

## Protocol Conversion

### Request Conversion (Responses API → Anthropic Messages API)

#### Top-level Parameters

| Responses API | Anthropic | Notes |
|---|---|---|
| `model` | `model` | Passthrough |
| `stream` | `stream` | **Always `true`** — proxy requires streaming from upstream to perform real-time SSE event conversion. Downstream always receives SSE regardless of this value. If downstream sends `stream: false`, proxy still streams upstream and returns the completed response as a single SSE stream |
| `instructions` | `system` | Merged with system items from input |
| `tools` | `tools` | Namespace flattening + registry |
| `tool_choice` | `tool_choice` | See mapping table |
| `parallel_tool_calls` | `disable_parallel_tool_use` | Semantics inverted |
| `reasoning.effort` | `thinking` + `output_config.effort` | Direct string forwarding, see mapping section |
| `temperature` | `temperature` | Passthrough |
| `top_p` | `top_p` | Passthrough |
| `max_output_tokens` | `max_tokens` | Field name differs |
| `metadata` | `metadata` | Forward `user_id`, strip other keys |
| `service_tier` | `service_tier` | Value mapping, see Unsupported Features section |

#### Upstream HTTP Headers

All requests to the Anthropic API include these HTTP headers:

| Header | Value | Source |
|---|---|---|
| `x-api-key` | API key | From downstream `Authorization` header (see Key Extraction below) |
| `anthropic-version` | `2023-06-01` | Configurable via `upstream.anthropic_version`, defaults to `2023-06-01` |
| `content-type` | `application/json` | Fixed |

#### `tool_choice` Mapping

| Responses API | Anthropic |
|---|---|
| `"auto"` | `{"type": "auto"}` |
| `"required"` | `{"type": "any"}` |
| `"none"` | `{"type": "none"}` |
| `{"type":"function","name":"x"}` | `{"type":"tool","name":"x"}` |

When `parallel_tool_calls: false`, the `disable_parallel_tool_use: true` field is placed **inside** the `tool_choice` object:

| Responses API | Anthropic |
|---|---|
| `"auto"` + `parallel_tool_calls: false` | `{"type": "auto", "disable_parallel_tool_use": true}` |
| `"required"` + `parallel_tool_calls: false` | `{"type": "any", "disable_parallel_tool_use": true}` |
| `{"type":"function","name":"x"}` + `parallel_tool_calls: false` | `{"type":"tool","name":"x","disable_parallel_tool_use": true}` |
| `"none"` + `parallel_tool_calls: false` | `{"type": "none"}` (no change, no tools to parallelize) |

#### `parallel_tool_calls` Mapping

`disable_parallel_tool_use` is embedded within the `tool_choice` object, not a separate top-level field.

| Responses API | Anthropic |
|---|---|
| `parallel_tool_calls: true` | Omit `disable_parallel_tool_use` from `tool_choice` object (default allows) |
| `parallel_tool_calls: false` | Add `disable_parallel_tool_use: true` to `tool_choice` object |

#### `reasoning` → `thinking` + `output_config.effort` Mapping

Anthropic Claude 4.6+ supports `output_config.effort` — a direct string parameter analogous to OpenAI's `reasoning.effort`. The proxy forwards effort values directly without custom integer conversion.

**When `reasoning` is present (effort specified):**

```json
// Responses API
{"reasoning": {"effort": "high"}}
```

```json
// Anthropic
{"thinking": {"type": "adaptive"}, "output_config": {"effort": "high"}}
```

- `reasoning.effort` → `output_config.effort`: direct string forwarding
- When reasoning present: set `thinking.type: "adaptive"` (Anthropic's modern thinking mode)
- `reasoning.effort` absent or `null` → omit both `thinking` and `output_config` (Anthropic defaults)

**Value mapping:**

| Responses API `reasoning.effort` | Anthropic `output_config.effort` | Notes |
|---|---|---|
| `"low"` | `"low"` | Direct match |
| `"medium"` | `"medium"` | Direct match |
| `"high"` | `"high"` | Direct match |
| `"xhigh"` | `"xhigh"` | Direct match (Opus 4.7+ / gpt-5.5) |
| `"none"` | Omit `thinking` and `output_config` | OpenAI defines this as "no reasoning" — disable thinking entirely |
| `"minimal"` | `"low"` | No Anthropic `minimal`; map to closest value |

**Why `adaptive` + `effort` instead of `budget_tokens`:**
- `budget_tokens` is **deprecated** on Claude Opus 4.6/Sonnet 4.6 and **rejected** (400 error) on Opus 4.7
- `output_config.effort` is the modern, GA parameter supported on all current Claude models
- Direct string forwarding preserves semantics without custom integer thresholds

#### `system` Parameter Construction

Merge order:
1. `instructions` field content
2. Input items with `role: "system"`

The `system` parameter is always emitted as an **array of content blocks**, even when there is only one text entry:

```json
{"system": [{"type": "text", "text": "You are a helpful assistant."}]}
```

The `instructions` string is wrapped: `"instructions text"` → `[{"type": "text", "text": "instructions text"}]`.

Omit `system` parameter entirely if both `instructions` and system input items are empty.

#### Input Items → Messages

| Responses API input item | Anthropic message |
|---|---|
| `{"type":"message","role":"user","content":[...]}` | `{"role":"user","content":[...]}` |
| `{"type":"message","role":"system","content":[...]}` | Extracted to top-level `system` |
| `{"type":"message","role":"assistant","content":[...]}` | `{"role":"assistant","content":[...]}` |
| `{"type":"function_call","call_id":"...","name":"x","arguments":"{...}"}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_xxx","name":"x","input":{...}"}]}` |
| `{"type":"function_call_output","call_id":"...","output":"..."}` | `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_xxx","content":"..."}]}` |
| `{"type":"reasoning","content":[{"type":"output_text","text":"..."}]}` | `{"role":"assistant","content":[{"type":"redacted_thinking"}]}` |

Key details:
- **ID mapping:** `call_id` ↔ `tool_use.id`. Proxy generates `toolu_` prefixed IDs, maintains bidirectional map
- **arguments → input:** JSON string → parsed JSON object. Parse failure → reject entire request
- **namespace restoration:** `function_call` with `namespace` + `name` → Anthropic `tool_use.name` = `mcp__{server}__{tool}`. Look up registry for original namespace and name
- **tool_result.content:** `function_call_output.output` (always a string) → Anthropic `tool_result.content` as plain string. Anthropic accepts both string and content blocks array; we always use the string form for simplicity and correctness
- **Anthropic message alternation:** Ensure user/assistant messages strictly alternate. Consecutive `function_call` items merged into same assistant message content array. Consecutive `function_call_output` items merged into same user message content array

#### User Content Block Mapping

| Responses API UserContent | Anthropic content block |
|---|---|
| `{"type":"input_text","text":"..."}` | `{"type":"text","text":"..."}` |
| `{"type":"input_image","image_url":"..."}` | `{"type":"image","source":{"type":"url","url":"..."}}` |
| `{"type":"input_image","image_url":{"url":"data:image/png;base64,..."}}` | `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}` |

#### `cache_control` Handling (Reference: LiteLLM)

Codex may send `cache_control: {"type": "ephemeral"}` on content blocks. Preserve these markers on corresponding Anthropic content blocks (Anthropic natively supports `cache_control`).

#### Tool Name Length Limits

Flattened namespace tool names may exceed 64 chars (Anthropic limit). Apply Codex's `sanitize_responses_api_tool_name` logic: truncate + hash suffix for uniqueness.

### Response Conversion (Anthropic → Responses API)

#### Content Blocks → Output Items

| Anthropic content block | Responses API output item |
|---|---|
| `{"type":"text","text":"..."}` | `{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"...","annotations":[]}]}` |
| `{"type":"thinking","thinking":"..."}` | `{"type":"reasoning","id":"rs_xxx","content":[{"type":"output_text","text":"..."}]}` |
| `{"type":"tool_use","id":"toolu_xxx","name":"mcp__svr__tool","input":{...}}` | `{"type":"function_call","id":"fc_xxx","call_id":"call_xxx","name":"tool","namespace":"mcp__svr__","arguments":"{...}"}` |

tool_use → function_call details:
- `input` (JSON object) → `arguments` (JSON string)
- `id` (`toolu_xxx`) → `call_id` (`call_xxx`), maintain ID map
- `name` looked up in registry: `mcp__svr__tool` → `namespace: "mcp__svr__"`, `name: "tool"`
- Tools not found in registry → plain function_call without `namespace` field
- Proxy generates `id` with `fc_` prefix for each item

#### Stop Reason → Status

| Anthropic `stop_reason` | Responses API `status` |
|---|---|
| `end_turn` | `"completed"` |
| `max_tokens` | `"incomplete"`, `incomplete_details.reason: "max_output_tokens"` |
| `stop_sequence` | `"completed"` |
| `tool_use` | `"completed"` (output contains function_call items) |
| `pause_turn` | `"completed"` |

#### Usage Mapping

| Anthropic | Responses API |
|---|---|
| `usage.input_tokens` | `usage.input_tokens` |
| `usage.output_tokens` | `usage.output_tokens` |
| `usage.cache_creation_input_tokens` | `usage.input_tokens_details.cached_tokens` (summed with cache_read) |
| `usage.cache_read_input_tokens` | `usage.input_tokens_details.cached_tokens` (summed with cache_creation) |

Calculation: `cached_tokens = cache_creation_input_tokens + cache_read_input_tokens`

Note: `output_tokens_details.reasoning_tokens` is omitted from response — Anthropic has no equivalent field, so we do not fabricate values.

### Streaming Conversion (Event-by-Event, Real-Time)

Event sequence (Anthropic → Responses API):

```
message_start
  → response.created
  → response.in_progress

content_block_start (index=0, type=text)
  → response.output_item.added
  → response.content_part.added

content_block_delta (index=0, type=text_delta)
  → response.output_text.delta                     (repeated)

content_block_stop (index=0)
  → response.output_text.done                      (accumulated text)
  → response.content_part.done
  → response.output_item.done

content_block_start (index=1, type=thinking)
  → response.output_item.added (type=reasoning, content=[])

content_block_delta (index=1, type=thinking_delta)
  → response.reasoning_text.delta                  (repeated, content_index)

content_block_delta (index=1, type=signature_delta)
  → Consumed and discarded (no Responses API event)

content_block_stop (index=1)
  → response.reasoning_text.done                   (accumulated text, content_index)
  → response.output_item.done

content_block_start (index=2, type=tool_use)
  → response.output_item.added (type=function_call)
  → Record tool_use index → call_id mapping

content_block_delta (index=2, type=input_json_delta)
  → response.function_call_arguments.delta         (repeated)

content_block_stop (index=2)
  → response.function_call_arguments.done           (accumulated arguments)
  → response.output_item.done                       (complete function_call)

message_delta (stop_reason, usage)
  → Record stop_reason and usage to internal state

message_stop
  → response.completed                              (with status, usage, output summary)
```

Multiple tool_use blocks in one response: each converted to independent `function_call` output item with its own `output_index`.

#### Response ID Handling

Response IDs are passed through from Anthropic where possible:

| Output item type | ID source | Format |
|---|---|---|
| Response | Anthropic `message_start.message.id` | Passthrough as-is (e.g., `msg_01XFDUDYJgAACzvnptvVoYEL`) |
| Message output | Same as response | Passthrough |
| Reasoning output | Proxy-generated | `rs_{uuid_v4}` |
| Function call output | Proxy-generated | `fc_{sequential}` |

Codex CLI treats all IDs as opaque strings with no format validation. Reference: CLIProxyAPI passes Anthropic `msg_...` IDs directly.

#### SSE Wire Format Examples

Each SSE event consists of an `event:` line and a `data:` line with JSON payload. Below are representative examples:

**Text streaming (Anthropic → Responses API):**
```
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","status":"in_progress","content":[]}}

event: response.content_part.added
data: {"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hello"}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: response.output_text.done
data: {"type":"response.output_text.done","output_index":0,"content_index":0,"text":"Hello world"}

event: response.content_part.done
data: {"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello world","annotations":[]}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Hello world","annotations":[]}]}}
```

**Thinking streaming (Anthropic thinking → Responses API raw reasoning text):**
```
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_001","content":[]}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Let me analyze..."}}

event: response.reasoning_text.delta
data: {"type":"response.reasoning_text.delta","output_index":1,"content_index":0,"delta":"Let me analyze..."}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"ErUB..."}}

(No Responses API event emitted — signature consumed and discarded)

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: response.reasoning_text.done
data: {"type":"response.reasoning_text.done","output_index":1,"content_index":0,"text":"Let me analyze..."}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_001","content":[{"type":"output_text","text":"Let me analyze..."}]}}
```

**Tool use streaming:**
```
event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"mcp__svr__tool","input":{}}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_002","call_id":"call_002","name":"tool","namespace":"mcp__svr__","status":"in_progress","arguments":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc_002","call_id":"call_002","delta":"{\"key\":"}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","output_index":2,"item_id":"fc_002","call_id":"call_002","arguments":"{\"key\":\"value\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_002","call_id":"call_002","name":"tool","namespace":"mcp__svr__","status":"completed","arguments":"{\"key\":\"value\"}"}}
```

**Stream end:**
```
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_xxx","status":"completed","output":[...],"usage":{"input_tokens":100,"output_tokens":15,"input_tokens_details":{"cached_tokens":0}}}}
```

## Unsupported Responses API Features

The following Responses API features have no Anthropic equivalent and are handled as follows:

| Feature | Behavior | Rationale |
|---|---|---|
| `previous_response_id` | Ignored (stripped) | Codex always sends full `input` history; this field is always `None` in HTTP requests. Anthropic has no server-side conversation state |
| `store` | Ignored (stripped) | Server-side response storage. Anthropic API is stateless, every request includes full history |
| `include` | Ignored (stripped) | Requests encrypted reasoning content. Anthropic returns thinking content inline via `thinking` blocks, not via `include` |
| `truncation` | Ignored (stripped) | Input context truncation control. Codex never sends this field. Anthropic naturally errors on context overflow (equivalent to `"disabled"` behavior) |
| `n` | Ignored (forced `1`) | Not in Responses API spec (removed from Chat Completions). Codex never sends it. Anthropic always returns single response |
| Built-in tools (`web_search`, `file_search`, `code_interpreter`) | Rejected with 400 error | Only function-type tools (including MCP namespace tools) are supported |

#### Forwarded Fields (with mapping)

The following fields are forwarded to Anthropic with value mapping where needed:

**`metadata`:** Direct forwarding of `metadata.user_id`. Both APIs accept `{"metadata": {"user_id": "..."}}`. Extra keys beyond `user_id` are stripped (Anthropic only supports `user_id`).

| Responses API | Anthropic | Notes |
|---|---|---|
| `metadata.user_id` | `metadata.user_id` | Direct passthrough |
| `metadata.*` (other keys) | Stripped | Anthropic only supports `user_id` |

**`service_tier`:** Forwarded with value mapping where Anthropic has an equivalent. Codex CLI sends `"priority"` or `"flex"`.

| Responses API | Anthropic | Notes |
|---|---|---|
| `"auto"` | `"auto"` | Direct match |
| `"default"` | `"standard_only"` | Anthropic uses different name |
| `"flex"` | Omit (no Anthropic equivalent) | Anthropic has no flex tier; request proceeds without tier specification |
| `"priority"` | Omit (no Anthropic equivalent) | Anthropic priority is per-account, not per-request; request proceeds without tier specification |

## MCP Namespace Handling

### Problem

Codex sends MCP tools as `{"type": "namespace", "name": "mcp__memory__", "tools": [...]}`. Anthropic has no namespace concept. Codex routes tool calls by the `namespace` field in `function_call` response items — not by tool name prefix.

### Solution

**Delimiter:** `__` (matches Codex convention: `MCP_TOOL_NAME_DELIMITER = "__"`)

**Registry-based approach:** No heuristics, no string splitting. Registry is the only authority.

1. **Request phase (flatten):**
   - Parse `namespace` tools from Responses API request
   - Flatten each child tool into Anthropic function with name `mcp__{server}__{tool}`
   - Build registry: `Map<flat_name, (namespace, tool_name)>`
   - Registry lifetime: within the single request-response cycle

2. **Response phase (unflatten):**
   - Anthropic returns `tool_use` with name `mcp__{server}__{tool}`
   - Look up registry → get `(namespace: "mcp__server__", tool_name: "tool")`
   - Emit `function_call` with `namespace` and `name` as separate fields
   - Registry miss → emit as plain `function_call` without `namespace`

### Tool Name Sanitization

Anthropic limits tool names to `^[a-zA-Z0-9_-]{1,64}$`. When flattened names exceed 64 chars:
- Truncate + append hash suffix (12 hex chars) for uniqueness
- Register both original and truncated names in registry
- **Collision detection:** After truncation, check if the resulting name already exists in the registry. If collision detected, hash the name with an incrementing counter (`SHA-1(name + ":" + counter)`) to produce a different hash suffix. Retry up to 8 times. If still colliding, reject the request with 400 error (`too_many_namespaced_tools`)

## Configuration System

### Configuration Items

All items use `.` separated paths. All have default values. `-c` config file is optional.

**Server:**

| Path | Type | Default | Description |
|---|---|---|---|
| `server.listen` | string | `"0.0.0.0:8080"` | Listen address (host:port) |
| `server.tls.cert` | string | `""` | Downstream TLS cert path, empty = HTTP |
| `server.tls.key` | string | `""` | Downstream TLS private key path |
| `server.shutdown_timeout` | u64 | `30` | Graceful shutdown max wait (seconds) |

**Upstream Client:**

| Path | Type | Default | Description |
|---|---|---|---|
| `upstream.tls.extra_ca_certs` | string[] | `[]` | Additional CA cert paths to trust |
| `upstream.tls.use_system_roots` | bool | `true` | Trust system root certificates |
| `upstream.proxy` | string | `""` | HTTP proxy address (CONNECT), empty = direct |
| `upstream.anthropic_version` | string | `"2023-06-01"` | Anthropic API version header value |

**Logging:**

| Path | Type | Default | Description |
|---|---|---|---|
| `log.console` | `Option<ConsoleLog>` | `Some(defaults)` | Console logging |
| `log.console.level` | string | `"info"` | Console log level |
| `log.file` | `Option<FileLog>` | `None` | File logging |
| `log.file.level` | string | `"debug"` | File log level |
| `log.file.dir` | string | `"./logs"` | Log file directory (created if missing) |
| `log.file.rotation` | string | `"daily"` | Rotation: daily/hourly/never |

### Option Semantics

- `false`, `0`, `null` → `None` (disabled)
- `true`, `1` → `Some(defaults)` (enabled with defaults)
- Specifying any sub-field → `Some`, unspecified sub-fields use defaults
- `log.console` defaults to `Some(defaults)` (enabled)
- `log.file` defaults to `None` (disabled)

### Three-Way Injection

Priority (high to low):
1. **CLI flag:** `-C path.key=value` (repeatable)
2. **Environment variable:** `CODEX_CONV_<PATH_KEY>` (uppercase, `.` → `_`)
3. **Config file:** `-c config.yaml`

```bash
# Zero config - just works
codex-conv

# With config file
codex-conv -c /etc/codex-conv/config.yaml

# Override via env var
CODEX_CONV_SERVER_LISTEN=0.0.0.0:9090 codex-conv

# Override via CLI flag
codex-conv -C server.listen=0.0.0.0:9090 -C log.file=true
```

### CLI Parameters

```
codex-conv [OPTIONS]

Options:
  -c, --config-file <PATH>       Config file path (optional)
  -C, --config-item <KEY=VALUE>  Override config item (repeatable)
  -h, --help                     Show help
  -V, --version                  Show version
```

## Error Handling

### HTTP Status Code Mapping

| Anthropic | Responses API |
|---|---|
| 400 | 400 |
| 401 | 401 |
| 403 | 403 |
| 404 | 404 |
| 413 | 400 |
| 429 | 429 |
| 500 | 500 |
| 529 | 503 |
| Unknown 4XX | 400 |
| Unknown 5XX | 500 |

### Error Body Structure

Anthropic:
```json
{
  "type": "error",
  "error": {
    "type": "invalid_request_error",
    "message": "..."
  },
  "request_id": "req_xxx"
}
```

Responses API:
```json
{
  "error": {
    "message": "...",
    "type": "invalid_request_error",
    "param": null,
    "code": "invalid_request"
  }
}
```

- **Structure:** Strictly Responses API error format
- **message:** Passthrough from Anthropic, unmodified
- **type/code:** Mapped per table below
- **param:** Always `null` (Anthropic has no equivalent)
- **request_id:** Logged but not returned in response (Responses API has no matching field)

### Error Type Mapping

| Anthropic `error.type` | Responses API `error.type` | `error.code` |
|---|---|---|
| `invalid_request_error` | `invalid_request_error` | `invalid_request` |
| `authentication_error` | `invalid_request_error` | `invalid_api_key` |
| `permission_error` | `invalid_request_error` | `invalid_api_key` |
| `not_found_error` | `invalid_request_error` | `model_not_found` |
| `request_too_large` | `invalid_request_error` | `request_too_large` |
| `rate_limit_error` | `invalid_request_error` | `rate_limit_exceeded` |
| `api_error` | `server_error` | `server_error` |
| `overloaded_error` | `server_error` | `server_error` |
| Unknown 4XX | `invalid_request_error` | `invalid_request` |
| Unknown 5XX | `server_error` | `server_error` |

### Streaming Error Handling

When Anthropic sends `event: error` during streaming, proxy emits in order:

1. `error` event (with converted error structure)
2. `response.failed` event (with full response object, `status: "failed"`)
3. `[DONE]` terminal marker

Streaming errors are not recoverable. Stream terminates after the sequence.

### Proxy-Originated Errors

| Scenario | HTTP | type | code |
|---|---|---|---|
| URL routing parse failure | 400 | `invalid_request_error` | `invalid_request` |
| Request body JSON parse failure | 400 | `invalid_request_error` | `invalid_request` |
| Upstream connection failure | 502 | `server_error` | `upstream_connection_failed` |
| Upstream response format invalid | 502 | `server_error` | `upstream_invalid_response` |
| Namespace registry miss | 400 | `invalid_request_error` | `unknown_tool` |
| Responses API field validation failure | 400 | `invalid_request_error` | `invalid_request` |

## TLS

### Downstream (Proxy → Codex)

- HTTP by default
- HTTPS via config: `server.tls.cert` + `server.tls.key`
- Empty cert/key = HTTP mode

### Upstream (Proxy → Anthropic)

- Supports HTTP and HTTPS upstreams
- Custom root CA certificates: `upstream.tls.extra_ca_certs`
- System root certificates: `upstream.tls.use_system_roots` (default: true)
- HTTP proxy: `upstream.proxy` (CONNECT method)
- rustls 0.23 requires crypto provider installation at startup

## Logging

- **Implementation:** tokio tracing + tracing-appender (async, non-blocking)
- **Console:** Default enabled, level configurable
- **File:** Default disabled, level configurable, directory auto-created, rotation supported
- **Level filter:** Programmatic via `LevelFilter`, not RUST_LOG env var
- No silent errors — all errors logged at minimum `warn` level

## Graceful Shutdown

- Listen for `SIGTERM` and `SIGINT` via `tokio::signal`
- On signal: stop accepting new connections, wait for in-flight tasks
- Max wait: `server.shutdown_timeout` seconds (default 30)
- Timeout → force close all connections

**In-flight task behavior during shutdown:**
- Waiting for upstream response → cancel upstream, send 503 to downstream
- Actively streaming → attempt `response.failed` + `[DONE]`, then close
- Not yet forwarded → return 503 to downstream

## Testing Strategy

### Unit Tests

Test the Conversion Task state machine as a whole:

| Category | Coverage |
|---|---|
| Request parsing | All input item types → Anthropic messages, namespace flatten + registry build, ID map generation, message alternation merge, content block mapping, cache_control passthrough, thinking/tool_choice/parallel_tool_calls mapping |
| Streaming events | Each Anthropic SSE event → correct Responses API SSE event, delta accumulation, done events with correct values, multi-tool_use, signature accumulation, index mapping |
| Completion | Each stop_reason → correct status mapping, usage mapping |
| Error paths | Anthropic streaming error → correct error + response.failed + [DONE] sequence |
| Namespace lifecycle | Flatten + build in request phase → lookup + restore in response phase |
| Edge cases | Empty content, long tool name truncation+hash, unknown tool, consecutive function_call merge, consecutive function_call_output merge, built-in tool rejection, private IP rejection |
| Config system | Three-way injection priority, Option semantics, defaults |
| URL routing | Protocol normalization, various upstream paths, invalid path rejection |

Each test constructs complete input (Responses API request or Anthropic SSE events), drives the state machine, and verifies output.

### Integration Tests

```
Codex CLI --[Responses API]--> codex-conv --[Anthropic API]--> Mock Server
```

**Components:**
- **codex-conv:** Real compiled binary, started on random port
- **Mock Anthropic Server:** `wiremock` crate, returns Anthropic API spec-compliant fixed responses (non-streaming and streaming)
- **Codex CLI:** Invoked via `std::process::Command`

**Environment isolation:**
- `CODEX_HOME` set to test directory config
- Pre-configured `config.toml` pointing to codex-conv
- No dependency on user environment or real API keys

**Test directory structure:**

```
tests/
└── integration/
    ├── fixtures/
    │   ├── codex_home/
    │   │   └── config.toml
    │   └── anthropic_responses/
    │       ├── simple_text.json
    │       ├── streaming_text.jsonl
    │       ├── tool_use.json
    │       ├── streaming_tool.jsonl
    │       ├── thinking.json
    │       ├── error_400.json
    │       └── ...
    ├── simple_text_test.rs
    ├── tool_call_test.rs
    ├── mcp_namespace_test.rs
    ├── thinking_test.rs
    ├── error_handling_test.rs
    └── multi_turn_test.rs
```

**Test scenarios:**

| Scenario | Mock returns | Verify |
|---|---|---|
| Simple text | Non-streaming text | Codex outputs correct text |
| Streaming text | SSE streaming text | Codex displays text incrementally |
| Tool call | tool_use response | Codex recognizes and executes tool |
| MCP namespace | namespace tool_use | Codex routes to correct MCP server |
| Extended thinking | thinking + text | Codex shows reasoning |
| Error handling | Various error codes | Codex receives correct error |
| Multi-turn | Sequential turns | Tool call history passed correctly |

## Project Structure

```
codex-conv/
├── Cargo.toml
├── src/
│   ├── main.rs              # Entry: CLI parse, config load, server start
│   ├── config.rs            # Config system: three-way injection, priority, Option semantics
│   ├── server.rs            # axum server: route registration, TLS, listen
│   ├── router.rs            # URL routing: extract upstream base URL
│   ├── conversion/
│   │   ├── mod.rs           # Conversion Task state machine definition
│   │   ├── request.rs       # State machine: request parsing and conversion phase
│   │   ├── response.rs      # State machine: response conversion and streaming phase
│   │   ├── namespace.rs     # Namespace registry: flatten, lookup, truncation+hash
│   │   ├── id_map.rs        # ID mapping: call_id ↔ toolu_id bidirectional
│   │   ├── content.rs       # Content block type mapping (text/image/thinking/tool_use)
│   │   ├── error.rs         # Error conversion: Anthropic error → Responses API error
│   │   └── thinking.rs      # Thinking/reasoning conversion + signature accumulation
│   ├── sse/
│   │   ├── mod.rs           # SSE read/write: bidirectional streaming
│   │   ├── anthropic.rs     # Anthropic SSE event parsing
│   │   └── responses.rs     # Responses API SSE event generation
│   ├── tls.rs               # TLS config: downstream certs, upstream custom roots
│   └── logging.rs           # Logging init: tracing + async appender
├── tests/
│   └── integration/
│       ├── fixtures/
│       │   ├── codex_home/
│       │   │   └── config.toml
│       │   └── anthropic_responses/
│       ├── simple_text_test.rs
│       ├── tool_call_test.rs
│       ├── mcp_namespace_test.rs
│       ├── thinking_test.rs
│       ├── error_handling_test.rs
│       └── multi_turn_test.rs
└── docs/
    └── superpowers/
        └── specs/
```

## Dependencies

```toml
[dependencies]
axum = "0.8"
tokio = { version = "1.52", features = ["full"] }
reqwest = { version = "0.13", default-features = false, features = ["stream", "rustls-tls"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
yaml_serde = "0.10"
clap = { version = "4.6", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = "0.3"
tracing-appender = "0.2"
rustls = "0.23"
tokio-rustls = "0.26"
reqwest-eventsource = "0.6"

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
```

## Technical Requirements

1. **Latest Rust edition**, all crates at latest stable versions (no alpha/beta/preview)
2. **Rust best practices:** avoid unnecessary clones, lazy evaluation, minimize iterations
3. **No silent errors:** all errors must be properly handled; if unsure, stop and ask user
4. **No unspecified behavior:** all ambiguous cases must be escalated to user
5. **Testing:** unit tests fully cover state machine, integration tests use real Codex CLI with mock Anthropic server
6. **Code quality:** must pass `cargo clippy` and `cargo fmt --check` with zero warnings
7. **No unsafe code:** the entire codebase must not contain any `unsafe` blocks
