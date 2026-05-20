//! Integration test: tool call round-trip with namespace.
//!
//! Scenario: Codex sends a request with MCP namespace tools. Anthropic returns
//! a tool_use response. Verify the namespace is flattened on request and restored
//! on response, and that function_call streaming events are correct.

use codex_conv::conversion::ConversionTask;
use codex_conv::conversion::request::convert_request;
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

#[test]
fn tool_call_round_trip_with_namespace() {
    // -- Request with MCP namespace tools --
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "stream": true,
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__memory__",
                "tools": [
                    {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
                ]
            }
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Search memory"}]}
        ]
    });
    let anth_request = convert_request(&mut task, request).unwrap();

    // Verify namespace was flattened.
    let tools = anth_request["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "mcp__memory__search");
    assert_eq!(tools[0]["type"], "custom");
    assert!(tools[0]["input_schema"].is_object());

    // Verify messages are correct.
    let messages = anth_request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");

    // -- Simulated Anthropic response with tool_use --
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":50,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp__memory__search","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"test\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":30}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_tool".to_string(),
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

    // Verify namespace restoration in function_call output.
    assert!(
        output_sse.contains("\"name\":\"search\""),
        "tool name should be restored to 'search'"
    );
    assert!(
        output_sse.contains("\"namespace\":\"mcp__memory__\""),
        "namespace should be restored to 'mcp__memory__'"
    );

    // Verify streaming events.
    assert!(
        output_sse.contains("event: response.function_call_arguments.delta"),
        "must emit function_call_arguments.delta"
    );
    assert!(
        output_sse.contains("event: response.function_call_arguments.done"),
        "must emit function_call_arguments.done"
    );

    // Verify terminal event.
    assert!(output_sse.contains("event: response.completed"), "must emit response.completed");

    // When stop_reason is tool_use, end_turn must be false.
    assert!(
        output_sse.contains("\"end_turn\":false"),
        "end_turn must be false when stop_reason is tool_use"
    );

    // Accumulated arguments in done event.
    assert!(
        output_sse.contains("\"arguments\":\"{\\\"query\\\":\\\"test\\\"}\""),
        "accumulated arguments must be correct JSON string"
    );
}

/// Test tool call without namespace (plain function tool).
#[test]
fn tool_call_without_namespace() {
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "stream": true,
        "tools": [
            {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]}
        ]
    });
    let _ = convert_request(&mut task, request).unwrap();

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_plain","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":20,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_exec","name":"exec_command","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls -la\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_plain".to_string(),
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

    // No namespace for plain function tools.
    assert!(
        output_sse.contains("\"name\":\"exec_command\""),
        "plain tool name must be preserved"
    );
    // Should NOT contain namespace field for tools without one.
    assert!(
        !output_sse.contains("\"namespace\":"),
        "plain function tools should not have namespace field"
    );
}
