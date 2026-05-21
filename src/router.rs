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

    let request_builder = client
        .post(&upstream_url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", &anthropic_version)
        .header("content-type", "application/json")
        .json(&anthropic_body);

    tracing::debug!(
        url = %upstream_url,
        version = %anthropic_version,
        "forwarding request to upstream"
    );

    // -- Step 7-9: Stream SSE events from upstream, convert, and forward downstream --
    let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);

    // Namespace registry is moved into the streaming task.
    let namespace_registry =
        std::sync::Arc::new(std::sync::Mutex::new(task.namespace_registry));
    let signature_cache = task.signature_cache.clone();

    // Spawn a task to consume the upstream SSE stream.
    tokio::spawn(async move {
        use crate::conversion::response::StreamingState;
        use crate::sse::anthropic::{parse_sse_events, AnthropicEvent};
        use crate::sse::responses::{format_done, format_responses_event, ResponsesEvent};

        let mut event_source = match reqwest_eventsource::EventSource::new(request_builder) {
            Ok(es) => es,
            Err(e) => {
                tracing::error!(error = %e, "failed to create EventSource for upstream request");
                let _ = tx.send(Ok(axum::body::Bytes::from(
                    "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Failed to connect to upstream\"}\n\n",
                ))).await;
                let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
                return;
            }
        };

        let mut streaming_state: Option<StreamingState> = None;
        let mut done_sent = false;

        while let Some(event_result) = event_source.next().await {
            match event_result {
                Ok(reqwest_eventsource::Event::Message(msg)) => {
                    // Reconstruct raw SSE text for the parser.
                    // reqwest_eventsource::Event::Message provides event (String) and data (String).
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
                                // Skip events before message_start.
                                continue;
                            }
                        }

                        let state = match streaming_state.as_mut() {
                            Some(s) => s,
                            None => continue,
                        };

                        let responses_events = state.process_event(a_event);

                        for r_event in responses_events {
                            // Check if this is the Done marker.
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
                Ok(reqwest_eventsource::Event::Open) => {
                    // Connection established, nothing to do.
                }
                Err(e) => {
                    // "Stream ended" is normal — Anthropic closes the connection after
                    // message_stop without a [DONE] marker. Only treat unexpected
                    // errors as real errors.
                    let err_str = e.to_string();
                    if err_str.contains("Stream ended") {
                        tracing::debug!("upstream SSE stream ended normally");
                    } else if streaming_state.is_some() {
                        // We already got message_start — the stream was partially
                        // delivered.  StreamingState::handle_error will produce
                        // error + response.failed + Done events.
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
                        // Never got message_start — the upstream likely returned a
                        // non-streaming error (e.g. 4xx/5xx) before any SSE events.
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

        // Write accumulated signatures to cache after stream ends.
        if let Some(state) = streaming_state.as_mut() {
            let sigs = state.drain_signatures();
            for (reasoning_id, signature) in sigs {
                signature_cache.insert(reasoning_id, signature);
            }
        }

        // Send final [DONE] only if not already sent (e.g. via error or Done event).
        if !done_sent {
            let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
        }
        drop(tx);
        let _ = event_source.close();
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
