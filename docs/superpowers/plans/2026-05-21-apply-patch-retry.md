# apply_patch Transparent Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When Anthropic generates an apply_patch with invalid format (neither freeform nor convertible unified diff), the proxy transparently retries with a tool_result error so Codex never falls back to exec_command.

**Architecture:** StreamingState gains content-block capture and apply_patch buffering. During delta accumulation, only positive detection is performed (looking for `*** Begin Patch`). At content_block_stop, unrecognized formats trigger conversion attempt then retry. The router wraps its streaming loop in an outer retry loop that constructs a new Anthropic request with the failed response as assistant message plus a tool_result error.

**Tech Stack:** Rust, tokio, axum, reqwest-eventsource, serde_json

**Spec:** `docs/superpowers/specs/2026-05-21-apply-patch-retry-design.md`

---

### Task 1: Add new fields to StreamingState

**Files:**
- Modify: `src/conversion/response.rs:18-65` (StreamingState struct and `new()`)

Add these fields to `StreamingState`:

```rust
/// Always-captured Anthropic content blocks for retry assistant message.
anthropic_content_blocks: Vec<Value>,
/// Whether we're processing a retry response (skip response.created/in_progress).
retry_mode: bool,
/// Set at content_block_stop when apply_patch was never confirmed valid AND
/// convert_to_freeform_patch() also fails. Router reads this flag to close upstream
/// and trigger retry.
apply_patch_invalid: bool,
/// Held-back OutputItemAdded event for apply_patch (released on confirmation or at stop).
buffered_apply_patch_item: Option<Value>,
/// Output index at the time apply_patch content_block_start was seen.
buffered_apply_patch_output_index: Option<usize>,
/// Whether *** Begin Patch has been detected in accumulated deltas for the current
/// apply_patch block. Reset to false at each content_block_start.
apply_patch_format_confirmed: bool,
/// (toolu_id, raw_patch_text) of the failed apply_patch, for retry error message.
failed_apply_patch_info: Option<(String, String)>,
```

Initialize all to defaults in `StreamingState::new()`:
- `anthropic_content_blocks: Vec::new()`
- `retry_mode: false`
- `apply_patch_invalid: false`
- `buffered_apply_patch_item: None`
- `buffered_apply_patch_output_index: None`
- `apply_patch_format_confirmed: false`
- `failed_apply_patch_info: None`

- [ ] **Step 1: Add fields to struct and new()**

Add the 7 fields listed above to the `StreamingState` struct definition (after `signature_store`). Add their initializers to `StreamingState::new()`.

- [ ] **Step 2: Verify compilation**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: 286 passed, 0 failed (new fields are private and unused, all existing tests still pass)

- [ ] **Step 3: Commit**

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: 287 passed (286 existing + 1 new), 0 failed

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: add retry fields to StreamingState"
```

---

### Task 2: Add public accessor methods to StreamingState

**Files:**
- Modify: `src/conversion/response.rs` (add methods after `drain_signatures()`)

Add these public methods. The tests in subsequent tasks will use these accessors.

```rust
/// Whether the last content_block_stop detected an invalid apply_patch format.
pub fn is_apply_patch_invalid(&self) -> bool {
    self.apply_patch_invalid
}

/// Take all captured Anthropic content blocks (for retry assistant message).
pub fn take_content_block_capture(&mut self) -> Vec<Value> {
    std::mem::take(&mut self.anthropic_content_blocks)
}

/// Take the failed apply_patch info: (toolu_id, raw_patch_text).
/// Returns empty strings if not set.
pub fn take_failed_apply_patch_info(&mut self) -> (String, String) {
    self.failed_apply_patch_info.take().unwrap_or_default()
}

/// Whether we're in retry mode.
pub fn is_retry_mode(&self) -> bool {
    self.retry_mode
}

/// View current output items (for testing).
pub fn output_items(&self) -> &[Value] {
    &self.output_items
}

/// Prepare state for a retry response.
/// Sets retry_mode, resets per-response accumulators, preserves output_items,
/// fc_counter, response_id, model, created_at, namespace_registry, echo fields.
pub fn prepare_for_retry(&mut self) {
    self.retry_mode = true;
    self.active_block = ActiveBlock::None;
    self.text_accumulator.clear();
    self.thinking_accumulator.clear();
    self.arguments_accumulator.clear();
    self.signature_accumulator.clear();
    self.current_reasoning_id.clear();
    self.current_fc_item_id.clear();
    self.current_fc_call_id.clear();
    self.current_is_custom = false;
    self.stop_reason = None;
    self.output_tokens = 0;
    self.apply_patch_format_confirmed = false;
    self.apply_patch_invalid = false;
    self.buffered_apply_patch_item = None;
    self.buffered_apply_patch_output_index = None;
    self.anthropic_content_blocks.clear();
    self.failed_apply_patch_info = None;
    self.signature_store.clear();
    self.tool_use_map.clear();
    // Preserved: output_items, fc_counter, response_id, model, created_at,
    // namespace_registry, tool_choice_echo, instructions_echo, parallel_tool_calls_echo,
    // input_tokens, cache_read_tokens
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn new_state_accessors_return_defaults() {
    let state = make_state();
    assert!(!state.is_retry_mode());
    assert!(!state.is_apply_patch_invalid());
    assert!(state.output_items().is_empty());
}

#[test]
fn prepare_for_retry_preserves_output_items() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_ORIGINAL", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    // Add a text block
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "text", "text": ""}),
    });
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "text_delta", "text": "Hello"}),
    });
    state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    assert_eq!(state.output_items().len(), 1);

    state.prepare_for_retry();

    // output_items preserved
    assert_eq!(state.output_items().len(), 1);
    // retry_mode is true
    assert!(state.is_retry_mode());
    // accumulators are cleared
    assert!(!state.is_apply_patch_invalid());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test new_state_accessors_return_defaults prepare_for_retry_preserves_output_items --lib 2>&1 | tail -10`
Expected: compilation error (methods don't exist yet)

- [ ] **Step 3: Implement the accessor methods**

Add all 6 methods shown above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test new_state_accessors_return_defaults prepare_for_retry_preserves_output_items --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass (286 existing + 2 new)

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: add public accessor methods and prepare_for_retry to StreamingState"
```

---

### Task 3: Capture content blocks at content_block_stop

**Files:**
- Modify: `src/conversion/response.rs:438-595` (`handle_content_block_stop`)

At the end of every arm in `handle_content_block_stop`, push an Anthropic-format content block to `self.anthropic_content_blocks`. This capture happens unconditionally so the retry always has the full assistant message.

The content block format matches what Anthropic sent:

- **Text arm** (line ~443-463): After existing code, push `json!({"type": "text", "text": text})`
- **Thinking arm** (line ~464-493): After existing code, push `json!({"type": "thinking", "thinking": thinking_text, "signature": signature})` (only if signature is non-empty, otherwise omit the field)
- **RedactedThinking arm** (line ~495-507): After existing code, push `json!({"type": "redacted_thinking", "data": encrypted_content})`
- **ToolUse arm** (line ~508-588): After existing code, push the Anthropic-format tool_use block:
  ```rust
  let toolu_id = self.tool_use_map.iter()
      .find(|(_, fid, _, _)| fid == &fc_id)
      .map(|(tid, _, _, _)| tid.clone())
      .unwrap_or_default();
  let raw_name = self.tool_use_map.iter()
      .find(|(_, fid, _, _)| fid == &fc_id)
      .map(|(_, _, _, n)| n.clone())
      .unwrap_or_default();
  // For the input, use the parsed JSON (Anthropic's original format).
  let input_value: Value = serde_json::from_str(&args).unwrap_or(json!({}));
  self.anthropic_content_blocks.push(json!({
      "type": "tool_use",
      "id": toolu_id,
      "name": raw_name,
      "input": input_value,
  }));
  ```

- [ ] **Step 1: Write the failing test**

Tests use `take_content_block_capture()` from Task 2:

```rust
#[test]
fn content_blocks_captured_at_stop() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    // Text block
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "text", "text": ""}),
    });
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "text_delta", "text": "Hello"}),
    });
    state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    // Tool use block
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 1,
        content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "run", "input": {}}),
    });
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 1,
        delta: json!({"type": "input_json_delta", "partial_json": "{\"cmd\":\"ls\"}"}),
    });
    state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

    let captured = state.take_content_block_capture();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["type"], "text");
    assert_eq!(captured[0]["text"], "Hello");
    assert_eq!(captured[1]["type"], "tool_use");
    assert_eq!(captured[1]["id"], "toolu_01");
    assert_eq!(captured[1]["name"], "run");
}

#[test]
fn thinking_captured_with_signature() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "thinking", "thinking": ""}),
    });
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "thinking_delta", "thinking": "hmm"}),
    });
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "signature_delta", "signature": "SIG123"}),
    });
    state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    let captured = state.take_content_block_capture();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["type"], "thinking");
    assert_eq!(captured[0]["thinking"], "hmm");
    assert_eq!(captured[0]["signature"], "SIG123");
}

#[test]
fn redacted_thinking_captured() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "redacted_thinking", "data": "ENCRYPTED_BLOB"}),
    });
    state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    let captured = state.take_content_block_capture();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["type"], "redacted_thinking");
    assert_eq!(captured[0]["data"], "ENCRYPTED_BLOB");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test content_blocks_captured_at_stop --lib 2>&1 | tail -5`
Expected: assertion failure (captured is empty)

- [ ] **Step 3: Implement content block capture**

At the end of each arm in `handle_content_block_stop`, before the closing `}`, push the corresponding Anthropic content block to `self.anthropic_content_blocks` as shown above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test content_blocks_captured_at_stop thinking_captured_with_signature redacted_thinking_captured --lib`
Expected: all 3 PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: capture Anthropic content blocks for retry assistant message"
```

---

### Task 4: Buffer apply_patch at content_block_start

**Files:**
- Modify: `src/conversion/response.rs:297-359` (`handle_content_block_start` tool_use arm)

When `is_custom_tool_call` is true (apply_patch), buffer the OutputItemAdded event instead of emitting it. Reset `apply_patch_format_confirmed` to false.

**Current behavior (line 297-359):** The tool_use arm always emits `vec![ResponsesEvent::OutputItemAdded { output_index, item }]`.

**New behavior when `is_custom_tool_call`:**
- Reset `self.apply_patch_format_confirmed = false`
- Store `(output_index, item)` in `self.buffered_apply_patch_output_index` and `self.buffered_apply_patch_item`
- Return `vec![]` (no events emitted yet)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn apply_patch_buffered_at_start_not_emitted() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
    });

    // Should NOT emit OutputItemAdded yet (buffered instead)
    assert!(!events.iter().any(|e| {
        let (t, _) = e.to_sse();
        t == "response.output_item.added"
    }), "apply_patch OutputItemAdded should be buffered, not emitted");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test apply_patch_buffered_at_start_not_emitted --lib`
Expected: FAIL (currently emits OutputItemAdded)

- [ ] **Step 3: Implement buffering in content_block_start**

In the `"tool_use"` arm of `handle_content_block_start`, after building the `item` value, add:

```rust
if is_custom_tool_call {
    self.apply_patch_format_confirmed = false;
    self.buffered_apply_patch_output_index = Some(output_index);
    self.buffered_apply_patch_item = Some(item);
    return vec![];
}
vec![ResponsesEvent::OutputItemAdded { output_index, item }]
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test apply_patch_buffered_at_start_not_emitted --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass (the existing apply_patch tests don't test intermediate streaming events, they test the final output which is unchanged at this point)

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: buffer apply_patch OutputItemAdded at content_block_start"
```

---

### Task 5: Positive detection during deltas and release buffer

**Files:**
- Modify: `src/conversion/response.rs:413-423` (`input_json_delta` arm in `handle_content_block_delta`)

**Current behavior (line 421-422):** When `self.current_is_custom`, return `vec![]` silently.

**New behavior:** When `self.current_is_custom`:
- Accumulate into `arguments_accumulator` as before
- If `!self.apply_patch_format_confirmed`:
  - Check if `self.arguments_accumulator.contains("*** Begin Patch")`
  - If found: set `apply_patch_format_confirmed = true`, release buffered OutputItemAdded, return it
- If `apply_patch_format_confirmed`: return `vec![]` (already released)
- If not confirmed: return `vec![]` (still waiting)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn apply_patch_freeform_released_on_positive_detection() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
    });

    // First delta: no *** Begin Patch yet
    let events = state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\""}),
    });
    assert!(!events.iter().any(|e| {
        let (t, _) = e.to_sse();
        t == "response.output_item.added"
    }));

    // Second delta: *** Begin Patch appears
    let events = state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "input_json_delta", "partial_json": "*** Begin Patch\\n*** Update File: test.txt\\n-old\\n+new\\n*** End Patch\"}"}),
    });
    assert!(events.iter().any(|e| {
        let (t, _) = e.to_sse();
        t == "response.output_item.added"
    }), "buffered OutputItemAdded should be released when *** Begin Patch detected");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test apply_patch_freeform_released_on_positive_detection --lib`
Expected: FAIL (buffer is never released)

- [ ] **Step 3: Implement positive detection and buffer release**

Replace the `if self.current_is_custom { return vec![]; }` block (lines 421-422) with:

```rust
if self.current_is_custom {
    // Positive detection: check if accumulated content contains *** Begin Patch
    if !self.apply_patch_format_confirmed
        && self.arguments_accumulator.contains("*** Begin Patch")
    {
        self.apply_patch_format_confirmed = true;
        // Release buffered OutputItemAdded
        let mut events = Vec::new();
        if let Some(item) = self.buffered_apply_patch_item.take() {
            let output_index = self
                .buffered_apply_patch_output_index
                .take()
                .unwrap_or(self.output_items.len());
            events.push(ResponsesEvent::OutputItemAdded { output_index, item });
        }
        return events;
    }
    return vec![];
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test apply_patch_freeform_released_on_positive_detection --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: positive detection of freeform format during apply_patch deltas"
```

---

### Task 6: Handle apply_patch at content_block_stop (conversion + invalid)

**Files:**
- Modify: `src/conversion/response.rs:508-588` (ToolUse arm in `handle_content_block_stop`)

This is where the two-outcome logic lives for apply_patch blocks that were never confirmed valid during deltas.

**In the `is_custom` arm of ToolUse (line 538-566):**

Replace the current logic with:

```rust
if is_custom {
    let raw_input = serde_json::from_str::<Value>(&args)
        .ok()
        .and_then(|v| {
            v.get("patch")
                .or_else(|| v.get("input"))
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or(args);

    if self.apply_patch_format_confirmed {
        // Already confirmed valid during deltas. Emit OutputItemDone.
        let patch = raw_input;
        let mut item = json!({
            "type": "custom_tool_call",
            "call_id": call_id,
            "name": name,
            "status": "completed",
            "input": patch,
        });
        if let Some(ns) = namespace {
            item["namespace"] = json!(ns);
        }
        self.output_items.push(item.clone());
        events.push(ResponsesEvent::OutputItemDone { output_index, item });
    } else {
        // Never confirmed during deltas. Try conversion.
        let converted = convert_to_freeform_patch(&raw_input);
        if converted.trim().starts_with("*** Begin Patch") {
            // Conversion succeeded. Release buffer + emit done.
            if let Some(buffered_item) = self.buffered_apply_patch_item.take() {
                let buffered_idx = self
                    .buffered_apply_patch_output_index
                    .take()
                    .unwrap_or(output_index);
                events.push(ResponsesEvent::OutputItemAdded {
                    output_index: buffered_idx,
                    item: buffered_item,
                });
            }
            let mut item = json!({
                "type": "custom_tool_call",
                "call_id": call_id,
                "name": name,
                "status": "completed",
                "input": converted,
            });
            if let Some(ns) = namespace {
                item["namespace"] = json!(ns);
            }
            self.output_items.push(item.clone());
            events.push(ResponsesEvent::OutputItemDone { output_index, item });
        } else {
            // Conversion also failed. Mark invalid for retry.
            self.apply_patch_invalid = true;
            let toolu_id = self.tool_use_map.iter()
                .find(|(_, fid, _, _)| fid == &fc_id)
                .map(|(tid, _, _, _)| tid.clone())
                .unwrap_or_default();
            self.failed_apply_patch_info = Some((toolu_id, raw_input.clone()));
            // Do NOT emit any events. Router will detect flag and trigger retry.
        }
    }
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn apply_patch_unified_diff_converted_at_stop() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
    });
    // Unified diff content (no *** Begin Patch)
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"--- a/test.txt\\n+++ b/test.txt\\n@@ -1 +1 @@\\n-old\\n+new\"}"}),
    });
    let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    // Should emit OutputItemAdded (released from buffer) + OutputItemDone
    assert!(events.iter().any(|e| {
        let (t, _) = e.to_sse();
        t == "response.output_item.added"
    }), "buffered OutputItemAdded should be released for converted patch");
    assert!(events.iter().any(|e| {
        let (t, d) = e.to_sse();
        t == "response.output_item.done" && d.contains("*** Begin Patch")
    }), "OutputItemDone should contain converted freeform patch");
    assert!(!state.is_apply_patch_invalid());
}

#[test]
fn apply_patch_invalid_format_flags_retry() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    state.process_event(AnthropicEvent::ContentBlockStart {
        index: 0,
        content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
    });
    // Unrecognized format
    state.process_event(AnthropicEvent::ContentBlockDelta {
        index: 0,
        delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"This is not a valid patch format at all.\"}"}),
    });
    let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

    // Should NOT emit any events (withheld for retry)
    assert!(events.is_empty(), "invalid patch should not emit events");
    assert!(state.is_apply_patch_invalid());
    let (toolu_id, raw_patch) = state.take_failed_apply_patch_info();
    assert_eq!(toolu_id, "toolu_01");
    assert!(raw_patch.contains("This is not a valid patch format"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test apply_patch_unified_diff_converted_at_stop apply_patch_invalid_format_flags_retry --lib`
Expected: FAIL

- [ ] **Step 3: Implement the new logic**

Replace the `if is_custom` block in `handle_content_block_stop` with the code shown above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test apply_patch_unified_diff_converted_at_stop apply_patch_invalid_format_flags_retry --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: apply_patch format validation at content_block_stop with conversion fallback"
```

---

### Task 7: Add retry_mode handling in handle_message_start

**Files:**
- Modify: `src/conversion/response.rs:196-224` (`handle_message_start`)

When `self.retry_mode` is true:
- Do NOT update `response_id` (keep original for response.completed)
- Accumulate usage tokens additively (add to existing values)
- Do NOT emit ResponseCreated / ResponseInProgress
- Return `vec![]`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn retry_mode_suppresses_response_created_and_keeps_id() {
    let mut state = make_state();
    // First response: set up state
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_ORIGINAL", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    // Verify response_id is msg_ORIGINAL
    let events = state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_ORIGINAL", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });
    // First call emitted response.created
    assert!(events.iter().any(|e| {
        let (t, _) = e.to_sse();
        t == "response.created"
    }));

    // Prepare for retry
    state.prepare_for_retry();

    // Retry response: should NOT emit response.created
    let retry_events = state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "output_tokens": 0}}),
    });
    assert!(retry_events.is_empty(), "retry message_start should emit no events");
    // response_id should still be msg_ORIGINAL (not msg_RETRY)
    // We can verify this by triggering message_stop and checking
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test retry_mode_suppresses_response_created_and_keeps_id --lib`
Expected: FAIL (retry message_start emits response.created)

- [ ] **Step 3: Implement retry_mode in handle_message_start**

At the top of `handle_message_start`, add:

```rust
if self.retry_mode {
    // Do NOT update response_id. Accumulate usage additively.
    if let Some(usage) = message.get("usage") {
        self.input_tokens += usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        self.cache_read_tokens += usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    }
    return vec![];
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test retry_mode_suppresses_response_created_and_keeps_id --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/conversion/response.rs
git commit -m "feat: retry_mode suppresses response.created and preserves response_id"
```

---

### Task 8: Add format_apply_patch_error function and retry request construction

**Files:**
- Modify: `src/router.rs` (add helper function, construct retry body)

Add the error message function at module level in `router.rs`:

```rust
fn format_apply_patch_error() -> &'static str {
    concat!(
        "Error: apply_patch format not accepted. The patch MUST be in Codex freeform format.\n\n",
        "Required format:\n",
        "*** Begin Patch\n",
        "*** Update File: <filepath>\n",
        "-<line to remove>\n",
        "+<line to add>\n",
        "*** End Patch\n\n",
        "Or for new files:\n",
        "*** Begin Patch\n",
        "*** Add File: <filepath>\n",
        "+<file content>\n",
        "*** End Patch\n\n",
        "Or for deleted files:\n",
        "*** Begin Patch\n",
        "*** Delete File: <filepath>\n",
        "*** End Patch\n\n",
        "The FIRST line MUST be exactly '*** Begin Patch'. ",
        "Do NOT use unified diff format (--- a/, +++ b/, @@ @@)."
    )
}
```

Add a function to build the retry request body:

```rust
fn build_retry_body(
    original_body: &serde_json::Value,
    captured_blocks: Vec<serde_json::Value>,
    toolu_id: &str,
    error_message: &str,
) -> serde_json::Value {
    let mut retry_body = original_body.clone();
    let messages = retry_body
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .expect("retry body must have messages array");

    // Append assistant message with all captured content blocks
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": captured_blocks,
    }));

    // Append user message with tool_result error
    messages.push(serde_json::json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": toolu_id,
            "is_error": true,
            "content": error_message,
        }]
    }));

    retry_body
}
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn format_apply_patch_error_starts_correctly() {
    let msg = format_apply_patch_error();
    assert!(msg.starts_with("Error: apply_patch format not accepted"));
    assert!(msg.contains("*** Begin Patch"));
    assert!(msg.contains("*** Update File"));
    assert!(msg.contains("*** Add File"));
    assert!(msg.contains("*** Delete File"));
    assert!(msg.contains("*** End Patch"));
}

#[test]
fn build_retry_body_appends_assistant_and_user_messages() {
    let original = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "messages": [{"role": "user", "content": "fix the bug"}],
        "max_tokens": 4096,
    });
    let captured = vec![
        serde_json::json!({"type": "text", "text": "I'll fix it."}),
        serde_json::json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {"patch": "bad format"}}),
    ];
    let body = build_retry_body(&original, &captured, "toolu_01", "Error message");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    // Original user message
    assert_eq!(messages[0]["role"], "user");
    // Appended assistant message
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
    // Appended user message with tool_result
    assert_eq!(messages[2]["role"], "user");
    let tool_result = &messages[2]["content"].as_array().unwrap()[0];
    assert_eq!(tool_result["type"], "tool_result");
    assert_eq!(tool_result["tool_use_id"], "toolu_01");
    assert_eq!(tool_result["is_error"], true);
    // Other fields preserved
    assert_eq!(body["model"], "claude-sonnet-4-20250514");
    assert_eq!(body["max_tokens"], 4096);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test format_apply_patch_error build_retry_body_appends --lib 2>&1 | tail -5`
Expected: compilation error (functions don't exist)

- [ ] **Step 3: Implement the two functions**

Add `format_apply_patch_error()` and `build_retry_body()` as `fn` (not `pub`) at module level in `src/router.rs`, before or after the existing helper functions.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test format_apply_patch_error build_retry_body_appends --lib`
Expected: PASS

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/router.rs
git commit -m "feat: add format_apply_patch_error and build_retry_body helpers"
```

---

### Task 9: Restructure router streaming loop with retry

**Files:**
- Modify: `src/router.rs:263-418` (the tokio::spawn block)

This is the most complex task. The current flat streaming loop becomes an outer retry loop.

**Changes needed:**

1. Move `client`, `api_key`, `anthropic_version`, `upstream_url`, `anthropic_body` into the spawn closure (clone before spawn).

2. Replace the flat streaming loop with:

```rust
tokio::spawn(async move {
    use crate::conversion::response::StreamingState;
    use crate::sse::anthropic::{parse_sse_events, AnthropicEvent};
    use crate::sse::responses::{format_done, format_responses_event, ResponsesEvent};

    let mut current_body = retry_anthropic_body.clone();

    let mut streaming_state: Option<StreamingState> = None;
    let mut done_sent = false;

    'retry_loop: loop {
        let request_builder = client
            .post(&upstream_url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", &anthropic_version)
            .header("content-type", "application/json")
            .json(&current_body);

        let mut event_source = match reqwest_eventsource::EventSource::new(request_builder) {
            Ok(es) => es,
            Err(e) => {
                tracing::error!(error = %e, "failed to create EventSource");
                let _ = tx.send(Ok(Bytes::from(
                    "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Failed to connect to upstream\"}\n\n",
                ))).await;
                let _ = tx.send(Ok(Bytes::from(format_done()))).await;
                return;
            }
        };

        while let Some(event_result) = event_source.next().await {
            match event_result {
                Ok(reqwest_eventsource::Event::Message(msg)) => {
                    let raw_chunk = format!("event: {}\ndata: {}\n\n", msg.event, msg.data);
                    let anthropic_events = parse_sse_events(&raw_chunk);

                    for a_event in anthropic_events {
                        // Create StreamingState lazily on first message_start.
                        if streaming_state.is_none() {
                            if let AnthropicEvent::MessageStart { ref message } = a_event {
                                let response_id = message.get("id").and_then(|v| v.as_str()).unwrap_or("msg_unknown").to_string();
                                let ns_reg = {
                                    let guard = namespace_registry.lock().unwrap();
                                    guard.clone()
                                };
                                streaming_state = Some(StreamingState::new(
                                    response_id,
                                    model.clone(),
                                    ns_reg,
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                    tool_choice_echo.clone(),
                                    instructions_echo.clone(),
                                    parallel_tool_calls_echo,
                                ));
                            } else {
                                continue;
                            }
                        }

                        let state = match streaming_state.as_mut() {
                            Some(s) => s,
                            None => continue,
                        };

                        let responses_events = state.process_event(a_event);

                        // Check if apply_patch was confirmed invalid
                        if state.is_apply_patch_invalid() {
                            // Continue consuming events silently until message stops
                            // (or just break — upstream will be closed at end of loop)
                            continue;
                        }

                        for r_event in responses_events {
                            if matches!(r_event, ResponsesEvent::Done) {
                                done_sent = true;
                                let _ = tx.send(Ok(Bytes::from(format_done()))).await;
                            } else {
                                let sse_str = format_responses_event(&r_event);
                                let _ = tx.send(Ok(Bytes::from(sse_str))).await;
                            }
                        }
                    }
                }
                Ok(reqwest_eventsource::Event::Open) => {}
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("Stream ended") {
                        tracing::debug!("upstream SSE stream ended normally");
                    } else if streaming_state.is_some() {
                        tracing::error!(error = %e, "SSE stream error from upstream (mid-stream)");
                        let state = streaming_state.as_mut().unwrap();
                        let error_val = serde_json::json!({
                            "type": "api_error",
                            "message": format!("Upstream stream error: {}", e)
                        });
                        let events = state.process_event(
                            crate::sse::anthropic::AnthropicEvent::Error { error: error_val },
                        );
                        for r_event in events {
                            if matches!(r_event, ResponsesEvent::Done) {
                                done_sent = true;
                                let _ = tx.send(Ok(Bytes::from(format_done()))).await;
                            } else {
                                let sse_str = format_responses_event(&r_event);
                                let _ = tx.send(Ok(Bytes::from(sse_str))).await;
                            }
                        }
                    } else {
                        tracing::error!(error = %e, "SSE stream error from upstream (no message_start)");
                        let _ = tx.send(Ok(Bytes::from(
                            "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Upstream stream error\"}\n\n",
                        ))).await;
                        let _ = tx.send(Ok(Bytes::from(format_done()))).await;
                        done_sent = true;
                    }
                    break;
                }
            }
        }

        // Close the event source
        let _ = event_source.close();

        // Drain signatures from this response
        if let Some(state) = streaming_state.as_mut() {
            let sigs = state.drain_signatures();
            for (reasoning_id, signature) in sigs {
                signature_cache.insert(reasoning_id, signature);
            }
        }

        // Check if retry is needed
        if let Some(state) = streaming_state.as_mut() {
            if state.is_apply_patch_invalid() {
                let captured = state.take_content_block_capture();
                let (toolu_id, _) = state.take_failed_apply_patch_info();
                let error_msg = format_apply_patch_error();

                // Build retry body
                current_body = build_retry_body(&retry_anthropic_body, captured, &toolu_id, error_msg);

                state.prepare_for_retry();
                tracing::info!(toolu_id = %toolu_id, "retrying apply_patch with format error feedback");
                continue 'retry_loop;
            }
        }

        break 'retry_loop;
    }

    // Send final [DONE] only if not already sent
    if !done_sent {
        let _ = tx.send(Ok(Bytes::from(format_done()))).await;
    }
    drop(tx);
});
```

3. Before the spawn, clone the needed values:

```rust
let retry_client = client.clone();
let retry_api_key = api_key.clone();
let retry_anthropic_version = anthropic_version.clone();
let retry_upstream_url = upstream_url.clone();
let retry_anthropic_body = anthropic_body.clone();
```

And use these in the closure (rename variables in the closure body accordingly).

- [ ] **Step 1: Write the failing test**

This task modifies the streaming handler in router.rs. It cannot be easily unit-tested in isolation. We verify via:
1. All existing unit tests still pass
2. All existing integration tests still pass

Write a unit test that verifies the helper functions work correctly together:

```rust
#[test]
fn retry_body_preserves_original_fields() {
    let original = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 8192,
        "system": "You are helpful",
        "messages": [{"role": "user", "content": "edit file"}],
        "tools": [{"name": "apply_patch", "type": "custom"}],
        "thinking": {"type": "enabled", "budget_tokens": 5000},
    });
    let captured = vec![
        serde_json::json!({"type": "text", "text": "Let me edit"}),
        serde_json::json!({"type": "tool_use", "id": "toolu_BAD", "name": "apply_patch", "input": "garbage"}),
    ];
    let body = build_retry_body(&original, &captured, "toolu_BAD", format_apply_patch_error());

    // Preserved
    assert_eq!(body["model"], "claude-sonnet-4-20250514");
    assert_eq!(body["max_tokens"], 8192);
    assert_eq!(body["system"], "You are helpful");
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["thinking"]["type"], "enabled");

    // Messages extended
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3); // original + assistant + user
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test retry_body_preserves_original_fields --lib`
Expected: PASS (helper functions already added in Task 8)

- [ ] **Step 3: Restructure the spawn block**

Apply the changes described above to `handle_responses` in `src/router.rs`:

1. Clone `client`, `api_key`, `anthropic_version`, `upstream_url`, `anthropic_body` before the spawn (rename to `retry_*`).
2. Move the `request_builder` creation inside the loop.
3. Wrap the streaming loop in `loop { ... }` with a `break` at the bottom.
4. After the streaming loop ends (upstream closed), check `is_apply_patch_invalid()` and construct retry if needed.
5. Use `retry_anthropic_body` for the original body reference and `current_body` for the active request.

- [ ] **Step 4: Run all unit tests**

Run: `cargo test --lib`
Expected: all pass (286 router tests + new tests)

- [ ] **Step 5: Commit**

```bash
git add src/router.rs
git commit -m "feat: restructure router streaming loop with apply_patch retry"
```

---

### Task 10: Verify all tests pass (unit + integration)

**Files:** None (verification only)

- [ ] **Step 1: Run all unit tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 2: Run integration tests**

Run: `cargo test --test integrations -- --ignored --test-threads=1`
Expected: all 6 integration tests pass, especially `apply_patch_tool_call`

- [ ] **Step 3: Commit if any fixes were needed**

If any fixes were needed during verification, commit them:

```bash
git add -A
git commit -m "fix: adjustments from integration test verification"
```
