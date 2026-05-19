# codex-conv Design Document

**Date:** 2026-05-13
**Status:** Draft

## Overview

`codex-conv` is a reverse proxy that converts between OpenAI Responses API (used by Codex CLI) and Anthropic Messages API. It enables using any Anthropic-compatible backend with Codex CLI without modification.

**Sole use case:** The only downstream client is Codex CLI connecting to Anthropic API-compatible models. This proxy is not a general-purpose Responses API-to-Anthropic converter. All design decisions are scoped to what Codex CLI actually sends and receives. Spec changes should be evaluated against Codex CLI behavior, not the full Responses API surface.

**Codex CLI tool types (source: `openai/codex` repository, Rust implementation):**
- `exec_command` — primary shell execution tool (replaces old Node.js `shell` tool). All shell commands go through this
- `apply_patch` — a freeform (grammar-based) function tool for editing files. Has its own handler (`ApplyPatchHandler`) with tool name `"apply_patch"`, sent as `ToolKind::Function` with `FreeformTool` format (Lark grammar). Also invokable via `exec_command` (intercepted by `intercept_apply_patch` which emits a warning)
- MCP tools — sent as namespaced function tools (`mcp__{server}__{tool}`)
- `update_plan` — plan tracking tool
- Codex does not use Responses API built-in tools (`web_search`, `file_search`, `code_interpreter`, `computer_use`, `image_generation`, etc.) in its default configuration. If the client sends built-in tools, the proxy converts them to Anthropic custom tools with derived `input_schema` (see Built-in Tool Conversion section)

### Critical Principle: Information Freshness

Codex CLI evolves rapidly (rewritten from Node.js to Rust, tools renamed, new tool types added). External articles and blog posts become outdated quickly — the `shell` tool description in many 2025 articles refers to the old Node.js implementation. **All factual claims about Codex CLI behavior must be verified against the current source code** ([`openai/codex` on GitHub](https://github.com/openai/codex)) or official documentation, not third-party articles. When researching, prefer primary sources and check publication dates.

### Core Principle: Perfect Forwarding

The proxy must act as a **transparent protocol converter** — only translating between two API formats. It must not:

- **Fabricate data:** Never inject values that don't come from upstream or downstream (no hardcoded token counts, no synthetic fields)
- **Drop data silently:** Every field from the downstream request must either be forwarded (mapped to Anthropic equivalent) or explicitly documented as unsupported with a verified rationale explaining why Anthropic has no equivalent
- **Override client intent:** Never replace a client-specified value with a proxy-decided value unless required by protocol format differences (e.g., `stream` forced to `true` because the proxy architecture requires SSE internally)
- **Invent behavior:** Never add custom logic beyond what either API defines (no custom threshold tables, no synthetic mappings, no heuristic decisions)

When a field exists in both APIs, forward it directly. When a field exists in only one API, document it with a verified explanation. When in doubt, forward.

### Non-Goal: Security

Security is explicitly a **non-goal** of this proxy. The design prioritizes **maximum compatibility, usability, and connectivity** between Codex CLI and Anthropic API. The proxy assumes a trusted deployment environment (localhost or private network) and does not implement SSRF protection, input sanitization, rate limiting, or any security hardening. Such concerns are outside the scope of protocol conversion and should be handled by infrastructure layers (firewalls, reverse proxies, etc.) if needed.

## Architecture

### High-Level

```
Codex CLI  --[HTTP POST + SSE]-->  codex-conv  --[HTTP POST + SSE]-->  Anthropic API
           <--[SSE events]--                   <--[SSE events]--
```

- **Mostly stateless across requests:** No session state, no `previous_response_id` handling. Exception: signature cache (TTL-based, see Thinking/Reasoning Round-Trip) for multi-turn thinking integrity
- **One-to-one request mapping:** Each Responses API request converts to exactly one Anthropic Messages API request
- **One task per connection:** Each request handled by a single tokio task
- **Fully parallel:** No per-session queues needed
- **Real-time SSE conversion:** Events are converted and forwarded immediately, not buffered

### Components

| Component | Responsibility |
|---|---|
| **URL Router** | Parse upstream base URL from request path |
| **Config Layer** | Three-way config injection (YAML / env vars / CLI flags), all with defaults |
| **Signature Cache** | TTL-based cache (3h default) for Anthropic thinking signatures, keyed by proxy-generated reasoning item ID (`rs_xxx`). Shared across all conversion tasks |
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
Write accumulated signatures to signature cache
        │
        ▼
Task ends, all in-flight state released
```

In-flight state maintained per request:
- Delta accumulators (for done events)
- Signature accumulator: collects `signature_delta` data per thinking content block (for signature cache)
- Tool call mappings: `content_block_index → (tool_use_id, call_id, name)`
- ID map: `toolu_xxx` ↔ `call_xxx` bidirectional
- Namespace registry: `flat_name → (namespace, tool_name)`
- Index mapping: Anthropic `content_block.index` → Responses API `output_index` + `content_index`

#### Thinking/Reasoning Round-Trip

(Reference: protocol_research.md entry #2 — comparative analysis of CLIProxyAPI, LiteLLM, token_proxy, aio-coding-hub, codex-bridge, llm-rosetta)

**The problem:** Anthropic returns encrypted signatures (`signature_delta`) and `redacted_thinking` blocks with opaque `data`. These must be passed back unchanged in multi-turn. The proxy is stateless across requests, creating a fundamental tension.

**Solution: Signature cache + encrypted_content passthrough + rectifier fallback.** Three-layer approach:

1. **Streaming response (Anthropic → Responses API):** Forward thinking text as `summary` content in reasoning items. Accumulate `signature_delta` data during streaming and store in a signature cache (keyed by the proxy-generated reasoning item ID `rs_xxx`, TTL 3 hours). Discard signatures from the SSE stream — Codex does not receive encrypted Anthropic thinking data (it receives plain text in `summary`).

2. **Subsequent request (Responses API → Anthropic):** When a `reasoning` input item appears, the conversion depends on available data:
   - **If `encrypted_content` is present:** Convert to `{"type": "redacted_thinking", "data": "<encrypted_content>"}` — pass through the opaque data as-is. This preserves Anthropic's encrypted content intact.
   - **If only `summary` text (no `encrypted_content`):** Look up signature cache. If cached signature exists, convert to `{"type": "thinking", "thinking": "<summary text>", "signature": "<cached signature>"}`. If no cached signature, use empty signature: `{"type": "thinking", "thinking": "<summary text>", "signature": ""}`.

3. **Rectifier fallback:** If Anthropic returns 400 due to invalid signatures, strip all thinking/redacted_thinking blocks from the request and retry. This loses multi-turn thinking context but ensures the request succeeds.

**Why this approach:** No existing implementation fully solves the stateless multi-turn problem. CLIProxyAPI silently drops reasoning input; token_proxy fabricates signatures; aio-coding-hub strips on error. This three-layer approach maximizes compatibility: preserves thinking when encrypted content or cached signatures are available, and falls back gracefully when neither exists.

## URL Routing

### Format

Codex base URL: `https://my.domain/https/api.anthropic.com`

Codex appends `responses`, sends to: `https://my.domain/https/api.anthropic.com/responses`

Note: the `/v1/` prefix commonly seen in Codex requests (e.g., `/v1/responses`) comes from the user-configured `base_url` in Codex config (typically ending in `/v1`), not from Codex itself. Codex appends only `responses`. The proxy should accept paths ending in `/responses` regardless of prefix.

Protocol normalization (all accepted):
- `/https/api.anthropic.com/responses`
- `/https:/api.anthropic.com/responses`
- `/https://api.anthropic.com/responses`
- `/v1/https/api.anthropic.com/responses` (if base_url includes `/v1`)

Proxy extracts:
- Upstream base URL: `https://api.anthropic.com` (no `/v1`, follows Anthropic convention)
- Upstream endpoint: `/v1/messages` (proxy appends, not configurable)

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
| `tools` | `tools` | Namespace flattening + registry. Field rename: `parameters` → `input_schema` |
| `tool_choice` | `tool_choice` | See mapping table |
| `parallel_tool_calls` | `disable_parallel_tool_use` | Semantics inverted |
| `reasoning.effort` | `thinking` + `output_config.effort` | Direct string forwarding, see mapping section |
| `temperature` | `temperature` | Passthrough with range clamping: Anthropic range 0–1 vs OpenAI 0–2. Values >1 must be clamped to 1. When thinking enabled, omit from Anthropic request (Anthropic requires default 1.0). Codex CLI never sends `temperature` (verified: `ResponsesApiRequest` struct in `codex-rs/codex-api/src/common.rs` has no temperature field), so this is not a practical concern |
| `top_p` | `top_p` | Passthrough. When thinking enabled, clamp to 0.95–1.0 range. Codex CLI never sends `top_p` (same source as temperature) |
| `max_output_tokens` | `max_tokens` | Field name differs |
| `metadata` | `metadata` | Forward `user_id`, strip other keys |
| `service_tier` | `service_tier` | Value mapping, see Unsupported Features section |
| `text.format` | `output_config.format` | See `text.format` → `output_config.format` Mapping below |
| `text.verbosity` | — | Ignored (stripped). Anthropic has no verbosity control |

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

**When `reasoning` is present:**

```json
// Responses API
{"reasoning": {"effort": "high", "summary": "auto"}}
```

```json
// Anthropic
{"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}
```

- `reasoning.effort` → `output_config.effort`: direct string forwarding
- When reasoning present: set `thinking.type: "adaptive"` (Anthropic's modern thinking mode)
- `thinking.display` is set based on `reasoning.summary` (see mapping table below)
- `reasoning` absent or `reasoning.effort: "none"` → omit both `thinking` and `output_config` (Anthropic defaults)

**`thinking.display` mapping (based on `reasoning.summary`):**

On Opus 4.7+, `thinking.display` defaults to `"omitted"` — no thinking text returned. The proxy must explicitly set `display: "summarized"` when the client wants reasoning text.

| Responses API `reasoning.summary` | Anthropic `thinking.display` | Notes |
|---|---|---|
| absent / `null` | `"summarized"` | Default to showing thinking when reasoning enabled |
| `"auto"` | `"summarized"` | Direct match — default behavior |
| `"concise"` | `"summarized"` | Anthropic has no granularity control, closest match |
| `"detailed"` | `"summarized"` | Same — no Anthropic granularity |
| `"none"` | `"omitted"` | Client explicitly opts out of reasoning summaries |

When `display: "omitted"`, Anthropic returns only `signature_delta` events (no `thinking_delta`). The proxy still emits a reasoning output item with empty `summary: []` — the signature needs caching for multi-turn integrity, and Codex uses reasoning item IDs to reference thinking in subsequent turns. No streaming logic change needed beyond the request-side change.

**Value mapping:**

| Responses API `reasoning.effort` | Anthropic `output_config.effort` | Notes |
|---|---|---|
| `"low"` | `"low"` | Direct match |
| `"medium"` | `"medium"` | Direct match |
| `"high"` | `"high"` | Direct match |
| `"xhigh"` | `"xhigh"` | Model-gated: only Opus 4.7 on Anthropic, GPT-5.2+ on OpenAI. Unsupported models return 400 |
| `"none"` | Omit `thinking` and `output_config` | OpenAI defines this as "no reasoning" — disable thinking entirely |
| `"minimal"` | `"low"` | Protocol-required approximation — Anthropic has no `minimal` level. Codex CLI can send this value (verified in ReasoningEffort enum). `"low"` is the closest available Anthropic level |

**Reverse direction note:** Anthropic supports `"max"` effort (Opus 4.7, Opus 4.6, Sonnet 4.6) which has no OpenAI equivalent. Since this proxy only converts Responses API → Anthropic (not reverse), this is not a concern. If reverse conversion is ever needed, `"max"` → `"high"` would be the closest approximation.

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

#### `text.format` → `output_config.format` Mapping

Codex sends `text.format` when requesting structured JSON output. Both APIs use a flat structure, but Anthropic has fewer fields:

| OpenAI `text.format` | Anthropic `output_config.format` | Notes |
|---|---|---|
| `{"type": "text"}` | Omit `output_config.format` | Default plain text — no format constraint |
| `{"type": "json_object"}` | `{"type": "json_object"}` | Direct match (older JSON mode) |
| `{"type": "json_schema", "name": "...", "schema": {...}, "strict": bool}` | `{"type": "json_schema", "schema": {...}}` | Forward `schema` only. `name` and `strict` have no Anthropic equivalent — Anthropic enforces schema compliance by default |

Codex's `create_text_param_for_request` constructs `text.format` with `type: "json_schema"`, `name: "codex_output_schema"`, and a user-provided JSON schema. The `name` and `strict` fields are dropped during conversion — Anthropic's `output_config.format` only accepts `type` and `schema`.

#### Input Items → Messages

| Responses API input item | Anthropic message |
|---|---|
| `{"type":"message","role":"user","content":[...]}` | `{"role":"user","content":[...]}` |
| `{"type":"message","role":"system","content":[...]}` | Extracted to top-level `system` |
| `{"type":"message","role":"assistant","content":[...]}` | `{"role":"assistant","content":[...]}` |
| `{"type":"function_call","call_id":"...","name":"x","arguments":"{...}"}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_xxx","name":"x","input":{...}"}]}` |
| `{"type":"function_call_output","call_id":"...","output":"..."}` | `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_xxx","content":"..."}]}` |
| `{"type":"reasoning","summary":[{"type":"summary_text","text":"..."}],"encrypted_content":"..."}` | `{"role":"assistant","content":[{"type":"redacted_thinking","data":"<encrypted_content>"}`]}` if `encrypted_content` present; otherwise `{"role":"assistant","content":[{"type":"thinking","thinking":"<summary>","signature":"<cached or empty>"}]}` (see Thinking/Reasoning Round-Trip) |
| `{"type":"custom_tool_call","call_id":"...","name":"x","input":"{...}"}` | Same mapping as `function_call`: `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_xxx","name":"x","input":{...}"}]}` |
| `{"type":"custom_tool_call_output","call_id":"...","output":"..."}` | Same mapping as `function_call_output`: `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_xxx","content":"..."}]}` |
| `{"type":"compaction","encrypted_content":"..."}` | Dropped — opaque OpenAI context management data, no Anthropic equivalent |
| `{"type":"context_compaction","encrypted_content":"..."}` | Dropped — same as `compaction` |
| `{"type":"compaction_trigger"}` | Dropped — same as `compaction` |
| `{"type":"tool_search_output","call_id":"...","status":"...","execution":"...","tools":[...]}` | Dropped — tool discovery metadata from a previous tool search. The relevant tools have already been registered in the current request's `tools` array. No Anthropic equivalent |
| `{"type":"mcp_tool_call_output","call_id":"...","output":{...}}` | Same mapping as `function_call_output`: `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_xxx","content":"<output as JSON string>"}]}`. Codex-specific variant for MCP tool results (uses `CallToolResult` from the MCP library instead of `FunctionCallOutputPayload`). The `output` is an object, not a string — serialize to JSON string for `tool_result.content`. Uses `call_id` for ID lookup in the same bidirectional map |
| Unknown input item types | Dropped with warning log. Codex uses `#[serde(other)]` catch-all for unknown items. The proxy follows the same behavior — unknown types are consumed and discarded to maintain forward compatibility with future API additions |
| Built-in tool history items (`web_search_call`, `file_search_call`, `code_interpreter_call`, `computer_call`, `computer_call_output`, `image_generation_call`, `shell_call`, `shell_call_output`, `local_shell_call`, `apply_patch_call`, `apply_patch_call_output`, `mcp_call`, `tool_search_call`) | See Built-in Tool Conversion → Input Direction section for per-type mapping |

Key details:
- **ID mapping:** `call_id` ↔ `tool_use.id`. Proxy generates `toolu_` prefixed IDs, maintains bidirectional map
- **arguments → input:** JSON string → parsed JSON object. Parse failure → reject entire request
- **namespace restoration:** `function_call` with `namespace` + `name` → Anthropic `tool_use.name` = `mcp__{server}__{tool}`. Look up registry for original namespace and name
- **tool_result.content:** `function_call_output.output` can be either a plain string or an array of content items:
  - If string → Anthropic `tool_result.content` as plain string (current default)
  - If array of content items → Anthropic `tool_result.content` as array of Anthropic content blocks, using the same content block mapping as User Content Block Mapping (e.g., `{type: "input_text", text: "..."}` → `{type: "text", text: "..."}`, `{type: "input_image", image_url: "..."}` → `{type: "image", source: {type: "url", url: "..."}}`)
- **`success` → `is_error`:** Codex's `function_call_output` has a `success` field (Codex extension, not standard Responses API). Map to Anthropic's `tool_result.is_error`: `success: false` → `is_error: true`; `success: true` or absent → omit `is_error` (default is no error)
- **Anthropic message alternation:** Ensure user/assistant messages strictly alternate. Consecutive same-role input items are merged:
  - Consecutive `reasoning` + `function_call` items → merged into same assistant message content array as `[thinking/redacted_thinking, tool_use]` blocks (preserving order). This is the common pattern: Codex sends a reasoning item followed by a function_call from the same model turn
  - Consecutive `function_call` items → merged into same assistant message content array as multiple `tool_use` blocks
  - Consecutive `function_call_output` items → merged into same user message content array as multiple `tool_result` blocks
  - Consecutive `custom_tool_call` items → merged same as `function_call`
  - Consecutive `custom_tool_call_output` items → merged same as `function_call_output`
- **`phase` stripping:** Message items may carry `phase: "commentary"` or `phase: "final_answer"`. Strip on input (Anthropic has no equivalent). Do not generate on output (Anthropic provides no signal).

#### User Content Block Mapping

| Responses API UserContent | Anthropic content block |
|---|---|
| `{"type":"input_text","text":"..."}` | `{"type":"text","text":"..."}` |
| `{"type":"input_image","image_url":"..."}` | `{"type":"image","source":{"type":"url","url":"..."}}` |
| `{"type":"input_image","image_url":{"url":"data:image/png;base64,..."}}` | `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}` |
| `{"type":"input_file","file_url":"..."}` | Dropped — Anthropic has no generic file input content block type. Anthropic supports `document` blocks for PDF/text files, but the schema differs significantly from OpenAI's `input_file`. Codex does not currently send `input_file` items |

#### `cache_control` Handling (Reference: LiteLLM)

Codex may send `cache_control: {"type": "ephemeral"}` on content blocks. Preserve these markers on corresponding Anthropic content blocks (Anthropic natively supports `cache_control`).

#### Tool Name Length Limits

Flattened namespace tool names may exceed 64 chars (Anthropic limit). Apply Codex's `sanitize_responses_api_tool_name` logic: truncate + hash suffix for uniqueness.

### Response Conversion (Anthropic → Responses API)

#### Content Blocks → Output Items

| Anthropic content block | Responses API output item |
|---|---|
| `{"type":"text","text":"..."}` | `{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"...","annotations":[]}]}` |
| `{"type":"thinking","thinking":"..."}` | `{"type":"reasoning","id":"rs_xxx","summary":[{"type":"summary_text","text":"..."}]}`. The `encrypted_content` and `content` fields are **omitted** — Anthropic returns plain thinking text with no encrypted payload, so there is nothing to populate. The fields are absent from the object, not `null` or empty |
| `{"type":"redacted_thinking","data":"..."}` | `{"type":"reasoning","id":"rs_xxx","summary":[],"encrypted_content":"<data>"}`. Anthropic returns `redacted_thinking` when its safety system redacts portions of thinking content. This can occur with any thinking mode (including `adaptive`) — it is server-initiated, not client-controlled. The opaque `data` maps to `encrypted_content` for round-trip preservation. No `summary` text is available. The proxy must cache the `data` alongside the `rs_xxx` ID so it can reconstruct `redacted_thinking` if this reasoning item appears in a subsequent input |
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
| `max_tokens` | `"incomplete"`, `incomplete_details.reason: "max_output_tokens"`. Emits `response.incomplete` event (not `response.completed`) |
| `stop_sequence` | `"completed"` |
| `tool_use` | `"completed"` (output contains function_call items) |
| `pause_turn` | `"completed"` |
| `refusal` | `"completed"` (model declined to generate content) |
| `model_context_window_exceeded` | `"incomplete"`, `incomplete_details.reason: "max_output_tokens"`. Emits `response.incomplete` event. Available on Sonnet 4.5+ by default; earlier models need beta header |

Note: The Responses API defines `incomplete_details.reason: "content_filter"` as a valid value. Anthropic does not have a distinct "content filter" stop reason — content filtering manifests as `refusal` stop reason (mapped to `"completed"`) or as a streaming `error` event. If Anthropic returns a streaming error due to content policy, the proxy maps it through the standard error handling path (`response.failed`). If a need arises to distinguish content filter from other errors in the future, the error message text can be inspected, but the proxy does not fabricate `incomplete_details.reason: "content_filter"` since no Anthropic signal produces it.

#### Usage Mapping

| Anthropic | Responses API |
|---|---|
| `usage.input_tokens` | `usage.input_tokens` |
| `usage.output_tokens` | `usage.output_tokens` |
| `usage.cache_creation_input_tokens` | Included in `usage.input_tokens` (no separate field in Responses API) |
| `usage.cache_read_input_tokens` | `usage.input_tokens_details.cached_tokens` |
| *(computed)* | `usage.total_tokens` = `input_tokens` + `output_tokens` |

`cache_creation_input_tokens` represents cache write cost (future savings), `cache_read_input_tokens` represents cache hits (current savings). Only cache reads map to `cached_tokens` — this matches the semantic meaning of "tokens served from cache." Cache creation tokens are folded into `input_tokens` total. Reference: CLIProxyAPI Chat Completions path uses the same mapping.

Additional Anthropic usage fields with no Responses API equivalent (dropped):
- `usage.cache_creation` (breakdown by TTL) — no Responses API cache breakdown fields
- `usage.server_tool_use` — proxy doesn't support server tools
- `usage.service_tier` — informational, no Responses API equivalent
- `usage.speed` — Anthropic fast mode indicator, no Responses API equivalent

Note: `output_tokens_details.reasoning_tokens` is omitted from response — Anthropic has no equivalent field, so we do not fabricate values.

### Streaming Conversion (Event-by-Event, Real-Time)

Event sequence (Anthropic → Responses API):

**Consumed events (no Responses API output):**

| Anthropic SSE event | Behavior |
|---|---|
| `ping` | Consumed silently, no event emitted. Anthropic sends these periodically during streaming to keep connections alive |

**Forwarded events:**

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
  → response.output_item.added (type=reasoning, summary=[])
  → response.reasoning_summary_part.added          (summary_index, empty part)

content_block_delta (index=1, type=thinking_delta)
  → response.reasoning_summary_text.delta          (repeated, summary_index)
  Note: `response.reasoning_text.delta` is NOT emitted — it maps to the `content` field (raw reasoning text from GPT-OSS models), which is not applicable to Anthropic thinking. The proxy only emits `reasoning_summary_text` events.

content_block_delta (index=1, type=signature_delta)
  → Consumed and discarded (no Responses API event)

content_block_stop (index=1)
  → response.reasoning_summary_text.done           (accumulated text, summary_index)
  → response.reasoning_summary_part.done           (final part with full text, summary_index)
  → response.output_item.done

content_block_start (index=N, type=redacted_thinking)
  → response.output_item.added (type=reasoning, summary=[], encrypted_content=<data>)
  → Cache the data field alongside the rs_xxx ID for round-trip

content_block_stop (index=N, type=redacted_thinking)
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
  → response.completed OR response.incomplete      (completed for normal end, incomplete for max_tokens)

[DONE]                                                  (terminal marker)
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

#### Response Object Structure

The `response.completed` and `response.incomplete` events carry a full response object. Fields included:

| Field | Source | Notes |
|---|---|---|
| `id` | Anthropic `message_start.message.id` | Passthrough |
| `object` | Fixed `"response"` | Responses API type discriminator |
| `created_at` | Proxy-generated | Unix timestamp at `response.created` emission |
| `completed_at` | Proxy-generated | Unix timestamp at `response.completed` emission |
| `model` | Request `model` field | Echo back the model the client sent in the request. Anthropic returns its own `model` field in the response, but it may differ from the request (e.g., sub-version routing). The proxy uses the request model to avoid confusion |
| `status` | Mapped from `stop_reason` | See Stop Reason → Status table |
| `output` | Accumulated during streaming | All output items generated during the request |
| `usage` | Mapped from Anthropic `usage` | See Usage Mapping table |
| `incomplete_details` | Conditional | Only present when `status: "incomplete"`, with `reason: "max_output_tokens"` |
| `metadata` | `null` | Proxy does not store metadata |
| `parallel_tool_calls` | From request | Echo back request value |
| `tool_choice` | From request | Echo back request value |
| `instructions` | From request | Echo back request value |

Fields omitted from the response object: `temperature`, `top_p`, `max_output_tokens`, `reasoning`, `text`, `previous_response_id`, `truncation`, `store`, `stream`, `stream_options`, `user` — these are request-only fields that the Responses API echoes back for stateless replay, but the proxy's response object does not need them since Codex maintains its own state.

#### SSE Wire Format Examples

Each SSE event consists of an `event:` line and a `data:` line with JSON payload. Below are representative examples. **Note:** OpenAI streaming events include a `sequence_number` field for ordering. Wire format examples omit this field for clarity, but the implementation must include it in every event.

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

**Thinking streaming (Anthropic thinking → Responses API reasoning summary):**
```
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_001","summary":[]}}

event: response.reasoning_summary_part.added
data: {"type":"response.reasoning_summary_part.added","output_index":1,"item_id":"rs_001","summary_index":0,"part":{"type":"summary_text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Let me analyze..."}}

event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","output_index":1,"item_id":"rs_001","summary_index":0,"delta":"Let me analyze..."}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"ErUB..."}}

(No Responses API event emitted — signature consumed and discarded)

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: response.reasoning_summary_text.done
data: {"type":"response.reasoning_summary_text.done","output_index":1,"item_id":"rs_001","summary_index":0,"text":"Let me analyze..."}

event: response.reasoning_summary_part.done
data: {"type":"response.reasoning_summary_part.done","output_index":1,"item_id":"rs_001","summary_index":0,"part":{"type":"summary_text","text":"Let me analyze..."}}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_001","summary":[{"type":"summary_text","text":"Let me analyze..."}]}}
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
data: {"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc_002","delta":"{\"key\":"}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","output_index":2,"item_id":"fc_002","arguments":"{\"key\":\"value\"}"}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_002","call_id":"call_002","name":"tool","namespace":"mcp__svr__","status":"completed","arguments":"{\"key\":\"value\"}"}}
```

**Stream end (normal completion):**
```
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}

event: response.completed
data: {"type":"response.completed","response":{"id":"msg_01XFDUDYJgAACzvnptvVoYEL","status":"completed","output":[...],"usage":{"input_tokens":100,"output_tokens":15,"input_tokens_details":{"cached_tokens":0}}}}

data: [DONE]
```

**Stream end (max_tokens — incomplete):**
```
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4096}}

event: message_stop
data: {"type":"message_stop"}

event: response.incomplete
data: {"type":"response.incomplete","response":{"id":"msg_01XFDUDYJgAACzvnptvVoYEL","status":"incomplete","output":[...],"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":100,"output_tokens":4096,"input_tokens_details":{"cached_tokens":0}}}}

data: [DONE]
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
| Built-in tools (`web_search`, `file_search`, `code_interpreter`, `computer_use`, `image_generation`, `local_shell`, `shell`, `apply_patch`, `mcp`, `tool_search`, `custom`) | Converted to Anthropic custom tools with derived `input_schema` | See Built-in Tool Conversion section. All tools are the client's tools — the proxy converts format only |
| `client_metadata` | Ignored (stripped) | OpenAI telemetry field containing installation ID and W3C trace context. Anthropic has no equivalent |
| `prompt_cache_key` | Ignored (stripped) | OpenAI server-side response caching key (set to thread ID). Anthropic has no server-side storage |
| `user` | Ignored (stripped) | Deprecated in favor of `safety_identifier`. Use `metadata.user_id` for user identification. Anthropic uses `metadata.user_id` directly |
| `safety_identifier` | Ignored (stripped) | OpenAI abuse detection identifier. Anthropic has no per-request safety identifier |
| `max_tool_calls` | Ignored (stripped) | OpenAI limit on built-in tool calls per response. Anthropic has no equivalent |
| `top_logprobs` | Ignored (stripped) | OpenAI log probability reporting. Anthropic has no logprobs feature in Messages API |
| `background` | Ignored (stripped) | OpenAI async/background mode. Proxy handles requests synchronously |
| `conversation` | Ignored (stripped) | OpenAI conversation-scoped requests (alternative to `input`). Codex uses `input` array |
| `context_management` | Ignored (stripped) | OpenAI context compaction config. Anthropic handles context differently |
| `prompt` | Ignored (stripped) | OpenAI prompt template reference. Codex uses `input` array directly |
| `prompt_cache_retention` | Ignored (stripped) | OpenAI prompt cache retention policy. Related to `prompt_cache_key` |
| `verbosity` (top-level) | Ignored (stripped) | OpenAI top-level verbosity control. Redundant with `text.verbosity`. Anthropic has no equivalent |
| `stream_options` | Ignored (stripped) | OpenAI streaming options (e.g., `include_usage`). Proxy controls streaming behavior internally |
| `reasoning.generate_summary` | Ignored (stripped) | Deprecated field. Replaced by `reasoning.summary`. Codex uses `reasoning.summary` |

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
| `"scale"` | Omit (no Anthropic equivalent) | Anthropic has no scale tier; request proceeds without tier specification |

## Built-in Tool Conversion

### Principle

ALL tools in the request are the client's tools — the proxy does not need Anthropic equivalents. The conversion is purely format-based: translate the tool definition so the model can generate a `tool_use`, then convert the `tool_use` back to the appropriate Responses API output item on response.

Both OpenAI built-in tools and Anthropic native tools are **schema-less** — they have no `input_schema`/`parameters` field. The proxy must provide explicit schemas when converting to Anthropic custom tools so the model knows what parameters to generate.

**Consistency requirement:** The schema the model sees during registration determines the shape of `tool_use.input` the model generates. The response-direction conversion must be consistent with this schema — the model's output must be directly mappable to the target Responses API output item without field renaming or restructuring. Therefore, the schema must mirror the exact nested structure of the corresponding output item's call parameters.

### Request Direction: Built-in Tool Definition → Anthropic Custom Tool

Each OpenAI built-in tool type in the `tools` array is converted to an Anthropic custom tool:

| Responses API tool type | Anthropic tool | Notes |
|---|---|---|
| `{"type": "web_search", ...}` | `{"type": "custom", "name": "web_search", "input_schema": {...}}` | Schema mirrors `web_search_call.action` |
| `{"type": "file_search", ...}` | `{"type": "custom", "name": "file_search", "input_schema": {...}}` | Schema mirrors `file_search_call.queries` |
| `{"type": "code_interpreter", ...}` | `{"type": "custom", "name": "code_interpreter", "input_schema": {...}}` | Schema mirrors `code_interpreter_call` call fields |
| `{"type": "computer_use_preview", ...}` | `{"type": "custom", "name": "computer_use", "input_schema": {...}}` | Schema mirrors `computer_call.action` |
| `{"type": "image_generation", ...}` | `{"type": "custom", "name": "image_generation", "input_schema": {...}}` | Schema for prompt |
| `{"type": "local_shell", ...}` | `{"type": "custom", "name": "local_shell", "input_schema": {...}}` | Schema mirrors `local_shell_call.action` |
| `{"type": "shell", ...}` | `{"type": "custom", "name": "shell", "input_schema": {...}}` | Schema mirrors `shell_call.action` |
| `{"type": "apply_patch", ...}` | `{"type": "custom", "name": "apply_patch", "input_schema": {...}}` | Schema mirrors `apply_patch_call.operation` |
| `{"type": "mcp", ...}` | **Dropped** (cannot convert) | Server-side hosted tool — OpenAI connects to MCP server and executes calls. A format-only proxy cannot replicate this. Codex does not use this type (uses `{"type": "namespace"}` instead, see MCP Namespace Handling) |
| `{"type": "tool_search", ...}` | `{"type": "custom", "name": "tool_search", "input_schema": {...}}` | Schema mirrors `tool_search_call.arguments` |
| `{"type": "custom", ...}` | `{"type": "custom", "name": "<name>", "input_schema": {...}}` | Freeform/custom tools. Two sub-cases: (1) If `parameters` (JSON Schema) is present, use it directly as `input_schema`. (2) If only `format` (grammar definition) is present, provide a minimal `input_schema` that accepts the raw grammar string as a single field — the proxy cannot fully convert a Lark grammar to JSON Schema, so the schema captures the grammar input as an opaque string. Codex uses this type for freeform tools (e.g., `apply_patch` with Lark grammar where `format.type: "lark"`) |
| `{"type": "computer", ...}` | `{"type": "custom", "name": "computer_use", "input_schema": {...}}` | GA version of `computer_use_preview`. Same schema as `computer_use_preview` |
| `{"type": "web_search_preview", ...}` | `{"type": "custom", "name": "web_search", "input_schema": {...}}` | Preview variant of `web_search`. Same schema as `web_search` |

**Tool config passthrough:** Built-in tools may carry configuration fields (e.g., `web_search.user_location`, `web_search.search_context_size`, `file_search.vector_store_ids`, `computer_use_preview.display_width`, `image_generation.input_image_mask`). These configuration fields have no Anthropic equivalent and are **dropped** during request conversion. They control server-side behavior on OpenAI's infrastructure that is not applicable when proxying to Anthropic. The custom tool schema only captures the parameters the model needs to generate in a tool call.

### Derived `input_schema` Definitions

Each schema mirrors the exact nested structure of its corresponding output item's call parameters [34]. This ensures the model's `tool_use.input` matches the schema exactly, and `function_call.arguments` preserves the complete structure via JSON serialization.

**`web_search`** — schema mirrors `web_search_call.action`:
```json
{
  "type": "object",
  "properties": {
    "type": {"type": "string", "enum": ["search", "open_page", "find_in_page"], "description": "Action type"},
    "query": {"type": "string", "description": "Search query (deprecated, use queries)"},
    "queries": {"type": "array", "items": {"type": "string"}, "description": "Search queries (for type=search)"},
    "sources": {"type": "array", "items": {"type": "object", "properties": {"type": {"type": "string"}, "url": {"type": "string"}}}, "description": "Source URLs (for type=search)"},
    "url": {"type": "string", "description": "URL (for open_page/find_in_page)"},
    "pattern": {"type": "string", "description": "Search pattern (for find_in_page)"}
  },
  "required": ["type"]
}
```

Model generates: `{"type": "search", "queries": ["..."]}` → serialized as `function_call.arguments`.

**`file_search`** — schema mirrors `file_search_call` call fields:
```json
{
  "type": "object",
  "properties": {
    "queries": {"type": "array", "items": {"type": "string"}, "description": "Search queries"}
  },
  "required": ["queries"]
}
```

Model generates: `{"queries": ["..."]}` → serialized as `function_call.arguments`.

**`code_interpreter`** — schema mirrors `code_interpreter_call` call fields:
```json
{
  "type": "object",
  "properties": {
    "code": {"type": "string", "description": "Code to execute"},
    "container_id": {"type": "string", "description": "Container ID to run code in"}
  },
  "required": ["code"]
}
```

Model generates: `{"code": "...", "container_id": "..."}` → serialized as `function_call.arguments`.

**`computer_use`** — schema mirrors `computer_call.action`:
```json
{
  "type": "object",
  "properties": {
    "type": {"type": "string", "enum": ["screenshot", "click", "double_click", "drag", "keypress", "move", "scroll", "type", "wait"]},
    "button": {"type": "string", "enum": ["left", "right", "wheel", "back", "forward"], "description": "Mouse button (click)"},
    "x": {"type": "integer", "description": "X coordinate"},
    "y": {"type": "integer", "description": "Y coordinate"},
    "text": {"type": "string", "description": "Text to type (type)"},
    "keys": {"type": "array", "items": {"type": "string"}, "description": "Keys (click, double_click, drag, keypress, move, scroll)"},
    "path": {"type": "array", "items": {"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}}}, "description": "Drag path (drag)"},
    "scroll_x": {"type": "integer", "description": "Horizontal scroll (scroll)"},
    "scroll_y": {"type": "integer", "description": "Vertical scroll (scroll)"}
  },
  "required": ["type"]
}
```

Model generates: `{"type": "click", "button": "left", "x": 405, "y": 157}` → serialized as `function_call.arguments`.

**`image_generation`** — schema for image prompt:
```json
{
  "type": "object",
  "properties": {
    "prompt": {"type": "string", "description": "Image generation prompt"}
  },
  "required": ["prompt"]
}
```

Model generates: `{"prompt": "..."}` → serialized as `function_call.arguments`. Note: `image_generation_call` does not have a `prompt` field — it has `result` (base64 output). The prompt field is an input-only parameter. Since all built-in tools return as `function_call`, this is not a problem — the prompt is preserved in `arguments`.

**`local_shell`** — schema mirrors `local_shell_call.action`:
```json
{
  "type": "object",
  "properties": {
    "type": {"type": "string", "enum": ["exec"]},
    "command": {"type": "array", "items": {"type": "string"}, "description": "Command and arguments"},
    "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment variables"},
    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds"},
    "user": {"type": "string", "description": "User to run as"},
    "working_directory": {"type": "string", "description": "Working directory"}
  },
  "required": ["type", "command"]
}
```

Model generates: `{"type": "exec", "command": ["ls", "-l"], "env": {}}` → serialized as `function_call.arguments`.

**`shell`** — schema mirrors `shell_call.action`:
```json
{
  "type": "object",
  "properties": {
    "commands": {"type": "array", "items": {"type": "string"}, "description": "Commands to execute"},
    "timeout_ms": {"type": "integer"},
    "max_output_length": {"type": "integer"}
  },
  "required": ["commands"]
}
```

Model generates: `{"commands": ["ls -l"]}` → serialized as `function_call.arguments`.

**`apply_patch`** — schema mirrors `apply_patch_call.operation`:
```json
{
  "type": "object",
  "properties": {
    "type": {"type": "string", "enum": ["create_file", "update_file", "delete_file"]},
    "path": {"type": "string", "description": "File path"},
    "diff": {"type": "string", "description": "V4A diff content (not needed for delete_file)"}
  },
  "required": ["type", "path"]
}
```

Model generates: `{"type": "update_file", "path": "lib/fib.py", "diff": "..."}` → serialized as `function_call.arguments`.

**`tool_search`** — schema mirrors `tool_search_call.arguments`:
```json
{
  "type": "object",
  "properties": {
    "goal": {"type": "string", "description": "Description of the desired tool capability"}
  },
  "required": ["goal"]
}
```

Model generates: `{"goal": "..."}` → serialized as `function_call.arguments`.

### Response Direction: Anthropic `tool_use` → Responses API Output Item

All built-in tool calls use the **same `function_call` mapping** as regular function tools. The model's `tool_use.input` (which matches the registered schema) is serialized as `function_call.arguments` — a JSON string that preserves the complete nested structure without loss.

```
Anthropic tool_use:  {"type":"tool_use","id":"toolu_xxx","name":"web_search","input":{"type":"search","queries":["..."]}}
                                            ↓
Responses API:       {"type":"function_call","id":"fc_xxx","call_id":"call_xxx","name":"web_search","arguments":"{\"type\":\"search\",\"queries\":[\"...\"]}"}
```

**Why not native built-in output types (e.g., `web_search_call`):** The registered schema determines `tool_use.input`. For round-trip consistency, the client must send back exactly what it received. If the proxy returned `web_search_call` with `action: {...}`, the client would need to send `web_search_call` back, but the `web_search_call` format mixes model input (`action`) with execution metadata (`status`, `id`). On subsequent turns, the proxy would need to extract `action` from `web_search_call` and reconstruct `tool_use.input`. This reconstruction is fragile — for `image_generation_call`, it's impossible (the output item has `result` but no `prompt` field). Returning `function_call` for all tools guarantees: schema ↔ `tool_use.input` ↔ `function_call.arguments` ↔ `tool_use.input` consistency with zero reconstruction logic.

**Tool name passthrough:** `tool_use.name` (e.g., `"web_search"`) becomes `function_call.name`. The client knows which built-in tool it registered and can dispatch accordingly. Tool names are NOT looked up in the namespace registry — built-in tools have no namespace.

### Input Direction: Built-in Tool History Items

Built-in tool history items can appear in the `input` array from two sources:

1. **Proxy-generated history (round-trip):** The proxy always returns `function_call` + `function_call_output` for built-in tools. On subsequent turns, the client sends these back, and the standard `function_call` / `function_call_output` mapping handles them. No special logic needed.

2. **Imported OpenAI API history:** History from actual OpenAI API calls uses native built-in output types (`web_search_call`, `computer_call`, etc.). These items must be converted to Anthropic messages using the per-type mappings below.

The proxy must handle both sources in the input array.

#### Native Built-in Type → Anthropic Message Mappings

These mappings apply only to imported OpenAI API history items. Each mapping extracts the model-relevant call parameters and places them as `tool_use.input`, matching the registered schema [34]:

| Responses API input item | Anthropic message |
|---|---|
| `{"type":"web_search_call","id":"ws_...","status":"completed","action":{...}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ws_...","name":"web_search","input":<action>}]}` |
| `{"type":"file_search_call","id":"fs_...","status":"completed","queries":[...],"results":[...]}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_fs_...","name":"file_search","input":{"queries":<queries>}}]}` followed by `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fs_...","content":"<results as JSON string>"}]}` |
| `{"type":"code_interpreter_call","id":"ci_...","code":"...","container_id":"...","outputs":[...]}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ci_...","name":"code_interpreter","input":{"code":"...","container_id":"..."}}]}` followed by `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ci_...","content":"<outputs as JSON string>"}]}` |
| `{"type":"computer_call","id":"comp_...","action":{...}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_comp_...","name":"computer_use","input":<action>}]}` |
| `{"type":"computer_call_output","call_id":"...","output":{...}}` | `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_comp_...","content":"<output as JSON string>"}]}` |
| `{"type":"image_generation_call","id":"ig_...","result":"base64...","revised_prompt":"..."}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ig_...","name":"image_generation","input":{"prompt":<revised_prompt>}}]}` followed by `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ig_...","content":"<result>"}]}` |
| `{"type":"shell_call","id":"sh_...","action":{...}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_sh_...","name":"shell","input":<action>}]}` |
| `{"type":"shell_call_output","call_id":"...","output":[...]}` | `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sh_...","content":"<output as JSON string>"}]}` |
| `{"type":"local_shell_call","id":"lsh_...","action":{...}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_lsh_...","name":"local_shell","input":<action>}]}` |
| `{"type":"apply_patch_call","id":"apc_...","operation":{...}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_apc_...","name":"apply_patch","input":<operation>}]}` |
| `{"type":"apply_patch_call_output","call_id":"...","output":"..."}` | `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_apc_...","content":"..."}]}` |
| `{"type":"mcp_list_tools","id":"mcp_lt_...","server_label":"...","tools":[...]}` | Dropped — infrastructure metadata, not a model-generated call |
| `{"type":"mcp_call","id":"mcp_c_...","name":"...","server_label":"...","arguments":"...","output":"..."}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_mcp_c_...","name":"mcp__<server_label>__<name>","input":<parsed arguments>}]}` followed by `{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_mcp_c_...","content":"<output>"}]}` |
| `{"type":"mcp_approval_request","id":"mcp_ar_...","name":"...","arguments":"..."}` | Dropped — approval flow metadata, not a model-generated call |
| `{"type":"tool_search_call","id":"ts_...","arguments":{"goal":"..."}}` | `{"role":"assistant","content":[{"type":"tool_use","id":"toolu_ts_...","name":"tool_search","input":{"goal":"..."}}]}` |

Key details for built-in tool history items:
- **ID mapping:** Built-in tool call IDs (e.g., `ws_...`, `fs_...`) map to Anthropic `toolu_` prefixed IDs using the same bidirectional ID map as function calls
- **Call-with-result items** (file_search_call, code_interpreter_call, image_generation_call, mcp_call): These items contain both the call and the result inline. They produce an assistant+user message pair: assistant with `tool_use`, user with `tool_result` containing the result data. For `mcp_call`, `arguments` is parsed from JSON string into the `input` object
- **Separate call/output items** (computer_call/computer_call_output, shell_call/shell_call_output, apply_patch_call/apply_patch_call_output): Call and output appear as separate input items, mapped to assistant `tool_use` and user `tool_result` respectively, linked by the same ID map
- **Call-only items** (web_search_call, local_shell_call): These have no separate output type. Map to assistant `tool_use` only
- **Message alternation:** These items participate in the same alternation merge logic as other input items — consecutive same-role items are merged into a single message
- **Schema consistency:** The `tool_use.input` in the Anthropic message must match the schema registered in the request direction. For tools with `action`/`operation` wrappers (web_search, computer_use, shell, local_shell, apply_patch), the call's `action`/`operation` object is placed directly as `input`. For tools with flat fields (file_search, code_interpreter, tool_search), the relevant call fields are placed as `input` properties
- **`mcp_call` imported history uses composite tool name:** Since `{"type": "mcp"}` is dropped (not converted), there is no `mcp_call_tool` registered. Imported `mcp_call` items use the composite name `mcp__<server_label>__<name>` and parse `arguments` as the `input` JSON object. This matches the MCP namespace convention used elsewhere in the proxy
- **`image_generation_call` uses `revised_prompt` for `input.prompt`:** The original prompt is not preserved in the history item. `revised_prompt` is the closest available field — it's the server's revised version of the original prompt. This is an inherent limitation of the OpenAI API format for this tool type

### `tool_choice` Interaction

When the request has `tool_choice: "none"` but includes built-in tools, the built-in tools are still registered (converted to custom tools) but `tool_choice: "none"` prevents the model from calling any of them. This matches the existing behavior for function tools.

When `tool_choice: "required"` or `tool_choice: {"type": "function", "name": "x"}`, the model is constrained to call tools. If the specified name matches a built-in tool name, the proxy resolves it correctly since all tools (built-in and function) share the same Anthropic custom tool namespace.

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
| `rate_limit_error` | `rate_limit_error` | `rate_limit_exceeded` |
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
| Edge cases | Empty content, long tool name truncation+hash, unknown tool, unknown input item types (dropped with warning), consecutive function_call merge, consecutive function_call_output merge, built-in tool conversion, built-in tool history item mapping, custom tool type registration, text.format → output_config.format, response object field completeness |
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
│   │   ├── thinking.rs      # Thinking/reasoning conversion + signature accumulation
│   │   └── signature_cache.rs # TTL-based signature cache for multi-turn thinking
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
