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

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_message_start(&mut self, message: Value) -> Vec<ResponsesEvent> {
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

                let mut item = json!({
                    "type": "function_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": display_name,
                    "status": "in_progress",
                    "arguments": "",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }

                vec![ResponsesEvent::OutputItemAdded { output_index, item }]
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
            }
            ActiveBlock::Thinking => {
                let thinking_text = std::mem::take(&mut self.thinking_accumulator);
                let signature = std::mem::take(&mut self.signature_accumulator);
                let reasoning_id = std::mem::take(&mut self.current_reasoning_id);

                // Store signature for cache write after stream ends.
                if !signature.is_empty() {
                    self.signature_store
                        .push((reasoning_id.clone(), signature));
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
            }
            ActiveBlock::ToolUse => {
                let args = std::mem::take(&mut self.arguments_accumulator);
                let fc_id = std::mem::take(&mut self.current_fc_item_id);
                let call_id = std::mem::take(&mut self.current_fc_call_id);

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

        // end_turn: false only when stop_reason is tool_use, omit otherwise.
        if stop_reason == "tool_use" {
            response_obj["end_turn"] = json!(false);
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
            "status": "failed",
            "error": {"code": code, "message": message},
            "output": self.output_items,
            "usage": Value::Null,
            "metadata": {},
        });

        vec![
            ResponsesEvent::Error {
                code: code.to_string(),
                message: message.to_string(),
            },
            ResponsesEvent::ResponseFailed {
                response: response_obj,
            },
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
        "request_too_large" => "context_length_exceeded",
        "rate_limit_error" => "rate_limit_exceeded",
        "billing_error" => "insufficient_quota",
        "overloaded_error" => "server_is_overloaded",
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

    // --- end_turn only when tool_use ---

    #[test]
    fn end_turn_false_only_when_tool_use() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "tool_use", "stop_sequence": null}),
            usage: json!({"output_tokens": 50}),
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
        assert_eq!(parsed["response"]["end_turn"], false);
    }

    #[test]
    fn end_turn_absent_when_not_tool_use() {
        let (_, parsed) = run_text_stream("Hello", "end_turn");
        assert!(
            parsed["response"].get("end_turn").is_none(),
            "end_turn should be absent when stop_reason != tool_use"
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
}
