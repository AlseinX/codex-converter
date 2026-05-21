use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use futures::StreamExt;
use tokio::sync::mpsc;

/// Parsed route information extracted from the request URL.
#[derive(Debug, Clone)]
pub struct RouteInfo {
    /// The upstream base URL (e.g., "https://api.anthropic.com").
    /// No trailing slash, no /v1 prefix.
    pub upstream_base_url: String,
}

impl RouteInfo {
    /// Parse the request path to extract the upstream base URL.
    ///
    /// Accepted formats (all normalized to `https://host`):
    /// - `/https/api.anthropic.com/responses`
    /// - `/https:/api.anthropic.com/responses`
    /// - `/https://api.anthropic.com/responses`
    /// - `/v1/https/api.anthropic.com/responses`
    ///
    /// Returns None if the path does not end in `/responses` or the host is missing.
    pub fn parse(path: &str) -> Option<Self> {
        // Must end with /responses.
        if !path.ends_with("/responses") {
            return None;
        }

        // Strip trailing /responses.
        let prefix = &path[..path.len() - "/responses".len()];

        // Strip optional /v1 prefix.
        let prefix = prefix.strip_prefix("/v1").unwrap_or(prefix);

        // Must start with /https.
        let after_https = prefix.strip_prefix("/https")?;

        // Normalize: strip optional :// or :/ or just /.
        let host_part = if after_https.starts_with("://") {
            &after_https[3..]
        } else if after_https.starts_with(":/") {
            &after_https[2..]
        } else if after_https.starts_with('/') {
            &after_https[1..]
        } else {
            return None;
        };

        if host_part.is_empty() {
            return None;
        }

        Some(RouteInfo {
            upstream_base_url: format!("https://{}", host_part),
        })
    }

    /// Build the full upstream URL for the Anthropic Messages API.
    pub fn upstream_messages_url(&self) -> String {
        format!("{}/v1/messages", self.upstream_base_url)
    }
}

/// Extract the API key from the Authorization header.
///
/// Accepts both `Bearer <key>` (case-insensitive prefix) and raw `<key>`.
/// Returns `None` if the header is missing or the extracted key is empty.
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let key = if let Some(stripped) = value.strip_prefix("Bearer ") {
        stripped
    } else if let Some(stripped) = value.strip_prefix("bearer ") {
        stripped
    } else {
        value
    };
    let trimmed = key.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: crate::config::AppConfig,
}

/// Build the main axum router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Catch-all route that accepts any path ending in /responses.
        .route("/{*path}", post(handle_responses))
        .with_state(state)
}

/// Format error message for invalid apply_patch format.
fn format_apply_patch_error() -> &'static str {
    concat!(
        "Error: apply_patch format not accepted. The patch MUST be in Codex freeform format.\n\n",
        "Required format:\n",
        "*** Begin Patch\n",
        "*** Update File: <filepath>\n",
        "-<line to remove>\n",
        "+<line to add>\n",
        "*** End Patch\n\n",
        "Or for new files:\n",
        "*** Begin Patch\n",
        "*** Add File: <filepath>\n",
        "+<file content>\n",
        "*** End Patch\n\n",
        "Or for deleted files:\n",
        "*** Begin Patch\n",
        "*** Delete File: <filepath>\n",
        "*** End Patch\n\n",
        "The FIRST line MUST be exactly '*** Begin Patch'. ",
        "Do NOT use unified diff format (--- a/, +++ b/, @@ @@)."
    )
}

/// Build the retry request body with captured content blocks and error message.
fn build_retry_body(
    original_body: &serde_json::Value,
    captured_blocks: Vec<serde_json::Value>,
    toolu_id: &str,
    error_message: &str,
) -> serde_json::Value {
    let mut retry_body = original_body.clone();
    let messages = retry_body
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .expect("retry body must have messages array");

    // Append assistant message with all captured content blocks
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": captured_blocks,
    }));

    // Append user message with tool_result error
    messages.push(serde_json::json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": toolu_id,
            "is_error": true,
            "content": error_message,
        }]
    }));

    retry_body
}

/// Build a reqwest client for upstream connections using the TLS/proxy config.
fn build_upstream_client(
    config: &crate::config::UpstreamConfig,
) -> Result<reqwest::Client, Box<dyn std::error::Error + Send + Sync>> {
    let builder = crate::tls::build_upstream_tls_config(&config.tls, &config.proxy)?;
    builder.build().map_err(|e| e.into())
}

/// Main handler for Responses API requests.
async fn handle_responses(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, (StatusCode, axum::Json<serde_json::Value>)> {
    // Extract API key from Authorization header.
    let api_key = extract_api_key(req.headers()).ok_or_else(|| {
        tracing::warn!("missing or empty Authorization header");
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": {
                    "message": "Missing or invalid Authorization header",
                    "type": "authentication_error",
                    "param": null,
                    "code": "invalid_api_key"
                }
            })),
        )
    })?;

    let path = req.uri().path().to_string();
    let route_info = RouteInfo::parse(&path).ok_or_else(|| {
        tracing::warn!(path = %path, "invalid route path");
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "message": format!("Invalid route path: {}", path),
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_request"
                }
            })),
        )
    })?;

    tracing::debug!(
        path = %path,
        upstream = %route_info.upstream_base_url,
        "routed request"
    );

    // -- Step 1: Read request body as bytes --
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to read request body");
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": {
                        "message": "Failed to read request body",
                        "type": "invalid_request_error",
                        "param": null,
                        "code": "invalid_request"
                    }
                })),
            )
        })?;

    // -- Step 2: Parse body as JSON --
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        tracing::error!(error = %e, "failed to parse request body as JSON");
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "message": format!("Invalid JSON in request body: {}", e),
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_request"
                }
            })),
        )
    })?;

    // -- Step 3: Extract echo-back fields before conversion --
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool_choice_echo = body_json
        .get("tool_choice")
        .map(|v| {
            if v.is_string() {
                v.as_str().unwrap_or("auto").to_string()
            } else {
                v.to_string()
            }
        })
        .unwrap_or_else(|| "auto".to_string());
    let instructions_echo = body_json
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let parallel_tool_calls_echo = body_json.get("parallel_tool_calls").and_then(|v| v.as_bool());

    // -- Step 4: Create ConversionTask and convert request --
    let mut task =
        crate::conversion::ConversionTask::new(route_info.upstream_base_url.clone());
    let anthropic_body =
        crate::conversion::request::convert_request(&mut task, body_json).map_err(|e| {
            tracing::error!(error = %e, "request conversion failed");
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": {
                        "message": format!("Request conversion error: {}", e),
                        "type": "invalid_request_error",
                        "param": null,
                        "code": "invalid_request"
                    }
                })),
            )
        })?;

    // -- Step 5: Build reqwest client --
    let client = build_upstream_client(&state.config.upstream).map_err(|e| {
        tracing::error!(error = %e, "failed to build upstream HTTP client");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": {
                    "message": "Internal proxy error: failed to build upstream client",
                    "type": "server_error",
                    "param": null,
                    "code": "server_error"
                }
            })),
        )
    })?;

    // -- Step 6: Build upstream request --
    let upstream_url = task.upstream_messages_url();
    let anthropic_version = state.config.upstream.anthropic_version.clone();

    // -- Step 7-9: Stream SSE events from upstream, convert, and forward downstream --

    tracing::debug!(
        url = %upstream_url,
        version = %anthropic_version,
        "forwarding request to upstream"
    );

    // Clone values needed for retry before moving into the spawn closure.
    let retry_client = client.clone();
    let retry_api_key = api_key.clone();
    let retry_anthropic_version = anthropic_version.clone();
    let retry_upstream_url = upstream_url.clone();
    let retry_anthropic_body = anthropic_body.clone();

    let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);

    // Namespace registry is moved into the streaming task.
    let namespace_registry =
        std::sync::Arc::new(std::sync::Mutex::new(task.namespace_registry));
    let signature_cache = task.signature_cache.clone();

    // Spawn a task to consume the upstream SSE stream, with retry support for
    // invalid apply_patch format.
    tokio::spawn(async move {
        use crate::conversion::response::StreamingState;
        use crate::sse::anthropic::{parse_sse_events, AnthropicEvent};
        use crate::sse::responses::{format_done, format_responses_event, ResponsesEvent};

        let mut current_body = retry_anthropic_body.clone();
        let mut streaming_state: Option<StreamingState> = None;
        let mut done_sent = false;

        'retry_loop: loop {
            // Create request_builder INSIDE the loop (fresh each retry).
            let request_builder = retry_client
                .post(&retry_upstream_url)
                .header("x-api-key", &retry_api_key)
                .header("anthropic-version", &retry_anthropic_version)
                .header("content-type", "application/json")
                .json(&current_body);

            let mut event_source = match reqwest_eventsource::EventSource::new(request_builder) {
                Ok(es) => es,
                Err(e) => {
                    tracing::error!(error = %e, "failed to create EventSource");
                    let _ = tx.send(Ok(axum::body::Bytes::from(
                        "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Failed to connect to upstream\"}\n\n",
                    ))).await;
                    let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
                    return;
                }
            };

            // Inner SSE streaming loop.
            while let Some(event_result) = event_source.next().await {
                match event_result {
                    Ok(reqwest_eventsource::Event::Message(msg)) => {
                        let raw_chunk = format!("event: {}\ndata: {}\n\n", msg.event, msg.data);
                        let anthropic_events = parse_sse_events(&raw_chunk);

                        for a_event in anthropic_events {
                            // Create StreamingState lazily on first message_start.
                            if streaming_state.is_none() {
                                if let AnthropicEvent::MessageStart { ref message } = a_event {
                                    let response_id = message
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("msg_unknown")
                                        .to_string();
                                    let ns_reg = {
                                        let guard = namespace_registry.lock().unwrap();
                                        guard.clone()
                                    };
                                    streaming_state = Some(StreamingState::new(
                                        response_id,
                                        model.clone(),
                                        ns_reg,
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs(),
                                        tool_choice_echo.clone(),
                                        instructions_echo.clone(),
                                        parallel_tool_calls_echo,
                                    ));
                                } else {
                                    continue;
                                }
                            }

                            let state = match streaming_state.as_mut() {
                                Some(s) => s,
                                None => continue,
                            };

                            let responses_events = state.process_event(a_event);

                            // If apply_patch invalid, consume silently (don't forward).
                            if state.is_apply_patch_invalid() {
                                continue;
                            }

                            for r_event in responses_events {
                                if matches!(r_event, ResponsesEvent::Done) {
                                    done_sent = true;
                                    let _ = tx
                                        .send(Ok(axum::body::Bytes::from(format_done())))
                                        .await;
                                } else {
                                    let sse_str = format_responses_event(&r_event);
                                    let _ = tx.send(Ok(axum::body::Bytes::from(sse_str))).await;
                                }
                            }
                        }
                    }
                    Ok(reqwest_eventsource::Event::Open) => {}
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("Stream ended") {
                            tracing::debug!("upstream SSE stream ended normally");
                        } else if streaming_state.is_some() {
                            tracing::error!(error = %e, "SSE stream error from upstream (mid-stream)");
                            let state = streaming_state.as_mut().unwrap();
                            let error_val = serde_json::json!({
                                "type": "api_error",
                                "message": format!("Upstream stream error: {}", e)
                            });
                            let events = state.process_event(
                                crate::sse::anthropic::AnthropicEvent::Error { error: error_val },
                            );
                            for r_event in events {
                                if matches!(r_event, ResponsesEvent::Done) {
                                    done_sent = true;
                                    let _ = tx
                                        .send(Ok(axum::body::Bytes::from(format_done())))
                                        .await;
                                } else {
                                    let sse_str = format_responses_event(&r_event);
                                    let _ = tx.send(Ok(axum::body::Bytes::from(sse_str))).await;
                                }
                            }
                        } else {
                            tracing::error!(error = %e, "SSE stream error from upstream (no message_start)");
                            let _ = tx
                                .send(Ok(axum::body::Bytes::from(
                                    "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Upstream stream error\"}\n\n",
                                )))
                                .await;
                            let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
                            done_sent = true;
                        }
                        break;
                    }
                }
            }

            // Close the event source (releases upstream connection).
            let _ = event_source.close();

            // Drain signatures from this response.
            if let Some(state) = streaming_state.as_mut() {
                let sigs = state.drain_signatures();
                for (reasoning_id, signature) in sigs {
                    signature_cache.insert(reasoning_id, signature);
                }
            }

            // Check if retry is needed.
            if let Some(state) = streaming_state.as_mut() {
                if state.is_apply_patch_invalid() {
                    let captured = state.take_content_block_capture();
                    let (toolu_id, _) = state.take_failed_apply_patch_info();
                    let error_msg = format_apply_patch_error();

                    // Build retry body using current_body (accumulates context across retries).
                    current_body = build_retry_body(&current_body, captured, &toolu_id, error_msg);

                    state.prepare_for_retry();
                    tracing::info!(toolu_id = %toolu_id, "retrying apply_patch with format error feedback");
                    continue 'retry_loop;
                }
            }

            break 'retry_loop;
        }

        // Send final [DONE] only if not already sent.
        if !done_sent {
            let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
        }
        drop(tx);
    });

    // Build downstream SSE response.
    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(body_stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_upstream_base_url_standard() {
        let route = RouteInfo::parse("/https/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_single_colon() {
        let route = RouteInfo::parse("/https:/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_double_slash() {
        let route = RouteInfo::parse("/https://api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_with_v1_prefix() {
        let route = RouteInfo::parse("/v1/https/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn rejects_non_responses_path() {
        let result = RouteInfo::parse("/v1/chat/completions");
        assert!(result.is_none());
    }

    #[test]
    fn rejects_missing_host() {
        let result = RouteInfo::parse("/https/responses");
        assert!(result.is_none());
    }

    #[test]
    fn extracts_custom_upstream() {
        let route = RouteInfo::parse("/https/my-proxy.example.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://my-proxy.example.com");
    }

    #[test]
    fn upstream_messages_url_includes_v1() {
        let route = RouteInfo::parse("/https/api.anthropic.com/responses").unwrap();
        assert_eq!(
            route.upstream_messages_url(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    // --- API key extraction tests ---

    use axum::http::header::AUTHORIZATION;

    #[test]
    fn extract_api_key_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer sk-test-123".parse().unwrap());
        let key = extract_api_key(&headers).unwrap();
        assert_eq!(key, "sk-test-123");
    }

    #[test]
    fn extract_api_key_lowercase_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "bearer sk-lowercase".parse().unwrap());
        let key = extract_api_key(&headers).unwrap();
        assert_eq!(key, "sk-lowercase");
    }

    #[test]
    fn extract_api_key_raw_token_no_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "sk-raw-token".parse().unwrap());
        let key = extract_api_key(&headers).unwrap();
        assert_eq!(key, "sk-raw-token");
    }

    #[test]
    fn extract_api_key_missing_header_returns_none() {
        let headers = HeaderMap::new();
        assert!(extract_api_key(&headers).is_none());
    }

    #[test]
    fn extract_api_key_empty_bearer_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer ".parse().unwrap());
        assert!(extract_api_key(&headers).is_none());
    }

    #[test]
    fn extract_api_key_empty_value_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "".parse().unwrap());
        assert!(extract_api_key(&headers).is_none());
    }

    // --- apply_patch helper function tests ---

    #[test]
    fn format_apply_patch_error_starts_correctly() {
        let msg = format_apply_patch_error();
        assert!(msg.starts_with("Error: apply_patch format not accepted"));
        assert!(msg.contains("*** Begin Patch"));
        assert!(msg.contains("*** Update File"));
        assert!(msg.contains("*** Add File"));
        assert!(msg.contains("*** Delete File"));
        assert!(msg.contains("*** End Patch"));
    }

    // --- Comprehensive format_apply_patch_error() tests ---

    #[test]
    fn format_apply_patch_error_starts_with_required_prefix() {
        let msg = format_apply_patch_error();
        assert!(
            msg.starts_with("Error: apply_patch format not accepted."),
            "Error message must start with the exact required prefix"
        );
    }

    #[test]
    fn format_apply_patch_error_contains_begin_patch() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("*** Begin Patch"),
            "Error message must contain '*** Begin Patch'"
        );
    }

    #[test]
    fn format_apply_patch_error_contains_update_file() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("*** Update File:"),
            "Error message must contain '*** Update File:'"
        );
    }

    #[test]
    fn format_apply_patch_error_contains_add_file() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("*** Add File:"),
            "Error message must contain '*** Add File:'"
        );
    }

    #[test]
    fn format_apply_patch_error_contains_delete_file() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("*** Delete File:"),
            "Error message must contain '*** Delete File:'"
        );
    }

    #[test]
    fn format_apply_patch_error_contains_end_patch() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("*** End Patch"),
            "Error message must contain '*** End Patch'"
        );
    }

    #[test]
    fn format_apply_patch_error_warns_against_unified_diff() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("Do NOT use unified diff format"),
            "Error message must warn against unified diff format"
        );
    }

    #[test]
    fn format_apply_patch_error_states_first_line_requirement() {
        let msg = format_apply_patch_error();
        assert!(
            msg.contains("The FIRST line MUST be exactly '*** Begin Patch'"),
            "Error message must state the first line requirement"
        );
    }

    #[test]
    fn format_apply_patch_error_is_deterministic() {
        let first = format_apply_patch_error();
        let second = format_apply_patch_error();
        assert_eq!(
            first, second,
            "Calling format_apply_patch_error() twice must return the same string"
        );
    }

    // --- Comprehensive build_retry_body() tests ---

    #[test]
    fn build_retry_body_appends_assistant_and_user_messages() {
        let original = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "fix the bug"}],
            "max_tokens": 4096,
        });
        let captured = vec![
            serde_json::json!({"type": "text", "text": "I'll fix it."}),
            serde_json::json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {"patch": "bad format"}}),
        ];
        let body = build_retry_body(&original, captured, "toolu_01", "Error message");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        // Original user message
        assert_eq!(messages[0]["role"], "user");
        // Appended assistant message
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
        // Appended user message with tool_result
        assert_eq!(messages[2]["role"], "user");
        let tool_result = &messages[2]["content"].as_array().unwrap()[0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["tool_use_id"], "toolu_01");
        assert_eq!(tool_result["is_error"], true);
        // Other fields preserved
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn build_retry_body_empty_captured_blocks() {
        let original = serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1024,
        });
        let body = build_retry_body(&original, vec![], "toolu_empty", "some error");
        let messages = body["messages"].as_array().unwrap();
        // Original user + assistant (empty content) + user tool_result
        assert_eq!(messages.len(), 3);
        let assistant_content = messages[1]["content"].as_array().unwrap();
        assert!(
            assistant_content.is_empty(),
            "Assistant content array should be empty when captured_blocks is empty"
        );
    }

    #[test]
    fn build_retry_body_multiple_tool_use_blocks() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "go"}],
            "max_tokens": 256,
        });
        let captured = vec![
            serde_json::json!({"type": "tool_use", "id": "toolu_a", "name": "apply_patch", "input": {"patch": "p1"}}),
            serde_json::json!({"type": "tool_use", "id": "toolu_b", "name": "apply_patch", "input": {"patch": "p2"}}),
            serde_json::json!({"type": "tool_use", "id": "toolu_c", "name": "shell", "input": {"cmd": "ls"}}),
        ];
        let body = build_retry_body(&original, captured, "toolu_b", "error");
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 3);
        assert_eq!(assistant_content[0]["id"], "toolu_a");
        assert_eq!(assistant_content[1]["id"], "toolu_b");
        assert_eq!(assistant_content[2]["id"], "toolu_c");
    }

    #[test]
    fn build_retry_body_preserves_model_field() {
        let original = serde_json::json!({
            "model": "claude-opus-4-20250514",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 8192,
        });
        let body = build_retry_body(&original, vec![], "toolu_1", "err");
        assert_eq!(body["model"], "claude-opus-4-20250514");
    }

    #[test]
    fn build_retry_body_preserves_max_tokens_field() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 16384,
        });
        let body = build_retry_body(&original, vec![], "toolu_1", "err");
        assert_eq!(body["max_tokens"], 16384);
    }

    #[test]
    fn build_retry_body_preserves_system_field() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
            "system": "You are a helpful assistant.",
        });
        let body = build_retry_body(&original, vec![], "toolu_1", "err");
        assert_eq!(body["system"], "You are a helpful assistant.");
    }

    #[test]
    fn build_retry_body_preserves_tools_field() {
        let tools = serde_json::json!([
            {"name": "apply_patch", "description": "Apply a patch"},
            {"name": "shell", "description": "Run a shell command"},
        ]);
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
            "tools": tools,
        });
        let body = build_retry_body(&original, vec![], "toolu_1", "err");
        assert_eq!(body["tools"], tools);
    }

    #[test]
    fn build_retry_body_preserves_thinking_field() {
        let thinking = serde_json::json!({
            "type": "enabled",
            "budget_tokens": 5000,
        });
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
            "thinking": thinking,
        });
        let body = build_retry_body(&original, vec![], "toolu_1", "err");
        assert_eq!(body["thinking"], thinking);
    }

    #[test]
    fn build_retry_body_user_message_has_correct_role_and_structure() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let body = build_retry_body(&original, vec![], "toolu_99", "err");
        let messages = body["messages"].as_array().unwrap();
        let user_msg = &messages[2];
        assert_eq!(user_msg["role"], "user");
        let content = user_msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "User message content must be an array with one block");
    }

    #[test]
    fn build_retry_body_tool_result_has_is_error_true() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let body = build_retry_body(&original, vec![], "toolu_err", "some error");
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"].as_array().unwrap()[0];
        assert_eq!(tool_result["is_error"], true);
    }

    #[test]
    fn build_retry_body_tool_result_has_correct_tool_use_id() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let body = build_retry_body(&original, vec![], "toolu_abc123", "err");
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"].as_array().unwrap()[0];
        assert_eq!(tool_result["tool_use_id"], "toolu_abc123");
    }

    #[test]
    fn build_retry_body_tool_result_contains_error_message() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let error_msg = format_apply_patch_error();
        let body = build_retry_body(&original, vec![], "toolu_1", error_msg);
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"].as_array().unwrap()[0];
        let content = tool_result["content"].as_str().unwrap();
        assert!(
            content.contains("apply_patch format not accepted"),
            "Tool result content must contain the apply_patch error message"
        );
        assert!(
            content.contains("*** Begin Patch"),
            "Tool result content must contain the patch format instructions"
        );
    }

    #[test]
    fn build_retry_body_preserves_thinking_text_and_tool_use_in_order() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let captured = vec![
            serde_json::json!({"type": "thinking", "thinking": "Let me reason about this..."}),
            serde_json::json!({"type": "text", "text": "Here's my answer."}),
            serde_json::json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {"patch": "*** Begin Patch\n*** End Patch"}}),
        ];
        let body = build_retry_body(&original, captured, "toolu_01", "bad patch");
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 3);
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert_eq!(assistant_content[0]["thinking"], "Let me reason about this...");
        assert_eq!(assistant_content[1]["type"], "text");
        assert_eq!(assistant_content[1]["text"], "Here's my answer.");
        assert_eq!(assistant_content[2]["type"], "tool_use");
        assert_eq!(assistant_content[2]["id"], "toolu_01");
    }

    #[test]
    fn build_retry_body_preserves_redacted_thinking_with_data_field() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let captured = vec![
            serde_json::json!({"type": "redacted_thinking", "data": "base64encodeddata=="}),
        ];
        let body = build_retry_body(&original, captured, "toolu_1", "err");
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 1);
        assert_eq!(assistant_content[0]["type"], "redacted_thinking");
        assert_eq!(assistant_content[0]["data"], "base64encodeddata==");
    }

    // --- Multi-retry accumulation test ---

    #[test]
    fn build_retry_body_accumulates_across_two_retries() {
        // Simulate an original body with one user message.
        let original = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "fix the bug"}],
            "max_tokens": 4096,
        });

        // First retry: captured blocks from the first response.
        let first_captured = vec![
            serde_json::json!({"type": "text", "text": "Attempting fix."}),
            serde_json::json!({"type": "tool_use", "id": "toolu_01", "name": "apply_patch", "input": {"patch": "bad1"}}),
        ];
        let first_retry = build_retry_body(&original, first_captured, "toolu_01", "first error");
        assert_eq!(first_retry["messages"].as_array().unwrap().len(), 3);

        // Second retry: use the first retry body as the "original" and retry again.
        let second_captured = vec![
            serde_json::json!({"type": "text", "text": "Second attempt."}),
            serde_json::json!({"type": "tool_use", "id": "toolu_02", "name": "apply_patch", "input": {"patch": "bad2"}}),
        ];
        let second_retry =
            build_retry_body(&first_retry, second_captured, "toolu_02", "second error");
        let messages = second_retry["messages"].as_array().unwrap();
        // Expected: original user + 1st assistant + 1st user_tool_result + 2nd assistant + 2nd user_tool_result
        assert_eq!(messages.len(), 5);
        // Verify ordering and roles.
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[4]["role"], "user");
        // Verify first tool_result points to toolu_01.
        let first_tool_result = &messages[2]["content"].as_array().unwrap()[0];
        assert_eq!(first_tool_result["tool_use_id"], "toolu_01");
        assert_eq!(first_tool_result["is_error"], true);
        assert_eq!(first_tool_result["content"], "first error");
        // Verify second tool_result points to toolu_02.
        let second_tool_result = &messages[4]["content"].as_array().unwrap()[0];
        assert_eq!(second_tool_result["tool_use_id"], "toolu_02");
        assert_eq!(second_tool_result["is_error"], true);
        assert_eq!(second_tool_result["content"], "second error");
        // Verify top-level fields still preserved after two retries.
        assert_eq!(second_retry["model"], "claude-sonnet-4-20250514");
        assert_eq!(second_retry["max_tokens"], 4096);
    }

    // --- Handler-level 401 tests using a test router ---

    use axum::body::Body as AxumBody;
    use tower::ServiceExt;

    fn test_app() -> axum::Router {
        use crate::config::AppConfig;
        let state = AppState {
            config: AppConfig::default(),
        };
        build_router(state)
    }

    #[tokio::test]
    async fn handler_returns_401_when_no_auth_header() {
        let app = test_app();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/https/api.anthropic.com/responses")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handler_returns_401_when_empty_bearer() {
        let app = test_app();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/https/api.anthropic.com/responses")
            .header("authorization", "Bearer ")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handler_passes_auth_with_bearer_token() {
        let app = test_app();
        let body = serde_json::json!({"model": "test", "input": []});
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/https/api.anthropic.com/responses")
            .header("authorization", "Bearer sk-valid-key")
            .header("content-type", "application/json")
            .body(AxumBody::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Auth passed -- handler proceeds past auth check.
        // The response may be 500 (upstream client TLS failure in test) or 200 (SSE stream),
        // but should NOT be 401 (auth rejected).
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn handler_passes_auth_with_raw_token() {
        let app = test_app();
        let body = serde_json::json!({"model": "test", "input": []});
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/https/api.anthropic.com/responses")
            .header("authorization", "sk-raw-key")
            .header("content-type", "application/json")
            .body(AxumBody::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Auth passed -- should NOT be 401.
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
