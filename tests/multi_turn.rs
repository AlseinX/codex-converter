//! Integration test: multi-turn conversation with signature cache.
//!
//! Scenario: Two-turn conversation where turn 1 produces a thinking block with
//! a signature. Turn 2 sends a follow-up request referencing the reasoning input
//! from turn 1. Verify the signature cache lookup works correctly and the
//! cached signature is included in the Anthropic request.

use codex_conv::conversion::{ConversionTask, SignatureCache};
use codex_conv::conversion::request::convert_request;
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
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

/// Helper to check if any content block of given type exists across messages.
fn has_content_block_of_type(messages: &[serde_json::Value], block_type: &str) -> bool {
    find_content_block_by_type(messages, block_type).is_some()
}

#[test]
fn multi_turn_with_thinking_and_tool_call() {
    let cache = Arc::new(SignatureCache::new(Duration::from_secs(10800)));

    // ========================================
    // Turn 1: User asks question -> model thinks + calls tool
    // ========================================
    let mut task1 = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request1 = json!({
        "model": "claude-sonnet-4-20250514",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": [
            {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What files are in the current directory?"}]}
        ]
    });
    let anth_req1 = convert_request(&mut task1, request1).unwrap();
    assert_eq!(anth_req1["thinking"]["type"], "adaptive");

    // Mock Anthropic response: thinking + tool_use.
    let anthropic_sse1 = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_turn1","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":30,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I should list the files."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG_TURN1_ABC"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_t1","name":"exec_command","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":50}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events1 = parse_sse_events(anthropic_sse1);
    let mut state1 = StreamingState::new(
        "msg_turn1".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task1.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output1 = String::new();
    let mut reasoning_id = String::new();
    let mut fc_call_id = String::new();
    let mut fc_arguments = String::new();

    for event in events1 {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        let responses_events = state1.process_event(event);
        for re in &responses_events {
            let (event_type, data) = re.to_sse();
            if event_type == "response.output_item.added" && data.contains("\"reasoning\"") {
                let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
                reasoning_id = parsed["item"]["id"].as_str().unwrap().to_string();
            }
            if event_type == "response.output_item.added" && data.contains("\"function_call\"") {
                let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
                fc_call_id = parsed["item"]["call_id"].as_str().unwrap().to_string();
            }
            if event_type == "response.function_call_arguments.done" {
                let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
                fc_arguments = parsed["arguments"].as_str().unwrap().to_string();
            }
            output1.push_str(&format_responses_event(re));
        }
    }

    // Verify turn 1 output.
    assert!(output1.contains("response.reasoning_summary_text.delta"));
    assert!(output1.contains("response.function_call_arguments.delta"));
    assert!(output1.contains("event: response.completed"));
    assert!(!output1.contains("SIG_TURN1_ABC"), "signature must not appear in SSE");

    // Cache signatures.
    for (id, sig) in state1.drain_signatures() {
        cache.insert(id, sig);
    }

    // Verify signature was cached.
    assert!(
        cache.get(&reasoning_id).is_some(),
        "signature for reasoning_id must be cached"
    );

    // ========================================
    // Turn 2: Follow-up with reasoning + tool result
    // ========================================
    let mut task2 = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request2 = json!({
        "model": "claude-sonnet-4-20250514",
        "reasoning": {"effort": "high", "summary": "auto"},
        "tools": [
            {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What files?"}]},
            {"type": "reasoning", "id": reasoning_id, "summary": [{"type": "summary_text", "text": "I should list the files."}]},
            {"type": "function_call", "call_id": fc_call_id, "name": "exec_command", "arguments": fc_arguments},
            {"type": "function_call_output", "call_id": fc_call_id, "output": "file1.txt\nfile2.txt\nfile3.txt"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Thanks, what about the largest file?"}]}
        ]
    });
    let anth_req2 = convert_request(&mut task2, request2).unwrap();

    // Verify the cached signature is used in the thinking block.
    let messages2 = anth_req2["messages"].as_array().unwrap();
    let thinking_block = find_content_block_by_type(messages2, "thinking")
        .expect("turn 2 should have a thinking block");

    assert_eq!(
        thinking_block["signature"], "SIG_TURN1_ABC",
        "cached signature from turn 1 must be included in turn 2 request"
    );

    // Verify the tool_use and tool_result are also in the messages.
    let has_tool_use = has_content_block_of_type(messages2, "tool_use");
    assert!(has_tool_use, "turn 2 must include tool_use from turn 1");

    let has_tool_result = has_content_block_of_type(messages2, "tool_result");
    assert!(has_tool_result, "turn 2 must include tool_result from turn 1");

    // Simulate turn 2 Anthropic response (simple text).
    let anthropic_sse2 = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_turn2","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":200,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The largest file is file2.txt"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events2 = parse_sse_events(anthropic_sse2);
    let mut state2 = StreamingState::new(
        "msg_turn2".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task2.namespace_registry.clone(),
        1717000001,
        "auto".to_string(),
        None,
        None,
    );

    let mut output2 = String::new();
    for event in events2 {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        let responses_events = state2.process_event(event);
        for re in &responses_events {
            output2.push_str(&format_responses_event(re));
        }
    }

    assert!(output2.contains("event: response.completed"));
    assert!(output2.contains("The largest file is file2.txt"));
}

/// Test multi-turn where turn 2 has encrypted_content instead of cached signature.
#[test]
fn multi_turn_with_encrypted_content() {
    let cache = Arc::new(SignatureCache::new(Duration::from_secs(10800)));

    // Turn 1: just convert a request with encrypted_content reasoning input.
    let mut task = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "id": "rs_encrypted", "summary": [], "encrypted_content": "OPAQUE_TURN1_DATA"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "response"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "follow up"}]}
        ]
    });
    let result = convert_request(&mut task, request).unwrap();
    let messages = result["messages"].as_array().unwrap();

    // The reasoning with encrypted_content must produce redacted_thinking.
    let has_redacted = find_content_block_by_type(messages, "redacted_thinking")
        .map(|b| b["data"] == "OPAQUE_TURN1_DATA")
        .unwrap_or(false);
    assert!(has_redacted, "encrypted_content must produce redacted_thinking block");

    // Message alternation must be correct: user, assistant(redacted_thinking+text), user.
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
}
