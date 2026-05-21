# apply_patch Transparent Retry Spec

## Research Background

### apply_patch in Codex CLI

Codex CLI has a built-in `apply_patch` tool for file editing. Its behavior is controlled by the `apply_patch_tool_type` field in `models_cache.json`:

- `apply_patch_tool_type: None` → ApplyPatchHandler is NOT registered. The model can only edit files via shell commands.
- `apply_patch_tool_type: "freeform"` → ApplyPatchHandler is registered as a `ToolSpec::Freeform`. The tool is NOT included in the API request's `tools` array (the model knows it from `base_instructions`). When the model calls it, the response is a `custom_tool_call` (not `function_call`) with raw text `input` (not JSON `arguments`).

The freeform patch format is:
```
*** Begin Patch
*** Update File: <path>
-<line to remove>
+<line to add>
*** End Patch
```

This format is defined by a Lark grammar file in the Codex source. OpenAI's API uses this grammar to constrain the model's output. Anthropic's API has no equivalent grammar constraint mechanism.

### Problem Discovery: Protocol Conversion Gaps

When Codex sends a request through our proxy to Anthropic, we found multiple protocol gaps that caused apply_patch to fail. These were discovered and fixed in earlier commits:

**Gap 1: ApplyPatchHandler not registered.** Codex only registers the handler when `apply_patch_tool_type` is set in `models_cache.json`. Our integration tests didn't create this file. Fixed by writing `models_cache.json` with `"apply_patch_tool_type": "freeform"` in the test CODEX_HOME.

**Gap 2: Tool definition not sent to Anthropic.** In freeform mode, Codex doesn't include `apply_patch` in the `tools` array. But Anthropic requires explicit tool definitions. Fixed by adding `inject_apply_patch_tool()` in `src/conversion/request.rs` that auto-injects an `apply_patch` tool definition with a string `input_schema` when absent.

**Gap 3: Response type mismatch.** Anthropic returns `tool_use` content blocks, but Codex dispatches apply_patch via `custom_tool_call` (not `function_call`). Fixed by detecting `apply_patch` tool names and generating `custom_tool_call` SSE events with raw text `input` instead of `function_call` with JSON `arguments`.

**Gap 4: Patch format mismatch.** Anthropic models produce unified diff format (`--- a/file, +++ b/file, @@ @@`). Codex requires freeform format. Fixed by adding `convert_to_freeform_patch()` converter.

**Gap 5: JSON wrapper.** Anthropic wraps tool input in JSON (`{"input":"..."}` or `{"patch":"..."}`). Codex's `custom_tool_call.input` expects raw text. Fixed by unwrapping the JSON layer.

**Gap 6: Echo-back format.** When Codex echoes a `custom_tool_call` in subsequent requests, the `input` field is raw text (not JSON). The proxy tried to parse it as JSON and failed. Fixed by wrapping raw text in `{"input": raw_text}` when JSON parsing fails for `custom_tool_call` items.

### Remaining Problem: Unrecognized Formats

After the above fixes, the proxy handles valid freeform and unified diff correctly. But when Anthropic produces a format that is neither (e.g., a description paragraph, a mixed format, or something unexpected), `convert_to_freeform_patch()` produces garbage. Codex's `ApplyPatchHandler` fails to parse, and Codex falls back to `exec_command`.

This fallback is the core problem. When Codex can't use apply_patch, it doesn't retry with apply_patch — it switches to shell commands entirely. This is not a simple retry; it's a mode switch that produces fragile, hard-to-debug behavior. The project's goal is maximum compatibility, so we must prevent this fallback.

### Approach Evolution

**Attempt 1: Improve convert_to_freeform_patch().**
More format detection cases. Problem: there are infinite possible wrong formats. We can't enumerate them all. This approach is fundamentally limited.

**Attempt 2: Proxy-level retry with model correction.**
When format is invalid, make a new Anthropic request with an error message. Problem: by the time we detect the invalid format at `content_block_stop`, SSE events (text, thinking) have already been forwarded to Codex. We can't unsend them. The Responses API has no cancel/reset mechanism.

**Attempt 3: Fake server error to prevent fallback.**
Send a fabricated `response.failed` server error to Codex when we detect invalid format. Codex sees a transient error and retries the original request (not falling back to exec_command). Problem: Codex retries with the same prompt, model likely produces the same invalid format. Infinite loop of identical failures.

**Attempt 4: Anthropic assistant message prefill.**
Use Anthropic's prefill feature: send a partial assistant message ending with `*** Begin Patch\n*** Update File: path\n` to guide the model into continuing in freeform format. The already-forwarded text/thinking remains valid; the model just continues from where we left off. This was the ideal solution — seamless, no duplicate content.

Research found this is **not viable**: Anthropic removed prefill support from all Claude 4.6+ models (Sonnet 4.6, Opus 4.6, Opus 4.7). Sending a messages array ending with an assistant message returns 400. Even on older models that support prefill, it only works for text content, not tool_use blocks.

**Attempt 5: Buffer + retry with tool_result pattern (current approach).**
Use Anthropic's standard tool-use retry pattern. When invalid format is detected:
1. Stop forwarding SSE events to Codex
2. Buffer the rest of the Anthropic response
3. Construct a retry request: original messages + assistant message (all received content blocks including the failed apply_patch) + user message with `tool_result` containing format error and freeform syntax definition
4. Stream the retry response through the same `tx` channel

Key insight from the conversation: the retry request includes all previously received content (text, thinking, failed apply_patch) in the assistant message. Since the model sees its own prior output, it has full context and will likely continue quickly to a corrected apply_patch. The retry response may include additional text/thinking, but multiple output items are valid in the Responses API.

An important refinement: when an invalid apply_patch is detected at `content_block_stop`, we close the upstream Anthropic connection immediately — we don't wait for any subsequent content blocks. From Anthropic's perspective, it's as if the user interrupted the response. This saves token budget (no wasted generation after the failed block) and reduces latency before the retry.

This approach is viable because:
- Anthropic's tool-use conversation pattern (assistant with `tool_use` → user with `tool_result`) is a standard API feature
- Closing the SSE connection mid-stream is equivalent to a user interrupt — Anthropic bills only for tokens generated up to that point
- The `tx` channel is shared across retry iterations within the same tokio::spawn task
- `StreamingState` can be put into `retry_mode` to suppress duplicate `response.created` events
- Thinking blocks (including `redacted_thinking`) are preserved in the retry assistant message as required by Anthropic's multi-turn protocol

### Format Detection Logic

Format detection is the core decision mechanism. It happens during delta accumulation, not at content_block_stop. The logic is intentionally simple and binary:

**During each `input_json_delta` for an apply_patch block:**
- Check if the accumulated `arguments_accumulator` contains `*** Begin Patch`
- If found: `apply_patch_format_confirmed = true`. Release buffered OutputItemAdded. Resume streaming. No further format checks.
- If enough content has accumulated to be certain it's NOT freeform (e.g., starts with `--- a/`, starts with a non-patch string, starts with `{` that doesn't contain `*** Begin Patch` after sufficient accumulation): `apply_patch_invalid = true`. No conversion attempt, no second chance.

There is no middle ground. The format is either freeform or it's not. We do NOT attempt `convert_to_freeform_patch()` as a silent fallback — the model should learn to produce the correct format. The `convert_to_freeform_patch()` function remains in the codebase but is only used for non-retry scenarios (e.g., if the proxy decides not to retry for some reason).

## Problem

The codex-conv proxy converts between Codex CLI (OpenAI Responses API) and Anthropic Messages API. When Anthropic generates an `apply_patch` tool_use response, the patch content must be in Codex freeform format (`*** Begin Patch / *** End Patch`). Anthropic models frequently produce unified diff or other unrecognized formats instead.

The current proxy attempts conversion via `convert_to_freeform_patch()` (handles unified diff), but when the format is entirely unrecognized, the proxy passes through garbage. Codex's `ApplyPatchHandler` fails to parse, and Codex falls back to `exec_command` (shell commands like `sed`/`echo`). This fallback is fragile and violates the project goal of maximum compatibility.

## Solution

Transparent retry within a single downstream request. Format validation happens early — as soon as `input_json_delta` fragments arrive. The logic is binary:

- If accumulated content contains `*** Begin Patch` → valid, release buffer, resume streaming. No further checks.
- If the content clearly does not start with freeform format → invalid, declared immediately. No conversion attempt, no waiting for content_block_stop.

Once invalid format is declared:

1. Stop forwarding SSE events to Codex
2. Wait for `content_block_stop` (complete the current block so the Anthropic response is in a clean state), then immediately close the upstream connection (saves token budget — no wasted generation after the invalid block)
3. Construct a retry request with the received content blocks as assistant message + a tool_result error containing the freeform syntax definition
4. Stream the retry response to Codex through the same SSE channel

Codex sees one continuous SSE stream. The retry is invisible. No retry limit — every invalid apply_patch triggers a retry.

## Architecture

### Lifecycle (within a single downstream request)

```
Codex request → proxy converts → Anthropic streams response
    ├─ text/thinking blocks → forwarded immediately
    ├─ apply_patch tool_use detected (content_block_start)
    │   ├─ Buffer OutputItemAdded (don't emit yet)
    │   ├─ Accumulate deltas (input_json_delta)
    │   │   ├─ Contains "*** Begin Patch" → VALID
    │   │   │   ├─ Release buffer, resume streaming
    │   │   │   └─ content_block_stop → emit OutputItemDone
    │   │   └─ Clearly NOT freeform format → INVALID (declared immediately)
    │   │       ├─ No conversion attempt, no further checks
    │   │       ├─ Wait for content_block_stop (clean block boundary)
    │   │       └─ Close upstream connection (like user interrupt)
    └─ (if retry triggered)
        ├─ Construct retry: original messages + assistant(received content) + user(tool_result error)
        ├─ New Anthropic request → stream response through same tx channel
        └─ Repeat if apply_patch still invalid
```

### Retry Request Construction

The retry request is a standard Anthropic Messages API call using the tool-use conversation pattern:

```json
{
  "messages": [
    ...original request messages,
    {
      "role": "assistant",
      "content": [
        // All content blocks from the failed response:
        // thinking/thinking with signature, text, redacted_thinking,
        // and the failed apply_patch tool_use
      ]
    },
    {
      "role": "user",
      "content": [
        {
          "type": "tool_result",
          "tool_use_id": "<toolu_id from failed apply_patch>",
          "is_error": true,
          "content": "<freeform format error message + syntax definition>"
        }
      ]
    }
  ]
}
```

All other fields from the original request (system, tools, model, thinking config, etc.) are preserved unchanged.

### Error Message Content

The `tool_result` error message tells the model:

1. The format was not accepted
2. The correct freeform syntax definition (*** Begin Patch, *** Update/Add/Delete File, +/- lines, *** End Patch)
3. Explicit instruction: "The FIRST line MUST be exactly `*** Begin Patch`. Do NOT use unified diff format."

The error message does NOT attempt to fix or reformat the patch — that requires model capability.

## Components

### StreamingState Changes (`src/conversion/response.rs`)

New fields:

| Field | Type | Purpose |
|-------|------|---------|
| `anthropic_content_blocks` | `Vec<Value>` | Always-captured Anthropic content blocks for retry assistant message |
| `retry_mode` | `bool` | Skip response.created/in_progress on retry response |
| `apply_patch_invalid` | `bool` | Set during delta accumulation when format is clearly not freeform. content_block_stop uses this as the signal to close upstream. |
| `buffered_apply_patch_item` | `Option<Value>` | Held-back OutputItemAdded for apply_patch |
| `buffered_apply_patch_output_index` | `Option<usize>` | Output index at content_block_start time |
| `apply_patch_format_confirmed` | `bool` | `*** Begin Patch` detected in accumulated deltas |
| `failed_apply_patch_info` | `Option<(String, String)>` | (toolu_id, raw_patch) for retry error |

New public methods:

- `is_apply_patch_invalid() -> bool`
- `take_content_block_capture() -> Vec<Value>`
- `take_failed_apply_patch_info() -> (String, String)`
- `prepare_for_retry()` — Sets retry_mode, resets accumulators, preserves output_items/fc_counter/response_id

### Behavior Changes

**content_block_start (apply_patch):** Buffer OutputItemAdded instead of emitting. Record output_index.

**content_block_delta (apply_patch):** Accumulate silently in `arguments_accumulator`. Two outcomes:
- Accumulated content contains `*** Begin Patch` → set `apply_patch_format_confirmed`, release buffered OutputItemAdded. From this point, normal streaming resumes (deltas still accumulated silently, OutputItemDone emitted at content_block_stop).
- Accumulated content clearly does NOT start with freeform format (enough content received to be certain) → set `apply_patch_invalid` immediately. No conversion attempt. The remaining deltas continue to be consumed but no events are emitted.

**content_block_stop (apply_patch):**
- If `apply_patch_format_confirmed`: emit OutputItemDone with raw patch. Normal flow.
- If `apply_patch_invalid`: this is the signal to close the upstream connection. The router detects this flag, closes EventSource, and begins retry. No events emitted for this block.

**content_block_stop (all blocks):** Push Anthropic-format content block to `anthropic_content_blocks` for potential retry.

**message_start (retry_mode):** Do NOT emit ResponseCreated/ResponseInProgress. Do NOT update response_id. Accumulate usage tokens additively.

**message_stop (retry_mode):** Emit ResponseCompleted with combined output_items (original + retry). Use original response_id and created_at.

### Router Changes (`src/router.rs`)

**Additional state capture:** Clone `client`, `api_key`, `anthropic_version`, `upstream_url`, `anthropic_body` into the tokio::spawn closure.

**Retry loop:** The current flat streaming loop becomes an outer retry loop:
1. Create EventSource from current request body
2. Consume SSE events, forward via tx channel
3. If `apply_patch_invalid` detected during delta accumulation: stop forwarding, continue consuming silently until `content_block_stop`, then close EventSource
4. If needs_retry: construct retry body, continue outer loop (creates new EventSource)
5. If no retry needed: break

The `tx` channel is shared across all retry iterations. Codex sees one continuous SSE stream.

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| Valid freeform on first try | Early detection (`*** Begin Patch` in deltas) releases buffer, normal streaming, no retry |
| Unified diff format | Not freeform → retry. The model is told the correct format via tool_result error. |
| Any non-freeform format | Retry with error message + freeform syntax definition |
| Retry also invalid | Retry again with accumulated context. No limit. |
| Model responds with text instead of apply_patch after retry | Forward normally. Model's choice. |
| Text/thinking already forwarded before detection | Already sent to Codex. Retry adds more items. Multiple output items are valid in Responses API. |
| Invalid apply_patch detected | Close upstream immediately after content_block_stop, construct retry. No tokens wasted on subsequent generation. |
| No apply_patch in response at all | No special handling, normal flow |
| Multiple apply_patch in one response | Each validated independently. Invalid ones trigger retry. |
| Thinking blocks in failed response | Must be preserved in retry assistant message (including signatures and redacted_thinking) — required by Anthropic multi-turn protocol |

## Constraints

- **Per-request state only.** No cross-request state. All retry state lives within the tokio::spawn task lifecycle.
- **Anthropic prefill removed on Claude 4.6+.** Cannot use assistant-message continuation. Must use standard tool-use retry pattern (assistant + user with tool_result).
- **Responses API has no cancel/reset mechanism.** Already-forwarded SSE events cannot be unsent. This is why early detection matters — the less we forward before detecting a problem, the cleaner the retry.
- **Thinking blocks must be preserved.** Anthropic requires thinking blocks (with signatures) to be passed back unmodified in multi-turn conversations. Dropping them causes API errors.

## Files to Modify

| File | Changes |
|------|---------|
| `src/conversion/response.rs` | StreamingState: new fields, content block capture, apply_patch buffering, early detection, format validation, retry mode |
| `src/router.rs` | Additional state capture, retry loop, EventSource lifecycle, retry request construction, format_apply_patch_error() |

No changes to SSE event types, request conversion, or other modules.
