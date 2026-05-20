//! Integration test: error handling scenarios.
//!
//! Tests that Anthropic streaming errors are correctly converted to
//! Responses API error events with the right error codes.

use codex_conv::conversion::response::StreamingState;
use codex_conv::conversion::namespace::NamespaceRegistry;
use codex_conv::sse::anthropic::AnthropicEvent;
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

fn make_state() -> StreamingState {
    StreamingState::new(
        "msg_err".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    )
}

/// Helper: process an error event and return the SSE output.
fn run_error_test(error_json: serde_json::Value) -> String {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_err", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: error_json,
    });

    let mut output_sse = String::new();
    for re in &events {
        output_sse.push_str(&format_responses_event(re));
    }
    output_sse
}

#[test]
fn streaming_rate_limit_error() {
    let output = run_error_test(json!({"type": "rate_limit_error", "message": "Too many requests"}));

    // Verify: error event + response.failed.
    assert!(output.contains("event: error"), "must emit error event");
    assert!(output.contains("\"code\":\"rate_limit_exceeded\""), "code must be rate_limit_exceeded");
    assert!(output.contains("\"message\":\"Too many requests\""), "message must passthrough");
    assert!(output.contains("event: response.failed"), "must emit response.failed");
    assert!(output.contains("\"status\":\"failed\""), "status must be failed");
}

#[test]
fn streaming_context_overflow_error() {
    let output = run_error_test(json!({
        "type": "invalid_request_error",
        "message": "prompt is too long: 210000 tokens > context window 200000"
    }));

    assert!(
        output.contains("\"code\":\"context_length_exceeded\""),
        "context overflow should map to context_length_exceeded"
    );
    assert!(output.contains("event: response.failed"), "must emit response.failed");
}

#[test]
fn streaming_generic_invalid_request_error() {
    let output = run_error_test(json!({
        "type": "invalid_request_error",
        "message": "bad parameter value"
    }));

    assert!(
        output.contains("\"code\":\"invalid_request\""),
        "non-context invalid_request should map to invalid_request"
    );
    assert!(output.contains("event: response.failed"));
}

#[test]
fn streaming_server_error() {
    let output = run_error_test(json!({
        "type": "api_error",
        "message": "Internal server error"
    }));

    assert!(
        output.contains("\"code\":\"server_error\""),
        "api_error must map to server_error"
    );
    assert!(output.contains("event: response.failed"));
}

#[test]
fn streaming_overloaded_error() {
    let output = run_error_test(json!({
        "type": "overloaded_error",
        "message": "Overloaded"
    }));

    assert!(
        output.contains("\"code\":\"server_is_overloaded\""),
        "overloaded_error must map to server_is_overloaded, NOT server_error"
    );
    assert!(
        !output.contains("\"code\":\"server_error\""),
        "should NOT be generic server_error"
    );
}

#[test]
fn streaming_authentication_error() {
    let output = run_error_test(json!({
        "type": "authentication_error",
        "message": "Invalid API key"
    }));

    assert!(
        output.contains("\"code\":\"invalid_api_key\""),
        "authentication_error must map to invalid_api_key"
    );
}

#[test]
fn streaming_permission_error() {
    let output = run_error_test(json!({
        "type": "permission_error",
        "message": "Access denied"
    }));

    assert!(
        output.contains("\"code\":\"invalid_api_key\""),
        "permission_error must map to invalid_api_key"
    );
}

#[test]
fn streaming_not_found_error() {
    let output = run_error_test(json!({
        "type": "not_found_error",
        "message": "Model not found"
    }));

    assert!(
        output.contains("\"code\":\"model_not_found\""),
        "not_found_error must map to model_not_found"
    );
}

#[test]
fn streaming_billing_error() {
    let output = run_error_test(json!({
        "type": "billing_error",
        "message": "Insufficient funds"
    }));

    assert!(
        output.contains("\"code\":\"insufficient_quota\""),
        "billing_error must map to insufficient_quota"
    );
}

#[test]
fn streaming_request_too_large_error() {
    let output = run_error_test(json!({
        "type": "request_too_large",
        "message": "Request body too large"
    }));

    assert!(
        output.contains("\"code\":\"context_length_exceeded\""),
        "request_too_large must map to context_length_exceeded"
    );
}

#[test]
fn error_event_then_failed_then_done_sequence() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_seq", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: json!({"type": "api_error", "message": "Internal server error"}),
    });

    // Must produce exactly 3 events: error + response.failed + Done.
    assert_eq!(events.len(), 3, "error must produce exactly error + response.failed + Done");

    let (t0, _) = events[0].to_sse();
    let (t1, _) = events[1].to_sse();
    assert_eq!(t0, "error", "first event must be error");
    assert_eq!(t1, "response.failed", "second event must be response.failed");
    // Third event is the Done terminal marker.
    assert!(matches!(events[2], codex_conv::sse::responses::ResponsesEvent::Done));
}
