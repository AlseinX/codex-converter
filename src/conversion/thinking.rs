use serde_json::{Value, json};
use std::sync::Arc;

use crate::conversion::SignatureCache;
use crate::conversion::response::StreamingState;

/// Result of reasoning -> thinking conversion.
pub struct ThinkingConfig {
    /// The `thinking` field for the Anthropic request (if reasoning was present and effort != "none").
    pub thinking: Option<Value>,
    /// The `output_config.effort` value (if reasoning was present and effort != "none").
    pub effort: Option<String>,
    /// Whether thinking is enabled (controls temperature omission).
    pub thinking_enabled: bool,
}

/// Convert Responses API `reasoning` field to Anthropic `thinking` + `output_config.effort`.
///
/// - `reasoning.effort` -> `output_config.effort` (direct string forwarding, with mappings)
/// - `reasoning.summary` -> `thinking.display`
/// - `reasoning.effort: "none"` -> omit both `thinking` and `output_config`
/// - `reasoning` absent -> omit both
pub fn convert_reasoning(reasoning: Option<&Value>) -> ThinkingConfig {
    let Some(reasoning) = reasoning else {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    };

    let effort_val = reasoning
        .get("effort")
        .and_then(|v| v.as_str())
        .unwrap_or("high");
    let summary_val = reasoning.get("summary").and_then(|v| v.as_str());

    // effort "none" means no thinking.
    if effort_val == "none" {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    }

    // Map effort value.
    let mapped_effort = match effort_val {
        "minimal" => "low", // Anthropic has no "minimal" level.
        other => other,     // "low", "medium", "high", "xhigh" pass through directly.
    };

    // Map summary to thinking.display.
    let display = match summary_val {
        Some("none") => "omitted",
        _ => "summarized", // absent, "auto", "concise", "detailed" all map to "summarized".
    };

    ThinkingConfig {
        thinking: Some(json!({
            "type": "adaptive",
            "display": display,
        })),
        effort: Some(mapped_effort.to_string()),
        thinking_enabled: true,
    }
}

/// Determine the Anthropic content block for a reasoning input item.
///
/// - If `encrypted_content` is present -> `redacted_thinking`
/// - Otherwise -> `thinking` with summary text and cached or empty signature
pub fn convert_reasoning_input(
    summary: &[Value],
    encrypted_content: Option<&str>,
    signature_cache: &crate::conversion::SignatureCache,
    reasoning_id: &str,
) -> Value {
    if let Some(data) = encrypted_content {
        json!({
            "type": "redacted_thinking",
            "data": data
        })
    } else {
        // Concatenate summary texts.
        let thinking_text = summary
            .iter()
            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");

        let signature = signature_cache.get(reasoning_id).unwrap_or_default();

        json!({
            "type": "thinking",
            "thinking": thinking_text,
            "signature": signature
        })
    }
}

/// After streaming completes, take accumulated signatures from StreamingState
/// and write them to the signature cache, keyed by reasoning ID.
pub fn drain_signatures_from_streaming_state(
    state: &mut StreamingState,
    cache: &Arc<SignatureCache>,
) {
    for (reasoning_id, signature) in state.drain_signatures() {
        cache.insert(reasoning_id, signature);
    }
}

/// Rectifier fallback: strip all thinking and redacted_thinking content blocks
/// from messages. Used when Anthropic returns 400 due to invalid signatures.
///
/// Operates on the `content` arrays within each message value. Non-content fields
/// (role, etc.) are preserved. Other block types (text, tool_use, etc.) are preserved.
pub fn strip_thinking_blocks(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            content.retain(|block| {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                block_type != "thinking" && block_type != "redacted_thinking"
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reasoning_returns_empty_config() {
        let config = convert_reasoning(None);
        assert!(config.thinking.is_none());
        assert!(config.effort.is_none());
        assert!(!config.thinking_enabled);
    }

    #[test]
    fn reasoning_high_effort_auto_summary() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "auto"})));
        assert!(config.thinking_enabled);
        assert_eq!(config.effort.as_deref(), Some("high"));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_effort_none_omits_all() {
        let config = convert_reasoning(Some(&json!({"effort": "none"})));
        assert!(config.thinking.is_none());
        assert!(config.effort.is_none());
        assert!(!config.thinking_enabled);
    }

    #[test]
    fn reasoning_summary_none_display_omitted() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "none"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "omitted");
    }

    #[test]
    fn reasoning_minimal_maps_to_low() {
        let config = convert_reasoning(Some(&json!({"effort": "minimal", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("low"));
    }

    #[test]
    fn reasoning_summary_absent_defaults_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "medium"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_summary_concise_maps_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "concise"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_summary_detailed_maps_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "detailed"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_xhigh_passthrough() {
        let config = convert_reasoning(Some(&json!({"effort": "xhigh", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn reasoning_low_passthrough() {
        let config = convert_reasoning(Some(&json!({"effort": "low", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("low"));
    }

    #[test]
    fn reasoning_input_encrypted_content_becomes_redacted_thinking() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "some summary"})],
            Some("ENCRYPTED_DATA"),
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "redacted_thinking");
        assert_eq!(result["data"], "ENCRYPTED_DATA");
    }

    #[test]
    fn reasoning_input_summary_only_empty_signature() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "I thought about it"})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["thinking"], "I thought about it");
        assert_eq!(result["signature"], "");
    }

    #[test]
    fn reasoning_input_summary_cached_signature() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "cached_sig_123".to_string());
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "thinking..."})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["signature"], "cached_sig_123");
    }

    #[test]
    fn reasoning_input_multiple_summaries_concatenated() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[
                json!({"type": "summary_text", "text": "part1"}),
                json!({"type": "summary_text", "text": "part2"}),
            ],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["thinking"], "part1part2");
    }

    #[test]
    fn reasoning_input_empty_summary() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(&[], None, &cache, "rs_001");
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["thinking"], "");
    }

    #[test]
    fn strip_thinking_blocks_removes_thinking() {
        let mut messages: Vec<Value> = json!([
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I thought", "signature": "sig"},
                {"type": "text", "text": "Hello"}
            ]}
        ])
        .as_array()
        .unwrap()
        .clone();
        strip_thinking_blocks(&mut messages);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn strip_thinking_blocks_removes_redacted_thinking() {
        let mut messages: Vec<Value> = json!([
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "encrypted_blob"},
                {"type": "text", "text": "Answer"}
            ]}
        ])
        .as_array()
        .unwrap()
        .clone();
        strip_thinking_blocks(&mut messages);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn strip_thinking_blocks_preserves_non_thinking_content() {
        let mut messages: Vec<Value> = json!([
            {"role": "user", "content": "user message"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Here is the code"},
                {"type": "tool_use", "id": "toolu_01", "name": "bash", "input": {"command": "ls"}}
            ]}
        ])
        .as_array()
        .unwrap()
        .clone();
        strip_thinking_blocks(&mut messages);
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn strip_thinking_blocks_mixed_content_preserved() {
        let mut messages: Vec<Value> = json!([
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reasoning", "signature": "sig1"},
                {"type": "text", "text": "step 1"},
                {"type": "redacted_thinking", "data": "blob"},
                {"type": "text", "text": "step 2"},
                {"type": "thinking", "thinking": "more reasoning", "signature": "sig2"}
            ]}
        ])
        .as_array()
        .unwrap()
        .clone();
        strip_thinking_blocks(&mut messages);
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "step 1");
        assert_eq!(content[1]["text"], "step 2");
    }

    #[test]
    fn strip_thinking_blocks_empty_messages() {
        let mut messages: Vec<Value> = vec![];
        strip_thinking_blocks(&mut messages);
        // Should not panic on empty input.
    }

    #[test]
    fn strip_thinking_blocks_no_content_field() {
        let mut messages: Vec<Value> = json!([
            {"role": "user", "text": "plain text message"}
        ])
        .as_array()
        .unwrap()
        .clone();
        strip_thinking_blocks(&mut messages);
        // Should not panic when message has no content array.
    }

    #[test]
    fn drain_signatures_writes_to_cache() {
        let cache = Arc::new(SignatureCache::new(std::time::Duration::from_secs(3600)));
        let mut state = crate::conversion::response::StreamingState::new(
            "resp_001".to_string(),
            "test-model".to_string(),
            crate::conversion::NamespaceRegistry::new(),
            1000,
            "auto".to_string(),
            None,
            None,
        );

        // Process a thinking content block start + delta + signature_delta + stop
        // to accumulate a signature in the internal store.
        use crate::sse::anthropic::AnthropicEvent;
        let _ = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        let _ = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "Let me think..."}),
        });
        // signature_delta accumulates silently
        let _ = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "ErUB_signature_data"}),
        });
        let _ = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Now drain signatures from the state into the cache.
        drain_signatures_from_streaming_state(&mut state, &cache);

        // The reasoning ID is auto-generated (rs_xxx format). We can verify
        // that after draining, calling drain again yields nothing.
        let empty = state.drain_signatures();
        assert!(empty.is_empty(), "drain should clear the store");

        // We can also verify that some signature was cached by checking the
        // existing response tests that validate signature accumulation.
    }

    #[test]
    fn drain_signatures_preserves_signature_in_cache() {
        let cache = Arc::new(SignatureCache::new(std::time::Duration::from_secs(3600)));
        let mut state = crate::conversion::response::StreamingState::new(
            "resp_002".to_string(),
            "test-model".to_string(),
            crate::conversion::NamespaceRegistry::new(),
            1000,
            "auto".to_string(),
            None,
            None,
        );

        use crate::sse::anthropic::AnthropicEvent;
        use crate::sse::responses::ResponsesEvent;
        // Process a full thinking block cycle with signature
        let _events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        let _ = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "reasoning text"}),
        });
        let _ = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "SIGNATURE_DATA"}),
        });
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        // Extract reasoning_id from OutputItemDone event
        let reasoning_id = events
            .iter()
            .find_map(|e| {
                if let ResponsesEvent::OutputItemDone { item, .. } = e {
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .expect("OutputItemDone should contain reasoning_id");

        // Drain into cache
        drain_signatures_from_streaming_state(&mut state, &cache);

        // Verify the signature was cached under the correct reasoning_id
        assert_eq!(
            cache.get(&reasoning_id),
            Some("SIGNATURE_DATA".to_string()),
            "signature should be cached for reasoning_id {}",
            reasoning_id
        );
    }
}
