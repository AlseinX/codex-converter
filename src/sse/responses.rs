use serde_json::{json, Value};

/// Responses API SSE events.
///
/// CRITICAL CONSTRAINTS:
/// - NEVER include `sequence_number` in any event (Codex ignores it, adds wire overhead).
/// - NEVER emit `response.incomplete` -- always use `response.completed`.
/// - NEVER emit `custom_tool_call_input.*` events -- all tools are function type.
/// - `response.failed` must carry a FULL response object with error inside.
#[derive(Debug)]
pub enum ResponsesEvent {
    // -- Lifecycle events --
    ResponseCreated {
        response_id: String,
        created_at: u64,
        model: String,
    },
    ResponseInProgress {
        response_id: String,
        created_at: u64,
        model: String,
    },

    // -- Output item events --
    OutputItemAdded {
        output_index: usize,
        item: Value,
    },
    OutputItemDone {
        output_index: usize,
        item: Value,
    },

    // -- Content part events (for message output items) --
    ContentPartAdded {
        output_index: usize,
        content_index: usize,
        part: Value,
    },
    ContentPartDone {
        output_index: usize,
        content_index: usize,
        part: Value,
    },

    // -- Text delta events --
    OutputTextDelta {
        output_index: usize,
        content_index: usize,
        delta: String,
    },
    OutputTextDone {
        output_index: usize,
        content_index: usize,
        text: String,
    },

    // -- Reasoning summary events --
    ReasoningSummaryPartAdded {
        output_index: usize,
        item_id: String,
        summary_index: usize,
    },
    ReasoningSummaryTextDelta {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        delta: String,
    },
    ReasoningSummaryTextDone {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        text: String,
    },
    ReasoningSummaryPartDone {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        part: Value,
    },

    // -- Function call events --
    FunctionCallArgumentsDelta {
        output_index: usize,
        item_id: String,
        delta: String,
    },
    FunctionCallArgumentsDone {
        output_index: usize,
        item_id: String,
        arguments: String,
    },

    // -- Terminal events --
    ResponseCompleted {
        response: Value,
    },
    ResponseFailed {
        response: Value,
    },

    // -- Error event --
    Error {
        code: String,
        message: String,
    },

    // -- Terminal marker --
    Done,
}

impl ResponsesEvent {
    /// Convert to SSE wire format: (event_type, data_json_string).
    ///
    /// NO sequence_number is ever included.
    pub fn to_sse(&self) -> (String, String) {
        match self {
            ResponsesEvent::ResponseCreated {
                response_id,
                created_at,
                model,
            } => {
                let data = json!({
                    "type": "response.created",
                    "response": {
                        "id": response_id,
                        "object": "response",
                        "created_at": created_at,
                        "model": model,
                        "status": "in_progress",
                        "output": [],
                    }
                });
                ("response.created".to_string(), data.to_string())
            }

            ResponsesEvent::ResponseInProgress {
                response_id,
                created_at,
                model,
            } => {
                let data = json!({
                    "type": "response.in_progress",
                    "response": {
                        "id": response_id,
                        "object": "response",
                        "created_at": created_at,
                        "model": model,
                        "status": "in_progress",
                        "output": [],
                    }
                });
                ("response.in_progress".to_string(), data.to_string())
            }

            ResponsesEvent::OutputItemAdded { output_index, item } => {
                let data = json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": item,
                });
                ("response.output_item.added".to_string(), data.to_string())
            }

            ResponsesEvent::OutputItemDone { output_index, item } => {
                let data = json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                });
                ("response.output_item.done".to_string(), data.to_string())
            }

            ResponsesEvent::ContentPartAdded {
                output_index,
                content_index,
                part,
            } => {
                let data = json!({
                    "type": "response.content_part.added",
                    "output_index": output_index,
                    "content_index": content_index,
                    "part": part,
                });
                ("response.content_part.added".to_string(), data.to_string())
            }

            ResponsesEvent::ContentPartDone {
                output_index,
                content_index,
                part,
            } => {
                let data = json!({
                    "type": "response.content_part.done",
                    "output_index": output_index,
                    "content_index": content_index,
                    "part": part,
                });
                ("response.content_part.done".to_string(), data.to_string())
            }

            ResponsesEvent::OutputTextDelta {
                output_index,
                content_index,
                delta,
            } => {
                let data = json!({
                    "type": "response.output_text.delta",
                    "output_index": output_index,
                    "content_index": content_index,
                    "delta": delta,
                });
                ("response.output_text.delta".to_string(), data.to_string())
            }

            ResponsesEvent::OutputTextDone {
                output_index,
                content_index,
                text,
            } => {
                let data = json!({
                    "type": "response.output_text.done",
                    "output_index": output_index,
                    "content_index": content_index,
                    "text": text,
                });
                ("response.output_text.done".to_string(), data.to_string())
            }

            ResponsesEvent::ReasoningSummaryPartAdded {
                output_index,
                item_id,
                summary_index,
            } => {
                let data = json!({
                    "type": "response.reasoning_summary_part.added",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "part": {"type": "summary_text", "text": ""},
                });
                (
                    "response.reasoning_summary_part.added".to_string(),
                    data.to_string(),
                )
            }

            ResponsesEvent::ReasoningSummaryTextDelta {
                output_index,
                item_id,
                summary_index,
                delta,
            } => {
                let data = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "delta": delta,
                });
                (
                    "response.reasoning_summary_text.delta".to_string(),
                    data.to_string(),
                )
            }

            ResponsesEvent::ReasoningSummaryTextDone {
                output_index,
                item_id,
                summary_index,
                text,
            } => {
                let data = json!({
                    "type": "response.reasoning_summary_text.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "text": text,
                });
                (
                    "response.reasoning_summary_text.done".to_string(),
                    data.to_string(),
                )
            }

            ResponsesEvent::ReasoningSummaryPartDone {
                output_index,
                item_id,
                summary_index,
                part,
            } => {
                let data = json!({
                    "type": "response.reasoning_summary_part.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "part": part,
                });
                (
                    "response.reasoning_summary_part.done".to_string(),
                    data.to_string(),
                )
            }

            ResponsesEvent::FunctionCallArgumentsDelta {
                output_index,
                item_id,
                delta,
            } => {
                let data = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": output_index,
                    "item_id": item_id,
                    "delta": delta,
                });
                (
                    "response.function_call_arguments.delta".to_string(),
                    data.to_string(),
                )
            }

            ResponsesEvent::FunctionCallArgumentsDone {
                output_index,
                item_id,
                arguments,
            } => {
                let data = json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "arguments": arguments,
                });
                (
                    "response.function_call_arguments.done".to_string(),
                    data.to_string(),
                )
            }

            // CRITICAL: Always response.completed, NEVER response.incomplete.
            ResponsesEvent::ResponseCompleted { response } => {
                let data = json!({
                    "type": "response.completed",
                    "response": response,
                });
                ("response.completed".to_string(), data.to_string())
            }

            // CRITICAL: response.failed carries FULL response object with error inside.
            ResponsesEvent::ResponseFailed { response } => {
                let data = json!({
                    "type": "response.failed",
                    "response": response,
                });
                ("response.failed".to_string(), data.to_string())
            }

            ResponsesEvent::Error { code, message } => {
                let data = json!({
                    "type": "error",
                    "code": code,
                    "message": message,
                });
                ("error".to_string(), data.to_string())
            }

            // Done is a special marker -- to_sse returns the [DONE] data payload.
            // The caller should use format_done() for wire format, but this allows
            // it to be mixed into the event stream.
            ResponsesEvent::Done => ("done".to_string(), "[DONE]".to_string())
        }
    }
}

/// Format an SSE event as a wire-level string: `event: ...\ndata: ...\n\n`.
pub fn format_sse(event_type: &str, data: &str) -> String {
    format!("event: {}\ndata: {}\n\n", event_type, data)
}

/// Format the [DONE] terminal marker.
pub fn format_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// Format a ResponsesEvent into wire-level SSE string.
pub fn format_responses_event(event: &ResponsesEvent) -> String {
    let (event_type, data) = event.to_sse();
    format_sse(&event_type, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_created_event() {
        let evt = ResponsesEvent::ResponseCreated {
            response_id: "msg_01ABC".to_string(),
            created_at: 1717000000,
            model: "claude-sonnet-4-20250514".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.created");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "response.created");
        assert_eq!(parsed["response"]["id"], "msg_01ABC");
        assert_eq!(parsed["response"]["object"], "response");
        assert_eq!(parsed["response"]["created_at"], 1717000000);
        assert_eq!(parsed["response"]["model"], "claude-sonnet-4-20250514");
        assert_eq!(parsed["response"]["status"], "in_progress");
        assert!(
            parsed.get("sequence_number").is_none(),
            "NEVER include sequence_number"
        );
    }

    #[test]
    fn output_item_added_message() {
        let evt = ResponsesEvent::OutputItemAdded {
            output_index: 0,
            item: json!({"type": "message", "role": "assistant", "status": "in_progress", "content": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_item.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 0);
        assert_eq!(parsed["item"]["type"], "message");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn content_part_added() {
        let evt = ResponsesEvent::ContentPartAdded {
            output_index: 0,
            content_index: 0,
            part: json!({"type": "output_text", "text": "", "annotations": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.content_part.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 0);
        assert_eq!(parsed["content_index"], 0);
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn output_text_delta() {
        let evt = ResponsesEvent::OutputTextDelta {
            output_index: 0,
            content_index: 0,
            delta: "Hello".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_text.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "Hello");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn output_text_done() {
        let evt = ResponsesEvent::OutputTextDone {
            output_index: 0,
            content_index: 0,
            text: "Hello world".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_text.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "Hello world");
    }

    #[test]
    fn content_part_done() {
        let evt = ResponsesEvent::ContentPartDone {
            output_index: 0,
            content_index: 0,
            part: json!({"type": "output_text", "text": "Hello world", "annotations": []}),
        };
        let (event_type, _data) = evt.to_sse();
        assert_eq!(event_type, "response.content_part.done");
    }

    #[test]
    fn output_item_done() {
        let evt = ResponsesEvent::OutputItemDone {
            output_index: 0,
            item: json!({"type": "message", "role": "assistant", "status": "completed", "content": []}),
        };
        let (event_type, _data) = evt.to_sse();
        assert_eq!(event_type, "response.output_item.done");
    }

    #[test]
    fn reasoning_summary_part_added() {
        let evt = ResponsesEvent::ReasoningSummaryPartAdded {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_part.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 1);
        assert_eq!(parsed["item_id"], "rs_001");
        assert_eq!(parsed["summary_index"], 0);
        assert_eq!(parsed["part"]["type"], "summary_text");
    }

    #[test]
    fn reasoning_summary_text_delta() {
        let evt = ResponsesEvent::ReasoningSummaryTextDelta {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            delta: "Let me think...".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_text.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "Let me think...");
    }

    #[test]
    fn reasoning_summary_text_done() {
        let evt = ResponsesEvent::ReasoningSummaryTextDone {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            text: "Full summary text".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_text.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "Full summary text");
    }

    #[test]
    fn reasoning_summary_part_done() {
        let evt = ResponsesEvent::ReasoningSummaryPartDone {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            part: json!({"type": "summary_text", "text": "Full summary text"}),
        };
        let (event_type, _data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_part.done");
    }

    #[test]
    fn function_call_arguments_delta() {
        let evt = ResponsesEvent::FunctionCallArgumentsDelta {
            output_index: 2,
            item_id: "fc_002".to_string(),
            delta: "{\"key\":".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.function_call_arguments.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "{\"key\":");
    }

    #[test]
    fn function_call_arguments_done() {
        let evt = ResponsesEvent::FunctionCallArgumentsDone {
            output_index: 2,
            item_id: "fc_002".to_string(),
            arguments: "{\"key\":\"value\"}".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.function_call_arguments.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["arguments"], "{\"key\":\"value\"}");
    }

    #[test]
    fn response_completed_never_incomplete() {
        let response_obj = json!({
            "id": "msg_01",
            "object": "response",
            "status": "completed",
            "output": [],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        });
        let evt = ResponsesEvent::ResponseCompleted {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(
            event_type, "response.completed",
            "NEVER emit response.incomplete"
        );
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "response.completed");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn response_completed_with_incomplete_details() {
        let response_obj = json!({
            "id": "msg_01",
            "status": "completed",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
            "usage": {"input_tokens": 100, "output_tokens": 4096, "total_tokens": 4196}
        });
        let evt = ResponsesEvent::ResponseCompleted {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.completed");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(
            parsed["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn response_failed_carries_full_response() {
        let response_obj = json!({
            "id": "msg_01",
            "object": "response",
            "created_at": 1717000000,
            "status": "failed",
            "error": {"code": "rate_limit_exceeded", "message": "Too many requests"},
            "output": [],
            "usage": null,
            "metadata": {}
        });
        let evt = ResponsesEvent::ResponseFailed {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.failed");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "failed");
        assert_eq!(parsed["response"]["error"]["code"], "rate_limit_exceeded");
    }

    #[test]
    fn error_event() {
        let evt = ResponsesEvent::Error {
            code: "context_length_exceeded".to_string(),
            message: "prompt is too long".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "error");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["code"], "context_length_exceeded");
        assert_eq!(parsed["message"], "prompt is too long");
    }

    #[test]
    fn format_sse_line() {
        let line = format_sse(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
        );
        assert_eq!(
            line,
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n"
        );
    }

    #[test]
    fn format_done_line() {
        let line = format_done();
        assert_eq!(line, "data: [DONE]\n\n");
    }

    #[test]
    fn no_sequence_number_in_any_event() {
        // Verify that every ResponsesEvent variant omits sequence_number.
        let events = vec![
            ResponsesEvent::ResponseCreated {
                response_id: "msg_01".into(),
                created_at: 0,
                model: "m".into(),
            },
            ResponsesEvent::ResponseInProgress {
                response_id: "msg_01".into(),
                created_at: 0,
                model: "m".into(),
            },
            ResponsesEvent::OutputItemAdded {
                output_index: 0,
                item: json!({}),
            },
            ResponsesEvent::ContentPartAdded {
                output_index: 0,
                content_index: 0,
                part: json!({}),
            },
            ResponsesEvent::OutputTextDelta {
                output_index: 0,
                content_index: 0,
                delta: "x".into(),
            },
            ResponsesEvent::OutputTextDone {
                output_index: 0,
                content_index: 0,
                text: "x".into(),
            },
            ResponsesEvent::ContentPartDone {
                output_index: 0,
                content_index: 0,
                part: json!({}),
            },
            ResponsesEvent::OutputItemDone {
                output_index: 0,
                item: json!({}),
            },
            ResponsesEvent::ReasoningSummaryPartAdded {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
            },
            ResponsesEvent::ReasoningSummaryTextDelta {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                delta: "x".into(),
            },
            ResponsesEvent::ReasoningSummaryTextDone {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                text: "x".into(),
            },
            ResponsesEvent::ReasoningSummaryPartDone {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                part: json!({}),
            },
            ResponsesEvent::FunctionCallArgumentsDelta {
                output_index: 0,
                item_id: "fc_0".into(),
                delta: "x".into(),
            },
            ResponsesEvent::FunctionCallArgumentsDone {
                output_index: 0,
                item_id: "fc_0".into(),
                arguments: "x".into(),
            },
            ResponsesEvent::ResponseCompleted {
                response: json!({}),
            },
            ResponsesEvent::ResponseFailed {
                response: json!({}),
            },
            ResponsesEvent::Error {
                code: "x".into(),
                message: "x".into(),
            },
            ResponsesEvent::Done,
        ];
        for evt in &events {
            let (_, data) = evt.to_sse();
            // Done is not JSON -- skip parsing.
            if matches!(evt, ResponsesEvent::Done) {
                assert_eq!(data, "[DONE]");
                continue;
            }
            let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
            assert!(
                parsed.get("sequence_number").is_none(),
                "Found sequence_number in {:?}: {}",
                evt,
                parsed
            );
        }
    }
}
