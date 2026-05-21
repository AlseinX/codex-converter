//! Integration test: extended thinking with signature cache round-trip.
//!
//! Scenario: Turn 1 sends a reasoning request, gets back a thinking block with
//! a signature. The signature is cached. Turn 2 sends a follow-up with a reasoning
//! input item referencing the cached signature.

use codex_conv::conversion::request::convert_request;
use codex_conv::conversion::response::StreamingState;
use codex_conv::conversion::{ConversionTask, SignatureCache};
use codex_conv::sse::anthropic::{AnthropicEvent, parse_sse_events};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Helper to find a content block of a given type across all messages.
fn find_content_block_by_type<'a>(
    messages: &'a [serde_json::Value],
    block_type: &str,
) -> Option<&'a serde_json::Value> {
    for m in messages {
        if let Some(arr) = m["content"].as_array() {
            for b in arr {
                if b["type"] == block_type {
                    return Some(b);
                }
            }
        }
    }
    None
}

#[test]
fn thinking_stream_with_signature_cache() {
    let cache = Arc::new(SignatureCache::new(Duration::from_secs(10800)));

    // -- First turn: thinking response --
    let mut task = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "reasoning": {"effort": "high", "summary": "auto"},
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Think about it"}]}]
    });
    let anth_request = convert_request(&mut task, request).unwrap();

    // Verify thinking params.
    assert_eq!(anth_request["thinking"]["type"], "adaptive");
    assert_eq!(anth_request["output_config"]["effort"], "high");
    // Temperature must be omitted when thinking enabled.
    assert!(
        anth_request.get("temperature").is_none(),
        "temperature must be omitted when thinking enabled"
    );

    // Simulated Anthropic thinking response.
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_think","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":50,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to analyze..."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"ErUB_cachesig"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Here is my answer."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":100}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_think".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output_sse = String::new();
    let mut reasoning_id = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            let (event_type, data) = re.to_sse();
            if event_type == "response.output_item.added" && data.contains("\"reasoning\"") {
                let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
                reasoning_id = parsed["item"]["id"].as_str().unwrap().to_string();
            }
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // Verify reasoning events emitted.
    assert!(output_sse.contains("event: response.reasoning_summary_part.added"));
    assert!(output_sse.contains("event: response.reasoning_summary_text.delta"));
    assert!(output_sse.contains("event: response.reasoning_summary_text.done"));
    assert!(output_sse.contains("event: response.reasoning_summary_part.done"));

    // Signature should NOT appear in SSE output (consumed and accumulated).
    assert!(
        !output_sse.contains("ErUB_cachesig"),
        "signature must NOT appear in SSE output"
    );

    // Thinking text should appear as reasoning summary delta.
    assert!(
        output_sse.contains("\"delta\":\"I need to analyze...\""),
        "thinking text must appear as reasoning summary delta"
    );

    // Text response should also be present.
    assert!(output_sse.contains("\"text\":\"Here is my answer.\""));

    // Write signature to cache.
    let signatures = state.drain_signatures();
    for (id, sig) in signatures {
        cache.insert(id, sig);
    }

    // -- Second turn: reasoning input with cached signature --
    let mut task2 = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request2 = json!({
        "model": "claude-sonnet-4-20250514",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Follow up"}]},
            {"type": "reasoning", "id": reasoning_id, "summary": [{"type": "summary_text", "text": "I need to analyze..."}]}
        ]
    });
    let anth_request2 = convert_request(&mut task2, request2).unwrap();
    let messages = anth_request2["messages"].as_array().unwrap();

    // Find the thinking block in the messages.
    let thinking_block =
        find_content_block_by_type(messages, "thinking").expect("should have a thinking block");

    assert_eq!(
        thinking_block["signature"], "ErUB_cachesig",
        "cached signature should be used in subsequent request"
    );
    assert_eq!(
        thinking_block["thinking"], "I need to analyze...",
        "thinking text should come from summary"
    );
}

/// Test reasoning with encrypted_content (redacted_thinking passthrough).
#[test]
fn reasoning_with_encrypted_content_passthrough() {
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "id": "rs_prev", "summary": [], "encrypted_content": "ENCRYPTED_DATA_HERE"}
        ]
    });
    let result = convert_request(&mut task, request).unwrap();
    let messages = result["messages"].as_array().unwrap();

    // Find the redacted_thinking block.
    let block = find_content_block_by_type(messages, "redacted_thinking")
        .expect("should have redacted_thinking block");

    assert_eq!(block["data"], "ENCRYPTED_DATA_HERE");
}

/// Test reasoning with no encrypted_content and no cached signature -> empty signature.
#[test]
fn reasoning_no_cache_empty_signature() {
    let cache = Arc::new(SignatureCache::new(Duration::from_secs(3600)));
    let mut task = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "id": "rs_no_cache", "summary": [{"type": "summary_text", "text": "Some thinking"}]}
        ]
    });
    let result = convert_request(&mut task, request).unwrap();
    let messages = result["messages"].as_array().unwrap();

    let block =
        find_content_block_by_type(messages, "thinking").expect("should have thinking block");

    // When no cached signature exists, use empty signature.
    assert_eq!(block["signature"], "");
    assert_eq!(block["thinking"], "Some thinking");
}

/// Test redacted_thinking streaming produces reasoning with encrypted_content.
#[test]
fn redacted_thinking_streaming() {
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let _ = convert_request(&mut task, json!({
        "model": "claude-sonnet-4-20250514",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    })).unwrap();

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_redact","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"OPAQUE_DATA"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_redact".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output_sse = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // Must have reasoning output item with encrypted_content.
    assert!(output_sse.contains("\"type\":\"reasoning\""));
    assert!(output_sse.contains("\"encrypted_content\":\"OPAQUE_DATA\""));
    // Summary should be empty for redacted_thinking.
    assert!(output_sse.contains("response.completed"));
}
