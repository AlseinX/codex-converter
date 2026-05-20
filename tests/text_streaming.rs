//! Integration test: simple text streaming through the full conversion pipeline.
//!
//! Scenario: Codex sends a Responses API request for a simple text completion.
//! Mock Anthropic returns a streaming text response. Verify the full pipeline
//! converts correctly from Responses API -> Anthropic -> back to Responses API SSE.

use codex_conv::conversion::{ConversionTask, NamespaceRegistry};
use codex_conv::conversion::request::convert_request;
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

/// Simulates a complete text streaming request-response cycle.
#[test]
fn simple_text_streaming_end_to_end() {
    // -- Request phase --
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "stream": true,
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Say hello"}]}
        ]
    });
    let anth_request = convert_request(&mut task, request).unwrap();

    // Verify Anthropic request structure.
    assert_eq!(anth_request["model"], "claude-sonnet-4-20250514");
    assert_eq!(anth_request["stream"], true);
    let messages = anth_request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["type"], "text");
    assert_eq!(messages[0]["content"][0]["text"], "Say hello");

    // -- Simulated Anthropic SSE response --
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01ABC","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);

    // -- Response conversion phase --
    let mut state = StreamingState::new(
        "msg_01ABC".to_string(),
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
            output_sse.push_str("data: [DONE]\n\n");
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // -- Verify output --
    // Lifecycle events.
    assert!(output_sse.contains("event: response.created"), "must emit response.created");
    assert!(output_sse.contains("event: response.output_item.added"), "must emit output_item.added");
    assert!(output_sse.contains("event: response.content_part.added"), "must emit content_part.added");

    // Delta events.
    assert!(output_sse.contains("event: response.output_text.delta"), "must emit output_text.delta");
    assert!(output_sse.contains("\"delta\":\"Hello!\""), "delta content must be Hello!");

    // Done events.
    assert!(output_sse.contains("event: response.output_text.done"), "must emit output_text.done");
    assert!(output_sse.contains("\"text\":\"Hello!\""), "accumulated text must be Hello!");
    assert!(output_sse.contains("event: response.content_part.done"), "must emit content_part.done");
    assert!(output_sse.contains("event: response.output_item.done"), "must emit output_item.done");

    // Terminal event.
    assert!(output_sse.contains("event: response.completed"), "must emit response.completed");
    assert!(output_sse.contains("data: [DONE]"), "must emit [DONE] terminal marker");

    // CRITICAL: NEVER response.incomplete.
    assert!(!output_sse.contains("response.incomplete"), "must NEVER emit response.incomplete");
    // CRITICAL: NEVER sequence_number.
    assert!(!output_sse.contains("sequence_number"), "must NEVER include sequence_number");
}

/// Verifies the response.completed object structure for a simple text response.
#[test]
fn response_completed_object_structure() {
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let _ = convert_request(&mut task, json!({
        "model": "claude-sonnet-4-20250514",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    })).unwrap();

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_struct","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_struct".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut completed_data: Option<serde_json::Value> = None;
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            let (event_type, data) = re.to_sse();
            if event_type == "response.completed" {
                completed_data = Some(serde_json::from_str(&data).unwrap());
            }
        }
    }

    let parsed = completed_data.expect("must have response.completed");
    let resp = &parsed["response"];

    // Required fields.
    assert_eq!(resp["id"], "msg_struct");
    assert_eq!(resp["object"], "response");
    assert_eq!(resp["status"], "completed");
    assert_eq!(resp["model"], "claude-sonnet-4-20250514");
    assert_eq!(resp["created_at"], 1717000000);
    assert!(resp.get("completed_at").is_some(), "completed_at must be present");

    // Usage.
    let usage = &resp["usage"];
    assert_eq!(usage["input_tokens"], 25);
    assert_eq!(usage["output_tokens"], 1);
    assert_eq!(usage["total_tokens"], 26);

    // Output items.
    let output = resp["output"].as_array().expect("output must be array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["role"], "assistant");
    assert_eq!(output[0]["status"], "completed");
    assert_eq!(output[0]["content"][0]["type"], "output_text");
    assert_eq!(output[0]["content"][0]["text"], "Hi");

    // No incomplete_details for end_turn.
    assert!(resp.get("incomplete_details").is_none());
}
