use serde_json::{json, Value};

/// Context overflow keyword detection heuristic.
/// Anthropic uses `invalid_request_error` for both context overflow and other invalid requests.
/// These keywords distinguish them.
const CONTEXT_OVERFLOW_KEYWORDS: &[&str] = &[
    "context window",
    "context length",
    "too many tokens",
    "prompt is too long",
];

/// Convert an Anthropic error type and message to a Responses API error code and HTTP status.
///
/// Returns (error_code, http_status).
///
/// CRITICAL mappings:
/// - `overloaded_error` → `server_error`
/// - `billing_error` → `insufficient_quota`
/// - `request_too_large` → `request_too_large`
/// - Context overflow detected by keyword heuristic on message text
pub fn convert_error_type(error_type: &str, message: &str, http_status: Option<u16>) -> (&'static str, u16) {
    match error_type {
        "invalid_request_error" => {
            // Check for context overflow via keyword heuristic.
            if is_context_overflow(message) {
                ("context_length_exceeded", 400)
            } else {
                ("invalid_request", 400)
            }
        }
        "authentication_error" => ("invalid_api_key", 401),
        "permission_error" => ("invalid_api_key", 403),
        "not_found_error" => ("model_not_found", 404),
        "request_too_large" => ("request_too_large", 413),
        "rate_limit_error" => ("rate_limit_exceeded", 429),
        "billing_error" => ("insufficient_quota", 402),
        "overloaded_error" => ("server_error", 503),
        "api_error" => ("server_error", 500),
        _ => {
            // Unknown error type — classify by HTTP status family.
            match http_status {
                Some(s) if (400..=499).contains(&s) => ("invalid_request", s),
                Some(s) if (500..=599).contains(&s) => ("server_error", s),
                _ => ("server_error", 500),
            }
        }
    }
}

/// Detect context overflow from Anthropic error message using keyword heuristic.
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    CONTEXT_OVERFLOW_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Map an Anthropic HTTP status code to Responses API status code.
///
/// Special mappings:
/// - 529 → 503 (overloaded)
/// - Unknown 4XX → 400
/// - Unknown 5XX → 500
pub fn map_http_status(status: u16) -> u16 {
    match status {
        400 | 401 | 402 | 403 | 404 | 429 | 500 => status,
        413 => 400,
        529 => 503,
        400..=499 => 400, // Unknown 4XX → 400
        500..=599 => 500, // Unknown 5XX → 500
        _ => 500,
    }
}

/// Map an Anthropic error type to a Responses API error type.
///
/// Mapping rules:
/// - `authentication_error` → `invalid_request_error`
/// - `permission_error` → `invalid_request_error`
/// - `not_found_error` → `invalid_request_error`
/// - `rate_limit_error` → `rate_limit_error`
/// - `overloaded_error` → `server_error`
/// - `billing_error` → `invalid_request_error`
/// - `api_error` → `api_error`
/// - `invalid_request_error` → `invalid_request_error`
/// - `request_too_large` → `invalid_request_error`
/// - Others → depends on HTTP status family
pub fn map_error_type(error_type: &str, http_status: Option<u16>) -> &'static str {
    match error_type {
        "invalid_request_error" | "authentication_error" | "permission_error"
        | "not_found_error" | "billing_error" | "request_too_large" => "invalid_request_error",
        "rate_limit_error" => "rate_limit_error",
        "overloaded_error" => "server_error",
        "api_error" => "api_error",
        _ => {
            match http_status {
                Some(s) if (400..=499).contains(&s) => "invalid_request_error",
                _ => "server_error",
            }
        }
    }
}

/// Convert a non-streaming Anthropic error body to Responses API error format.
///
/// Anthropic format:
/// ```json
/// {"type": "error", "error": {"type": "...", "message": "..."}, "request_id": "..."}
/// ```
///
/// Responses API format:
/// ```json
/// {"error": {"message": "...", "type": "...", "param": null, "code": "..."}}
/// ```
pub fn convert_non_streaming_error(body: &Value) -> (u16, Value) {
    let error_obj = body.get("error").cloned().unwrap_or(json!({}));
    let error_type = error_obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("api_error");
    let message = error_obj
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error");

    let (code, http_status) = convert_error_type(error_type, message, None);

    // Map Anthropic error type to Responses API error type.
    let mapped_type = map_error_type(error_type, None);

    // Log request_id for debugging (not returned in response).
    if let Some(req_id) = body.get("request_id").and_then(|v| v.as_str()) {
        tracing::debug!(request_id = req_id, "Anthropic error request_id");
    }

    let responses_body = json!({
        "error": {
            "message": message,
            "type": mapped_type,
            "param": Value::Null,
            "code": code,
        }
    });

    (http_status, responses_body)
}

/// Build a proxy-originated error response body.
pub fn proxy_error_response(_http_status: u16, code: &str, message: &str) -> Value {
    json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": code,
        }
    })
}

/// Build a streaming error `event: error` SSE data payload.
///
/// Wire format:
/// ```json
/// {"type": "error", "code": "<code>", "message": "<message>"}
/// ```
pub fn streaming_error_event(code: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "code": code,
        "message": message,
    })
}

/// Build a streaming `response.failed` event with accumulated output items.
///
/// The response object has `status: "failed"` and includes any output items
/// that were accumulated before the error occurred.
pub fn streaming_failed_response(
    response_id: &str,
    model: &str,
    accumulated_output: &[Value],
    error_code: &str,
    error_message: &str,
) -> Value {
    json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": chrono_offset_now_secs(),
            "completed_at": chrono_offset_now_secs(),
            "model": model,
            "status": "failed",
            "error": {
                "code": error_code,
                "message": error_message,
            },
            "output": accumulated_output,
            "usage": Value::Null,
            "metadata": Value::Null,
        }
    })
}

/// Return current Unix timestamp in seconds.
fn chrono_offset_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_request_maps_to_invalid_request() {
        let (code, status) = convert_error_type("invalid_request_error", "bad parameter", None);
        assert_eq!(code, "invalid_request");
        assert_eq!(status, 400);
    }

    #[test]
    fn invalid_request_context_overflow_maps_to_context_length() {
        let (code, status) = convert_error_type(
            "invalid_request_error",
            "prompt is too long: 210000 tokens > context window 200000",
            None,
        );
        assert_eq!(code, "context_length_exceeded");
        assert_eq!(status, 400);
    }

    #[test]
    fn invalid_request_too_many_tokens_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "too many tokens: 300000 > 200000", None);
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn invalid_request_context_length_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "exceeds context length", None);
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn invalid_request_context_window_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "exceeds the context window", None);
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn authentication_error_maps_to_invalid_api_key() {
        let (code, status) = convert_error_type("authentication_error", "invalid x-api-key", None);
        assert_eq!(code, "invalid_api_key");
        assert_eq!(status, 401);
    }

    #[test]
    fn permission_error_maps_to_invalid_api_key() {
        let (code, status) = convert_error_type("permission_error", "forbidden", None);
        assert_eq!(code, "invalid_api_key");
        assert_eq!(status, 403);
    }

    #[test]
    fn not_found_error_maps_to_model_not_found() {
        let (code, status) = convert_error_type("not_found_error", "model not found", None);
        assert_eq!(code, "model_not_found");
        assert_eq!(status, 404);
    }

    #[test]
    fn request_too_large_maps_to_request_too_large() {
        let (code, status) = convert_error_type("request_too_large", "request too large", None);
        assert_eq!(code, "request_too_large");
        assert_eq!(status, 413);
    }

    #[test]
    fn rate_limit_error_maps_to_rate_limit() {
        let (code, status) = convert_error_type("rate_limit_error", "too many requests", None);
        assert_eq!(code, "rate_limit_exceeded");
        assert_eq!(status, 429);
    }

    #[test]
    fn billing_error_maps_to_insufficient_quota() {
        let (code, status) = convert_error_type("billing_error", "insufficient funds", None);
        assert_eq!(code, "insufficient_quota");
        assert_eq!(status, 402);
    }

    #[test]
    fn overloaded_error_maps_to_server_error() {
        let (code, status) = convert_error_type("overloaded_error", "overloaded", None);
        assert_eq!(code, "server_error");
        assert_eq!(status, 503, "overloaded_error must map to 503");
    }

    #[test]
    fn api_error_maps_to_server_error() {
        let (code, status) = convert_error_type("api_error", "internal error", None);
        assert_eq!(code, "server_error");
        assert_eq!(status, 500);
    }

    #[test]
    fn unknown_error_type_maps_to_server_error() {
        let (code, status) = convert_error_type("some_unknown_error", "something broke", None);
        assert_eq!(code, "server_error");
        assert_eq!(status, 500);
    }

    #[test]
    fn unknown_error_type_with_4xx_status() {
        let (code, status) = convert_error_type("some_unknown_error", "something broke", Some(418));
        assert_eq!(code, "invalid_request");
        assert_eq!(status, 418);
    }

    #[test]
    fn unknown_error_type_with_5xx_status() {
        let (code, status) = convert_error_type("some_unknown_error", "something broke", Some(599));
        assert_eq!(code, "server_error");
        assert_eq!(status, 599);
    }

    #[test]
    fn unknown_4xx_maps_to_400() {
        let status = map_http_status(418);
        assert_eq!(status, 400);
    }

    #[test]
    fn unknown_5xx_maps_to_500() {
        let status = map_http_status(599);
        assert_eq!(status, 500);
    }

    #[test]
    fn http_529_maps_to_503() {
        let status = map_http_status(529);
        assert_eq!(status, 503);
    }

    #[test]
    fn known_http_statuses_passthrough() {
        assert_eq!(map_http_status(400), 400);
        assert_eq!(map_http_status(401), 401);
        assert_eq!(map_http_status(402), 402);
        assert_eq!(map_http_status(403), 403);
        assert_eq!(map_http_status(404), 404);
        assert_eq!(map_http_status(413), 400, "413 must map to 400");
        assert_eq!(map_http_status(429), 429);
        assert_eq!(map_http_status(500), 500);
    }

    #[test]
    fn non_streaming_error_body_conversion() {
        let anthropic_body = json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": "Rate limit reached"
            },
            "request_id": "req_123"
        });
        let (status, responses_body) = convert_non_streaming_error(&anthropic_body);
        assert_eq!(status, 429);
        assert_eq!(responses_body["error"]["code"], "rate_limit_exceeded");
        assert_eq!(responses_body["error"]["message"], "Rate limit reached");
        assert_eq!(responses_body["error"]["type"], "rate_limit_error");
        assert_eq!(responses_body["error"]["param"], Value::Null);
        assert!(
            responses_body.get("request_id").is_none(),
            "request_id should not be in response"
        );
    }

    #[test]
    fn proxy_error_body_format() {
        let body = proxy_error_response(400, "invalid_request", "Bad request from proxy");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["message"], "Bad request from proxy");
        assert_eq!(body["error"]["param"], Value::Null);
    }

    #[test]
    fn context_overflow_keywords_case_insensitive() {
        // All keywords should match regardless of case
        assert!(is_context_overflow("CONTEXT WINDOW exceeded"));
        assert!(is_context_overflow("Context Length is too big"));
        assert!(is_context_overflow("TOO MANY TOKENS in prompt"));
        assert!(is_context_overflow("Your PROMPT IS TOO LONG"));
    }

    #[test]
    fn non_overflow_message_not_detected() {
        assert!(!is_context_overflow("invalid parameter value"));
        assert!(!is_context_overflow("model not available"));
        assert!(!is_context_overflow("rate limit exceeded"));
    }

    #[test]
    fn non_streaming_error_with_missing_error_field() {
        let body = json!({"type": "error"});
        let (status, responses_body) = convert_non_streaming_error(&body);
        assert_eq!(status, 500); // Falls through to api_error default
        assert_eq!(responses_body["error"]["code"], "server_error");
        assert_eq!(responses_body["error"]["message"], "Unknown error");
    }

    #[test]
    fn non_streaming_error_context_overflow_detection() {
        let body = json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "prompt is too long: exceeds context window"
            }
        });
        let (status, responses_body) = convert_non_streaming_error(&body);
        assert_eq!(status, 400);
        assert_eq!(responses_body["error"]["code"], "context_length_exceeded");
    }

    #[test]
    fn streaming_error_event_format() {
        let event = streaming_error_event("rate_limit_exceeded", "Rate limited");
        assert_eq!(event["type"], "error");
        assert_eq!(event["code"], "rate_limit_exceeded");
        assert_eq!(event["message"], "Rate limited");
    }

    #[test]
    fn streaming_failed_response_structure() {
        let output = json!([
            {"type": "message", "id": "msg_01", "role": "assistant", "content": "partial"}
        ]);
        let output_arr: &[Value] = output.as_array().unwrap();
        let response = streaming_failed_response(
            "resp_test123",
            "claude-sonnet-4-20250514",
            output_arr,
            "server_error",
            "Internal server error",
        );
        assert_eq!(response["type"], "response.failed");
        let resp = &response["response"];
        assert_eq!(resp["id"], "resp_test123");
        assert_eq!(resp["object"], "response");
        assert!(resp["created_at"].as_u64().unwrap() > 0);
        assert!(resp["completed_at"].as_u64().unwrap() > 0);
        assert_eq!(resp["model"], "claude-sonnet-4-20250514");
        assert_eq!(resp["status"], "failed");
        assert_eq!(resp["error"]["code"], "server_error");
        assert_eq!(resp["error"]["message"], "Internal server error");
        assert_eq!(resp["output"], output);
        assert_eq!(resp["usage"], Value::Null);
        assert_eq!(resp["metadata"], Value::Null);
    }

    #[test]
    fn streaming_failed_response_empty_output() {
        let response = streaming_failed_response(
            "resp_empty",
            "claude-sonnet-4-20250514",
            &[],
            "invalid_api_key",
            "Bad key",
        );
        assert_eq!(response["response"]["output"], json!([]));
    }

    #[test]
    fn proxy_error_missing_auth() {
        let body = proxy_error_response(401, "invalid_api_key", "Missing Authorization header");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "Missing Authorization header");
    }

    #[test]
    fn proxy_error_upstream_failure() {
        let body = proxy_error_response(502, "server_error", "Upstream connection failed");
        assert_eq!(body["error"]["code"], "server_error");
        assert_eq!(body["error"]["message"], "Upstream connection failed");
    }
}
