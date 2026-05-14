# Protocol Research Log

All protocol conversion decisions must be documented here with references to official documentation. Every mapping in the design spec should trace back to an entry in this file.

---

## 1. Anthropic `signature_delta` events during streaming for thinking blocks

### Description
Does the Anthropic Messages API return `signature_delta` events during streaming when extended thinking is enabled? This determines whether the proxy needs to handle or discard these events.

### Result
Confirmed. The Anthropic API sends `signature_delta` events inside `content_block_delta` just before `content_block_stop` for every thinking block. The event format is `{"type":"content_block_delta","index":N,"delta":{"type":"signature_delta","signature":"..."}}`. When `display: "omitted"` is set, the thinking block receives only a single `signature_delta` with no `thinking_delta` events. The proxy discards these as they are Anthropic-internal and not needed by Codex CLI [1][2].

### Reference
- [1] Anthropic Streaming Messages docs — https://platform.claude.com/docs/en/build-with-claude/streaming (see "Thinking delta" and "Signature delta" sections)
- [2] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking

---

## 2. Thinking/reasoning round-trip: Anthropic thinking ↔ OpenAI reasoning

### Description
How should the proxy handle the round-trip conversion between Anthropic's `thinking`/`redacted_thinking` blocks and OpenAI's `reasoning` items? Anthropic requires `redacted_thinking` blocks to be passed back unchanged in multi-turn conversations, but the proxy is stateless across requests and cannot persist Anthropic signatures between calls.

### Result

**Anthropic requirements:** `redacted_thinking` blocks have a `data` field containing opaque encrypted content. They must be passed back unchanged in multi-turn. `thinking` blocks carry a `signature` field that Anthropic uses to verify integrity. Failing to provide valid signatures/data may cause 400 errors [1].

**OpenAI structure:** Reasoning items have `summary` (array of `{type: "summary_text", text: "..."}`, required) and optional `encrypted_content` (opaque string) and `content` (raw reasoning text) fields [2][3].

**How existing implementations handle this:**

- **CLIProxyAPI** [4]: Forwards thinking text as `summary` in output. For input, `reasoning` items from Responses API are silently dropped — the converter only handles `message`, `function_call`, and `function_call_output` types. In the Codex→Claude direction, it validates signatures using `isFernetLikeReasoningSignature()` and maps valid ones to `encrypted_content`; invalid ones are silently dropped. It also implements a TTL-based signature cache (3 hours) for multi-turn preservation.

- **LiteLLM** [5]: Concatenates `thinking` block text into `reasoning_content` string field. `redacted_thinking` blocks are collected into `thinking_blocks` but their data is NOT included in `reasoning_content`. When converting back, thinking blocks are prepended to assistant content. Known bugs: stream_chunk_builder drops multiple thinking/redacted blocks (issue #20698), multi-turn fails when assistant message doesn't start with thinking block (issue #9020).

- **token_proxy** [6]: Generates a fake SHA-256 hash of thinking text as the `signature` field. This will cause Anthropic 400 errors because Anthropic validates that signatures match what it originally generated — SHA-256 hashes are not valid Anthropic signatures.

- **aio-coding-hub / cc-switch** [7]: Implements a "thinking signature rectifier" pattern — intercepts 400 errors from Anthropic caused by invalid signatures, strips all thinking/redacted_thinking blocks, and retries. This recovers from errors but loses all multi-turn thinking continuity.

- **codex-bridge** [8]: Completely discards `thinking` blocks — returns empty strings for both `thinking` content blocks and `thinking_delta` SSE events. No reasoning round-trip at all.

- **llm-rosetta / argo-proxy** [9]: Uses an IR (Intermediate Representation) with `ReasoningPart(reasoning=text, signature=encrypted_data)`. Preserves signatures through the IR. Maps Anthropic `signature` to OpenAI `encrypted_content`. Architecturally correct but complex.

**Evaluation:** No existing implementation fully solves the stateless multi-turn problem. The approaches fall into categories: (a) drop thinking entirely (codex-bridge), (b) fake signatures that break (token_proxy), (c) strip on error sacrificing continuity (aio-coding-hub), (d) silently drop reasoning input (CLIProxyAPI), (e) IR-based preservation (llm-rosetta, architecturally correct but complex). CLIProxyAPI's signature cache is the most pragmatic partial solution.

**Design decision for codex-conv:** Use the signature cache approach inspired by CLIProxyAPI:
1. **Output (Anthropic → Responses API):** Forward thinking text as `summary` content. Accumulate `signature_delta` data during streaming and cache it (keyed by response ID + content block index, TTL 3 hours). Discard signatures from the SSE stream.
2. **Input (Responses API → Anthropic):** When `reasoning` input items contain `encrypted_content`, attempt to use it as `redacted_thinking.data`. When they contain only `summary` text, use `{"type": "thinking", "thinking": "<text>", "signature": ""}` with empty signature.
3. **Limitation:** If `encrypted_content` is absent and the signature cache has expired, Anthropic may reject the request. In that case, implement the rectifier pattern as a fallback: strip thinking blocks and retry.

This approach maximizes compatibility: it preserves thinking when signatures are available (like llm-rosetta), falls back gracefully (like aio-coding-hub), and doesn't fabricate data (unlike token_proxy).

### Reference
- [1] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking (see "Redacted thinking blocks" section)
- [2] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [3] OpenAI Reasoning guide — https://developers.openai.com/api/docs/guides/reasoning
- [4] CLIProxyAPI source — https://github.com/router-for-me/CLIProxyAPI (see `claude_openai-responses_response.go`, `codex_claude_request.go`, `signature_cache.go`)
- [5] LiteLLM source — https://github.com/BerriAI/litellm (see anthropic message conversion, thinking_blocks handling)
- [6] token_proxy source — https://github.com/mxyhi/token_proxy (see `thinking_signature` function)
- [7] aio-coding-hub source — https://github.com/dyndynjyxa/aio-coding-hub (see `thinking_signature_rectifier_400.rs`)
- [8] codex-bridge source — https://github.com/nicholasyangyang/codex-bridge
- [9] llm-rosetta source — https://github.com/Oaklight/llm-rosetta (see `content_ops.py`, `ReasoningPart` IR)

---

## 3. Anthropic `budget_tokens` deprecation and rejection

### Description
Is `budget_tokens` (manual extended thinking) still supported on current Claude models? This determines whether the proxy can use `budget_tokens` or must use `output_config.effort`.

### Result
Confirmed. On Opus 4.7, manual extended thinking with `budget_tokens` returns a 400 error — it is no longer supported. On Opus 4.6 and Sonnet 4.6, `budget_tokens` is deprecated but still functional. The official wording states it "will be removed in a future release." The proxy must use `thinking: {type: "adaptive"}` with `output_config.effort` instead [1][2].

### Reference
- [1] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking (see "Supported models" section)
- [2] Anthropic Effort docs — https://platform.claude.com/docs/en/build-with-claude/effort

---

## 4. Anthropic `output_config.effort` as modern thinking control

### Description
How does `output_config.effort` work? Is it the replacement for `budget_tokens`? Where is it placed in the request?

### Result
Confirmed. The `effort` parameter is placed inside `output_config` (a separate top-level field), not inside the `thinking` object. It controls all tokens in the response (text, tool calls, thinking), not just thinking tokens. It does not require thinking to be enabled. For all current Claude models, combine with `thinking: {type: "adaptive"}` [1].

### Reference
- [1] Anthropic Effort docs — https://platform.claude.com/docs/en/build-with-claude/effort

---

## 5. Effort level mapping: OpenAI `reasoning.effort` ↔ Anthropic `output_config.effort`

### Description
How should effort levels map between the two APIs? Anthropic supports `low`, `medium`, `high`, `xhigh`, `max` while OpenAI supports `low`, `medium`, `high`, `xhigh`, `minimal`, `none`. Several values have no direct equivalent.

### Result

**Anthropic levels:** `low`, `medium`, `high` (default when omitted), `xhigh` (Opus 4.7 only), `max` (Opus 4.7/4.6, Sonnet 4.6) [1].

**OpenAI levels:** `low`, `medium`, `high`, `xhigh`, `minimal`, `none` [2].

**How existing implementations handle this:**

- **CLIProxyAPI** [3]: `minimal` → `"low"`. `xhigh` → `"max"` if model supports it, else `"high"`. `max` → `"max"` if supported, else `"high"`. `auto` → `"high"`. `none` → sets `thinking.type: "disabled"`. Also has legacy `ConvertLevelToBudget` with integer mappings (e.g., `xhigh` → 32768, `max` → 128000).

- **LiteLLM** [4]: Maps `reasoning_effort` values to `budget_tokens` integers. Supports `low/medium/high/xhigh/max/minimal`.

- **token_proxy** [5]: Direct string forwarding for matching levels.

**Evaluation:** CLIProxyAPI's approach is the most comprehensive. The key decisions are: `minimal` → `"low"` (Anthropic has no minimal), `xhigh` → `"max"` for supported models (Opus 4.7), `none` → disable thinking entirely.

**Design decision for codex-conv:** Follow CLIProxyAPI's mapping with model-aware gating: `minimal` → `"low"`, `none` → omit thinking and output_config, `xhigh` → `"xhigh"` (not `"max"` — they are semantically different on Anthropic; `"xhigh"` is the direct match for OpenAI's `"xhigh"`). Models that don't support a level will return 400 from Anthropic, which is correct behavior — the proxy should not silently downgrade.

### Reference
- [1] Anthropic Effort docs — https://platform.claude.com/docs/en/build-with-claude/effort (see "Effort levels" table)
- [2] OpenAI Reasoning guide — https://developers.openai.com/api/docs/guides/reasoning
- [3] CLIProxyAPI source: `internal/thinking/convert.go` — https://github.com/router-for-me/CLIProxyAPI
- [4] LiteLLM source — https://github.com/BerriAI/litellm
- [5] token_proxy source — https://github.com/mxyhi/token_proxy

---

## 6. Temperature and top_p handling when thinking is enabled

### Description
Must temperature and top_p be restricted when thinking is enabled? How should the proxy handle these parameters?

### Result

**Anthropic constraints:** "Thinking isn't compatible with `temperature` or `top_k` modifications." Temperature must be 1.0 or omitted. `top_p` range is 0.95–1.0 when thinking enabled. For Opus 4.7, setting `temperature`/`top_p`/`top_k` to non-default values returns 400 even when thinking is disabled [1].

**OpenAI range:** Temperature 0–2, top_p 0–1. Codex CLI never sends `temperature` [2].

**How existing implementations handle this:**

- **CLIProxyAPI** [3]: In Chat Completions path, passes temperature and top_p through directly — no special handling when thinking is enabled. In Responses API path, temperature and top_p are not handled at all (parameters would be lost).

- **LiteLLM** [4]: Passes temperature through; does not strip based on thinking state.

- **codex-bridge** [5]: No temperature/top_p handling.

**Evaluation:** None of the implementations handle this correctly. Since Codex CLI never sends `temperature`, the practical risk is low, but the proxy should still protect against invalid combinations.

**Design decision for codex-conv:** When `reasoning` is present (thinking enabled): omit `temperature` from the Anthropic request regardless of what was sent, clamp `top_p` to the 0.95–1.0 range. When `reasoning` is absent: pass temperature through with range clamping (OpenAI 0–2 → Anthropic 0–1). This is safer than any existing implementation without being unnecessarily complex.

### Reference
- [1] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking (see "Feature compatibility" section)
- [2] Codex CLI source — https://github.com/openai/codex (Codex never sends temperature in requests)
- [3] CLIProxyAPI source: `claude_openai-request.go`, `claude_openai-responses_request.go` — https://github.com/router-for-me/CLIProxyAPI
- [4] LiteLLM source — https://github.com/BerriAI/litellm
- [5] codex-bridge source — https://github.com/nicholasyangyang/codex-bridge

---

## 7. Stop reason mapping: Anthropic `stop_reason` → Responses API `status`

### Description
How should Anthropic stop reasons map to Responses API status values? Some Anthropic stop reasons (like `pause_turn`, `refusal`, `model_context_window_exceeded`) have no direct Responses API equivalent.

### Result

**Anthropic stop reasons** [1]: `end_turn`, `max_tokens`, `stop_sequence`, `tool_use`, `pause_turn`, `refusal`, `model_context_window_exceeded`.

**Responses API statuses** [2]: `completed`, `failed`, `in_progress`, `cancelled`, `queued`, `incomplete`. The `incomplete_details.reason` can be `"max_output_tokens"` or `"content_filter"`.

**How existing implementations handle this:**

- **CLIProxyAPI** [3]: In the Claude→Responses direction, **always emits `response.completed` with `status: "completed"`** regardless of Anthropic's `stop_reason`. It does not read Claude's stop_reason to set a different status. The `incomplete_details` field is hardcoded to `null`. It never emits `response.incomplete`. In the Codex→Claude direction, `pause_turn` and `refusal` are passed through as-is.

- **LiteLLM** [4]: Maps `end_turn` → `stop`, `max_tokens` → `length`, `tool_use` → `tool_calls`. No handling of `pause_turn`, `refusal`, or `model_context_window_exceeded` in the Responses API path.

- **codex-bridge** [5]: No stop reason mapping — always returns completed.

**Evaluation:** CLIProxyAPI's approach of always returning `"completed"` is the simplest but loses information. It means Codex CLI never sees `incomplete` status when `max_tokens` is hit, which could cause it to misinterpret truncated responses as complete.

**Design decision for codex-conv:** Map `max_tokens` → `"incomplete"` with `incomplete_details.reason: "max_output_tokens"` and emit `response.incomplete` (not `response.completed`). This is more correct than CLIProxyAPI. For `pause_turn` → `"completed"` (Claude server tools iteration limit, not applicable in proxy context). For `refusal` → `"completed"` (model declined to generate). For `model_context_window_exceeded` → `"incomplete"` with `incomplete_details.reason: "max_output_tokens"` (closest semantic match). This preserves the critical distinction between normal completion and truncation.

### Reference
- [1] Anthropic Handling Stop Reasons docs — https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
- [2] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [3] CLIProxyAPI source: `claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI
- [4] LiteLLM source — https://github.com/BerriAI/litellm
- [5] codex-bridge source — https://github.com/nicholasyangyang/codex-bridge

---

## 8. Anthropic rate limit error type

### Description
What error type does Anthropic return for rate limiting (HTTP 429)?

### Result
Confirmed. Anthropic returns `rate_limit_error` as the error type for HTTP 429 responses. The error format is `{"type": "error", "error": {"type": "rate_limit_error", "message": "..."}}`. This is distinct from 529 `overloaded_error` which indicates API overload [1].

### Reference
- [1] Anthropic Errors docs — https://platform.claude.com/docs/en/api/errors

---

## 9. Reasoning item structure: `summary` vs `content` fields

### Description
Does the Responses API reasoning item use `summary` or `content` for its text data? How should the proxy map Anthropic thinking text?

### Result

**OpenAI structure** [1][2]: Reasoning items have three content fields: `summary` (required, array of `{type: "summary_text", text: "..."}`), `content` (optional, array of `{type: "reasoning_text", text: "..."}` for raw reasoning text), and `encrypted_content` (optional opaque string).

**How existing implementations handle this:**

- **CLIProxyAPI** [3]: Maps Anthropic `thinking_delta` text to `response.reasoning_summary_text.delta`. Emits `reasoning_summary_part.added` before the first delta. In the final output, includes `summary: [{type: "summary_text", text: <content>}]`. Does NOT populate `content` or `encrypted_content` in the output direction.

- **LiteLLM** [4]: Concatenates thinking text into a flat `reasoning_content` string field (Chat Completions format). Does not use Responses API reasoning item structure.

- **codex-bridge** [5]: Discards thinking entirely.

- **llm-rosetta** [6]: Maps Anthropic `thinking` text to `summary` and `signature` to `encrypted_content` through an IR.

**Evaluation:** CLIProxyAPI's approach of using `summary` for thinking text is the most correct and widely compatible. The `content` field is for raw reasoning text from GPT-OSS models and is not applicable to Anthropic thinking.

**Design decision for codex-conv:** Map Anthropic thinking text to `summary: [{type: "summary_text", text: "..."}]`. Use `response.reasoning_summary_text.delta`/`.done` events for streaming. Do not populate `content` field (no raw reasoning text from Anthropic). This matches CLIProxyAPI and llm-rosetta.

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [2] OpenAI Reasoning guide — https://developers.openai.com/api/docs/guides/reasoning
- [3] CLIProxyAPI source: `claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI
- [4] LiteLLM source — https://github.com/BerriAI/litellm
- [5] codex-bridge source — https://github.com/nicholasyangyang/codex-bridge
- [6] llm-rosetta source — https://github.com/Oaklight/llm-rosetta

---

## 10. Reasoning SSE event names: `reasoning_summary_text` vs `reasoning_text`

### Description
What are the correct SSE event names for reasoning streaming? Both `response.reasoning_summary_text.delta` and `response.reasoning_text.delta` exist — which should the proxy use?

### Result

**Both event types exist** [1]: `response.reasoning_summary_text.delta`/`.done` (for `summary` field, uses `summary_index`) and `response.reasoning_text.delta`/`.done` (for `content` field, uses `content_index`).

**How existing implementations handle this:**

- **CLIProxyAPI** [2]: Uses `response.reasoning_summary_text.delta` and `response.reasoning_summary_text.done` exclusively. Emits `response.reasoning_summary_part.added` before the first text delta. Does NOT emit `response.reasoning_text.delta`/`.done` events.

- **LiteLLM** [3]: Uses Chat Completions format, not Responses API streaming events.

**Evaluation:** Since the proxy maps Anthropic thinking to `summary` (not `content`), only the `reasoning_summary_text` events are needed. This is consistent across all implementations that use Responses API streaming.

**Design decision for codex-conv:** Use `response.reasoning_summary_text.delta`/`.done` for streaming thinking text. Do NOT emit `response.reasoning_text.delta`/`.done` events (these are for raw reasoning content from GPT-OSS models). This matches CLIProxyAPI.

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [2] CLIProxyAPI source: `claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI
- [3] LiteLLM source — https://github.com/BerriAI/litellm

---

## 11. `response.incomplete` event for truncated responses

### Description
Should the proxy emit `response.incomplete` when Anthropic returns `max_tokens` as the stop reason? How do existing implementations handle this?

### Result

**OpenAI defines** `response.incomplete` as a terminal streaming event for incomplete responses, with `incomplete_details.reason` being `"max_output_tokens"` or `"content_filter"` [1].

**How existing implementations handle this:**

- **CLIProxyAPI** [2]: **Never emits `response.incomplete`.** Always emits `response.completed` with `status: "completed"` and `incomplete_details: null`, regardless of Anthropic's `stop_reason`. This means Codex CLI receives `"completed"` status even when the response was truncated by `max_tokens`.

- **LiteLLM** [3]: Does not emit `response.incomplete` in the Anthropic conversion path.

- **codex-bridge** [4]: Does not emit `response.incomplete`.

**Evaluation:** No existing implementation emits `response.incomplete`. CLIProxyAPI's approach of always returning `"completed"` means the downstream client (Codex CLI) cannot distinguish between a complete response and a truncated one. This is a known inaccuracy.

**Design decision for codex-conv:** Emit `response.incomplete` (not `response.completed`) when Anthropic returns `stop_reason: "max_tokens"`. Set `status: "incomplete"` and `incomplete_details: {reason: "max_output_tokens"}`. This is more correct than CLIProxyAPI's approach and gives Codex CLI proper signal about truncation. The risk is that Codex CLI may not handle `response.incomplete` well if it was designed around CLIProxyAPI's behavior, but the Responses API spec defines this event explicitly.

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [2] CLIProxyAPI source: `claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI
- [3] LiteLLM source — https://github.com/BerriAI/litellm
- [4] codex-bridge source — https://github.com/nicholasyangyang/codex-bridge

---

## 12. OpenAI `function_call_arguments.delta` event fields

### Description
Does the `response.function_call_arguments.delta` event include a `call_id` field?

### Result
No. The `response.function_call_arguments.delta` event has fields: `type`, `delta`, `item_id`, `output_index`, `sequence_number`. There is no `call_id` field. Call association uses `item_id` and `output_index`. The `call_id` is available on the parent output item established by the preceding `response.output_item.added` event. The `.done` event similarly has no `call_id` — it has `arguments`, `name`, `output_index`, and `sequence_number` [1].

CLIProxyAPI confirms this: its delta event template only includes `item_id`, `output_index`, `delta`, and `sequence_number` [2].

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [2] CLIProxyAPI source: `claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI

---

## 13. OpenAI service tiers

### Description
What service tiers does the OpenAI Responses API support? Is `"scale"` a valid tier?

### Result
Confirmed. The `service_tier` field accepts: `"auto"`, `"default"`, `"flex"`, `"priority"`, `"scale"`, or `null`. All five named tiers are valid values. `"auto"` delegates to project settings. Anthropic has no equivalent for `"flex"`, `"priority"`, or `"scale"` [1].

### Reference
- [1] OpenAI Responses API reference — https://developers.openai.com/api/reference/responses/overview/ (service_tier field definition)

---

## 14. OpenAI Responses API built-in tool types

### Description
What built-in tool types does the Responses API support? Which does Codex CLI use?

### Result
The Responses API supports: `web_search`, `file_search`, `code_interpreter`, `computer_use`, `image_generation`, `mcp`, `shell`, `local_shell`, `apply_patch`, `function`, `tool_search`, and others. Codex CLI uses only `function`-type tools (source: `openai/codex` repository). Codex sends `exec_command` as its primary shell tool and `apply_patch` as a freeform function tool. MCP tools are sent as namespaced function tools. Codex does not send Responses API built-in tool types [1][2][3].

### Reference
- [1] OpenAI Using Tools guide — https://developers.openai.com/api/docs/guides/tools
- [2] Codex CLI source: `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs
- [3] Codex CLI source: `codex-rs/core/src/tools/handlers/apply_patch.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/apply_patch.rs

---

## 15. Codex CLI tool architecture (`exec_command` vs old `shell`)

### Description
What tool does Codex CLI send for shell command execution? Is it `shell`, `container.exec`, or `exec_command`?

### Result
The current Rust-based Codex CLI (in `openai/codex` repository) uses `exec_command` as the primary tool, with `ToolKind::Function`. The tool name is `"exec_command"` (verified in `exec_command.rs`: `ToolName::plain("exec_command")`). Many external articles from 2025 reference `shell` or `container.exec` — these refer to the old Node.js implementation. The `intercept_apply_patch` function in `exec_command.rs` intercepts `apply_patch` commands sent via `exec_command` and routes them to the patch handler, emitting a warning to use the `apply_patch` tool instead [1][2].

### Reference
- [1] Codex CLI source: `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs
- [2] Codex CLI source: `codex-rs/core/src/tools/handlers/apply_patch.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/apply_patch.rs

---

## 16. Codex CLI `apply_patch` tool forms

### Description
In what forms does Codex CLI send `apply_patch`? Is it a built-in Responses API tool or a function tool?

### Result
`apply_patch` exists in two forms in the current Codex CLI: (1) As a freeform function tool with its own handler (`ApplyPatchHandler`), tool name `"apply_patch"`, `ToolKind::Function`, using `FreeformToolFormat` with Lark grammar — this is the preferred form. (2) As a command routed through `exec_command` where `intercept_apply_patch` detects and intercepts it, routing to the patch handler with a warning message. Both forms are `function`-type tools from the proxy's perspective — no Responses API built-in tool type is involved [1][2][3].

### Reference
- [1] Codex CLI source: `codex-rs/core/src/tools/handlers/apply_patch.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/apply_patch.rs
- [2] Codex CLI source: `codex-rs/core/src/tools/handlers/apply_patch_spec.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/apply_patch_spec.rs
- [3] Codex CLI system prompt: `codex-rs/core/prompt_with_apply_patch_instructions.md` — https://github.com/openai/codex/blob/main/codex-rs/core/prompt_with_apply_patch_instructions.md

---

## 17. OpenAI `summary_text` content type structure

### Description
What is the exact structure of the `summary_text` type within reasoning items?

### Result
Confirmed. The structure is `{"type": "summary_text", "text": "..."}`. No other fields exist — no `annotations`, no `logprobs`. This same structure appears in `response.reasoning_summary_part.added` (carrying a `part`), `response.reasoning_summary_text.delta` (carrying partial `delta`), and `response.reasoning_summary_text.done` (carrying finalized `text`) [1].

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
