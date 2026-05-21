use crate::conversion::namespace::NamespaceRegistry;
use crate::sse::anthropic::AnthropicEvent;
use crate::sse::responses::ResponsesEvent;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Tracks which type of content block is currently active.
#[derive(Debug, Clone, PartialEq)]
enum ActiveBlock {
    None,
    Text,
    Thinking,
    RedactedThinking { encrypted_content: String },
    ToolUse,
}

/// In-flight state for a single streaming conversion.
pub struct StreamingState {
    /// Response ID from Anthropic message_start.
    response_id: String,
    /// Model from request (echoed back in response.completed).
    model: String,
    /// Namespace registry from request phase.
    namespace_registry: NamespaceRegistry,
    /// Timestamp when response.created was emitted.
    created_at: u64,
    /// Accumulated output items for response.completed.
    output_items: Vec<Value>,
    /// Text delta accumulator for the active text block.
    text_accumulator: String,
    /// Thinking delta accumulator for the active thinking block.
    thinking_accumulator: String,
    /// Arguments accumulator for the active tool_use block.
    arguments_accumulator: String,
    /// Signature accumulator for the active thinking block (consumed silently, cached later).
    signature_accumulator: String,
    /// Which content block type is currently active.
    active_block: ActiveBlock,
    /// Current reasoning item ID for the active thinking block.
    current_reasoning_id: String,
    /// Current function call item ID for the active tool_use block.
    current_fc_item_id: String,
    /// Current function call call_id for the active tool_use block.
    current_fc_call_id: String,
    /// Function call counter for sequential ID generation.
    fc_counter: u64,
    /// Whether the current tool_use block is a custom_tool_call (freeform tool).
    current_is_custom: bool,
    /// Tool use info per Anthropic content block index: (toolu_id, fc_id, call_id, raw_name).
    tool_use_map: Vec<(String, String, String, String)>,
    /// Stop reason from message_delta.
    stop_reason: Option<String>,
    /// Input tokens from message_start usage.
    input_tokens: u64,
    /// Cache read tokens from message_start usage.
    cache_read_tokens: u64,
    /// Output tokens from message_delta usage.
    output_tokens: u64,
    /// Echo-back fields from request.
    tool_choice_echo: String,
    instructions_echo: Option<String>,
    parallel_tool_calls_echo: Option<bool>,
    /// Accumulated signatures keyed by reasoning_id, for writing to cache after stream ends.
    signature_store: Vec<(String, String)>,
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
}

/// Convert a patch string to Codex freeform format.
///
/// If the input already starts with `*** Begin Patch`, it is returned as-is.
/// If the input looks like unified diff format (contains `---` or `diff --git`),
/// it is converted to freeform format.
/// Otherwise, the input is returned as-is (no conversion attempted).
fn convert_to_freeform_patch(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("*** Begin Patch") {
        return input.to_string();
    }

    // Heuristic: only attempt conversion if the input looks like unified diff.
    let has_diff_markers = trimmed.lines().any(|line| {
        line.starts_with("--- ") || line.starts_with("diff --git")
    });
    if !has_diff_markers {
        return input.to_string();
    }

    // Parse unified diff → freeform.
    let mut out = String::from("*** Begin Patch\n");
    let mut current_file: Option<String> = None;
    for line in trimmed.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            // --- a/path: note the file (use the --- side for the path)
            current_file = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("--- ") {
            current_file = Some(path.to_string());
        } else if line.starts_with("+++ ") {
            // +++ b/path or +++ /dev/null: use this path if we haven't seen one
            let path = line.trim_start_matches("+++ b/")
                .trim_start_matches("+++ ");
            if current_file.is_none() {
                current_file = Some(path.to_string());
            }
            if let Some(ref f) = current_file {
                out.push_str(&format!("*** Update File: {f}\n"));
            }
            current_file = None; // consumed
        } else if line.starts_with("@@") {
            // Skip hunk header
        } else if line.starts_with("diff --git") || line.starts_with("index ") {
            // Skip git diff metadata
        } else if let Some(rest) = line.strip_prefix('-') {
            out.push_str(&format!("-{rest}\n"));
        } else if let Some(rest) = line.strip_prefix('+') {
            out.push_str(&format!("+{rest}\n"));
        } else if line.starts_with(' ') {
            // Context line — include as-is (no prefix)
            out.push_str(&format!("{}\n", &line[1..]));
        } else if !line.is_empty() {
            // Other non-empty line: include as context
            out.push_str(&format!("{line}\n"));
        }
    }
    out.push_str("*** End Patch\n");
    out
}

/// Return current Unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl StreamingState {
    pub fn new(
        response_id: String,
        model: String,
        namespace_registry: NamespaceRegistry,
        created_at: u64,
        tool_choice_echo: String,
        instructions_echo: Option<String>,
        parallel_tool_calls_echo: Option<bool>,
    ) -> Self {
        Self {
            response_id,
            model,
            namespace_registry,
            created_at,
            output_items: Vec::new(),
            text_accumulator: String::new(),
            thinking_accumulator: String::new(),
            arguments_accumulator: String::new(),
            signature_accumulator: String::new(),
            active_block: ActiveBlock::None,
            current_reasoning_id: String::new(),
            current_fc_item_id: String::new(),
            current_fc_call_id: String::new(),
            current_is_custom: false,
            fc_counter: 0,
            tool_use_map: Vec::new(),
            stop_reason: None,
            input_tokens: 0,
            cache_read_tokens: 0,
            output_tokens: 0,
            tool_choice_echo,
            instructions_echo,
            parallel_tool_calls_echo,
            signature_store: Vec::new(),
            anthropic_content_blocks: Vec::new(),
            retry_mode: false,
            apply_patch_invalid: false,
            buffered_apply_patch_item: None,
            buffered_apply_patch_output_index: None,
            apply_patch_format_confirmed: false,
            failed_apply_patch_info: None,
        }
    }

    /// Process a single Anthropic SSE event and return zero or more Responses API SSE events.
    pub fn process_event(&mut self, event: AnthropicEvent) -> Vec<ResponsesEvent> {
        match event {
            AnthropicEvent::MessageStart { message } => self.handle_message_start(message),
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => self.handle_content_block_start(index, content_block),
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                self.handle_content_block_delta(index, delta)
            }
            AnthropicEvent::ContentBlockStop { index } => self.handle_content_block_stop(index),
            AnthropicEvent::MessageDelta { delta, usage } => {
                self.handle_message_delta(delta, usage)
            }
            AnthropicEvent::MessageStop => self.handle_message_stop(),
            AnthropicEvent::Error { error } => self.handle_error(error),
            AnthropicEvent::Ping => vec![], // Consumed silently.
            AnthropicEvent::Done => vec![], // [DONE] handled by caller.
            AnthropicEvent::Unknown { .. } => vec![], // Forward compat: ignore unknown.
        }
    }

    /// Get accumulated signatures for writing to cache after stream ends.
    /// Returns (reasoning_id, signature) pairs.
    pub fn drain_signatures(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.signature_store)
    }

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

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_message_start(&mut self, message: Value) -> Vec<ResponsesEvent> {
        if self.retry_mode {
            if let Some(usage) = message.get("usage") {
                self.input_tokens += usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                self.cache_read_tokens += usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            }
            return vec![];
        }

        // Record response ID and usage from the initial message.
        if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
            self.response_id = id.to_string();
        }
        if let Some(usage) = message.get("usage") {
            self.input_tokens = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            self.cache_read_tokens = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        }

        vec![
            ResponsesEvent::ResponseCreated {
                response_id: self.response_id.clone(),
                created_at: self.created_at,
                model: self.model.clone(),
            },
            ResponsesEvent::ResponseInProgress {
                response_id: self.response_id.clone(),
                created_at: self.created_at,
                model: self.model.clone(),
            },
        ]
    }

    fn handle_content_block_start(
        &mut self,
        _index: usize,
        content_block: Value,
    ) -> Vec<ResponsesEvent> {
        let block_type = content_block
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match block_type {
            "text" => {
                self.text_accumulator.clear();
                self.active_block = ActiveBlock::Text;
                let output_index = self.output_items.len();
                let item = json!({
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                });
                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                    ResponsesEvent::ContentPartAdded {
                        output_index,
                        content_index: 0,
                        part: json!({"type": "output_text", "text": "", "annotations": []}),
                    },
                ]
            }
            "thinking" => {
                self.thinking_accumulator.clear();
                self.signature_accumulator.clear();
                self.active_block = ActiveBlock::Thinking;
                let output_index = self.output_items.len();
                let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
                self.current_reasoning_id = reasoning_id.clone();
                let item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [],
                });
                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                    ResponsesEvent::ReasoningSummaryPartAdded {
                        output_index,
                        item_id: reasoning_id,
                        summary_index: 0,
                    },
                ]
            }
            "redacted_thinking" => {
                let data = content_block
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.active_block = ActiveBlock::RedactedThinking {
                    encrypted_content: data.clone(),
                };
                let output_index = self.output_items.len();
                let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
                self.current_reasoning_id = reasoning_id.clone();
                let item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [],
                    "encrypted_content": data,
                });
                vec![ResponsesEvent::OutputItemAdded { output_index, item }]
            }
            "tool_use" => {
                let toolu_id = content_block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = content_block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.arguments_accumulator.clear();
                self.active_block = ActiveBlock::ToolUse;

                let fc_id = format!("fc_{}", self.fc_counter);
                let call_id = format!("call_{}", self.fc_counter);
                self.current_fc_item_id = fc_id.clone();
                self.current_fc_call_id = call_id.clone();
                self.tool_use_map
                    .push((toolu_id, fc_id.clone(), call_id.clone(), name.clone()));
                self.fc_counter += 1;

                let output_index = self.output_items.len();

                // Look up namespace for the tool name.
                let (display_name, namespace) =
                    if let Some(entry) = self.namespace_registry.lookup(&name) {
                        (entry.tool_name.clone(), Some(entry.namespace.clone()))
                    } else {
                        (name.clone(), None)
                    };

                // Freeform tools (injected by the proxy, not originally in Codex tools
                // array) must use "custom_tool_call" format, not "function_call".
                // Codex dispatches on the `type` field and only recognizes freeform
                // tools via `custom_tool_call`.
                let is_custom_tool_call = name == "apply_patch";
                self.current_is_custom = is_custom_tool_call;

                let mut item = if is_custom_tool_call {
                    json!({
                        "type": "custom_tool_call",
                        "call_id": call_id,
                        "name": display_name,
                        "status": "in_progress",
                        "input": "",
                    })
                } else {
                    json!({
                        "type": "function_call",
                        "id": fc_id,
                        "call_id": call_id,
                        "name": display_name,
                        "status": "in_progress",
                        "arguments": "",
                    })
                };
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }

                if is_custom_tool_call {
                    self.apply_patch_format_confirmed = false;
                    self.buffered_apply_patch_output_index = Some(output_index);
                    self.buffered_apply_patch_item = Some(item);
                    vec![]
                } else {
                    vec![ResponsesEvent::OutputItemAdded { output_index, item }]
                }
            }
            _ => {
                tracing::warn!(
                    block_type = block_type,
                    "unknown content block type in content_block_start"
                );
                vec![]
            }
        }
    }

    fn handle_content_block_delta(&mut self, _index: usize, delta: Value) -> Vec<ResponsesEvent> {
        let delta_type = delta
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match delta_type {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.text_accumulator.push_str(text);
                let output_index = self.output_items.len();
                vec![ResponsesEvent::OutputTextDelta {
                    output_index,
                    content_index: 0,
                    delta: text.to_string(),
                }]
            }
            "thinking_delta" => {
                let thinking = delta
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.thinking_accumulator.push_str(thinking);
                let output_index = self.output_items.len();
                vec![ResponsesEvent::ReasoningSummaryTextDelta {
                    output_index,
                    item_id: self.current_reasoning_id.clone(),
                    summary_index: 0,
                    delta: thinking.to_string(),
                }]
            }
            "signature_delta" => {
                // Accumulate signature for cache, do NOT emit any SSE event.
                let sig = delta
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.signature_accumulator.push_str(sig);
                vec![]
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.arguments_accumulator.push_str(partial);
                // For custom_tool_call (freeform tools), accumulate silently
                // and emit the full input at content_block_stop.
                if self.current_is_custom {
                    if !self.apply_patch_format_confirmed
                        && self.arguments_accumulator.contains("*** Begin Patch")
                    {
                        self.apply_patch_format_confirmed = true;
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
                let output_index = self.output_items.len();
                vec![ResponsesEvent::FunctionCallArgumentsDelta {
                    output_index,
                    item_id: self.current_fc_item_id.clone(),
                    delta: partial.to_string(),
                }]
            }
            _ => {
                tracing::debug!(delta_type = delta_type, "unknown delta type, ignoring");
                vec![]
            }
        }
    }

    fn handle_content_block_stop(&mut self, _index: usize) -> Vec<ResponsesEvent> {
        let output_index = self.output_items.len();
        let mut events = Vec::new();

        match std::mem::replace(&mut self.active_block, ActiveBlock::None) {
            ActiveBlock::Text => {
                let text = std::mem::take(&mut self.text_accumulator);
                events.push(ResponsesEvent::OutputTextDone {
                    output_index,
                    content_index: 0,
                    text: text.clone(),
                });
                events.push(ResponsesEvent::ContentPartDone {
                    output_index,
                    content_index: 0,
                    part: json!({"type": "output_text", "text": text, "annotations": []}),
                });
                let item = json!({
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                });
                self.output_items.push(item.clone());
                events.push(ResponsesEvent::OutputItemDone { output_index, item });

                // Capture Anthropic-format content block for retry
                self.anthropic_content_blocks.push(json!({"type": "text", "text": text}));
            }
            ActiveBlock::Thinking => {
                let thinking_text = std::mem::take(&mut self.thinking_accumulator);
                let signature = std::mem::take(&mut self.signature_accumulator);
                let reasoning_id = std::mem::take(&mut self.current_reasoning_id);

                // Store signature for cache write after stream ends.
                if !signature.is_empty() {
                    self.signature_store
                        .push((reasoning_id.clone(), signature.clone()));
                }

                events.push(ResponsesEvent::ReasoningSummaryTextDone {
                    output_index,
                    item_id: reasoning_id.clone(),
                    summary_index: 0,
                    text: thinking_text.clone(),
                });
                events.push(ResponsesEvent::ReasoningSummaryPartDone {
                    output_index,
                    item_id: reasoning_id.clone(),
                    summary_index: 0,
                    part: json!({"type": "summary_text", "text": thinking_text}),
                });
                let item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [{"type": "summary_text", "text": thinking_text}],
                });
                self.output_items.push(item.clone());
                events.push(ResponsesEvent::OutputItemDone { output_index, item });

                // Capture Anthropic-format content block for retry
                let mut thinking_block = json!({"type": "thinking", "thinking": thinking_text.clone()});
                if !signature.is_empty() {
                    thinking_block["signature"] = json!(signature);
                }
                self.anthropic_content_blocks.push(thinking_block);
            }
            ActiveBlock::RedactedThinking { encrypted_content } => {
                let reasoning_id = std::mem::take(&mut self.current_reasoning_id);
                let mut item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [],
                });
                if !encrypted_content.is_empty() {
                    item["encrypted_content"] = json!(encrypted_content);
                }
                self.output_items.push(item.clone());
                events.push(ResponsesEvent::OutputItemDone { output_index, item });

                // Capture Anthropic-format content block for retry
                self.anthropic_content_blocks.push(json!({"type": "redacted_thinking", "data": encrypted_content}));
            }
            ActiveBlock::ToolUse => {
                let args = std::mem::take(&mut self.arguments_accumulator);
                let fc_id = std::mem::take(&mut self.current_fc_item_id);
                let call_id = std::mem::take(&mut self.current_fc_call_id);
                let is_custom = std::mem::replace(&mut self.current_is_custom, false);

                // Get the name and namespace from the tool_use_map.
                let (name, namespace) = self
                    .tool_use_map
                    .iter()
                    .find(|(_, fid, _, _)| fid == &fc_id)
                    .map(|(_, _, _, raw_name)| {
                        let ns = self
                            .namespace_registry
                            .lookup(raw_name)
                            .map(|e| e.namespace.clone());
                        let display_name = ns
                            .as_ref()
                            .map(|_| {
                                self.namespace_registry
                                    .lookup(raw_name)
                                    .unwrap()
                                    .tool_name
                                    .clone()
                            })
                            .unwrap_or_else(|| raw_name.clone());
                        (display_name, ns)
                    })
                    .unwrap_or_else(|| ("unknown".to_string(), None));

                if is_custom {
                    let raw_input = serde_json::from_str::<Value>(&args)
                        .ok()
                        .and_then(|v| {
                            v.get("patch")
                                .or_else(|| v.get("input"))
                                .and_then(|p| p.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| args.clone());

                    if self.apply_patch_format_confirmed {
                        // Already confirmed valid during deltas. Emit OutputItemDone with raw patch.
                        let mut item = json!({
                            "type": "custom_tool_call",
                            "call_id": call_id,
                            "name": name,
                            "status": "completed",
                            "input": raw_input,
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
                            self.failed_apply_patch_info = Some((toolu_id, raw_input));
                            // Do NOT emit any events. Router will detect flag and trigger retry.
                        }
                    }
                }
                else {
                    events.push(ResponsesEvent::FunctionCallArgumentsDone {
                        output_index,
                        item_id: fc_id.clone(),
                        arguments: args.clone(),
                    });

                    let mut item = json!({
                        "type": "function_call",
                        "id": fc_id,
                        "call_id": call_id,
                        "name": name,
                        "status": "completed",
                        "arguments": args,
                    });
                    if let Some(ns) = namespace {
                        item["namespace"] = json!(ns);
                    }
                    self.output_items.push(item.clone());
                    events.push(ResponsesEvent::OutputItemDone { output_index, item });
                }

                // Capture Anthropic-format content block for retry
                let toolu_id = self.tool_use_map.iter()
                    .find(|(_, fid, _, _)| fid == &fc_id)
                    .map(|(tid, _, _, _)| tid.clone())
                    .unwrap_or_default();
                let raw_name = self.tool_use_map.iter()
                    .find(|(_, fid, _, _)| fid == &fc_id)
                    .map(|(_, _, _, n)| n.clone())
                    .unwrap_or_default();
                let input_value: Value = serde_json::from_str(&args).unwrap_or_default();
                self.anthropic_content_blocks.push(json!({
                    "type": "tool_use",
                    "id": toolu_id,
                    "name": raw_name,
                    "input": input_value,
                }));
            }
            ActiveBlock::None => {
                // No active block, nothing to do.
            }
        }

        events
    }

    fn handle_message_delta(&mut self, delta: Value, usage: Value) -> Vec<ResponsesEvent> {
        // Record stop reason and output usage. No SSE events emitted.
        if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
            self.stop_reason = Some(sr.to_string());
        }
        self.output_tokens = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        vec![]
    }

    fn handle_message_stop(&mut self) -> Vec<ResponsesEvent> {
        let completed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let stop_reason = self
            .stop_reason
            .take()
            .unwrap_or_else(|| "end_turn".to_string());

        // Always "completed", never "incomplete" (B2 critical constraint).
        let status = "completed";

        // Build usage object.
        let total_tokens = self.input_tokens + self.output_tokens;
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": total_tokens,
        });
        if self.cache_read_tokens > 0 {
            usage["input_tokens_details"] = json!({"cached_tokens": self.cache_read_tokens});
        }

        // Build response object for response.completed.
        let mut response_obj = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": completed_at,
            "model": self.model,
            "status": status,
            "output": self.output_items.clone(),
            "usage": usage,
            "metadata": Value::Null,
        });

        // Incomplete details when max_tokens or model_context_window_exceeded.
        if stop_reason == "max_tokens" || stop_reason == "model_context_window_exceeded" {
            response_obj["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }

        // Echo back request fields.
        response_obj["parallel_tool_calls"] = json!(self.parallel_tool_calls_echo);
        response_obj["tool_choice"] = json!(self.tool_choice_echo);
        if let Some(ref instructions) = self.instructions_echo {
            response_obj["instructions"] = json!(instructions);
        }

        vec![ResponsesEvent::ResponseCompleted {
            response: response_obj,
        }]
    }

    fn handle_error(&mut self, error: Value) -> Vec<ResponsesEvent> {
        let error_type = error
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("api_error");
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");

        let code = convert_streaming_error_code(error_type, message);

        let response_obj = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": now_secs(),
            "model": self.model,
            "status": "failed",
            "error": {"code": code, "message": message},
            "output": self.output_items,
            "usage": Value::Null,
            "metadata": Value::Null,
        });

        vec![
            ResponsesEvent::Error {
                code: code.to_string(),
                message: message.to_string(),
            },
            ResponsesEvent::ResponseFailed {
                response: response_obj,
            },
            ResponsesEvent::Done,
        ]
    }
}

/// Map Anthropic streaming error type + message to a Responses API error code.
/// This is a simplified version for streaming errors; full error handling is in error.rs (Task 14).
fn convert_streaming_error_code(error_type: &str, message: &str) -> &'static str {
    match error_type {
        "invalid_request_error" => {
            if is_context_overflow(message) {
                "context_length_exceeded"
            } else {
                "invalid_request"
            }
        }
        "authentication_error" => "invalid_api_key",
        "permission_error" => "invalid_api_key",
        "not_found_error" => "model_not_found",
        "request_too_large" => "request_too_large",
        "rate_limit_error" => "rate_limit_exceeded",
        "billing_error" => "insufficient_quota",
        "overloaded_error" => "server_error",
        _ => "server_error",
    }
}

/// Context overflow keyword detection heuristic.
const CONTEXT_OVERFLOW_KEYWORDS: &[&str] = &[
    "context window",
    "context length",
    "too many tokens",
    "prompt is too long",
];

fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    CONTEXT_OVERFLOW_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_state() -> StreamingState {
        StreamingState::new(
            "msg_test".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            Some("You are helpful.".to_string()),
            None,
        )
    }

    /// Helper: run a full text stream and return the response.completed data.
    fn run_text_stream(text: &str, stop_reason: &str) -> (Vec<ResponsesEvent>, serde_json::Value) {
        let mut state = make_state();
        let mut all_events = Vec::new();

        all_events.extend(state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": text}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStop { index: 0 }));
        all_events.extend(state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": stop_reason, "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::MessageStop));

        let completed = all_events
            .iter()
            .find(|e| {
                let (t, _) = e.to_sse();
                t == "response.completed"
            })
            .unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        (all_events, parsed)
    }

    // --- Text streaming ---

    #[test]
    fn text_stream_produces_correct_events() {
        let mut state = make_state();
        let mut all_events = Vec::new();

        // message_start
        let events = state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-20250514", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        all_events.extend(events);
        // Should produce response.created
        assert!(
            all_events
                .iter()
                .any(|e| { let (t, _) = e.to_sse(); t == "response.created" }),
            "should emit response.created"
        );

        // content_block_start (text)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }));
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.content_part.added"
        }));

        // content_block_delta (text)
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello"}),
        });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_text.delta"
        }));

        // content_block_stop
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_text.done"
        }));
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.content_part.done"
        }));
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }));
    }

    // --- Thinking streaming ---

    #[test]
    fn thinking_stream_produces_reasoning_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // content_block_start (thinking)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.added" && d.contains("\"reasoning\"")
        }), "should emit reasoning output_item.added");
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_part.added"
        }));

        // thinking_delta
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "Let me..."}),
        });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_text.delta"
        }));

        // signature_delta -- should NOT produce any SSE event (consumed and accumulated)
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "ErUB..."}),
        });
        assert!(
            events.is_empty(),
            "signature_delta should produce no SSE output"
        );

        // content_block_stop
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_text.done"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_part.done"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }));
    }

    // --- Tool use streaming ---

    #[test]
    fn tool_use_stream_produces_function_call_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "exec_command", "input": {}}),
        });
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.added" && d.contains("\"function_call\"")
        }));

        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"cmd\":"}),
        });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.function_call_arguments.delta"
        }));

        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.function_call_arguments.done"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }));
    }

    // --- Ping consumed ---

    #[test]
    fn ping_produces_no_output() {
        let mut state = make_state();
        let events = state.process_event(AnthropicEvent::Ping);
        assert!(events.is_empty());
    }

    // --- Completion always response.completed ---

    #[test]
    fn completion_always_response_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let _ = state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 15}),
        });
        // MessageDelta does not produce SSE events.

        let events = state.process_event(AnthropicEvent::MessageStop);
        assert!(
            events.iter().any(|e| {
                let (t, _) = e.to_sse();
                t == "response.completed"
            }),
            "message_stop must produce response.completed, NEVER response.incomplete"
        );
    }

    #[test]
    fn max_tokens_still_response_completed() {
        let (_, parsed) = run_text_stream("partial", "max_tokens");
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(
            parsed["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    // --- Usage mapping ---

    #[test]
    fn usage_mapping_in_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 50}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 15}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events
            .iter()
            .find(|e| {
                let (t, _) = e.to_sse();
                t == "response.completed"
            })
            .unwrap()
            .to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["output_tokens"], 15);
        assert_eq!(usage["total_tokens"], 115);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 50);
        assert!(
            usage.get("reasoning_tokens").is_none(),
            "reasoning_tokens should be omitted"
        );
    }

    // --- Output item accumulator ---

    #[test]
    fn output_items_accumulated_in_completed() {
        let (_, parsed) = run_text_stream("Hello", "end_turn");
        let output = parsed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "output should contain 1 item");
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "Hello");
    }

    // --- Redacted thinking streaming ---

    #[test]
    fn redacted_thinking_stream_produces_reasoning_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // content_block_start (redacted_thinking)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "redacted_thinking", "data": "ENCRYPTED_BLOB"}),
        });
        // Should emit output_item.added with encrypted_content
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.added" && d.contains("encrypted_content")
        }));

        // content_block_stop (redacted_thinking) -- no delta events expected
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.done" && d.contains("encrypted_content")
        }));
    }

    // --- Multiple text deltas accumulated ---

    #[test]
    fn multiple_text_deltas_accumulated() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello"}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": " world"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // output_text.done should contain accumulated text "Hello world"
        let done_event = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_text.done"
        }).unwrap();
        let (_, data) = done_event.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "Hello world");

        // output_item.done should also contain accumulated text
        let item_done = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }).unwrap();
        let (_, data) = item_done.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["item"]["content"][0]["text"], "Hello world");
    }

    // --- Multiple tool use blocks get independent output_index ---

    #[test]
    fn multiple_tool_use_blocks_independent_indices() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // First tool_use at output_index 0
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "tool_a", "input": {}}),
        });
        let added0 = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).unwrap();
        let (_, data) = added0.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 0);

        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Second tool_use at output_index 1
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "tool_b", "input": {}}),
        });
        let added1 = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).unwrap();
        let (_, data) = added1.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 1);

        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        // Verify both items accumulated
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "tool_use", "stop_sequence": null}),
            usage: json!({"output_tokens": 50}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let output = parsed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[1]["type"], "function_call");
    }

    // --- Tool use with namespace ---

    #[test]
    fn tool_use_with_namespace_lookup() {
        let mut registry = NamespaceRegistry::new();
        registry.register(
            "mcp__memory__search".to_string(),
            "mcp__memory__".to_string(),
            "search".to_string(),
        );
        let mut state = StreamingState::new(
            "msg_test".to_string(),
            "m".to_string(),
            registry,
            1717000000,
            "auto".to_string(),
            None,
            None,
        );
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "mcp__memory__search", "input": {}}),
        });
        let added = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).unwrap();
        let (_, data) = added.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["item"]["name"], "search");
        assert_eq!(parsed["item"]["namespace"], "mcp__memory__");
    }

    // --- Signature accumulation ---

    #[test]
    fn signature_accumulated_and_drainable() {
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
            delta: json!({"type": "thinking_delta", "thinking": "some text"}),
        });
        // Signature delta consumed silently
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "sig_part1"}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "sig_part2"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        let sigs = state.drain_signatures();
        assert_eq!(sigs.len(), 1);
        assert!(sigs[0].0.starts_with("rs_"));
        assert_eq!(sigs[0].1, "sig_part1sig_part2");
    }

    // --- Error handling (streaming) ---

    #[test]
    fn streaming_error_produces_error_and_failed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let events = state.process_event(AnthropicEvent::Error {
            error: json!({"type": "api_error", "message": "Internal server error"}),
        });

        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "error"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.failed"
        }));

        // Verify error event content
        let error_evt = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "error"
        }).unwrap();
        let (_, data) = error_evt.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["code"], "server_error");
        assert_eq!(parsed["message"], "Internal server error");

        // Verify response.failed content
        let failed_evt = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.failed"
        }).unwrap();
        let (_, data) = failed_evt.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "failed");
        assert_eq!(parsed["response"]["error"]["code"], "server_error");
        assert_eq!(parsed["response"]["id"], "msg_test");
        // Verify completed_at is present and non-zero (Rs6)
        assert!(
            parsed["response"]["completed_at"].as_u64().unwrap() > 0,
            "response.failed must include completed_at"
        );
        // Verify metadata is null (Rs1)
        assert_eq!(parsed["response"]["metadata"], Value::Null);
    }

    #[test]
    fn streaming_rate_limit_error() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        let events = state.process_event(AnthropicEvent::Error {
            error: json!({"type": "rate_limit_error", "message": "Too many requests"}),
        });
        let error_evt = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "error"
        }).unwrap();
        let (_, data) = error_evt.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["code"], "rate_limit_exceeded");
    }

    #[test]
    fn streaming_context_overflow_error() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        let events = state.process_event(AnthropicEvent::Error {
            error: json!({"type": "invalid_request_error", "message": "prompt is too long: 210000 tokens > context window 200000"}),
        });
        let error_evt = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "error"
        }).unwrap();
        let (_, data) = error_evt.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["code"], "context_length_exceeded");
    }

    // --- Response object field completeness ---

    #[test]
    fn response_completed_has_all_echo_fields() {
        let mut state = StreamingState::new(
            "msg_test".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            Some("You are helpful.".to_string()),
            Some(true),
        );
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-20250514", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events[0].to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let resp = &parsed["response"];

        assert_eq!(resp["id"], "msg_test");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["created_at"], 1717000000);
        assert_eq!(resp["model"], "claude-sonnet-4-20250514");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["metadata"], Value::Null);
        assert_eq!(resp["tool_choice"], "auto");
        assert_eq!(resp["instructions"], "You are helpful.");
        assert_eq!(resp["parallel_tool_calls"], true);
        assert!(resp.get("completed_at").is_some());
        assert!(resp.get("output").is_some());
        assert!(resp.get("usage").is_some());
    }

    // --- model_context_window_exceeded produces incomplete_details ---

    #[test]
    fn model_context_window_exceeded_has_incomplete_details() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "model_context_window_exceeded", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events[0].to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(
            parsed["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    // --- message_start updates response_id ---

    #[test]
    fn message_start_updates_response_id() {
        let mut state = make_state();
        let events = state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_FROM_UPSTREAM", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        let (_, data) = events[0].to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["id"], "msg_FROM_UPSTREAM");
    }

    // --- Done event produces no output ---

    #[test]
    fn done_event_produces_no_output() {
        let mut state = make_state();
        let events = state.process_event(AnthropicEvent::Done);
        assert!(events.is_empty());
    }

    // --- Unknown event produces no output ---

    #[test]
    fn unknown_event_produces_no_output() {
        let mut state = make_state();
        let events = state.process_event(AnthropicEvent::Unknown {
            event_type: "future_event".to_string(),
            data: json!({"foo": "bar"}),
        });
        assert!(events.is_empty());
    }

    // --- Mixed text + thinking + tool_use streaming ---

    #[test]
    fn mixed_content_blocks_all_accumulated() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // Thinking block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "Let me think..."}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Text block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "text_delta", "text": "The answer is 42."}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        // Complete
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 20}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events[0].to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let output = parsed["response"]["output"].as_array().unwrap();

        // Should have 2 output items: reasoning + message
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "Let me think...");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "The answer is 42.");
    }

    // --- Error with partial output items accumulated before error ---

    #[test]
    fn error_after_partial_output() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Text block completed before error
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Partial"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Error arrives
        let events = state.process_event(AnthropicEvent::Error {
            error: json!({"type": "api_error", "message": "Something went wrong"}),
        });

        let failed = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.failed"
        }).unwrap();
        let (_, data) = failed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();

        // Output should contain the completed text item
        let output = parsed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "Partial");
    }

    // --- Response completion verification tests (Task 15) ---

    #[test]
    fn usage_cache_creation_folded_into_input_tokens() {
        // cache_creation_input_tokens should be folded into input_tokens
        // (Anthropic includes it in input_tokens already; no separate field in Responses API).
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0, "cache_creation_input_tokens": 25}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        // cache_creation_input_tokens has no Responses API equivalent -- must not appear.
        assert!(usage.get("cache_creation_input_tokens").is_none());
        // reasoning_tokens omitted (Codex uses unwrap_or(0)).
        assert!(usage.get("output_tokens_details").is_none());
        assert!(usage.get("reasoning_tokens").is_none());
    }

    #[test]
    fn usage_no_cache_read_omits_input_tokens_details() {
        // When no cache_read_input_tokens, input_tokens_details must be absent entirely.
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 50, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        assert_eq!(usage["input_tokens"], 50);
        assert_eq!(usage["output_tokens"], 10);
        assert_eq!(usage["total_tokens"], 60);
        assert!(
            usage.get("input_tokens_details").is_none(),
            "input_tokens_details should be omitted when no cached tokens"
        );
    }

    #[test]
    fn all_stop_reasons_produce_response_completed() {
        // CRITICAL: NEVER emit response.incomplete. Every stop_reason must produce
        // response.completed. Test all known Anthropic stop reasons.
        let stop_reasons = vec![
            "end_turn",
            "max_tokens",
            "stop_sequence",
            "tool_use",
            "pause_turn",
            "refusal",
            "model_context_window_exceeded",
        ];

        for sr in &stop_reasons {
            let mut state = make_state();
            state.process_event(AnthropicEvent::MessageStart {
                message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
            });
            state.process_event(AnthropicEvent::MessageDelta {
                delta: json!({"stop_reason": sr, "stop_sequence": null}),
                usage: json!({"output_tokens": 5}),
            });
            let events = state.process_event(AnthropicEvent::MessageStop);

            // Must have exactly one response.completed event.
            let completed_count = events.iter().filter(|e| {
                let (t, _) = e.to_sse();
                t == "response.completed"
            }).count();
            assert_eq!(
                completed_count, 1,
                "stop_reason='{}' must produce exactly one response.completed, got {}",
                sr, completed_count
            );

            // Must NEVER have response.incomplete.
            let has_incomplete = events.iter().any(|e| {
                let (t, _) = e.to_sse();
                t == "response.incomplete"
            });
            assert!(
                !has_incomplete,
                "stop_reason='{}' must NEVER produce response.incomplete",
                sr
            );

            // Status must be "completed" for all stop reasons.
            let (_, data) = events.iter().find(|e| {
                let (t, _) = e.to_sse();
                t == "response.completed"
            }).unwrap().to_sse();
            let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
            assert_eq!(
                parsed["response"]["status"], "completed",
                "stop_reason='{}' must have status 'completed', got '{}'",
                sr, parsed["response"]["status"]
            );
        }
    }

    #[test]
    fn echo_fields_when_none() {
        // When parallel_tool_calls and instructions are None, they should still
        // appear in the response (parallel_tool_calls: null, instructions absent).
        let mut state = StreamingState::new(
            "msg_test".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "required".to_string(),
            None, // instructions_echo = None
            None, // parallel_tool_calls_echo = None
        );
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let resp = &parsed["response"];

        assert_eq!(resp["parallel_tool_calls"], Value::Null);
        assert_eq!(resp["tool_choice"], "required");
        // instructions should be absent when None was provided.
        assert!(resp.get("instructions").is_none());
    }

    #[test]
    fn incomplete_details_only_for_max_tokens_and_context_window() {
        // incomplete_details should ONLY appear for max_tokens and
        // model_context_window_exceeded. Other stop reasons should not have it.
        let reasons_with_incomplete = vec!["max_tokens", "model_context_window_exceeded"];
        let reasons_without_incomplete = vec!["end_turn", "stop_sequence", "tool_use", "pause_turn", "refusal"];

        for sr in &reasons_with_incomplete {
            let (_, parsed) = run_text_stream("text", sr);
            assert_eq!(
                parsed["response"]["incomplete_details"]["reason"], "max_output_tokens",
                "stop_reason='{}' should have incomplete_details",
                sr
            );
        }

        for sr in &reasons_without_incomplete {
            let (_, parsed) = run_text_stream("text", sr);
            assert!(
                parsed["response"].get("incomplete_details").is_none(),
                "stop_reason='{}' should NOT have incomplete_details",
                sr
            );
        }
    }

    #[test]
    fn response_id_passthrough_from_message_start() {
        // id field in response.completed must come from Anthropic message_start,
        // not from the proxy-generated initial value.
        let mut state = StreamingState::new(
            "msg_initial".to_string(),
            "m".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            None,
            None,
        );
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_UPSTREAM_PASSTHROUGH", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            parsed["response"]["id"], "msg_UPSTREAM_PASSTHROUGH",
            "response id must be from Anthropic message_start, not initial value"
        );
    }

    #[test]
    fn model_echo_from_request_not_anthropic() {
        // model field must echo the request model, NOT Anthropic's response model.
        let mut state = StreamingState::new(
            "msg_test".to_string(),
            "gpt-4o".to_string(), // Request model (what client sent)
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            None,
            None,
        );
        // Anthropic returns its own model name in message_start.
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-20250514", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            parsed["response"]["model"], "gpt-4o",
            "model must echo the request model, not Anthropic's response model"
        );
    }

    #[test]
    fn object_field_always_response() {
        // object field must always be "response" regardless of stop_reason.
        let (_, parsed) = run_text_stream("text", "end_turn");
        assert_eq!(parsed["response"]["object"], "response");

        let (_, parsed) = run_text_stream("text", "tool_use");
        assert_eq!(parsed["response"]["object"], "response");

        let (_, parsed) = run_text_stream("text", "max_tokens");
        assert_eq!(parsed["response"]["object"], "response");
    }

    #[test]
    fn output_order_preserved_in_completed() {
        // Output items in response.completed must appear in the same order
        // they were produced during streaming.
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // Thinking block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "hmm"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Text block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "text_delta", "text": "answer"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        // Tool use block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 2,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "run", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 2,
            delta: json!({"type": "input_json_delta", "partial_json": "{}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 2 });

        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "tool_use", "stop_sequence": null}),
            usage: json!({"output_tokens": 20}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let output = parsed["response"]["output"].as_array().unwrap();

        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[2]["type"], "function_call");
    }

    // --- context overflow error code mapping ---

    #[test]
    fn convert_streaming_error_code_context_overflow() {
        assert_eq!(
            convert_streaming_error_code(
                "invalid_request_error",
                "prompt is too long: 210000 tokens"
            ),
            "context_length_exceeded"
        );
        assert_eq!(
            convert_streaming_error_code(
                "invalid_request_error",
                "exceeds context window"
            ),
            "context_length_exceeded"
        );
        assert_eq!(
            convert_streaming_error_code(
                "invalid_request_error",
                "too many tokens: 300000"
            ),
            "context_length_exceeded"
        );
        assert_eq!(
            convert_streaming_error_code("invalid_request_error", "bad parameter"),
            "invalid_request"
        );
    }

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

    // --- apply_patch retry mechanism tests ---

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
        assert!(!events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }), "apply_patch OutputItemAdded should be buffered, not emitted");
    }

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
        // First delta: no *** Begin Patch
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\""}),
        });
        assert!(!events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }));
        // Second delta: *** Begin Patch appears
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "*** Begin Patch\\n*** Update File: test.txt\\n-old\\n+new\\n*** End Patch\"}"}),
        });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "buffered OutputItemAdded should be released when *** Begin Patch detected");
    }

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
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"--- a/test.txt\\n+++ b/test.txt\\n@@ -1 +1 @@\\n-old\\n+new\"}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "buffered OutputItemAdded should be released for converted patch");
        assert!(events.iter().any(|e| { let (t, d) = e.to_sse(); t == "response.output_item.done" && d.contains("*** Begin Patch") }),
            "OutputItemDone should contain converted freeform patch");
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
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"This is not a valid patch format at all.\"}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.is_empty(), "invalid patch should not emit events");
        assert!(state.is_apply_patch_invalid());
        let (toolu_id, raw_patch) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "toolu_01");
        assert!(raw_patch.contains("This is not a valid patch format"));
    }

    #[test]
    fn retry_mode_suppresses_response_created() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_ORIGINAL", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.prepare_for_retry();
        let events = state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "output_tokens": 0}}),
        });
        assert!(events.is_empty(), "retry message_start should emit no events");
    }

    // --- convert_to_freeform_patch tests ---

    #[test]
    fn freeform_already_freeform_returned_as_is() {
        // Input starting with *** Begin Patch should be returned unchanged.
        let input = "*** Begin Patch\n*** Update File: foo.txt\n-old\n+new\n*** End Patch";
        let result = convert_to_freeform_patch(input);
        assert_eq!(result, input);
    }

    #[test]
    fn freeform_already_freeform_with_leading_whitespace() {
        // Leading/trailing whitespace is trimmed for the check, but original is returned.
        let input = "  *** Begin Patch\n*** End Patch\n";
        let result = convert_to_freeform_patch(input);
        assert_eq!(result, input);
    }

    #[test]
    fn freeform_simple_unified_diff() {
        let input = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(result.starts_with("*** Begin Patch\n"));
        assert!(result.contains("*** Update File: file.txt\n"));
        assert!(result.contains("-old\n"));
        assert!(result.contains("+new\n"));
        assert!(result.ends_with("*** End Patch\n"));
        // Hunk header @@ should NOT appear in output
        assert!(!result.contains("@@"));
    }

    #[test]
    fn freeform_unified_diff_exact_output() {
        let input = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new";
        let expected = "*** Begin Patch\n*** Update File: file.txt\n-old\n+new\n*** End Patch\n";
        assert_eq!(convert_to_freeform_patch(input), expected);
    }

    #[test]
    fn freeform_diff_git_prefix() {
        // diff --git prefix should be handled: metadata lines skipped, content converted.
        let input = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(result.starts_with("*** Begin Patch\n"));
        assert!(result.contains("*** Update File: file.txt\n"));
        assert!(result.contains("-old\n"));
        assert!(result.contains("+new\n"));
        assert!(!result.contains("diff --git"));
    }

    #[test]
    fn freeform_context_lines_prefix_stripped() {
        // Context lines (leading space) should be included without the leading space.
        let input = "--- a/file.txt\n+++ b/file.txt\n unchanged\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(result.contains("unchanged\n"), "context line should not have leading space");
        // The output should NOT contain " unchanged" with the space
        assert!(!result.contains(" unchanged"), "context line prefix space should be stripped");
    }

    #[test]
    fn freeform_hunk_headers_skipped() {
        let input = "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n line1\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(!result.contains("@@"), "hunk headers should be removed from output");
    }

    #[test]
    fn freeform_git_index_metadata_skipped() {
        let input = "diff --git a/file.txt b/file.txt\nindex abc123..def456 100644\n--- a/file.txt\n+++ b/file.txt\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(!result.contains("index abc123"), "git index line should be removed");
        assert!(!result.contains("diff --git"), "diff --git line should be removed");
        assert!(result.contains("-old\n"));
        assert!(result.contains("+new\n"));
    }

    #[test]
    fn freeform_empty_input() {
        // Empty string: no diff markers, so returned as-is.
        let result = convert_to_freeform_patch("");
        assert_eq!(result, "");
    }

    #[test]
    fn freeform_whitespace_only_input() {
        // Whitespace-only input: no diff markers, returned as-is.
        let result = convert_to_freeform_patch("   \n  \n");
        assert_eq!(result, "   \n  \n");
    }

    #[test]
    fn freeform_non_patch_garbage_returned_as_is() {
        // Random text without diff markers should be returned unchanged.
        let input = "hello world\nthis is not a patch\n";
        let result = convert_to_freeform_patch(input);
        assert_eq!(result, input);
    }

    #[test]
    fn freeform_multiple_files_in_one_diff() {
        // Two file sections in one diff should produce two *** Update File: sections.
        let input = "\
--- a/file1.txt
+++ b/file1.txt
-old1
+new1
--- a/file2.txt
+++ b/file2.txt
-old2
+new2";
        let result = convert_to_freeform_patch(input);
        assert!(result.contains("*** Update File: file1.txt\n"), "should have file1 section");
        assert!(result.contains("*** Update File: file2.txt\n"), "should have file2 section");
        assert!(result.contains("-old1\n"));
        assert!(result.contains("+new1\n"));
        assert!(result.contains("-old2\n"));
        assert!(result.contains("+new2\n"));
        // Only one Begin/End pair wrapping everything
        assert_eq!(result.matches("*** Begin Patch").count(), 1);
        assert_eq!(result.matches("*** End Patch").count(), 1);
    }

    #[test]
    fn freeform_add_file_dev_null() {
        // New file diff: --- /dev/null, +++ b/newfile.txt
        // Current implementation: --- /dev/null sets current_file to "/dev/null",
        // then +++ b/newfile.txt does NOT override current_file (it's already Some),
        // so the output uses "/dev/null" as the file path. This documents actual behavior.
        let input = "--- /dev/null\n+++ b/newfile.txt\n+content";
        let result = convert_to_freeform_patch(input);
        // The current implementation emits *** Update File: /dev/null
        // because current_file is set by --- /dev/null and +++ doesn't override it.
        assert!(result.contains("*** Update File:"), "should have an Update File section");
        assert!(result.contains("+content\n"), "should include added content");
        assert!(result.starts_with("*** Begin Patch\n"));
        assert!(result.ends_with("*** End Patch\n"));
    }

    #[test]
    fn freeform_delete_file() {
        // Delete file diff: --- a/oldfile.txt, +++ /dev/null
        // current_file = "oldfile.txt" from --- line.
        // +++ /dev/null: path becomes "/dev/null" but current_file is already Some,
        // so *** Update File: oldfile.txt is emitted.
        let input = "--- a/oldfile.txt\n+++ /dev/null\n-old content";
        let result = convert_to_freeform_patch(input);
        assert!(result.contains("*** Update File: oldfile.txt\n"),
            "should use --- side path for delete");
        assert!(result.contains("-old content\n"), "should include removed content");
    }

    #[test]
    fn freeform_context_line_without_leading_space() {
        // Lines that don't start with -, +, space, @@, ---, +++, diff, index, and are
        // non-empty are included as-is (fall through to the else branch at line 135-138).
        let input = "--- a/file.txt\n+++ b/file.txt\nsome other line\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        assert!(result.contains("some other line\n"),
            "unrecognized non-empty lines should be included as-is");
    }

    #[test]
    fn freeform_empty_lines_within_diff_skipped() {
        // Empty lines within a diff body are skipped (line.is_empty() check at line 135).
        let input = "--- a/file.txt\n+++ b/file.txt\n\n-old\n\n+new";
        let result = convert_to_freeform_patch(input);
        // Should not have double newlines from the empty diff lines
        assert!(result.contains("-old\n"));
        assert!(result.contains("+new\n"));
        // Count lines in output to ensure no extra blank lines from diff body
        let out_lines: Vec<&str> = result.lines().collect();
        assert!(!out_lines.iter().any(|l| l.is_empty() && !l.starts_with('*')),
            "empty lines in diff body should be skipped");
    }

    #[test]
    fn freeform_only_marker_no_content() {
        // A diff with just headers and no content lines.
        let input = "--- a/file.txt\n+++ b/file.txt";
        let result = convert_to_freeform_patch(input);
        assert_eq!(result, "*** Begin Patch\n*** Update File: file.txt\n*** End Patch\n");
    }

    #[test]
    fn freeform_trailing_whitespace_on_markers() {
        // --- a/path with extra content after path should still extract correctly.
        let input = "--- a/file.txt\t\n+++ b/file.txt\n-old\n+new";
        let result = convert_to_freeform_patch(input);
        // strip_prefix("--- a/") gets "file.txt\t", which includes the tab.
        // This documents the current behavior.
        assert!(result.contains("file.txt\t"), "trailing whitespace in path is preserved");
    }

    #[test]
    fn freeform_diff_git_without_traditional_headers() {
        // A diff with diff --git but no --- / +++ lines.
        // The has_diff_markers check passes (diff --git present), but no file header
        // is found. The content lines should still be processed.
        let input = "diff --git a/file.txt b/file.txt\n-old line\n+new line";
        let result = convert_to_freeform_patch(input);
        assert!(result.starts_with("*** Begin Patch\n"));
        assert!(result.contains("-old line\n"));
        assert!(result.contains("+new line\n"));
        assert!(result.ends_with("*** End Patch\n"));
    }

    #[test]
    fn freeform_preserves_original_for_already_freeform() {
        // Ensure the ORIGINAL input string (not trimmed) is returned for freeform.
        let input = "\n\n*** Begin Patch\n*** Update File: x\n-old\n+new\n*** End Patch\n\n";
        let result = convert_to_freeform_patch(input);
        assert_eq!(result, input, "original input with surrounding whitespace must be preserved");
    }

    #[test]
    fn freeform_multiple_context_and_change_lines() {
        // A realistic multi-line diff with context, additions, and deletions.
        let input = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,7 +10,7 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
 }";
        let result = convert_to_freeform_patch(input);
        let expected = "\
*** Begin Patch
*** Update File: src/main.rs
fn main() {
-    println!(\"old\");
+    println!(\"new\");
}
*** End Patch
";
        assert_eq!(result, expected);
    }

    // =========================================================================
    // Comprehensive StreamingState retry behavior tests
    // =========================================================================

    // --- Scenario 1: prepare_for_retry resets ALL per-response fields ---

    #[test]
    fn prepare_for_retry_resets_active_block() {
        // Start a tool_use block, then prepare_for_retry should reset active_block
        // so that a new content_block_start works without error.
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.prepare_for_retry();
        // After reset, a new text block should be accepted (active_block was reset to None)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "new content_block_start should work after prepare_for_retry");
    }

    #[test]
    fn prepare_for_retry_clears_text_accumulator() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "stale text"}),
        });
        state.prepare_for_retry();
        // Stream a new text block after retry - accumulator should be fresh
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "fresh"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        let done = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.output_text.done" }).unwrap();
        let (_, data) = done.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "fresh", "text accumulator should be cleared after prepare_for_retry");
    }

    #[test]
    fn prepare_for_retry_clears_thinking_accumulator() {
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
            delta: json!({"type": "thinking_delta", "thinking": "stale thinking"}),
        });
        state.prepare_for_retry();
        // New thinking block after retry should not contain old data
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "fresh thinking"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        let done = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.reasoning_summary_text.done" }).unwrap();
        let (_, data) = done.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "fresh thinking");
    }

    #[test]
    fn prepare_for_retry_clears_arguments_accumulator() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "my_tool", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"stale\":"}),
        });
        state.prepare_for_retry();
        // New tool_use block should start with empty arguments
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "my_tool2", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"fresh\":true}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        let done = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.function_call_arguments.done" }).unwrap();
        let (_, data) = done.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["arguments"], "{\"fresh\":true}");
    }

    #[test]
    fn prepare_for_retry_clears_signature_accumulator() {
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
            delta: json!({"type": "signature_delta", "signature": "stale_sig"}),
        });
        state.prepare_for_retry();
        // New thinking block with fresh signature
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "fresh"}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "fresh_sig"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        let sigs = state.drain_signatures();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].1, "fresh_sig", "signature accumulator should be cleared after prepare_for_retry");
    }

    #[test]
    fn prepare_for_retry_resets_current_is_custom() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Start an apply_patch (custom_tool_call) block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.prepare_for_retry();
        // Now start a non-custom tool_use - should emit OutputItemAdded immediately
        // (not buffered like apply_patch)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "regular_tool", "input": {}}),
        });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "non-custom tool should emit OutputItemAdded immediately after retry reset");
    }

    #[test]
    fn prepare_for_retry_resets_stop_reason() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "tool_use", "stop_sequence": null}),
            usage: json!({"output_tokens": 50}),
        });
        state.prepare_for_retry();
        // Stream a complete response in retry mode
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        // stop_reason was reset, new one is end_turn
        assert_eq!(parsed["response"]["status"], "completed");
        assert!(parsed["response"].get("incomplete_details").is_none(),
            "end_turn should not have incomplete_details (stop_reason was properly reset)");
    }

    #[test]
    fn prepare_for_retry_resets_output_tokens() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 999}),
        });
        state.prepare_for_retry();
        // Retry response with output_tokens=10
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["usage"]["output_tokens"], 10,
            "output_tokens should be 10 from retry response, not 999+10");
    }

    #[test]
    fn prepare_for_retry_resets_apply_patch_format_confirmed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Start and confirm an apply_patch
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"*** Begin Patch\\n*** End Patch\"}"}),
        });
        // Now apply_patch_format_confirmed is true
        state.prepare_for_retry();
        // After retry, a new apply_patch should be buffered again (not immediately released)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "apply_patch", "input": {}}),
        });
        assert!(!events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "apply_patch should be buffered again after retry (format_confirmed was reset)");
    }

    #[test]
    fn prepare_for_retry_resets_apply_patch_invalid() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Trigger invalid apply_patch
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"not a patch\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(state.is_apply_patch_invalid());
        state.prepare_for_retry();
        assert!(!state.is_apply_patch_invalid(), "apply_patch_invalid should be reset to false");
    }

    #[test]
    fn prepare_for_retry_clears_buffered_apply_patch() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Add a text block to output_items so we can verify new items come after it
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "first"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert_eq!(state.output_items().len(), 1);

        // Start an apply_patch (buffers the item)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.prepare_for_retry();
        // After retry, stream a new text block. It should be at output_index=1 (the preserved item)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        let added = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }).unwrap();
        let (_, data) = added.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        // output_index should be 1 because there's 1 item from the first response
        assert_eq!(parsed["output_index"], 1, "new item should come after preserved output items");
    }

    #[test]
    fn prepare_for_retry_clears_anthropic_content_blocks() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
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
        // Add another text block that we'll leave in anthropic_content_blocks
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "text_delta", "text": "World"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        state.prepare_for_retry();
        let captured = state.take_content_block_capture();
        assert!(captured.is_empty(), "anthropic_content_blocks should be cleared after prepare_for_retry");
    }

    #[test]
    fn prepare_for_retry_clears_failed_apply_patch_info() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"not a patch\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        // Failed info is set
        assert_eq!(state.take_failed_apply_patch_info().0, "toolu_01");
        // Now set it again and test prepare_for_retry clears it
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"also invalid\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        state.prepare_for_retry();
        let (toolu_id, raw) = state.take_failed_apply_patch_info();
        assert!(toolu_id.is_empty(), "failed_apply_patch_info should be cleared after prepare_for_retry");
        assert!(raw.is_empty());
    }

    // --- Scenario 1b: prepare_for_retry preserves cross-response fields ---

    #[test]
    fn prepare_for_retry_preserves_output_items_and_fc_counter() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // Add text block -> output_items has 1 item
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "First"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        // Add tool_use -> fc_counter incremented to 1
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "tool_a", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        assert_eq!(state.output_items().len(), 2);

        state.prepare_for_retry();

        // output_items preserved
        assert_eq!(state.output_items().len(), 2);
        assert_eq!(state.output_items()[0]["type"], "message");
        assert_eq!(state.output_items()[1]["type"], "function_call");

        // fc_counter preserved: next tool_use should use fc_1/call_1 (not fc_0/call_0)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_03", "name": "tool_b", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        let done = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.output_item.done" }).unwrap();
        let (_, data) = done.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["item"]["id"], "fc_1", "fc_counter should be preserved across retry");
        assert_eq!(parsed["item"]["call_id"], "call_1");
    }

    #[test]
    fn prepare_for_retry_preserves_response_id_model_created_at() {
        let mut state = StreamingState::new(
            "msg_initial".to_string(),
            "gpt-4o".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            None,
            None,
        );
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_UPSTREAM", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.prepare_for_retry();

        // Complete the retry with a text block
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "retry text"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let resp = &parsed["response"];
        // response_id should be preserved from initial response (msg_UPSTREAM, not msg_RETRY)
        assert_eq!(resp["id"], "msg_UPSTREAM", "response_id should be preserved");
        // model should echo the request model
        assert_eq!(resp["model"], "gpt-4o", "model should be preserved");
        // created_at should be preserved
        assert_eq!(resp["created_at"], 1717000000, "created_at should be preserved");
    }

    #[test]
    fn prepare_for_retry_preserves_input_tokens_and_cache_read_tokens() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 30}}),
        });
        state.prepare_for_retry();
        // Retry response should ADD to existing input_tokens
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 50, "output_tokens": 0, "cache_read_input_tokens": 10}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        assert_eq!(usage["input_tokens"], 150, "input_tokens should accumulate: 100 + 50");
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 40,
            "cache_read_tokens should accumulate: 30 + 10");
    }

    // --- Scenario 3: Content capture with invalid apply_patch ---

    #[test]
    fn content_capture_includes_invalid_apply_patch() {
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
            delta: json!({"type": "text_delta", "text": "Some text"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        // Invalid apply_patch
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_bad", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"not a real patch\"}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        assert!(events.is_empty(), "invalid apply_patch should produce no SSE events");
        assert!(state.is_apply_patch_invalid());

        // Content capture should include BOTH the text block AND the failed apply_patch
        let captured = state.take_content_block_capture();
        assert_eq!(captured.len(), 2, "should capture both text and failed apply_patch");
        assert_eq!(captured[0]["type"], "text");
        assert_eq!(captured[0]["text"], "Some text");
        assert_eq!(captured[1]["type"], "tool_use");
        assert_eq!(captured[1]["id"], "toolu_bad");
        assert_eq!(captured[1]["name"], "apply_patch");
    }

    // --- Scenario 4: output_items does NOT contain invalid apply_patch ---

    #[test]
    fn output_items_excludes_invalid_apply_patch() {
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
        // Invalid apply_patch
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_bad", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"invalid\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        let items = state.output_items();
        assert_eq!(items.len(), 1, "output_items should only contain the text block, not the invalid apply_patch");
        assert_eq!(items[0]["type"], "message");
    }

    // --- Scenario 5: take_failed_apply_patch_info on fresh state ---

    #[test]
    fn take_failed_apply_patch_info_fresh_state_returns_empty() {
        let mut state = make_state();
        let (toolu_id, raw) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "", "fresh state should return empty toolu_id");
        assert_eq!(raw, "", "fresh state should return empty raw patch");
    }

    // --- Scenario 6: take_failed_apply_patch_info twice ---

    #[test]
    fn take_failed_apply_patch_info_twice_returns_empty_second_time() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_99", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"garbage\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        let (toolu_id, raw) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "toolu_99");
        assert!(raw.contains("garbage"));

        let (toolu_id2, raw2) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id2, "", "second take should return empty toolu_id");
        assert_eq!(raw2, "", "second take should return empty raw patch");
    }

    // --- Scenario 7: Retry-mode text block emits content events normally ---

    #[test]
    fn retry_mode_text_block_emits_content_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.prepare_for_retry();
        // Retry message_start should be silent
        let events = state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });
        assert!(events.is_empty(), "retry message_start should emit no events");

        // But content events should work normally
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "retry mode should still emit OutputItemAdded for text block");
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.content_part.added" }),
            "retry mode should still emit ContentPartAdded");

        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Retry text"}),
        });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_text.delta" }),
            "retry mode should still emit OutputTextDelta");

        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_text.done" }));
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.done" }));
    }

    // --- Scenario 8: Retry-mode valid apply_patch (buffer -> early detection -> release -> done) ---

    #[test]
    fn retry_mode_valid_apply_patch_freeform() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.prepare_for_retry();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });

        // apply_patch block
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_retry", "name": "apply_patch", "input": {}}),
        });
        // Should be buffered, no events
        assert!(!events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }));

        // Send freeform patch
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"*** Begin Patch\\n*** Update File: a.txt\\n-old\\n+new\\n*** End Patch\"}"}),
        });
        // Should release buffer on *** Begin Patch detection
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "retry mode: buffered apply_patch should be released on *** Begin Patch detection");

        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.done" }));
        assert!(!state.is_apply_patch_invalid());
    }

    // --- Scenario 9: Input tokens accumulation across retries ---

    #[test]
    fn input_tokens_accumulate_across_retries() {
        let mut state = make_state();
        // First response: input_tokens=100
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        // Text block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "First"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.prepare_for_retry();
        // Retry response: input_tokens=50
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 50, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Retry"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["usage"]["input_tokens"], 150,
            "input_tokens should accumulate across retries: 100 + 50 = 150");
    }

    // --- Scenario 10: response.completed after retry contains both responses' items ---

    #[test]
    fn response_completed_after_retry_contains_both_responses_items() {
        let mut state = make_state();
        // First response: text block "First"
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_ORIG", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "First"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        state.prepare_for_retry();

        // Retry response: text block "Second" + message_stop
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Second"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed = events.iter().find(|e| { let (t, _) = e.to_sse(); t == "response.completed" }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let output = parsed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "response.completed should contain items from both responses");
        // First item from original response
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "First");
        // Second item from retry response
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "Second");
    }

    // --- Scenario 11: apply_patch_format_confirmed reset per block ---

    #[test]
    fn apply_patch_format_confirmed_reset_on_second_block() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // First apply_patch: valid freeform (confirmed)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"*** Begin Patch\\n*** End Patch\"}"}),
        });
        // apply_patch_format_confirmed is now true
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        // Should succeed
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.done" }));

        // Second apply_patch: invalid format (format_confirmed should be reset to false)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "apply_patch", "input": {}}),
        });
        // Send invalid content
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"not a valid patch at all\"}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        // The second apply_patch should be invalid (format_confirmed was reset at content_block_start)
        assert!(events.is_empty(), "second apply_patch should fail (format_confirmed was reset)");
        assert!(state.is_apply_patch_invalid(),
            "second apply_patch should be flagged as invalid (format_confirmed was reset per block)");
    }

    // =========================================================================
    // Scenario 17: First apply_patch invalid, second never arrives
    // =========================================================================

    #[test]
    fn scenario_17_first_invalid_apply_patch_flags_and_stores_info() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // Stream an invalid apply_patch (garbage content, no patch markers)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_BAD", "name": "apply_patch", "input": {}}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"this is garbage not a patch at all\"}"}),
        });
        // No events during delta for apply_patch (buffered)
        assert!(!events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "delta should not release buffered apply_patch");

        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Verify: apply_patch is flagged invalid
        assert!(state.is_apply_patch_invalid(),
            "apply_patch should be flagged as invalid");

        // Verify: NO events emitted for the invalid apply_patch
        assert!(events.is_empty(),
            "invalid apply_patch should emit zero events");

        // Verify: failed_apply_patch_info has the correct toolu_id
        let (toolu_id, raw_patch) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "toolu_BAD",
            "failed_apply_patch_info should contain toolu_BAD's toolu_id");
        assert!(raw_patch.contains("garbage"),
            "raw patch text should contain the garbage content");
    }

    // =========================================================================
    // Scenario 19: Full double-retry StreamingState cycle
    // =========================================================================

    #[test]
    fn scenario_19_double_retry_cycle() {
        let mut state = make_state();

        // --- First response: text "Hello" + invalid apply_patch ---
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_ORIG", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 50, "output_tokens": 0}}),
        });
        // Text block "Hello"
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Invalid apply_patch #1
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_FIRST", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"bad patch 1\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        assert!(state.is_apply_patch_invalid(), "first apply_patch should be invalid");

        // --- Call prepare_for_retry() ---
        state.prepare_for_retry();
        assert!(!state.is_apply_patch_invalid(), "invalid flag reset after prepare_for_retry");

        // --- Retry response: text "Retry" + ANOTHER invalid apply_patch ---
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY1", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 30, "output_tokens": 0}}),
        });
        // Text block "Retry"
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Retry"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Invalid apply_patch #2
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_SECOND", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"bad patch 2\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });
        assert!(state.is_apply_patch_invalid(), "second apply_patch should also be invalid");

        // --- Verify output_items contains "Hello" and "Retry" ---
        let items = state.output_items();
        assert_eq!(items.len(), 2, "output_items should have both text blocks from both responses");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "Hello");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["content"][0]["text"], "Retry");

        // --- Verify failed_apply_patch_info has the SECOND apply_patch's info ---
        let (toolu_id, raw_patch) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "toolu_SECOND",
            "failed_apply_patch_info should be from the SECOND (most recent) invalid apply_patch");
        assert!(raw_patch.contains("bad patch 2"),
            "raw patch should be from the second invalid apply_patch");

        // --- Verify anthropic_content_blocks from retry response captured ---
        let captured = state.take_content_block_capture();
        // The retry response had text "Retry" + invalid apply_patch => 2 captured blocks
        assert!(!captured.is_empty(),
            "anthropic_content_blocks should have been captured from the retry response");
        assert!(captured.iter().any(|b| b["type"] == "text" && b["text"] == "Retry"),
            "captured blocks should include the Retry text block");

        // --- Call prepare_for_retry() again (second retry) ---
        state.prepare_for_retry();
        assert!(!state.is_apply_patch_invalid(), "invalid flag reset for third attempt");
        assert!(state.is_retry_mode(), "retry_mode should be true");
        assert_eq!(state.output_items().len(), 2,
            "output_items should still be preserved after second prepare_for_retry");

        // --- Verify state is ready for a third attempt ---
        // Stream a valid text block as a third attempt
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY2", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 20, "output_tokens": 0}}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        // Should emit events normally (state is ready)
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.added" }),
            "third attempt should emit OutputItemAdded for text block");

        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Third"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| { let (t, _) = e.to_sse(); t == "response.output_item.done" }),
            "third attempt text block should complete normally");

        // Verify all three responses' text items accumulated
        let items = state.output_items();
        assert_eq!(items.len(), 3, "output_items should have all three text blocks");
    }

    // =========================================================================
    // Scenario 47: Buffer released exactly once
    // =========================================================================

    #[test]
    fn scenario_47_buffer_released_exactly_once() {
        let mut state = make_state();
        let mut all_events: Vec<ResponsesEvent> = Vec::new();

        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // Start apply_patch
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {}}),
        });

        // First delta with *** Begin Patch => triggers release of buffered item
        let delta_events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"*** Begin Patch\\n*** Update File: x.txt\\n-old\\n+new\\n*** End Patch\"}"}),
        });
        all_events.extend(delta_events);

        // Send additional deltas
        let delta_events2 = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": ""}),
        });
        all_events.extend(delta_events2);

        // content_block_stop
        let stop_events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        all_events.extend(stop_events);

        // Count OutputItemAdded events
        let added_count = all_events.iter().filter(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).count();

        assert_eq!(added_count, 1,
            "OutputItemAdded should be emitted exactly once (from the delta detection), got {}",
            added_count);

        // OutputItemDone should be emitted at content_block_stop
        let done_count = all_events.iter().filter(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }).count();
        assert_eq!(done_count, 1,
            "OutputItemDone should be emitted exactly once at content_block_stop, got {}",
            done_count);

        // No duplicate OutputItemAdded
        assert!(added_count == 1, "no duplicate OutputItemAdded should exist");
    }

    // =========================================================================
    // Scenario 52: Namespace registry preserved across retries
    // =========================================================================

    #[test]
    fn scenario_52_namespace_registry_preserved_across_retries() {
        let mut registry = NamespaceRegistry::new();
        registry.register(
            "mcp__memory__search".to_string(),
            "mcp__memory__".to_string(),
            "search".to_string(),
        );
        let mut state = StreamingState::new(
            "msg_test".to_string(),
            "m".to_string(),
            registry,
            1717000000,
            "auto".to_string(),
            None,
            None,
        );

        // First response: use a namespaced tool
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        // OutputItemAdded is emitted at ContentBlockStart for regular tool_use
        let start_events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "mcp__memory__search", "input": {}}),
        });

        // Verify namespace lookup worked on first response
        let added = start_events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).unwrap();
        let (_, data) = added.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["item"]["name"], "search");
        assert_eq!(parsed["item"]["namespace"], "mcp__memory__");

        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"q\":\"test\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // prepare_for_retry
        state.prepare_for_retry();

        // Retry response: use the same namespaced tool again
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 5, "output_tokens": 0}}),
        });
        // OutputItemAdded is emitted at ContentBlockStart for regular tool_use
        let start_events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_02", "name": "mcp__memory__search", "input": {}}),
        });

        // Verify namespace lookup still works after retry (at ContentBlockStart)
        let added = start_events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }).unwrap();
        let (_, data) = added.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["item"]["name"], "search",
            "namespace lookup should still work after retry");
        assert_eq!(parsed["item"]["namespace"], "mcp__memory__",
            "namespace should still be resolved correctly after retry");

        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"q\":\"retry\"}"}),
        });
        let stop_events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Verify OutputItemDone also has correct namespace after retry
        let done = stop_events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }).unwrap();
        let (_, done_data) = done.to_sse();
        let done_parsed: serde_json::Value = serde_json::from_str(&done_data).unwrap();
        assert_eq!(done_parsed["item"]["name"], "search",
            "OutputItemDone should have correct display name after retry");
        assert_eq!(done_parsed["item"]["namespace"], "mcp__memory__",
            "OutputItemDone should have correct namespace after retry");
    }

    // =========================================================================
    // Scenario 16: SSE event order - text emitted before apply_patch detection
    // =========================================================================

    #[test]
    fn scenario_16_text_before_apply_patch_event_order() {
        let mut state = make_state();
        let mut all_events: Vec<ResponsesEvent> = Vec::new();
        let mut event_types: Vec<String> = Vec::new();

        // message_start
        all_events.extend(state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        }));

        // Text block (completed)
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Some text"}),
        }));
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStop { index: 0 }));

        // apply_patch content_block_start
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_bad", "name": "apply_patch", "input": {}}),
        }));
        // apply_patch delta (invalid)
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"not valid\"}"}),
        }));
        // apply_patch content_block_stop (should emit nothing for invalid)
        all_events.extend(state.process_event(AnthropicEvent::ContentBlockStop { index: 1 }));

        // Build event type list
        for e in &all_events {
            let (t, _) = e.to_sse();
            event_types.push(t);
        }

        // Find positions of text-related events
        let text_added_pos = event_types.iter().position(|t| t == "response.output_item.added");
        let text_done_pos = event_types.iter().position(|t| t == "response.output_item.done");

        // Verify text events exist
        assert!(text_added_pos.is_some(), "should have OutputItemAdded for text block");
        assert!(text_done_pos.is_some(), "should have OutputItemDone for text block");

        // Verify text OutputItemAdded comes before text OutputItemDone
        let added_pos = text_added_pos.unwrap();
        let done_pos = text_done_pos.unwrap();
        assert!(added_pos < done_pos,
            "text OutputItemAdded should come before text OutputItemDone");

        // Verify no apply_patch events exist (invalid => no events)
        let apply_patch_events: Vec<_> = event_types.iter()
            .filter(|t| **t == "response.output_item.added" || **t == "response.output_item.done")
            .collect();
        // We should have exactly 2: one added + one done, both for text
        assert_eq!(apply_patch_events.len(), 2,
            "should only have text-related added/done events, no apply_patch events");

        // Verify text output_items are present even though apply_patch was invalid
        let items = state.output_items();
        assert_eq!(items.len(), 1, "should have 1 output item (text only)");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["text"], "Some text");

        // Verify apply_patch was flagged invalid
        assert!(state.is_apply_patch_invalid());
    }

    // =========================================================================
    // Scenario 54: Patch input field "input" key fallback
    // =========================================================================

    #[test]
    fn scenario_54_input_key_fallback_detected_as_invalid() {
        // When the patch content is under {"input": "..."} instead of {"patch": "..."},
        // the fallback in handle_content_block_stop should still extract it and flag
        // it as invalid if the content is not a valid patch format.
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // apply_patch with "input" key instead of "patch" key, containing garbage
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_input_key", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"input\":\"garbage text\"}"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // The invalid format should be detected via the "input" key fallback
        assert!(state.is_apply_patch_invalid(),
            "apply_patch with 'input' key should be flagged as invalid when content is garbage");

        // No events should be emitted for invalid apply_patch
        assert!(events.is_empty(),
            "invalid apply_patch with 'input' key should emit zero events");

        // take_failed_apply_patch_info should return content from the "input" key
        let (toolu_id, raw_patch) = state.take_failed_apply_patch_info();
        assert_eq!(toolu_id, "toolu_input_key",
            "failed_apply_patch_info should have the correct toolu_id");
        assert!(raw_patch.contains("garbage text"),
            "raw patch should contain the content extracted from the 'input' key, got: {}", raw_patch);
    }

    // =========================================================================
    // Scenario 40: Output tokens from failed response not accumulated
    // =========================================================================

    #[test]
    fn scenario_40_output_tokens_reset_after_failed_response() {
        let mut state = make_state();

        // First response: message_start (input_tokens=100)
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });

        // Text block
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Partial"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Invalid apply_patch at content_block_stop
        // Note: there's no message_delta/message_stop (upstream closed early)
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: json!({"type": "tool_use", "id": "toolu_bad", "name": "apply_patch", "input": {}}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 1,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"patch\":\"garbage\"}"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 1 });

        // output_tokens was never set (no message_delta), so it should be 0
        // But we verify the important thing: prepare_for_retry resets it

        // Call prepare_for_retry()
        state.prepare_for_retry();

        // Verify output_tokens was reset to 0 by prepare_for_retry
        // We verify by running a retry response and checking the final usage
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_RETRY", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}),
        });

        // Complete the retry normally
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Retry"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 25}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);

        let completed = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap();
        let (_, data) = completed.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();

        // output_tokens should be exactly 25 from the retry response, not accumulated from before
        assert_eq!(parsed["response"]["usage"]["output_tokens"], 25,
            "output_tokens should be 25 from retry response only (reset by prepare_for_retry)");

        // input_tokens was preserved (100 from first response, 0 from retry = 100)
        assert_eq!(parsed["response"]["usage"]["input_tokens"], 100,
            "input_tokens should be preserved from first response (100)");
    }
}
