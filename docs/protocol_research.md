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

**Anthropic constraints:** "Thinking isn't compatible with `temperature` or `top_k` modifications." Temperature must be 1.0 or omitted. `top_p` range is 0.95–1.0 when thinking enabled [1]. For Opus 4.7, setting `temperature`/`top_p`/`top_k` to non-default values returns 400 **unconditionally** (regardless of whether thinking is enabled or disabled) — this is a model-level restriction, not a thinking-mode restriction [6][7].

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
- [6] Anthropic "What's new in Claude Opus 4.7" — https://platform.claude.com/docs/en/about-claude/models/whats-new-claude-4-7 (confirms temperature/top_p/top_k are deprecated unconditionally for Opus 4.7)
- [7] Anthropic Migration Guide — https://platform.claude.com/docs/en/about-claude/models/migration-guide (confirms sampling parameters removed for Opus 4.7, recommends omitting them entirely)

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

---

## 18. OpenAI `response.reasoning_summary_part.added` event existence and fields

### Description
Does the event `response.reasoning_summary_part.added` officially exist in the OpenAI Responses API streaming events spec? What are its fields and when is it emitted? What is its relationship to `response.reasoning_summary_text.delta`?

### Result

**Official existence:** Confirmed. The event is documented in the OpenAI Responses API streaming events reference [1]. It appears alongside `response.reasoning_summary_part.done`.

**Fields:**
- `type`: always `"response.reasoning_summary_part.added"`
- `item_id`: string — ID of the reasoning item this summary part belongs to
- `output_index`: number — index of the output item
- `part`: object `{ text, type }` where `type` is always `"summary_text"` and `text` is the summary part text
- `sequence_number`: number — event sequence number
- `summary_index`: number — index of the summary part within the reasoning summary

**Emission timing:** Emitted when a new reasoning summary part is added. In practice, it is emitted once at the start of a reasoning block (before any text deltas), carrying an empty or initial `part`.

**Relationship to `response.reasoning_summary_text.delta`:**
- `response.reasoning_summary_part.added` signals the creation of a summary part container.
- `response.reasoning_summary_text.delta` carries incremental text (`delta: string`) for that part.
- `response.reasoning_summary_text.done` signals completion of the text, carrying the full `text`.
- `response.reasoning_summary_part.done` signals the part itself is completed, carrying the final `part` object.

In other words: `part.added` opens the part, `text.delta` streams the content, `text.done` finalizes the text stream, and `part.done` closes the part.

**CLIProxyAPI behavior:** CLIProxyAPI emits `response.reasoning_summary_part.added` when it sees a Claude `content_block_start` with `type: "thinking"`. It sends the event with an empty `part.text` before emitting any `response.reasoning_summary_text.delta` events. On `content_block_stop`, it emits `response.reasoning_summary_text.done` followed by `response.reasoning_summary_part.done` [2].

### Reference
- [1] OpenAI Responses streaming events reference — https://developers.openai.com/api/reference/resources/responses/streaming-events/
- [2] CLIProxyAPI source: `internal/translator/claude/openai/responses/claude_openai-responses_response.go` — https://github.com/router-for-me/CLIProxyAPI

---

## 19. Anthropic `service_tier` accepted values

### Description
What values does the Anthropic Messages API accept for the `service_tier` parameter? The spec maps OpenAI `"default"` → `"standard_only"` and `"auto"` → `"auto"` — are these correct?

### Result
Confirmed. The Anthropic Messages API accepts exactly two values for `service_tier`: `"auto"` and `"standard_only"`. The spec's mapping of OpenAI `"default"` → Anthropic `"standard_only"` and `"auto"` → `"auto"` is correct [1].

Note: the Anthropic response `usage` object returns a different field (`usage.service_tier`) with values `"standard"`, `"priority"`, or `"batch"` — these describe which tier was actually used, not input parameter values.

### Reference
- [1] Anthropic Messages API docs — https://platform.claude.com/docs/en/api (see `service_tier` parameter)

---

## 20. Codex CLI `update_plan` tool

### Description
Does the Codex CLI have a tool called `update_plan`? What type is it and when is it sent?

### Result
Confirmed. `update_plan` is a function-type tool (`ToolKind::Function`) in the current Rust-based Codex CLI. It is always registered in the tool registry. The handler is `PlanHandler` in `codex-rs/core/src/tools/handlers/plan.rs`. The tool spec (in `plan_spec.rs`) defines it as a `ResponsesApiTool` with parameters: `explanation` (optional string) and `plan` (required array of `{step, status}` objects). It is a non-mutating tool (no filesystem or environment changes). It is not allowed in Plan mode [1][2].

From the proxy's perspective, `update_plan` is a regular function-type tool (like `exec_command` and `apply_patch`) — no special handling needed beyond standard function tool conversion.

### Reference
- [1] Codex CLI source: `codex-rs/core/src/tools/handlers/plan.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/plan.rs
- [2] Codex CLI source: `codex-rs/core/src/tools/handlers/plan_spec.rs` — https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/plan_spec.rs

---

## 21. Complete field inventory: directly mappable fields

### Description
Which fields have a clear semantic mapping between the OpenAI Responses API and the Anthropic Messages API? This entry provides a comprehensive inventory of all fields that the proxy can convert between the two protocols, grouped by direction and transform complexity.

### Result

#### A. Request Direction: Responses API → Anthropic Messages API

##### A1. Top-level parameters with direct mapping

| Responses API field | Anthropic field | Transform | Source |
|---|---|---|---|
| `model` | `model` | Passthrough (string) | OpenAI [1], Anthropic [2] |
| `max_output_tokens` | `max_tokens` | Field rename (integer) | OpenAI [1], Anthropic [2] |
| `temperature` | `temperature` | Range clamping: OpenAI 0–2 → Anthropic 0–1. Values >1 clamped to 1. When thinking enabled or Opus 4.7: omit | OpenAI [1], Anthropic [2], entry #6 |
| `top_p` | `top_p` | Passthrough. When thinking enabled: clamp to 0.95–1.0. Opus 4.7: omit | OpenAI [1], Anthropic [2], entry #6 |
| `stream` | `stream` | Always forced `true` (proxy architecture requires upstream streaming) | OpenAI [1], Anthropic [3] |
| `metadata.user_id` | `metadata.user_id` | Direct passthrough (string, max 256 chars on Anthropic) | OpenAI [1], Anthropic [2] |
| `service_tier` | `service_tier` | Value mapping: `"auto"` → `"auto"`, `"default"` → `"standard_only"`, `"flex"`/`"priority"`/`"scale"` → omit | OpenAI [1], Anthropic [2], entry #19 |
| `instructions` | `system` | String → wrapped as `[{type: "text", text: "..."}]` array | OpenAI [1], Anthropic [2] |

##### A2. Parameters with structural transform

| Responses API field | Anthropic field | Transform | Source |
|---|---|---|---|
| `reasoning.effort` | `thinking.type` + `output_config.effort` | Set `thinking.type: "adaptive"`, forward effort string directly. `"none"` → omit both. `"minimal"` → `"low"` | OpenAI [1], Anthropic [2], entries #3/#4/#5 |
| `tool_choice` | `tool_choice` | `"auto"` → `{type:"auto"}`, `"required"` → `{type:"any"}`, `"none"` → `{type:"none"}`, `{type:"function",name}` → `{type:"tool",name}` | OpenAI [1], Anthropic [2] |
| `parallel_tool_calls` | `tool_choice.disable_parallel_tool_use` | Semantics inverted. Embedded inside `tool_choice` object, not top-level | OpenAI [1], Anthropic [2] |
| `tools` (function) | `tools` | `parameters` → `input_schema`. Namespace tools flattened: `mcp__{server}__{tool}` | OpenAI [1], Anthropic [2] |

##### A3. Input item type mapping

| Responses API input item | Anthropic message | Transform | Source |
|---|---|---|---|
| `{type:"message", role:"user"}` | `{role:"user", content:[...]}` | Content block mapping (see A4) | OpenAI [1], Anthropic [2] |
| `{type:"message", role:"assistant"}` | `{role:"assistant", content:[...]}` | Content block mapping | OpenAI [1], Anthropic [2] |
| `{type:"message", role:"system"}` | Top-level `system` parameter | Extracted from input, merged with `instructions` | OpenAI [1], Anthropic [2] |
| `{type:"function_call", call_id, name, arguments}` | `{role:"assistant", content:[{type:"tool_use", id, name, input}]}` | `call_id` → `toolu_` prefixed id. `arguments` (JSON string) → `input` (JSON object). Namespace restoration | OpenAI [1], Anthropic [2] |
| `{type:"function_call_output", call_id, output}` | `{role:"user", content:[{type:"tool_result", tool_use_id, content}]}` | `call_id` → mapped `toolu_` id. `output` (string) → `content` (string) | OpenAI [1], Anthropic [2] |
| `{type:"reasoning", encrypted_content, summary}` | `{role:"assistant", content:[{type:"redacted_thinking", data}]}` | If `encrypted_content` present | OpenAI [1], Anthropic [2], entry #2 |
| `{type:"reasoning", summary}` (no encrypted_content) | `{role:"assistant", content:[{type:"thinking", thinking, signature}]}` | Signature from cache or empty string | OpenAI [1], Anthropic [2], entry #2 |

##### A4. User content block mapping

| Responses API content block | Anthropic content block | Source |
|---|---|---|
| `{type:"input_text", text}` | `{type:"text", text}` | OpenAI [1], Anthropic [2] |
| `{type:"input_image", image_url:"https://..."}` | `{type:"image", source:{type:"url", url:"..."}}` | OpenAI [1], Anthropic [2] |
| `{type:"input_image", image_url:{url:"data:image/png;base64,..."}}` | `{type:"image", source:{type:"base64", media_type:"image/png", data:"..."}}` | OpenAI [1], Anthropic [2] |

##### A5. Cache control passthrough

Anthropic natively supports `cache_control` on content blocks, system blocks, and tool definitions [2]. Codex may send `cache_control: {type: "ephemeral"}` — preserve as-is on corresponding Anthropic content blocks. LiteLLM also preserves cache_control markers [4].

#### B. Response Direction: Anthropic Messages API → Responses API

##### B1. Top-level response fields

| Anthropic field | Responses API field | Transform | Source |
|---|---|---|---|
| `id` | `id` | Passthrough (e.g., `msg_01XFDUDYJgAACzvnptvVoYEL`) | Anthropic [2], OpenAI [5] |
| (generated) | `object` | Hardcoded `"response"` | OpenAI [5] |
| (generated) | `created_at` | Proxy-generated Unix timestamp | OpenAI [5] |
| (generated) | `completed_at` | Proxy-generated Unix timestamp (when completed) | OpenAI [5] |
| `stop_reason` | `status` | See B3 | Anthropic [2], OpenAI [5] |
| `stop_reason` + `usage` | `incomplete_details` | `max_tokens`/`model_context_window_exceeded` → `{reason:"max_output_tokens"}` | Anthropic [2], OpenAI [5], entry #7 |

##### B2. Content block → output item mapping

| Anthropic content block | Responses API output item | Source |
|---|---|---|
| `{type:"text", text}` | `{type:"message", role:"assistant", content:[{type:"output_text", text, annotations:[]}]}` | Anthropic [2], OpenAI [5] |
| `{type:"thinking", thinking}` | `{type:"reasoning", id:"rs_xxx", summary:[{type:"summary_text", text}]}`. `encrypted_content` omitted | Anthropic [2], OpenAI [5], entries #9/#17 |
| `{type:"tool_use", id, name, input}` | `{type:"function_call", id:"fc_xxx", call_id:"call_xxx", name, namespace, arguments}`. `input` (object) → `arguments` (JSON string) | Anthropic [2], OpenAI [5] |

##### B3. Stop reason → status mapping

| Anthropic `stop_reason` | Responses API `status` | Terminal event | Source |
|---|---|---|---|
| `end_turn` | `"completed"` | `response.completed` | Anthropic [6], OpenAI [5], entry #7 |
| `stop_sequence` | `"completed"` | `response.completed` | Anthropic [6], OpenAI [5] |
| `tool_use` | `"completed"` | `response.completed` | Anthropic [6], OpenAI [5] |
| `pause_turn` | `"completed"` | `response.completed` | Anthropic [6], OpenAI [5] |
| `refusal` | `"completed"` | `response.completed` | Anthropic [6], OpenAI [5] |
| `max_tokens` | `"incomplete"` | `response.incomplete` | Anthropic [6], OpenAI [5], entry #7 |
| `model_context_window_exceeded` | `"incomplete"` | `response.incomplete` | Anthropic [6], OpenAI [5], entry #7 |

##### B4. Usage mapping

| Anthropic field | Responses API field | Transform | Source |
|---|---|---|---|
| `usage.input_tokens` | `usage.input_tokens` | Passthrough | Anthropic [2], OpenAI [5] |
| `usage.output_tokens` | `usage.output_tokens` | Passthrough | Anthropic [2], OpenAI [5] |
| `usage.cache_read_input_tokens` | `usage.input_tokens_details.cached_tokens` | Field rename | Anthropic [2], OpenAI [5] |
| `usage.cache_creation_input_tokens` | (folded into `input_tokens`) | No separate field in Responses API | Anthropic [2], OpenAI [5] |
| (computed) | `usage.total_tokens` | `input_tokens + output_tokens` | OpenAI [5] |

##### B5. Streaming event mapping

Full event-by-event mapping documented in spec Streaming Conversion section. Key events:

| Anthropic SSE event | Responses API SSE event | Source |
|---|---|---|
| `message_start` | `response.created` + `response.in_progress` | Anthropic [3], OpenAI [7], entry #1 |
| `content_block_start` (text) | `response.output_item.added` + `response.content_part.added` | Anthropic [3], OpenAI [7] |
| `content_block_delta` (text_delta) | `response.output_text.delta` | Anthropic [3], OpenAI [7] |
| `content_block_stop` (text) | `response.output_text.done` + `response.content_part.done` + `response.output_item.done` | Anthropic [3], OpenAI [7] |
| `content_block_start` (thinking) | `response.output_item.added` + `response.reasoning_summary_part.added` | Anthropic [3], OpenAI [7], entry #18 |
| `content_block_delta` (thinking_delta) | `response.reasoning_summary_text.delta` | Anthropic [3], OpenAI [7], entries #10/#18 |
| `content_block_delta` (signature_delta) | (consumed, no event) | Anthropic [3], entry #1 |
| `content_block_stop` (thinking) | `response.reasoning_summary_text.done` + `response.reasoning_summary_part.done` + `response.output_item.done` | Anthropic [3], OpenAI [7], entry #18 |
| `content_block_start` (tool_use) | `response.output_item.added` | Anthropic [3], OpenAI [7] |
| `content_block_delta` (input_json_delta) | `response.function_call_arguments.delta` | Anthropic [3], OpenAI [7], entry #12 |
| `content_block_stop` (tool_use) | `response.function_call_arguments.done` + `response.output_item.done` | Anthropic [3], OpenAI [7] |
| `message_delta` (stop_reason, usage) | Internal state recorded | Anthropic [3], OpenAI [7] |
| `message_stop` | `response.completed` or `response.incomplete` + `[DONE]` | Anthropic [3], OpenAI [7], entry #11 |
| `error` (streaming) | `error` + `response.failed` + `[DONE]` | Anthropic [2], OpenAI [7] |

##### B6. Error mapping

| Anthropic `error.type` | Responses API `error.type` | `error.code` | Source |
|---|---|---|---|
| `invalid_request_error` | `invalid_request_error` | `invalid_request` | Anthropic [2], OpenAI [5] |
| `authentication_error` | `invalid_request_error` | `invalid_api_key` | Anthropic [2], OpenAI [5] |
| `permission_error` | `invalid_request_error` | `invalid_api_key` | Anthropic [2], OpenAI [5] |
| `not_found_error` | `invalid_request_error` | `model_not_found` | Anthropic [2], OpenAI [5] |
| `request_too_large` | `invalid_request_error` | `request_too_large` | Anthropic [2], OpenAI [5] |
| `rate_limit_error` | `rate_limit_error` | `rate_limit_exceeded` | Anthropic [2], OpenAI [5], entry #8 |
| `api_error` | `server_error` | `server_error` | Anthropic [2], OpenAI [5] |
| `overloaded_error` | `server_error` | `server_error` | Anthropic [2], OpenAI [5] |

### Reference
- [1] OpenAI Responses API — https://developers.openai.com/api/reference/responses/overview/
- [2] Anthropic Messages API — https://platform.claude.com/docs/en/api
- [3] Anthropic Streaming Messages — https://platform.claude.com/docs/en/build-with-claude/streaming
- [4] LiteLLM source — https://github.com/BerriAI/litellm (cache_control passthrough)
- [5] OpenAI Responses API response fields — https://developers.openai.com/api/reference/responses/overview/ (response object)
- [6] Anthropic Handling Stop Reasons — https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
- [7] OpenAI Responses streaming events — https://developers.openai.com/api/reference/resources/responses/streaming-events/

---

## 22. Complete field inventory: unmappable/dropped fields

### Description
Which fields from each API have NO meaningful equivalent on the other side and must be dropped, ignored, or fabricated? This entry documents every field that cannot be mapped, with the verified rationale for why no mapping exists.

### Result

#### A. Responses API Request Fields with No Anthropic Equivalent (Dropped)

| Responses API field | Behavior | Rationale | Source |
|---|---|---|---|
| `previous_response_id` | Ignored (stripped) | OpenAI uses server-side conversation state to chain requests. Anthropic API is stateless — every request must include full conversation history. Codex always sends full `input` array, so this field is always `None` in HTTP requests | OpenAI [1], Anthropic [2] |
| `store` | Ignored (stripped) | OpenAI server-side response storage for later retrieval. Anthropic has no server-side storage. Codex doesn't rely on stored responses (it resends full history) | OpenAI [1] |
| `include` | Ignored (stripped) | OpenAI parameter requesting encrypted reasoning content in the response (e.g., `["reasoning.encrypted_content"]`). Anthropic returns thinking content inline via `thinking` blocks, not through a separate include mechanism. The proxy discards Anthropic signatures and does not generate OpenAI-encrypted content | OpenAI [1], Anthropic [2] |
| `truncation` | Ignored (stripped) | OpenAI input context truncation strategy (`"auto"`, `"disabled"`, `"last"`). Anthropic naturally errors on context overflow (equivalent to `"disabled"` behavior). Codex never sends this field | OpenAI [1], Anthropic [2] |
| `n` | Forced to `1` | Number of response choices. Removed from Responses API spec (was in Chat Completions). Codex never sends it. Anthropic always returns a single response | OpenAI [1] |
| `metadata.*` (non-`user_id` keys) | Stripped | Anthropic `metadata` only supports `user_id` (string, max 256 chars). OpenAI `metadata` accepts arbitrary key-value pairs. Only `user_id` is forwarded | OpenAI [1], Anthropic [2] |
| `service_tier` (`"flex"`, `"priority"`, `"scale"`) | Omitted from Anthropic request | These OpenAI service tiers have no Anthropic equivalent. `"auto"` and `"default"` map to Anthropic `"auto"` and `"standard_only"`. The three unmappable tiers are silently dropped | OpenAI [1], Anthropic [2], entry #19 |
| `text.format` (JSON schema output) | Converted to `output_config.format` | OpenAI `text.format` with `type: "json_schema"` maps to Anthropic `output_config.format`. Only `schema` is forwarded — `name` and `strict` have no Anthropic equivalent (Anthropic enforces schema compliance by default). See spec `text.format` → `output_config.format` Mapping section | OpenAI [1], Anthropic [2], entry #38 |
| `text.verbosity` | Ignored (stripped) | OpenAI output verbosity control (`"low"/"medium"/"high"`). Anthropic has no equivalent parameter | OpenAI [1] |
| `user` | Ignored (stripped) | OpenAI end-user identifier. While Anthropic has `metadata.user_id`, the `user` field from OpenAI is a different mechanism. If `metadata.user_id` is present, that takes precedence. Codex does not send `user` | OpenAI [1] |
| Built-in tool types (`web_search`, `file_search`, `code_interpreter`, `computer_use`, `image_generation`) | Rejected with 400 | These are Responses API built-in tool types (not function-type). Anthropic has no equivalent for web search/file search/code interpreter as built-in tools. Codex never sends these (uses only function-type tools) | OpenAI [1], entry #14 |
| Input `reasoning.content` field | Not forwarded | OpenAI reasoning items can carry a `content` field (array of `{type: "reasoning_text", text}` for raw reasoning text from GPT-OSS models). Anthropic has no raw reasoning text concept — only `thinking` (summary) and `redacted_thinking`. The proxy maps `summary` only | OpenAI [1], entry #9 |

#### B. Responses API Request Fields Requiring Special Handling (Not Directly Dropped, Not Directly Mapped)

| Responses API field | Behavior | Notes | Source |
|---|---|---|---|
| `reasoning.encrypted_content` | Conditional conversion | If present on input reasoning item → `redacted_thinking.data`. This is the round-trip path for Anthropic encrypted content. See entry #2 | OpenAI [1], Anthropic [2], entry #2 |
| `reasoning.summary` | Conditional conversion | If no `encrypted_content` → `thinking` block with cached or empty signature. See entry #2 | OpenAI [1], Anthropic [2], entry #2 |
| `reasoning` (overall structure) | Structural conversion | OpenAI `{effort, summary}` → Anthropic `thinking.type` + `output_config.effort`. Different nesting structure | OpenAI [1], Anthropic [2], entries #3/#4/#5 |

#### C. Anthropic Response Fields with No Responses API Equivalent (Dropped)

| Anthropic field | Behavior | Rationale | Source |
|---|---|---|---|
| `type` (always `"message"`) | Dropped | Responses API uses `object: "response"` with different top-level structure. No semantic mapping | Anthropic [2], OpenAI [5] |
| `role` (always `"assistant"`) | Dropped | Implied by the output item type in Responses API. No separate field | Anthropic [2], OpenAI [5] |
| `model` (in response) | Dropped | Could map to Responses API `model` field but Codex already knows the model. Not forwarded to reduce response size | Anthropic [2], OpenAI [5] |
| `stop_sequence` | Dropped | Anthropic returns which custom stop sequence matched (if any). Responses API has no equivalent field. The `stop_sequences` feature is not used in the proxy context | Anthropic [2], OpenAI [5] |
| `usage.cache_creation_input_tokens` | Folded into `input_tokens` | Anthropic reports cache write cost. Responses API has no separate field. Folded into `input_tokens` total. Only cache reads map to `cached_tokens` — matches semantic meaning of "tokens served from cache" | Anthropic [2], OpenAI [5] |
| `usage.cache_creation` (breakdown) | Dropped | Anthropic returns cache creation breakdown by TTL (`ephemeral_5m_input_tokens`, `ephemeral_1h_input_tokens`). Responses API has no cache breakdown fields | Anthropic [2] |
| `usage.server_tool_use` | Dropped | Anthropic reports server tool use counts (e.g., `web_search_requests`). Proxy doesn't support server tools | Anthropic [2] |
| `usage.service_tier` | Dropped | Anthropic reports which tier was actually used (`"standard"`, `"priority"`, `"batch"`). Responses API has no matching response field | Anthropic [2] |
| `content[].signature` (thinking blocks) | Consumed and cached, not forwarded | Anthropic thinking signatures are encrypted internal data. They are accumulated during streaming and stored in the signature cache for multi-turn round-trip. They are never sent to Codex CLI | Anthropic [2], [3], entry #1 |
| `content[].cache_control` | Dropped | Internal Anthropic caching hint. Not meaningful to forward to Codex CLI | Anthropic [2] |
| `content[].citations` (text blocks) | Dropped | Anthropic text citation feature. Responses API uses different annotation types (`file_citation`, `url_citation`). Codex doesn't use Anthropic citations | Anthropic [2], OpenAI [5] |
| `content[].caller` (tool_use blocks) | Dropped | Anthropic tool caller information (`direct`, `code_execution_*`). Not applicable in proxy context | Anthropic [2] |
| `container` (top-level) | Dropped | Anthropic container lifecycle management. Proxy doesn't support container tools | Anthropic [2] |

#### D. Anthropic Request Fields Never Received (Codex Does Not Send)

These Anthropic request fields exist in the API but Codex CLI never generates them, so the proxy never needs to convert them:

| Anthropic field | Why Codex doesn't send it | Source |
|---|---|---|
| `top_k` | Codex never sends `top_k`. OpenAI Responses API has no equivalent | Anthropic [2] |
| `stop_sequences` | Codex never sends custom stop sequences | Anthropic [2] |
| `thinking.display` | Anthropic controls how thinking appears (`"summarized"`, `"omitted"`). Codex sends `reasoning.effort`, not `display` | Anthropic [2] |
| `thinking.budget_tokens` | Deprecated (rejected on Opus 4.7). Codex uses `reasoning.effort` → `output_config.effort` instead | Anthropic [2], entry #3 |
| `container` | Container lifecycle management. Codex doesn't use Anthropic containers | Anthropic [2] |
| `inference_geo` | Geographic inference routing. Codex doesn't send this | Anthropic [2] |
| `mcp_servers` | Anthropic native MCP support (proxy handles MCP at the Codex level via namespace flattening) | Anthropic [2] |
| `output_config.format` | Mapped from `text.format` when present. Codex sends `text.format` with JSON schema for structured output (name: `"codex_output_schema"`) | Anthropic [2], entry #38 |
| `tools[].cache_control` | Anthropic cache breakpoint on tool definitions. Codex may send `cache_control` on content, but not specifically on tool definitions | Anthropic [2] |
| `tools[].type` | Anthropic `custom` tool type. Proxy always generates `custom` type | Anthropic [2] |
| `tools[].allowed_callers` | Anthropic tool access control. Not applicable in proxy context | Anthropic [2] |
| `tools[].defer_loading` | Anthropic deferred tool loading. Not applicable in proxy context | Anthropic [2] |
| `tools[].eager_input_streaming` | Anthropic streaming control for tool input. Not applicable | Anthropic [2] |
| `tools[].input_examples` | Anthropic tool input examples. Codex sends tool descriptions, not input examples | Anthropic [2] |
| `tools[].strict` | Anthropic strict tool validation. Codex tool specs don't include `strict` | Anthropic [2] |
| Content block types: `image`, `document`, `search_result`, `web_search_tool_result`, etc. | Codex only sends `input_text` and `input_image` user content. Anthropic-only content block types are never generated by the conversion | Anthropic [2] |

#### E. Responses API Response Fields the Proxy Must Fabricate

These Responses API response fields have no Anthropic source — the proxy must generate them:

| Responses API field | Source | Rationale |
|---|---|---|
| `object` | Hardcoded `"response"` | Required by Responses API response structure. Anthropic uses `type: "message"` |
| `created_at` | Proxy-generated timestamp | Anthropic doesn't return creation time in the message object |
| `completed_at` | Proxy-generated timestamp | Anthropic doesn't return completion time |
| `usage.total_tokens` | Computed: `input_tokens + output_tokens` | Responses API expects total; Anthropic doesn't provide it |
| `output[].id` (reasoning) | Proxy-generated `rs_{uuid_v4}` | Anthropic thinking blocks have no ID; Responses API requires one per output item |
| `output[].id` (function_call) | Proxy-generated `fc_{sequential}` | Anthropic `tool_use` has `id` (`toolu_xxx`) but Responses API needs separate `id` and `call_id` |
| `output[].call_id` (function_call) | Proxy-generated `call_{sequential}` | Anthropic uses `toolu_xxx` as tool use ID; Responses API uses separate `call_id` for matching |
| `output[].annotations` (output_text) | Hardcoded `[]` | Anthropic text has no annotations; Codex expects the field (possibly optional) |
| `output[].status` | Derived from `stop_reason` or `"in_progress"` during streaming | Responses API items have status; Anthropic content blocks don't |

### Reference
- [1] OpenAI Responses API — https://developers.openai.com/api/reference/responses/overview/
- [2] Anthropic Messages API — https://platform.claude.com/docs/en/api
- [3] Anthropic Streaming Messages — https://platform.claude.com/docs/en/build-with-claude/streaming
- [4] LiteLLM source — https://github.com/BerriAI/litellm
- [5] OpenAI Responses streaming events — https://developers.openai.com/api/reference/resources/responses/streaming-events/

---

## 23. Signature cache key design: reasoning item ID vs response ID + block index

### Description
What should the signature cache key be? The spec originally said "keyed by response ID + content block index" but input reasoning items only carry proxy-generated `rs_xxx` IDs.

### Result

**Problem with response ID + block index:** When Codex sends a reasoning input item in a subsequent turn, it carries only the proxy-generated `rs_xxx` ID (e.g., `rs_001`). It does NOT carry the original Anthropic response ID or content block index. A cache keyed by response ID + block index cannot be looked up.

**CLIProxyAPI approach:** Uses `modelGroup(modelName) + SHA-256(text)[:16]` as the cache key — a hash of the model group and the thinking text content. This works because the same thinking text produces the same hash, enabling lookup by content. However, this means different thinking blocks with identical text would collide [1].

**Design decision for codex-conv:** Key the cache by the proxy-generated reasoning item ID (`rs_xxx`). Rationale:
- Every output reasoning item gets a unique `rs_xxx` ID generated by the proxy
- Codex echoes these IDs back in subsequent input arrays
- The proxy can directly look up `rs_xxx` → cached signature
- No hash collisions, no text matching, deterministic lookup
- Simpler than CLIProxyAPI's model+text approach

### Reference
- [1] CLIProxyAPI source: `internal/cache/signature_cache.go` — https://github.com/router-for-me/CLIProxyAPI

---

## 24. Anthropic `redacted_thinking` blocks in responses (safety redaction)

### Description
Can Anthropic return `redacted_thinking` blocks in responses when using `thinking.type: "adaptive"` without `display: "omitted"`? The spec had no output mapping for `redacted_thinking`.

### Result

**Confirmed:** Anthropic may return `redacted_thinking` blocks in ANY thinking mode, including `adaptive`. This is a server-initiated safety mechanism — Anthropic's safety system redacts portions of thinking content that trigger safety filters. It is independent of `display` setting and cannot be controlled by the client [1].

The `redacted_thinking` block has `{"type": "redacted_thinking", "data": "..."}` where `data` is opaque encrypted content. The Anthropic docs explicitly warn that filtering only `thinking` blocks silently drops `redacted_thinking` and "breaks the multi-turn protocol" [1].

**CLIProxyAPI behavior:** Silently drops `redacted_thinking` blocks entirely (test case "AC2: redacted_thinking must be ignored"). This is lossy but tolerated with a rectifier fallback for 400 errors [2].

**Design decision for codex-conv:** Map `redacted_thinking` to reasoning output item with `encrypted_content` populated from `data`. This preserves the opaque content for round-trip. The proxy must cache the `data` alongside the `rs_xxx` ID so it can reconstruct `redacted_thinking` if this reasoning item appears in subsequent input. This is superior to CLIProxyAPI's approach of silently dropping [2].

This is rare in practice (coding assistance rarely triggers safety redaction) but must be handled to avoid breaking multi-turn conversations.

### Reference
- [1] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking (see "Redacted thinking blocks" section)
- [2] CLIProxyAPI source: `internal/translator/claude/openai/responses/claude_openai-responses_request.go` — https://github.com/router-for-me/CLIProxyAPI

---

## 25. Codex CLI reasoning + function_call input item adjacency

### Description
Can reasoning items appear adjacent to function_call items in Codex CLI's input array? If so, how should they be merged into Anthropic messages?

### Result

**Confirmed:** Codex CLI sends reasoning items adjacent to function_call items. Evidence from test snapshot `pending_input_user_input_no_preempt_after_reasoning.snap` shows: `03:reasoning` followed by `04:function_call/shell` in the input array [1].

The Codex CLI source code classifies `ResponseItem::Reasoning` as both an API message and a model-generated item (`is_api_message()` and `is_model_generated_item()` in `context_manager/history.rs`), confirming it is treated as assistant-side output [2].

**Merge rule:** Consecutive `reasoning` + `function_call` items from the same model turn must be merged into the same Anthropic assistant message content array: `[{type: thinking/redacted_thinking}, {type: tool_use}]`. This matches CLIProxyAPI's non-streaming handler which merges all blocks into one Claude message content array [3].

**Spec update:** The spec's message alternation rule (previously covering only function_call + function_call_output) must explicitly include reasoning items in the merging logic.

### Reference
- [1] Codex CLI test snapshot: `codex-rs/core/tests/suite/snapshots/all__suite__pending_input__pending_input_user_input_no_preempt_after_reasoning.snap`
- [2] Codex CLI source: `codex-rs/core/src/context_manager/history.rs`
- [3] CLIProxyAPI source: `internal/translator/codex/claude/codex_claude_response.go` — https://github.com/router-for-me/CLIProxyAPI

---

## 26. Codex CLI `service_tier` sending behavior

### Description
Does Codex CLI actually send `service_tier` in its API requests? The spec claims "Codex CLI sends `priority` or `flex`" but this was unverified.

### Result

**Confirmed:** Codex CLI sends `service_tier` in its Responses API requests. The `ResponsesApiRequest` struct in `codex-rs/codex-api/src/common.rs` includes `service_tier: Option<String>`. Values are `"priority"` (from internal `ServiceTier::Fast`) or `"flex"` (from `ServiceTier::Flex`). It is user-configurable via TUI slash commands (`/model fast`, `/model flex`) and is filtered through `model_info.supports_service_tier()` before sending. When `None`, it is omitted from JSON via `#[serde(skip_serializing_if = "Option::is_none")]` [1].

The spec's claim at L583 ("Codex CLI sends `priority` or `flex`") is correct.

### Reference
- [1] Codex CLI source: `codex-rs/codex-api/src/common.rs`, `codex-rs/protocol/src/config_types.rs` — https://github.com/openai/codex

---

## 27. `thinking.display` parameter: values, defaults, streaming behavior, and model compatibility

### Description
What are the exact behaviors of the `thinking.display` parameter in the Anthropic Messages API? This determines how the proxy should handle thinking display when converting between OpenAI Responses API reasoning and Anthropic thinking. Specifically: accepted values, default behavior per model, compatibility with `thinking.type: "adaptive"`, field location, behavior on older models, streaming events with omitted mode, and the difference between "summarized" and "omitted" in streaming.

### Result

**Q1: Accepted values.** `thinking.display` accepts exactly two values: `"summarized"` and `"omitted"` [1][2].

**Q2: Default behavior (when `display` is not set).** Model-dependent [1][2]:
- **Claude Opus 4.7 and Claude Mythos Preview:** Default is `"omitted"` — thinking blocks are returned with an empty `thinking` field. You must set `display: "summarized"` explicitly to receive summarized thinking text.
- **Claude Opus 4.6, Claude Sonnet 4.6, and earlier Claude 4 models:** Default is `"summarized"` — thinking blocks contain summarized thinking text.
- **Older models (Sonnet 3.7, etc.):** These models return full (non-summarized) thinking. The `display` parameter was not applicable/introduced until Claude 4 models.
- **Invalid with `thinking.type: "disabled"`:** Setting `display` when thinking is disabled is invalid — there is nothing to display.

**Q3: Works with `thinking.type: "adaptive"`.** YES. Confirmed. The official docs explicitly show `display` used with adaptive thinking: `thinking = {"type": "adaptive", "display": "omitted"}` or `thinking = {"type": "adaptive", "display": "summarized"}` [2]. On Claude Opus 4.7, adaptive is the only supported thinking mode, and `display` defaults to `"omitted"`. When using adaptive thinking and the model skips thinking for a simple request, no thinking block is produced regardless of `display` setting.

**Q4: Field location.** Inside the `thinking` object, alongside `type` and (for manual mode) `budget_tokens` [1][2]:
```json
{"type": "adaptive", "display": "summarized"}
{"type": "enabled", "budget_tokens": 10000, "display": "omitted"}
```

**Q5: Older models.** Sonnet 4.6 and earlier Claude 4 models: `display` defaults to `"summarized"` and can be set to `"omitted"`. Pre-Claude 4 models (Sonnet 3.7, etc.): these return full (non-summarized) thinking inherently. The `display` parameter is a Claude 4+ feature.

**Q6: `signature_delta` with `display: "omitted"`.** YES. When `display: "omitted"` is set, the thinking block opens, a single `signature_delta` arrives (with no `thinking_delta` events), and the block closes. The `signature` field is identical whether `display` is `"summarized"` or `"omitted"` — it carries the encrypted full thinking for multi-turn continuity [1]. Streaming example with omitted:
```
event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EosnCkYICxIMMb3LzNrMu..."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}
```

**Q7: Streaming event differences.** With `"summarized"`: thinking blocks contain summarized text, streamed via `thinking_delta` events, followed by `signature_delta` before `content_block_stop`. With `"omitted"`: NO `thinking_delta` events are emitted at all — only a single `signature_delta` is sent. Text streaming begins immediately after the thinking block closes. The `signature` field is the same in both modes — only the visible `thinking` text differs (summarized text vs. empty string) [1].

**Key behavioral notes for proxy conversion:**
- `display` values can be switched between turns in a conversation (supported) [1].
- The billed output tokens are the same regardless of `display` setting — you pay for full thinking tokens either way [1][2].
- The `thinking` field in the response is empty (`""`) when `display: "omitted"`, but the `signature` field carries the full encrypted thinking [1].
- For multi-turn: any text placed in the `thinking` field of a round-tripped omitted block is ignored by the server — it decrypts the `signature` to reconstruct the original thinking [1].

**Design decision for codex-conv:** The proxy should NOT set `display` in requests to Anthropic. Rationale:
1. The proxy always wants thinking text to stream to Codex CLI as reasoning summaries.
2. The default on Opus 4.7 is `"omitted"`, which would hide thinking text from the proxy.
3. Therefore, the proxy should explicitly set `display: "summarized"` when constructing the `thinking` object for models that support it (Claude 4+ models).
4. This ensures thinking text is always available for mapping to OpenAI `reasoning.summary` items.

### Reference
- [1] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking (see "Controlling thinking display" and "Streaming thinking" sections)
- [2] Anthropic Adaptive Thinking docs — https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking (see "Working with thinking blocks" and "Controlling thinking display" sections)

---

## 28. Codex CLI `reasoning.summary` field and mapping to `thinking.display`

### Description
Codex CLI sends a `reasoning.summary` field with values `"auto"`, `"concise"`, `"detailed"`, `"none"`. How should this map to Anthropic's `thinking.display` parameter?

### Result

**Codex CLI source:** The `Reasoning` struct in `codex-rs/protocol/src/models.rs` contains `effort: Option<ReasoningEffort>` and `summary: Option<ReasoningSummary>`. `ReasoningSummary` values: `"auto"`, `"concise"`, `"detailed"`, `"none"` [1].

**Anthropic side:** `thinking.display` accepts `"summarized"` or `"omitted"`. Anthropic has no granularity control (no "concise" vs "detailed") [2].

**Mapping:**

| `reasoning.summary` | `thinking.display` | Rationale |
|---|---|---|
| absent / `null` | `"summarized"` | Default to showing thinking when reasoning enabled |
| `"auto"` | `"summarized"` | Direct match — default behavior |
| `"concise"` | `"summarized"` | No Anthropic granularity, closest match |
| `"detailed"` | `"summarized"` | Same |
| `"none"` | `"omitted"` | Client explicitly opts out of reasoning summaries |

This is pure protocol conversion — every value maps deterministically. No heuristic decisions.

### Reference
- [1] Codex CLI source: `codex-rs/protocol/src/models.rs` — https://github.com/openai/codex
- [2] Anthropic Extended Thinking docs — https://platform.claude.com/docs/en/build-with-claude/extended-thinking

---

## 29. Codex CLI dropped request fields: `text.verbosity`, `client_metadata`, `prompt_cache_key`

### Description
Codex CLI sends several request fields that have no Anthropic equivalent. What are these fields and should they be documented?

### Result

Three fields identified from Codex CLI source [1]:

1. **`text.verbosity`:** Part of `TextControls` struct alongside `format`. Values: `"low"`, `"medium"`, `"high"`. Controls output verbosity. Anthropic has no equivalent parameter.

2. **`client_metadata`:** A `HashMap<String, String>` in `ResponsesApiRequest`. Contains installation ID (`codex-installation-id`) and W3C trace context (`traceparent`, `tracestate`). This is a separate field from OpenAI's `metadata`. Anthropic has no equivalent.

3. **`prompt_cache_key`:** An `Option<String>` set to the thread ID. Used for OpenAI server-side response caching. Anthropic has no server-side storage.

All three are already implicitly dropped (not forwarded) since they have no Anthropic equivalent. Documenting them explicitly improves spec completeness.

### Reference
- [1] Codex CLI source: `codex-rs/codex-api/src/common.rs`, `codex-rs/protocol/src/models.rs` — https://github.com/openai/codex

---

## 30. `function_call_output.output` content array variant and `success` field

### Description
Codex CLI can send `function_call_output.output` as either a plain string or an array of content items. The spec only handles the string case. Additionally, `function_call_output` has a `success` field that should map to Anthropic's `tool_result.is_error`.

### Result

**Array variant:** The `FunctionCallOutputBody` enum in `codex-rs/protocol/src/models.rs` is `#[serde(untagged)]` with two variants: `Text(String)` and `ContentItems(Vec<FunctionCallOutputContentItem>)` [1]. Content items include `InputText { text: String }` and `InputImage { image_url: String, detail: Option<ImageDetail> }` — the same content types used in user messages.

The comment in source states: "The `output` field for `function_call_output` uses a dedicated payload type with custom serialization. On the wire it is either: a plain string (`content`) or an array of structured content items (`content_items`)" [1].

**Mapping:** When array → convert each content item using the existing User Content Block Mapping (input_text → text, input_image → image). When string → same as current behavior.

**`success` field:** `FunctionCallOutputPayload` has `body: FunctionCallOutputBody` and `success: Option<bool>` [1]. This is a Codex extension (not standard Responses API). Anthropic's `tool_result` has `is_error: bool`. Mapping: `success: false` → `is_error: true`; `success: true` or absent → omit `is_error`.

**`custom_tool_call_output`** uses the exact same `FunctionCallOutputPayload` type for its `output` field [1].

### Reference
- [1] Codex CLI source: `codex-rs/protocol/src/models.rs` — https://github.com/openai/codex

---

## 31. Anthropic `ping` streaming event and Codex `compaction`/`custom_tool_call`/`phase` items

### Description
Several minor protocol elements not covered in the spec: Anthropic's `ping` SSE events, Codex's `compaction`/`context_compaction` input items, `custom_tool_call`/`custom_tool_call_output` items, and `Message.phase`.

### Result

**`ping` events:** Anthropic sends `ping` events during streaming to keep connections alive [1]. These should be consumed silently with no Responses API event emitted.

**`compaction` items:** Codex CLI has `compaction` and `context_compaction` input item types containing `encrypted_content: string` — opaque OpenAI-specific data for context management [2]. These have no Anthropic equivalent and must be dropped.

**`custom_tool_call`/`custom_tool_call_output`:** Codex CLI defines these with the same structure as `function_call`/`function_call_output` [2]:
- `custom_tool_call`: `{type, status?, call_id, name, input}` — `input` is a string (same as `function_call.arguments`)
- `custom_tool_call_output`: `{type, call_id, name?, output: FunctionCallOutputPayload}` — uses the same payload type as `function_call_output`

The comment in source: "custom_tool_call_output.output uses the same wire encoding as function_call_output.output so freeform tools can return either plain text or structured content items" [2].

Mapping: identical to `function_call`/`function_call_output`.

**`Message.phase`:** Output messages can carry `phase: "commentary"` or `phase: "final_answer"` — used by Codex's TUI for state indicator behavior during streaming [2]. Anthropic has no equivalent. Strip on input, do not generate on output.

### Reference
- [1] Anthropic Streaming Messages docs — https://platform.claude.com/docs/en/build-with-claude/streaming
- [2] Codex CLI source: `codex-rs/protocol/src/models.rs`, `codex-rs/app-server-protocol/schema/typescript/ResponseItem.ts` — https://github.com/openai/codex

---

## 32. Comprehensive re-audit: spec coverage verification

### Description
Second full audit of the spec against latest official sources to find any remaining conversion gaps.

### Result

Verified against four sources in parallel:
1. Codex CLI latest source (`openai/codex`) — all `ResponsesApiRequest` fields and `ResponseItem` variants
2. Anthropic Messages API latest docs — all parameters, response fields, streaming events
3. OpenAI Responses API latest SDK types — all request/response/streaming types
4. Internal spec consistency — cross-reference between sections

**All Codex CLI request fields confirmed covered:**
- `model`, `instructions`, `input`, `tools`, `tool_choice`, `parallel_tool_calls`, `reasoning`, `store`, `stream`, `include`, `service_tier`, `prompt_cache_key`, `text` (verbosity + format), `client_metadata` — all mapped or documented as dropped

**All Codex CLI input item types confirmed covered:**
- `message` (user/assistant/system), `function_call`, `function_call_output`, `reasoning`, `custom_tool_call`, `custom_tool_call_output`, `compaction`, `context_compaction`, `compaction_trigger`

**All Anthropic streaming events confirmed covered:**
- `message_start`, `content_block_start` (text/thinking/redacted_thinking/tool_use), `content_block_delta` (text_delta/thinking_delta/signature_delta/input_json_delta), `content_block_stop`, `message_delta`, `message_stop`, `ping`, `error`

**Two minor documentation gaps found and fixed:**
1. `too_many_namespaced_tools` proxy error code mentioned in collision detection section but missing from proxy-originated errors table → added
2. Additional Anthropic usage response fields (`usage.cache_creation`, `usage.server_tool_use`, `usage.service_tier`, `usage.speed`) not documented as dropped → added

**No new conversion gaps found.** The spec covers all fields, events, and item types that Codex CLI actually sends and receives. New Responses API features found in the SDK (background mode, conversation, context_management, item_reference, built-in tool types, refusal content type, audio events, etc.) are not used by Codex CLI and are out of scope per the spec's "sole use case: Codex CLI" principle.

### Reference
- [1] Codex CLI source: `codex-rs/codex-api/src/common.rs`, `codex-rs/protocol/src/models.rs` — https://github.com/openai/codex
- [2] Anthropic Messages API — https://platform.claude.com/docs/en/api
- [3] Anthropic Streaming Messages — https://platform.claude.com/docs/en/build-with-claude/streaming
- [4] OpenAI Python SDK: `src/openai/types/responses/` — https://github.com/openai/openai-python

## 33. Anthropic Messages API built-in tool types: complete request/response format

### Description
What are the exact tool definitions, configuration parameters, response content block types, and streaming events for each Anthropic native built-in tool type? This research supports conversion design between OpenAI Responses API built-in tools and Anthropic native tools.

### Result

Anthropic built-in tools fall into two execution categories:

**Client tools** return `stop_reason: "tool_use"` with `tool_use` content blocks. The client must execute the tool and return a `tool_result` message. These include: bash, text_editor, computer_use, memory.

**Server tools** execute on Anthropic infrastructure and return results directly in the response content array. These include: web_search, web_fetch, code_execution, advisor, tool_search, mcp_toolset. Server tools use `server_tool_use` and tool-specific result blocks (not `tool_result` messages). Server tool IDs use the `srvtoolu_` prefix (vs. `toolu_` for client tools).

All built-in tools are schema-less — no `input_schema` appears in the tool definition because the schema is built into the model. Tools use date-stamped version strings as the `type` value.

#### 33.1 Web Search Tool

**Tool definition** [1]:
```json
{
  "type": "web_search_20260209",
  "name": "web_search",
  "max_uses": 5,
  "allowed_domains": ["example.com"],
  "blocked_domains": ["spam.com"],
  "user_location": {
    "type": "approximate",
    "city": "San Francisco",
    "region": "California",
    "country": "US",
    "timezone": "America/Los_Angeles"
  }
}
```
All fields except `type` are optional. Available versions: `web_search_20260209`, `web_search_20250305`. `max_uses` limits total searches per turn. `user_location` provides geographic context for results.

**Response content blocks** [1]:
- `server_tool_use`: `{"type": "server_tool_use", "id": "srvtoolu_...", "name": "web_search", "input": {"query": "search terms"}}`
- `web_search_tool_result`: `{"type": "web_search_tool_result", "tool_use_id": "srvtoolu_...", "content": {"type": "web_search_result", "results": [{"url": "...", "title": "...", "page_age": "...", "encrypted_content": "..."}]}}`
- Citations (always enabled): `web_search_result_location` with `{url, title, encrypted_index, cited_text}`

**Error codes**: `too_many_requests`, `invalid_input`, `max_uses_exceeded`, `query_too_long`, `unavailable`.

**Stop reason**: Can return `pause_turn` for long-running searches [1].

#### 33.2 Web Fetch Tool

**Tool definition** [2]:
```json
{
  "type": "web_fetch_20260209",
  "name": "web_fetch",
  "max_uses": 10,
  "allowed_domains": ["example.com"],
  "blocked_domains": ["spam.com"],
  "citations": {"enabled": true},
  "max_content_tokens": 100000
}
```
All fields except `type` are optional. Available versions: `web_fetch_20260209`, `web_fetch_20250910`. Requires beta header `web-fetch-2025-09-10`. URL validation: can only fetch URLs previously present in the conversation context.

**Response content blocks** [2]:
- `server_tool_use`: `{"type": "server_tool_use", "id": "srvtoolu_...", "name": "web_fetch", "input": {"url": "https://..."}}`
- `web_fetch_tool_result`: `{"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_...", "content": {"type": "web_fetch_result", "url": "...", "content": {"type": "document", "source": {"type": "base64", "media_type": "text/html", "data": "..."}, "title": "...", "citations": [...]}, "retrieved_at": "2026-..."}}`
- Citations (when `citations.enabled: true`): `char_location` with `{document_index, document_title, start_char_index, end_char_index, cited_text}`

**Error codes**: `invalid_input`, `url_too_long`, `url_not_allowed`, `url_not_accessible`, `too_many_requests`, `unsupported_content_type`, `max_uses_exceeded`, `unavailable`.

#### 33.3 Code Execution Tool

**Tool definition** [3]:
```json
{
  "type": "code_execution_20250825",
  "name": "code_execution"
}
```
No additional configuration parameters. Available versions: `code_execution_20260120`, `code_execution_20250825`. Provides two sub-tools automatically:
- `bash_code_execution`: Run shell commands. Input: `{command: "..."}`.
- `text_editor_code_execution`: View/create/edit files. Commands: `view`, `create`, `str_replace`.

**Response content blocks (bash)** [3]:
```json
{"type": "server_tool_use", "id": "srvtoolu_...", "name": "bash_code_execution", "input": {"command": "ls -la"}}
{"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_...", "content": {"type": "bash_code_execution_result", "stdout": "...", "stderr": "...", "return_code": 0}}
```

**Response content blocks (text editor)** [3]:
```json
{"type": "server_tool_use", "id": "srvtoolu_...", "name": "text_editor_code_execution", "input": {"command": "view", "path": "/tmp/file.py"}}
{"type": "text_editor_code_execution_tool_result", "tool_use_id": "srvtoolu_...", "content": {"type": "text_editor_code_execution_result", "path": "...", "content": "..."}}
```

**Error block**:
```json
{"type": "bash_code_execution_tool_result", "tool_use_id": "...", "content": {"type": "bash_code_execution_tool_result_error", "error_code": "..."}}
```

**Error codes**: `unavailable`, `execution_time_exceeded`, `container_expired`, `invalid_tool_input`, `too_many_requests`, `file_not_found` (text_editor only), `string_not_found` (text_editor only).

**Container**: When code execution is used, the response includes `container: {id: "...", expires_at: "..."}`. Containers expire 30 days after creation. The `container_upload` content block type enables uploading files to the sandbox [3].

**Stop reason**: Can return `pause_turn` for long-running executions [3].

#### 33.4 Computer Use Tool

**Tool definition** [4]:
```json
{
  "type": "computer_20250124",
  "name": "computer",
  "display_width_px": 1024,
  "display_height_px": 768,
  "display_number": 0
}
```
Schema-less client tool. Available versions: `computer_20250124`, `computer_20241022`. Requires beta header: `computer-use-2025-01-24` or `computer-use-2024-10-22`. `display_number` is optional.

**Input schema** (built into model) [4]:
- Basic actions: `screenshot`, `left_click`, `type`, `key`, `mouse_move`.
- Enhanced actions (computer_20250124 only): `scroll`, `left_click_drag`, `right_click`, `middle_click`, `double_click`, `triple_click`, `left_mouse_down`, `left_mouse_up`, `hold_key`, `wait`.

**Response content blocks**: Standard `tool_use` content block with `name: "computer"` and action-specific input. Client must execute and return `tool_result` with screenshot image.

#### 33.5 Bash Tool

**Tool definition** [5]:
```json
{
  "type": "bash_20250124",
  "name": "bash"
}
```
Schema-less client tool. Available versions: `bash_20250124`, `bash_20241022`. No configuration parameters beyond `type` and `name`.

**Input schema** (built into model) [5]:
- `{command: "..."}` — required, the shell command to execute.
- `{restart: true}` — optional, restart the bash session.

**Response content blocks**: Standard `tool_use` content block with `name: "bash"` and `{command: "..."}` input. Client must execute and return `tool_result`.

#### 33.6 Text Editor Tool

**Tool definition** [6]:
```json
{
  "type": "text_editor_20250728",
  "name": "str_replace_based_edit_tool",
  "max_characters": 10000
}
```
Schema-less client tool. Available versions: `text_editor_20250728`, `text_editor_20250124`, `text_editor_20241022`. `max_characters` parameter only available on `text_editor_20250728` and later.

**Input schema** (built into model) [6]:
- Commands: `view`, `str_replace`, `create`, `insert`, `undo_edit`.
- Each command takes `path` (required) plus command-specific fields:
  - `view`: `{command: "view", path: "...", view_range: [start, end]}`
  - `str_replace`: `{command: "str_replace", path: "...", old_str: "...", new_str: "..."}`
  - `create`: `{command: "create", path: "...", file_text: "..."}`
  - `insert`: `{command: "insert", path: "...", insert_line: N, new_str: "..."}`
  - `undo_edit`: `{command: "undo_edit", path: "..."}`

**Response content blocks**: Standard `tool_use` content block with `name: "str_replace_based_edit_tool"`. Client must execute and return `tool_result`.

#### 33.7 Memory Tool

**Tool definition** [7]:
```json
{
  "type": "memory_20250818",
  "name": "memory"
}
```
Schema-less client tool. Client-side execution (returns `stop_reason: "tool_use"`). Input: memory operations (store/recall). Available version: `memory_20250818`.

#### 33.8 Other Server Tools

**Advisor**: `{"type": "advisor_20260301", "name": "advisor"}` — Beta. Server-side execution.

**Tool Search**: Types `tool_search_tool_regex_20251119` and `tool_search_tool_bm25_20251119` — GA. Server-side. Used for searching MCP tool definitions.

**MCP Connector**: `{"type": "mcp_toolset", "name": "...", "endpoint_url": "...", "tools": [...]}` — Beta. Server-side. Connects to external MCP servers.

#### 33.9 Streaming Events for Server Tools

Server tools follow a consistent streaming pattern [1][2][3]:

```
event: content_block_start
data: {"type": "content_block_start", "index": N,
       "content_block": {"type": "server_tool_use", "id": "srvtoolu_...", "name": "tool_name"}}

event: content_block_delta
data: {"type": "content_block_delta", "index": N,
       "delta": {"type": "input_json_delta", "partial_json": "..."}}

event: content_block_stop
data: {"type": "content_block_stop", "index": N}

// Pause while tool executes on server

event: content_block_start
data: {"type": "content_block_start", "index": N+1,
       "content_block": {"type": "<tool>_tool_result", "tool_use_id": "srvtoolu_...", "content": {...}}}

event: content_block_stop
data: {"type": "content_block_stop", "index": N+1}
```

Client tools use the standard `tool_use` content block pattern (same as custom tools) with `tool_use` type in `content_block_start` and `input_json_delta` in `content_block_delta`.

#### 33.10 Complete Tool Catalog

| Tool | `type` values | Execution | Status |
|---|---|---|---|
| Web search | `web_search_20260209`, `web_search_20250305` | Server | GA |
| Web fetch | `web_fetch_20260209`, `web_fetch_20250910` | Server | GA |
| Code execution | `code_execution_20260120`, `code_execution_20250825` | Server | GA |
| Advisor | `advisor_20260301` | Server | Beta |
| Tool search | `tool_search_tool_regex_20251119`, `tool_search_tool_bm25_20251119` | Server | GA |
| MCP connector | `mcp_toolset` | Server | Beta |
| Memory | `memory_20250818` | Client | GA |
| Bash | `bash_20250124`, `bash_20241022` | Client | GA |
| Text editor | `text_editor_20250728`, `text_editor_20250124`, `text_editor_20241022` | Client | GA |
| Computer use | `computer_20250124`, `computer_20241022` | Client | Beta |

### Reference
- [1] Anthropic Web search tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/web-search-tool
- [2] Anthropic Web fetch tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/web-fetch-tool
- [3] Anthropic Code execution tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/code-execution-tool
- [4] Anthropic Computer use tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/computer-use-tool
- [5] Anthropic Bash tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/bash-tool
- [6] Anthropic Text editor tool — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/text-editor-tool
- [7] Anthropic Tool use overview — https://platform.claude.com/docs/en/docs/agents-and-tools/tool-use/overview

---

## 34. OpenAI Responses API built-in tool output item types: complete call and output structures

### Description
What are the exact JSON structures for all OpenAI Responses API built-in tool output items? For each tool type, what parameters does the model generate when calling the tool (the "call" side), and what is the output/result item structure? This is needed to create Anthropic custom tool `input_schema` definitions for each built-in tool type if conversion is required.

The source of truth is the OpenAI Python SDK type definitions (auto-generated from the OpenAPI spec by Stainless), which define the exact field names, types, and discriminated unions [1]. These are cross-referenced against official tool guides [2] where available.

### Result

The complete `ResponseOutputItem` union type from the SDK defines all possible output items in a response's `output` array [1]:

```
ResponseOutputMessage | ResponseFileSearchToolCall | ResponseFunctionToolCall |
ResponseFunctionWebSearch | ResponseComputerToolCall | ResponseReasoningItem |
ResponseCompactionItem | ImageGenerationCall | ResponseCodeInterpreterToolCall |
LocalShellCall | ResponseFunctionShellToolCall | ResponseFunctionShellToolCallOutput |
ResponseApplyPatchToolCall | ResponseApplyPatchToolCallOutput | McpCall | McpListTools |
McpApprovalRequest | ResponseCustomToolCall
```

Note: `tool_search_call` and `tool_search_output` do NOT appear in this union in the current SDK version — they appear as separate item types. The SDK also defines `ResponseToolSearchCall` and `ResponseToolSearchOutputItem` but they are not part of the `ResponseOutputItem` alias.

Below are the exact structures for each built-in tool type.

#### 34.1 web_search_call (ResponseFunctionWebSearch)

**Type discriminator:** `"web_search_call"`

**Structure** [1][2]:
```json
{
  "id": "ws_67c9fa0502748190b7dd390736892e100be649c1a5ff9609",
  "type": "web_search_call",
  "status": "completed",
  "action": {
    "type": "search",
    "query": "latest news about AI",
    "queries": ["latest news about AI"],
    "sources": [{"type": "url", "url": "https://..."}]
  }
}
```

**Action types** (discriminated union on `action.type`) [1]:
- `search`: `{type: "search", query: string, queries?: string[], sources?: [{type: "url", url: string}]}`
  - `query` is DEPRECATED — use `queries` instead
  - `sources` lists URLs used in the search
- `open_page`: `{type: "open_page", url?: string}` — reasoning models only
- `find_in_page`: `{type: "find_in_page", pattern: string, url: string}` — reasoning models only

**Status values:** `"in_progress"`, `"searching"`, `"completed"`, `"failed"` [1]

**No separate output type.** The `web_search_call` item is self-contained — results are referenced via URL citations in the assistant message's `output_text` annotations.

#### 34.2 file_search_call (ResponseFileSearchToolCall)

**Type discriminator:** `"file_search_call"`

**Structure** [1][3]:
```json
{
  "id": "fs_67c09ccea8c48191ade9367e3ba71515",
  "type": "file_search_call",
  "status": "completed",
  "queries": ["What is deep research?"],
  "results": [
    {
      "file_id": "file-abc123",
      "filename": "research.pdf",
      "text": "relevant passage...",
      "score": 0.95,
      "attributes": {"key": "value"}
    }
  ]
}
```

**Fields** [1]:
- `id`: string (unique ID)
- `type`: always `"file_search_call"`
- `status`: `"in_progress"`, `"searching"`, `"completed"`, `"incomplete"`, `"failed"`
- `queries`: `string[]` (required, search queries used)
- `results`: `null` or array of result objects with `file_id`, `filename`, `text`, `score`, `attributes` (all optional within result)

**No separate output type.** Results are inline on the call item.

#### 34.3 code_interpreter_call (ResponseCodeInterpreterToolCall)

**Type discriminator:** `"code_interpreter_call"`

**Structure** [1]:
```json
{
  "id": "ci_...",
  "type": "code_interpreter_call",
  "status": "completed",
  "container_id": "cntr_...",
  "code": "import pandas as pd\ndf = pd.read_csv('data.csv')\nprint(df.head())",
  "outputs": [
    {"type": "logs", "logs": "   col1  col2\n0     1     2"},
    {"type": "image", "url": "https://..."}
  ]
}
```

**Fields** [1]:
- `id`: string (unique ID)
- `type`: always `"code_interpreter_call"`
- `status`: `"in_progress"`, `"completed"`, `"incomplete"`, `"interpreting"`, `"failed"`
- `container_id`: string (required, the container used to run code)
- `code`: `string | null` (the code to run, null if not available)
- `outputs`: `null` or array of output items:
  - `{type: "logs", logs: string}` — stdout/stderr output
  - `{type: "image", url: string}` — image output URL

**No separate output type.** The model generates the `code` field. The runtime executes it in the container and populates `outputs`. Results are inline on the call item.

#### 34.4 computer_call (ResponseComputerToolCall)

**Type discriminator:** `"computer_call"`

**Call structure** [1][4]:
```json
{
  "id": "comp_...",
  "type": "computer_call",
  "call_id": "call_001",
  "status": "completed",
  "action": {
    "type": "click",
    "button": "left",
    "x": 405,
    "y": 157
  },
  "actions": [
    {"type": "click", "button": "left", "x": 405, "y": 157},
    {"type": "type", "text": "hello"}
  ],
  "pending_safety_checks": []
}
```

**Action types** (discriminated union on `action.type`) [1]:
- `screenshot`: `{type: "screenshot"}` — no additional params
- `click`: `{type: "click", button: "left"|"right"|"wheel"|"back"|"forward", x: int, y: int, keys?: string[]}`
- `double_click`: `{type: "double_click", x: int, y: int, keys?: string[]}`
- `drag`: `{type: "drag", path: [{x: int, y: int}], keys?: string[]}`
- `keypress`: `{type: "keypress", keys: string[]}`
- `move`: `{type: "move", x: int, y: int, keys?: string[]}`
- `scroll`: `{type: "scroll", x: int, y: int, scroll_x: int, scroll_y: int, keys?: string[]}`
- `type`: `{type: "type", text: string}`
- `wait`: `{type: "wait"}`

**Note:** The SDK has both `action` (single action, optional) and `actions` (batched actions via `ComputerActionList`, optional) [1].

**Status values:** `"in_progress"`, `"completed"`, `"incomplete"` [1]

**Output type** (ResponseComputerToolCallOutputItem) — provided by client as input on next turn [1]:
```json
{
  "id": "comp_out_...",
  "type": "computer_call_output",
  "call_id": "call_001",
  "status": "completed",
  "output": {
    "type": "computer_screenshot",
    "image_url": "data:image/png;base64,...",
    "detail": "original"
  },
  "acknowledged_safety_checks": [
    {"id": "psc_...", "code": "...", "message": "..."}
  ]
}
```

#### 34.5 image_generation_call (ImageGenerationCall)

**Type discriminator:** `"image_generation_call"`

**Structure** [1][5]:
```json
{
  "id": "ig_123",
  "type": "image_generation_call",
  "status": "completed",
  "result": "base64_encoded_image_data...",
  "revised_prompt": "A gray tabby cat hugging an otter..."
}
```

**Fields** [1]:
- `id`: string (unique ID)
- `type`: always `"image_generation_call"`
- `status`: `"in_progress"`, `"completed"`, `"generating"`, `"failed"`
- `result`: `string | null` — base64-encoded generated image

**No separate output type.** The result is inline on the call item.

#### 34.6 shell_call (ResponseFunctionShellToolCall)

**Type discriminator:** `"shell_call"`

**Call structure** [1][6]:
```json
{
  "id": "sh_...",
  "type": "shell_call",
  "call_id": "call_9d14ac6f2b73485e91c0f4da6e1b27c8",
  "status": "in_progress",
  "action": {
    "commands": ["ls -l"],
    "timeout_ms": 120000,
    "max_output_length": 4096
  },
  "environment": {"type": "local"},
  "created_by": null
}
```

**Action fields** [1]:
- `commands`: `string[]` (required, commands to run)
- `timeout_ms`: `int | null` (optional)
- `max_output_length`: `int | null` (optional)

**Environment types** (discriminated union on `environment.type`) [1]:
- `local`: `{type: "local"}` — run on local machine
- `container_reference`: `{type: "container_reference", id: string}` — run in existing container
- `null` / omitted — auto/container

**Status values:** `"in_progress"`, `"completed"`, `"incomplete"` [1]

**Output type** (ResponseFunctionShellToolCallOutput) [1]:
```json
{
  "id": "sh_out_...",
  "type": "shell_call_output",
  "call_id": "call_...",
  "status": "completed",
  "max_output_length": 4096,
  "output": [
    {
      "stdout": "total 42\ndrwxr-xr-x  ...",
      "stderr": "",
      "outcome": {"type": "exit", "exit_code": 0}
    }
  ],
  "created_by": null
}
```

**Output.outcome types** (discriminated union on `outcome.type`) [1]:
- `exit`: `{type: "exit", exit_code: int}`
- `timeout`: `{type: "timeout"}`

#### 34.7 local_shell_call (LocalShellCall)

**Type discriminator:** `"local_shell_call"`

**IMPORTANT:** This is a DISTINCT type from `shell_call` with a different action structure.

**Call structure** [1]:
```json
{
  "id": "lsh_...",
  "type": "local_shell_call",
  "call_id": "call_...",
  "status": "completed",
  "action": {
    "type": "exec",
    "command": ["ls", "-l"],
    "env": {"KEY": "value"},
    "timeout_ms": 120000,
    "user": null,
    "working_directory": "/path/to/dir"
  }
}
```

**Action fields** (LocalShellCallAction) [1]:
- `type`: always `"exec"`
- `command`: `string[]` (required — NOTE: array of strings, not a single string)
- `env`: `Record<string, string>` (required, environment variables)
- `timeout_ms`: `int | null` (optional)
- `user`: `string | null` (optional user to run as)
- `working_directory`: `string | null` (optional working directory)

**Status values:** `"in_progress"`, `"completed"`, `"incomplete"` [1]

**No separate output type defined in the SDK output item union.** The `local_shell_call` does not have a corresponding `local_shell_call_output` in the `ResponseOutputItem` union — it is a standalone call item. This is different from `shell_call` which has `ResponseFunctionShellToolCallOutput`.

Note: The official local shell guide shows `command` as a single string in example code, but the SDK type definition uses `List[str]` (array). The SDK is the authoritative source for the wire format [1][7].

#### 34.8 apply_patch_call (ResponseApplyPatchToolCall)

**Type discriminator:** `"apply_patch_call"`

**Call structure** [1][8]:
```json
{
  "id": "apc_...",
  "type": "apply_patch_call",
  "call_id": "call_Rjsqzz96C5xzPb0jUWJFRTNW",
  "status": "completed",
  "operation": {
    "type": "update_file",
    "path": "lib/fib.py",
    "diff": "@@\n-def fib(n):\n+def fibonacci(n):\n..."
  },
  "created_by": null
}
```

**Operation types** (discriminated union on `operation.type`) [1]:
- `create_file`: `{type: "create_file", path: string, diff: string}` — full file V4A diff
- `update_file`: `{type: "update_file", path: string, diff: string}` — V4A diff
- `delete_file`: `{type: "delete_file", path: string}` — no diff field

**Status values:** `"in_progress"`, `"completed"` [1]

**Output type** (ResponseApplyPatchToolCallOutput) [1]:
```json
{
  "id": "apc_out_...",
  "type": "apply_patch_call_output",
  "call_id": "call_abc",
  "status": "completed",
  "output": null,
  "created_by": null
}
```

**Output fields** [1]:
- `status`: `"completed"` or `"failed"`
- `output`: `string | null` — optional error message (e.g., "Error: File not found at path 'lib/baz.py'")

#### 34.9 MCP tools (mcp_list_tools, mcp_call, mcp_approval_request)

**Three distinct item types** for MCP tools [1]:

**34.9.1 mcp_list_tools (McpListTools)**

```json
{
  "id": "mcp_lt_...",
  "type": "mcp_list_tools",
  "server_label": "my_server",
  "tools": [
    {
      "name": "get_weather",
      "description": "Get current weather",
      "input_schema": {...},
      "annotations": {...}
    }
  ],
  "error": null
}
```

**Fields** [1]:
- `id`: string
- `type`: always `"mcp_list_tools"`
- `server_label`: string (required)
- `tools`: array of `McpListToolsTool` objects with `name`, `input_schema` (required), `description`, `annotations` (optional)
- `error`: `string | null` — error message if server could not list tools

**34.9.2 mcp_call (McpCall)**

```json
{
  "id": "mcp_c_...",
  "type": "mcp_call",
  "name": "get_weather",
  "server_label": "my_server",
  "arguments": "{\"city\": \"San Francisco\"}",
  "status": "completed",
  "output": "The weather in San Francisco is 65F and sunny.",
  "error": null,
  "approval_request_id": null
}
```

**Fields** [1]:
- `id`: string
- `type`: always `"mcp_call"`
- `name`: string (required, tool name)
- `server_label`: string (required, MCP server label)
- `arguments`: string (required, JSON string of arguments)
- `status`: `"in_progress"`, `"completed"`, `"incomplete"`, `"calling"`, `"failed"` (optional)
- `output`: `string | null` — output from the tool call
- `error`: `string | null` — error from the tool call
- `approval_request_id`: `string | null` — for MCP approval flow

**34.9.3 mcp_approval_request (McpApprovalRequest)**

```json
{
  "id": "mcp_ar_...",
  "type": "mcp_approval_request",
  "name": "delete_files",
  "server_label": "my_server",
  "arguments": "{\"path\": \"/important/data\"}"
}
```

**Fields** [1]:
- `id`: string
- `type`: always `"mcp_approval_request"`
- `name`: string (required, tool name)
- `server_label`: string (required)
- `arguments`: string (required, JSON string of arguments)

#### 34.10 tool_search_call (ResponseToolSearchCall) and tool_search_output (ResponseToolSearchOutputItem)

**Note:** These types are defined in the SDK but are NOT part of the `ResponseOutputItem` union in the current version. They may appear as separate item types.

**34.10.1 tool_search_call** [1]:
```json
{
  "id": "ts_...",
  "type": "tool_search_call",
  "execution": "client",
  "call_id": "call_abc123",
  "status": "completed",
  "arguments": {
    "goal": "Find the shipping ETA tool for order_42."
  },
  "created_by": null
}
```

**Fields** [1]:
- `id`: string
- `type`: always `"tool_search_call"`
- `execution`: `"server"` or `"client"`
- `call_id`: `string | null` — null for server execution
- `status`: `"in_progress"`, `"completed"`, `"incomplete"`
- `arguments`: `object` (required, search arguments)

**34.10.2 tool_search_output** [1]:
```json
{
  "id": "tso_...",
  "type": "tool_search_output",
  "execution": "server",
  "call_id": null,
  "status": "completed",
  "tools": [
    {
      "type": "namespace",
      "name": "crm",
      "tools": [
        {"type": "function", "name": "list_open_orders", ...}
      ]
    }
  ],
  "created_by": null
}
```

**Fields** [1]:
- `id`: string
- `type`: always `"tool_search_output"`
- `execution`: `"server"` or `"client"`
- `call_id`: `string | null`
- `status`: `"in_progress"`, `"completed"`, `"incomplete"`
- `tools`: array of `Tool` objects (namespaces containing function definitions)

### Reference
- [1] OpenAI Python SDK type definitions (auto-generated from OpenAPI spec) — https://github.com/openai/openai-python/tree/main/src/openai/types/responses/ (specifically: `response_output_item.py`, `response_code_interpreter_tool_call.py`, `response_function_web_search.py`, `response_file_search_tool_call.py`, `response_computer_tool_call.py`, `response_function_shell_tool_call.py`, `response_function_shell_tool_call_output.py`, `response_apply_patch_tool_call.py`, `response_apply_patch_tool_call_output.py`, `response_tool_search_call.py`, `response_tool_search_output_item.py`, `response_computer_tool_call_output_item.py`)
- [2] OpenAI Web search guide — https://developers.openai.com/api/docs/guides/tools-web-search
- [3] OpenAI File search guide — https://developers.openai.com/api/docs/guides/tools-file-search
- [4] OpenAI Computer use guide — https://developers.openai.com/api/docs/guides/tools-computer-use
- [5] OpenAI Image generation guide — https://developers.openai.com/api/docs/guides/tools-image-generation
- [6] OpenAI Shell tool guide — https://developers.openai.com/api/docs/guides/tools-shell
- [7] OpenAI Local shell guide — https://developers.openai.com/api/docs/guides/tools-local-shell
- [8] OpenAI Apply patch guide — https://developers.openai.com/api/docs/guides/tools-apply-patch

## 35. Built-in tool conversion design: schema-less tool type → Anthropic custom tool

### Description
How should the proxy handle OpenAI Responses API built-in tool types when they appear in the request's `tools` array? Specifically: what schema to register, how response output items map back, and whether parameters can be perfectly forwarded.

### Result
All built-in tool types are converted to Anthropic custom tools (`type: "custom"`) with derived `input_schema`. The response direction uniformly returns `function_call` for all built-in tools.

**Why custom tools, not Anthropic native tools:** Anthropic native tools (web_search_20260305, bash_20250124, etc.) [1] carry server-side execution semantics — the Anthropic API executes them and returns results inline. In a proxy context, the downstream client owns tool execution. Custom tools let the model generate `tool_use` blocks that the proxy converts to Responses API `function_call` items.

**Why `function_call` for all tools, not native built-in output types:** Round-trip consistency requires that tool registration → tool call → tool result form a closed loop where `tool_use.input` is preserved exactly. `function_call.arguments` serializes `tool_use.input` as a JSON string — lossless, no reconstruction needed. Native built-in output types (e.g., `web_search_call`) mix model input with execution metadata (`status`, `id`) and some don't preserve model input at all (`image_generation_call` has `result` but no `prompt` field) [2]. Using `function_call` guarantees: schema ↔ `tool_use.input` ↔ `function_call.arguments` ↔ `tool_use.input` consistency with zero reconstruction logic.

**Schema structure requirement:** Both Anthropic native tools and OpenAI built-in tools are schema-less [1][2]. The derived schema mirrors the nested structure of the corresponding output item's call parameters:
- Tools with `action` wrapper (web_search, computer_use, shell, local_shell): schema top-level = `action` object
- Tools with `operation` wrapper (apply_patch): schema top-level = `operation` object
- Tools with flat call fields (file_search, code_interpreter, mcp_call, tool_search): schema top-level = flat fields

**Perfect forwarding:** The model's `tool_use.input` is serialized as `function_call.arguments` (JSON string). On subsequent turns, `function_call.arguments` is parsed back to `tool_use.input`. Zero loss, zero reconstruction.

**Tool config fields (dropped):** Built-in tools carry configuration (e.g., `web_search.search_context_size`, `file_search.vector_store_ids`, `computer_use_preview.display_width`) that controls OpenAI server-side behavior. These have no Anthropic equivalent and are dropped during conversion.

### Reference
- [1] Entry #33 — Anthropic native tool types
- [2] Entry #34 — OpenAI built-in tool output item structures (source for derived input_schema)

---

## 36. OpenAI Responses API `{"type": "mcp"}` tool: exact field structure, discovery flow, and output items

### Description
What is the exact field structure of `{"type": "mcp", ...}` in the Responses API `tools` array? How does MCP tool discovery work (does OpenAI connect to MCP servers server-side)? What output item types does the MCP tool produce? And what is the relationship between `{"type": "mcp"}` in the request and `mcp_call`/`mcp_list_tools` output items?

### Result

**Field structure — remote MCP server variant:**
```json
{
  "type": "mcp",
  "server_label": "dmcp",
  "server_description": "A Dungeons and Dragons MCP server.",
  "server_url": "https://dmcp-server.deno.dev/sse",
  "require_approval": "never",
  "allowed_tools": ["roll"],
  "authorization": "<OAuth token>",
  "defer_loading": false
}
```

**Field structure — connector variant (built-in MCP wrappers for third-party services):**
```json
{
  "type": "mcp",
  "server_label": "google_calendar",
  "connector_id": "connector_googlecalendar",
  "authorization": "ya29.A0AS3H6...",
  "require_approval": "never"
}
```

Available connector IDs: `connector_dropbox`, `connector_gmail`, `connector_googlecalendar`, `connector_googledrive`, `connector_microsoftteams`, `connector_outlookcalendar`, `connector_outlookemail`, `connector_sharepoint` [1].

**Field descriptions:**
| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | Must be `"mcp"` |
| `server_label` | Yes | User-defined label identifying the MCP server (used in output items) |
| `server_url` | No* | URL of the remote MCP server (Streamable HTTP or HTTP/SSE transport). Required for remote servers, mutually exclusive with `connector_id` |
| `server_description` | No | Description to help the model understand the server's purpose |
| `connector_id` | No* | ID of a built-in connector. Mutually exclusive with `server_url` |
| `require_approval` | No | Controls tool approval: `"never"`, `"always"`, or `{"never": {"tool_names": ["tool1"]}}` for per-tool granularity. Default requires approval for all tools |
| `allowed_tools` | No | Allowlist of tool names the model can use. Omit to allow all |
| `authorization` | No | OAuth token for the MCP server or connector. Not stored by OpenAI — must be sent with every request |
| `defer_loading` | No | If `true`, tools are not loaded at request start. Model uses `tool_search` to discover tools on demand |

**MCP tool discovery flow (server-side):** When a request includes `{"type": "mcp", "server_url": "..."}`, OpenAI's servers connect to the specified MCP server endpoint, call `tools/list`, and register all discovered tools in the response context. The discovered tools are cached in the context — if `previous_response_id` is used to continue the conversation, `mcp_list_tools` is not re-fetched. When `defer_loading: true` is set, tools are not loaded initially; instead, the model uses `tool_search` to discover tools on demand [1][2].

**This is a hosted tool — OpenAI's servers handle MCP execution server-side.** The caller does not connect to the MCP server. A format-only proxy CANNOT handle `{"type": "mcp"}` because the MCP tool execution happens on OpenAI's infrastructure [1].

**Output item types produced by the MCP tool flow:**

1. `mcp_list_tools` — produced after tool discovery, contains available tools:
```json
{
  "id": "mcpl_68a6102a4968819c8177b05584dd627b0679e572a900e618",
  "type": "mcp_list_tools",
  "server_label": "dmcp",
  "tools": [
    {
      "annotations": null,
      "description": "Given a string of text describing a dice roll...",
      "input_schema": {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"diceRollExpression": {"type": "string"}},
        "required": ["diceRollExpression"],
        "additionalProperties": false
      },
      "name": "roll"
    }
  ]
}
```

2. `mcp_call` — produced when the model invokes an MCP tool:
```json
{
  "id": "mcp_68a6102d8948819c9b1490d36d5ffa4a0679e572a900e618",
  "type": "mcp_call",
  "approval_request_id": null,
  "arguments": "{\"diceRollExpression\":\"2d4 + 1\"}",
  "error": null,
  "name": "roll",
  "output": "4",
  "server_label": "dmcp"
}
```

3. `mcp_approval_request` — produced when tool requires human approval (based on `require_approval` setting):
```json
{
  "id": "mcpr_68a619e1d82c8190b50c1ccba7ad18ef0d2d23a86136d339",
  "type": "mcp_approval_request",
  "arguments": "{\"diceRollExpression\":\"2d4 + 1\"}",
  "name": "roll",
  "server_label": "dmcp"
}
```

4. `mcp_approval_response` — input item sent by the client to approve/reject:
```json
{
  "type": "mcp_approval_response",
  "approve": true,
  "approval_request_id": "mcpr_682d498e3bd4819196a0ce1664f8e77b04ad1e533afccbfa"
}
```

**Relationship between `{"type": "mcp"}` and output items:** The `{"type": "mcp"}` tool in the request tells OpenAI's servers to connect to the MCP server at `server_url`. The server then: (a) connects and calls `tools/list`, emitting `mcp_list_tools`; (b) when the model decides to use a tool, emits `mcp_call` after executing it server-side; (c) if approval is required, emits `mcp_approval_request` and waits for `mcp_approval_response` input before executing [1][2].

### Reference
- [1] OpenAI "Use MCP tools and connectors" guide — https://developers.openai.com/api/docs/guides/tools-connectors-mcp
- [2] OpenAI Cookbook "Connecting to MCP servers" guide — https://cookbook.openai.com/examples/mcp/mcp_tool

---

## 37. Codex CLI MCP tool wire format: `{"type": "namespace"}` wrapping (not `{"type": "mcp"}`)

### Description
Does Codex CLI use the public `{"type": "mcp"}` tool type on the wire, or does it use a different mechanism? This is critical for the proxy design because the wire format determines what the proxy actually receives and must convert.

### Result

**Codex CLI does NOT use `{"type": "mcp"}` on the wire.** Instead, it wraps MCP tools using a `{"type": "namespace"}` tool type that is a proprietary ChatGPT-backend extension, NOT part of the public Responses API spec [1][2].

**Wire format (what Codex sends to the backend):**
```json
{
  "type": "namespace",
  "name": "mcp__memory__",
  "tools": [
    {
      "type": "function",
      "name": "read_entities",
      "parameters": {
        "type": "object",
        "properties": {...},
        "required": [...]
      }
    },
    ...
  ]
}
```

The backend responds with:
```json
{
  "type": "function_call",
  "id": "fc_...",
  "call_id": "call_...",
  "name": "read_entities",
  "namespace": "mcp__memory__",
  "arguments": "{...}",
  "status": "completed"
}
```

Codex uses the `namespace` field in `function_call` response items to route the call to the correct MCP server. It does NOT use tool name prefixes for routing — the `namespace` field is the routing mechanism [2].

**Why this matters for the proxy:**

1. **The `{"type": "namespace"}` type is proprietary** — only the ChatGPT websocket backend understands it. Custom backends (llama.cpp, LM Studio, DeepSeek, Ollama) cannot handle it — tools get silently dropped or rejected [1].

2. **Codex issue #23186** tracks this problem: when using custom `model_providers.X` with `wire_api = "responses"` and transport `responses_http`, the `namespace` type is sent but not understood by non-ChatGPT backends. The proposed fix is to flatten namespace tools into top-level `{"type": "function", "function": {"name": "mcp__<server>__<tool>", "parameters": {...}}}` [1].

3. **For our proxy:** We will receive `{"type": "namespace", "name": "mcp__<server>__", "tools": [...]}` in the request tools array (not `{"type": "mcp"}`). The proxy must flatten these into Anthropic custom tools with names like `mcp__<server>__<tool>`. On the response path, the proxy must reconstruct the `namespace` field in `function_call` items so Codex can route tool calls to the correct MCP server. This is already described in the spec's MCP Namespace Handling section [3].

4. **`{"type": "mcp"}` is irrelevant for Codex proxy scenarios** — Codex never sends it. The `{"type": "mcp"}` type in the public Responses API is a hosted tool where OpenAI's servers handle MCP execution server-side [36]. Codex handles MCP execution client-side and therefore uses the namespace-based approach instead.

**How existing implementations handle this:**

- **Codex CLI** [2]: Uses `{"type": "namespace"}` wrapping natively. The MCP handler in `codex-rs/core/src/tools/handlers/mcp.rs` registers tools with the session, and `codex-rs/core/src/session/mcp.rs` packages them as namespace tools for the Responses API request. The `responses_api.rs` tool serialization handles the namespace wrapping.

- **Proposed Codex fix (issue #23186)** [1]: Flatten into top-level function tools with `mcp__<server>__<tool>` naming convention. This would make tools compatible with any Responses API backend but loses the namespace-based routing that Codex relies on.

- **Our proxy design (spec section "MCP Namespace Handling")** [3]: Takes the namespace tools from the request, flattens them into Anthropic custom tools with composite names, maintains a registry for round-trip reconstruction, and emits `function_call` items with the `namespace` field on the response path. This preserves Codex's routing mechanism while converting to/from Anthropic's format.

### Reference
- [1] Codex CLI issue #23186 — "MCP tools silently dropped with custom backends" — https://github.com/openai/codex/issues/23186
- [2] Codex CLI source code: `codex-rs/core/src/tools/handlers/mcp.rs`, `codex-rs/core/src/session/mcp.rs`, `codex-rs/tools/src/responses_api.rs` — https://github.com/openai/codex
- [3] Design spec "MCP Namespace Handling" section — `docs/superpowers/specs/2026-05-13-codex-conv-design.md`
- [4] Entry #36 — OpenAI Responses API `{"type": "mcp"}` tool field structure

---

## 38. Comprehensive spec audit: missing conversion coverage

### Description
Full audit of the design spec against latest Anthropic Messages API, OpenAI Responses API, and Codex CLI source code to identify missing conversion coverage. Goal: maximize feature conversion support without introducing non-conversion-essential behavioral logic.

### Result

Verified against official sources. The following gaps were identified and fixed:

**`output_config` and `thinking.display` confirmed real** [1][2][3]:
- `output_config` is a top-level Anthropic request parameter with `effort` and `format` sub-fields, documented at `/api/messages/create`
- `thinking.display` is a request parameter accepting `"summarized"` and `"omitted"`, documented at Extended Thinking docs

**New conversion mappings added:**
1. `{"type": "custom"}` tool type — Codex uses this for freeform tools (e.g., apply_patch with Lark grammar). Mapped to Anthropic custom tool with derived `input_schema` [4]
2. `{"type": "computer"}` and `{"type": "web_search_preview"}` — GA/preview variants of existing types. Same schema mapping [5]
3. `text.format` → `output_config.format` — Codex sends `text.format` with `type: "json_schema"`, `name: "codex_output_schema"`, and a JSON schema for structured output. Anthropic's `output_config.format` only accepts `type` and `schema` — `name` and `strict` have no Anthropic equivalent. Verified against OpenAI Python SDK (`ResponseFormatTextJSONSchemaConfig`), Anthropic Python SDK (`JSONOutputFormatParam` with only `type` + `schema`), and Codex CLI source (`TextFormat` struct in `common.rs`) [4][5][6][7]
4. `tool_search_output` input item — tool discovery metadata. Dropped (tools already registered in current request) [4]
5. `mcp_tool_call_output` input item — Codex-specific variant for MCP tool results. Mapped same as `function_call_output` [4]
6. Unknown input items — Codex uses `#[serde(other)]` catch-all. Proxy drops with warning log [4]
7. `usage.total_tokens` — computed as `input_tokens + output_tokens` in response [5]
8. `response.completed` response object — added `object`, `created_at`, `completed_at`, `model`, `metadata`, `parallel_tool_calls`, `tool_choice`, `instructions` fields [5]
9. `input_file` content block — dropped (Anthropic `document` block has different schema) [1]
10. `incomplete_details.reason: "content_filter"` — documented as not producible from Anthropic signals [5]

**Dropped fields documented (13 new entries):** `user`, `safety_identifier`, `max_tool_calls`, `top_logprobs`, `background`, `conversation`, `context_management`, `prompt`, `prompt_cache_retention`, `verbosity` (top-level), `stream_options`, `reasoning.generate_summary` [5]

**Non-gaps verified:**
- `custom_tool_call_input.delta` SSE event — not needed because Codex maps both it and `function_call_arguments.delta` to the same `ToolCallInputDelta` handler [4]
- `success` field on `function_call_output` — `skip_serializing_if None` means present only when explicitly set; mapping is correct when present [4]

### Reference
- [1] Anthropic Messages API: Create a Message — https://platform.claude.com/docs/en/api/messages/create
- [2] Anthropic Extended Thinking — https://platform.claude.com/docs/en/build-with-claude/extended-thinking
- [3] Anthropic Adaptive Thinking — https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking
- [4] Codex CLI source code: `codex-rs/codex-api/src/common.rs`, `codex-rs/protocol/src/models.rs`, `codex-rs/tools/src/tool_spec.rs`, `codex-rs/codex-api/src/sse/responses.rs` — https://github.com/openai/codex
- [5] OpenAI Responses API — https://developers.openai.com/api/reference/responses/
- [6] OpenAI Python SDK: `ResponseFormatTextJSONSchemaConfig` — https://github.com/openai/openai-python/blob/master/src/openai/types/responses/response_format_text_json_schema_config.py (flat structure with `name`, `schema`, `strict` as siblings of `type`)
- [7] Anthropic Python SDK: `JSONOutputFormatParam` — https://github.com/anthropics/anthropic-sdk-python/blob/master/src/anthropic/types/json_output_format_param.py (only `type` and `schema`, no `name` or `strict`)

---

## 39. Codex CLI "developer" role in input messages

### Description
During integration testing against a live Anthropic API backend, requests were rejected with 422 Unprocessable Entity. Investigation revealed that Codex CLI sends messages with `role: "developer"` in the Responses API input, which the Anthropic Messages API does not accept.

### Result
Codex CLI v0.130.0 sends a `developer` role message as the first input item, containing system-level instructions (permissions, skills, etc.). The Anthropic Messages API only accepts `user`, `assistant`, and `system` roles. The proxy must map `developer` → `user` during request conversion.

Verified by inspecting the converted request body in proxy debug logs: the first message had `role: "developer"` which caused 422 from upstream. After mapping to `user`, the request was accepted.

This mapping is safe because:
1. The `developer` role is a Codex CLI concept for separating system instructions from user content
2. Anthropic's `system` field already handles top-level system instructions
3. The `developer` message content is forwarded as a user message, which Anthropic processes normally

### Reference
- [1] Codex CLI v0.130.0 source code — developer role usage in request construction
- [2] Anthropic Messages API — supported roles: https://platform.claude.com/docs/en/api/messages

---

## 40. Anthropic SSE stream termination behavior (no [DONE] marker)

### Description
During integration testing, the `reqwest-eventsource` library reported errors on every request with "Stream ended". Investigation was needed to determine if this was a real error or normal behavior.

### Result
The Anthropic Messages API SSE stream terminates by simply closing the HTTP connection after the `message_stop` event. Unlike OpenAI's API which sends an explicit `data: [DONE]` marker, Anthropic does not send any terminal marker. The `reqwest-eventsource` library interprets this connection close as an error ("Stream ended"), but it is actually normal behavior.

The proxy must handle this gracefully:
- When EventSource yields `Err("Stream ended")` AND a StreamingState exists (meaning message_start was received), treat it as normal stream termination
- Only emit error events when the stream ends BEFORE receiving message_start (indicating an actual upstream error like 4xx/5xx)

This was confirmed by comparing direct `curl` requests to the upstream (which complete normally) with the EventSource behavior.

### Reference
- [1] Anthropic Streaming Messages docs — https://platform.claude.com/docs/en/build-with-claude/streaming
- [2] reqwest-eventsource crate — https://docs.rs/reqwest-eventsource/0.6

---

## 41. Codex CLI tool registration: confirmed tools in practice

### Description
During integration testing, the exact set of tools Codex CLI registers in the Responses API request was captured, along with confirmation that `apply_patch` is called as a native function tool.

### Result
Codex CLI v0.130.0 registers these 12 tools in the `tools` array:
1. `exec_command` — shell execution
2. `write_stdin` — stdin pipe to running process
3. `update_plan` — plan tracking
4. `request_user_input` — user interaction
5. `apply_patch` — file editing (function tool with freeform grammar)
6. `web_search` — web search
7. `view_image` — image viewing
8. `spawn_agent` — agent spawning
9. `send_input` — agent input
10. `resume_agent` — agent resume
11. `wait_agent` — agent wait
12. `close_agent` — agent close

The `apply_patch` tool is confirmed to be called as a native function tool (NOT via exec_command workaround). Proxy logs showed the tool_use history building up with both `exec_command` (for file reads) and `apply_patch` (for actual edits):
- Request 1: `tool_uses=[]` (initial prompt)
- Request 2: `tool_uses=["exec_command"]` (read file)
- Request 3: `tool_uses=["exec_command", "apply_patch"]` (read file, apply patch)
- Subsequent requests: additional tool calls for verification

### Reference
- [1] Codex CLI v0.130.0 — proxy debug logs captured during integration testing
- [2] Codex CLI source code: `codex-rs/tools/src/tool_spec.rs` — https://github.com/openai/codex
