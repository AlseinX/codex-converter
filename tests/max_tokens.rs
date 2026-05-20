//! Integration test: max_tokens truncation always produces response.completed.
//!
//! CRITICAL (B2): Never emit response.incomplete. Codex CLI treats it as a
//! retryable error, causing an infinite retry loop. The proxy must always emit
//! response.completed with incomplete_details as informational metadata.

use codex_conv::conversion::response::StreamingState;
use codex_conv::conversion::namespace::NamespaceRegistry;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;

#[test]
fn max_tokens_produces_response_completed_not_incomplete() {
    let mut state = StreamingState::new(
        "msg_max".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_max","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"This is a partial response that got cut off because of max tok"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4096}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut output_sse = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            output_sse.push_str("data: [DONE]\n\n");
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // CRITICAL: Must have response.completed, NOT response.incomplete.
    assert!(
        output_sse.contains("event: response.completed"),
        "max_tokens MUST produce response.completed, never response.incomplete"
    );
    assert!(
        !output_sse.contains("response.incomplete"),
        "NEVER emit response.incomplete (causes infinite retry in Codex CLI)"
    );

    // incomplete_details must be present as informational metadata.
    assert!(
        output_sse.contains("\"incomplete_details\""),
        "incomplete_details must be present"
    );
    assert!(
        output_sse.contains("\"reason\":\"max_output_tokens\""),
        "incomplete_details reason must be max_output_tokens"
    );

    // Status must still be "completed".
    assert!(
        output_sse.contains("\"status\":\"completed\""),
        "status must be completed even with max_tokens"
    );
}

/// Test that model_context_window_exceeded also produces response.completed
/// with incomplete_details (same behavior as max_tokens).
#[test]
fn model_context_window_exceeded_produces_response_completed() {
    let mut state = StreamingState::new(
        "msg_ctx".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_ctx","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200000,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded","stop_sequence":null},"usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
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

    assert!(output_sse.contains("event: response.completed"));
    assert!(!output_sse.contains("response.incomplete"));
    assert!(output_sse.contains("\"reason\":\"max_output_tokens\""));
    assert!(output_sse.contains("\"status\":\"completed\""));
}

/// Verify that end_turn does NOT produce incomplete_details.
#[test]
fn end_turn_no_incomplete_details() {
    let mut state = StreamingState::new(
        "msg_end".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_end","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":50,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
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

    assert!(output_sse.contains("event: response.completed"));
    assert!(
        !output_sse.contains("incomplete_details"),
        "end_turn should NOT produce incomplete_details"
    );
}
