use serde_json::Value;

/// Parsed Anthropic SSE event.
#[derive(Debug)]
pub enum AnthropicEvent {
    /// `message_start` — contains the initial message object.
    MessageStart { message: Value },
    /// `content_block_start` — a new content block begins.
    ContentBlockStart { index: usize, content_block: Value },
    /// `content_block_delta` — incremental content for current block.
    ContentBlockDelta { index: usize, delta: Value },
    /// `content_block_stop` — current content block ended.
    ContentBlockStop { index: usize },
    /// `message_delta` — stop_reason and output token usage.
    MessageDelta { delta: Value, usage: Value },
    /// `message_stop` — the message is complete.
    MessageStop,
    /// `error` — an error occurred during streaming.
    Error { error: Value },
    /// `ping` — keep-alive, consumed silently.
    Ping,
    /// `[DONE]` — terminal marker.
    Done,
    /// Unknown event type — preserved for forward compatibility.
    Unknown { event_type: String, data: Value },
}

/// Parse a raw SSE text stream into a list of AnthropicEvent.
///
/// Handles:
/// - `event: <type>\ndata: <json>\n` pairs
/// - `data: [DONE]` terminal marker
/// - Empty lines between events
/// - Malformed data lines (logged as warnings, returned as Unknown)
pub fn parse_sse_events(raw: &str) -> Vec<AnthropicEvent> {
    let mut events = Vec::new();
    let mut current_event_type: Option<String> = None;
    let mut current_data: Option<String> = None;

    for line in raw.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event_type = Some(ev.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data: ") {
            let data_str = data.trim();

            // Check for terminal [DONE] marker.
            if data_str == "[DONE]" {
                events.push(AnthropicEvent::Done);
                current_event_type = None;
                current_data = None;
                continue;
            }

            current_data = Some(data_str.to_string());
        } else if line.is_empty() {
            // Empty line = end of current event. Emit if we have data.
            if let Some(data_str) = current_data.take() {
                let event_type = current_event_type.take();
                if let Some(event) = build_event(event_type.as_deref(), &data_str) {
                    events.push(event);
                }
            }
        }
        // Other lines (comments, etc.) are ignored.
    }

    // Handle final event if stream doesn't end with empty line.
    if let Some(data_str) = current_data.take() {
        let event_type = current_event_type.take();
        if let Some(event) = build_event(event_type.as_deref(), &data_str) {
            events.push(event);
        }
    }

    events
}

/// Build an AnthropicEvent from the event type and data string.
fn build_event(event_type: Option<&str>, data: &str) -> Option<AnthropicEvent> {
    let parsed: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(data = %data, error = %e, "malformed SSE data, skipping");
            // Return a minimal Unknown so the caller can count events.
            return Some(AnthropicEvent::Unknown {
                event_type: event_type.unwrap_or("unknown").to_string(),
                data: Value::String(data.to_string()),
            });
        }
    };

    // Use the `type` field from the parsed JSON as the primary discriminator.
    // Fall back to the SSE event: line if the JSON lacks a type field.
    let type_str = parsed
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| event_type.unwrap_or("unknown"));

    match type_str {
        "message_start" => {
            let message = parsed.get("message").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::MessageStart { message })
        }
        "content_block_start" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let content_block = parsed.get("content_block").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            })
        }
        "content_block_delta" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = parsed.get("delta").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::ContentBlockDelta { index, delta })
        }
        "content_block_stop" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Some(AnthropicEvent::ContentBlockStop { index })
        }
        "message_delta" => {
            let delta = parsed.get("delta").cloned().unwrap_or(Value::Null);
            let usage = parsed.get("usage").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::MessageDelta { delta, usage })
        }
        "message_stop" => Some(AnthropicEvent::MessageStop),
        "error" => {
            let error = parsed.get("error").cloned().unwrap_or(parsed.clone());
            Some(AnthropicEvent::Error { error })
        }
        "ping" => Some(AnthropicEvent::Ping),
        _ => {
            tracing::debug!(event_type = type_str, "unknown Anthropic SSE event type");
            Some(AnthropicEvent::Unknown {
                event_type: type_str.to_string(),
                data: parsed,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_start() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01ABC","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0}}}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::MessageStart { message } => {
                assert_eq!(message["id"], "msg_01ABC");
            }
            _ => panic!("expected MessageStart, got {:?}", events[0]),
        }
    }

    #[test]
    fn parse_content_block_start_text() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(content_block["type"], "text");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_thinking() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 1);
                assert_eq!(content_block["type"], "thinking");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_tool_use() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"exec_command","input":{}}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 2);
                assert_eq!(content_block["type"], "tool_use");
                assert_eq!(content_block["id"], "toolu_01ABC");
                assert_eq!(content_block["name"], "exec_command");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_redacted_thinking() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"ENCRYPTED"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 1);
                assert_eq!(content_block["type"], "redacted_thinking");
                assert_eq!(content_block["data"], "ENCRYPTED");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_delta_text() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 0);
                assert_eq!(delta["type"], "text_delta");
                assert_eq!(delta["text"], "Hello");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_thinking() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Let me analyze..."}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 1);
                assert_eq!(delta["type"], "thinking_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_signature() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"ErUB..."}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 1);
                assert_eq!(delta["type"], "signature_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_input_json() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 2);
                assert_eq!(delta["type"], "input_json_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_stop() {
        let raw = r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStop { index } => {
                assert_eq!(*index, 0);
            }
            _ => panic!("expected ContentBlockStop"),
        }
    }

    #[test]
    fn parse_message_delta() {
        let raw = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta["stop_reason"], "end_turn");
                assert_eq!(usage["output_tokens"], 15);
            }
            _ => panic!("expected MessageDelta"),
        }
    }

    #[test]
    fn parse_message_delta_max_tokens() {
        let raw = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4096}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta["stop_reason"], "max_tokens");
            }
            _ => panic!("expected MessageDelta"),
        }
    }

    #[test]
    fn parse_message_stop() {
        let raw = r#"event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageStop => {}
            _ => panic!("expected MessageStop"),
        }
    }

    #[test]
    fn parse_error_event() {
        let raw = r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::Error { error } => {
                assert_eq!(error["type"], "overloaded_error");
                assert_eq!(error["message"], "Overloaded");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn parse_ping_consumed() {
        let raw = r#"event: ping
data: {"type":"ping"}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::Ping => {}
            _ => panic!("expected Ping"),
        }
    }

    #[test]
    fn parse_done_marker() {
        let raw = "data: [DONE]";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::Done => {}
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn parse_multiple_events() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

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
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 7);
        assert!(matches!(&events[0], AnthropicEvent::MessageStart { .. }));
        assert!(matches!(
            &events[1],
            AnthropicEvent::ContentBlockStart { .. }
        ));
        assert!(matches!(
            &events[2],
            AnthropicEvent::ContentBlockDelta { .. }
        ));
        assert!(matches!(
            &events[3],
            AnthropicEvent::ContentBlockStop { .. }
        ));
        assert!(matches!(&events[4], AnthropicEvent::MessageDelta { .. }));
        assert!(matches!(&events[5], AnthropicEvent::MessageStop));
        assert!(matches!(&events[6], AnthropicEvent::Done));
    }

    #[test]
    fn malformed_data_skipped_with_warning() {
        let raw = r#"event: message_start
data: {bad json}

event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        // The malformed event should produce an Unknown variant, message_stop still parsed.
        assert!(!events.is_empty());
        assert!(matches!(
            &events[events.len() - 1],
            AnthropicEvent::MessageStop
        ));
    }

    #[test]
    fn empty_lines_between_events_ok() {
        let raw = r#"event: ping
data: {"type":"ping"}

event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn unknown_event_type_becomes_unknown() {
        let raw = r#"event: future_event_type
data: {"type":"future_event_type","data":"something"}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::Unknown { event_type, data } => {
                assert_eq!(event_type, "future_event_type");
                assert_eq!(data["type"], "future_event_type");
            }
            _ => panic!("expected Unknown"),
        }
    }
}
