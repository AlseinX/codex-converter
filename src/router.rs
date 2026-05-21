use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use futures::StreamExt;
use std::pin::Pin;
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
        let host_part = if let Some(stripped) = after_https.strip_prefix("://") {
            stripped
        } else if let Some(stripped) = after_https.strip_prefix(":/") {
            stripped
        } else {
            after_https.strip_prefix('/')?
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

/// Type alias for a boxed SSE event stream used by the retry loop.
type SseStream = Pin<
    Box<
        dyn futures::Stream<Item = Result<reqwest_eventsource::Event, reqwest_eventsource::Error>>
            + Send,
    >,
>;

/// Type alias for the factory that creates SSE event streams.
/// Given the current request body, returns a stream of SSE events (or an error).
type SseStreamFactory = Box<
    dyn FnMut(&serde_json::Value) -> Result<SseStream, Box<dyn std::error::Error + Send + Sync>>
        + Send,
>;

/// Echo-back fields extracted from the original request, used during SSE conversion.
struct EchoFields {
    model: String,
    tool_choice: String,
    instructions: Option<String>,
    parallel_tool_calls: Option<bool>,
}

/// Run the streaming SSE conversion with retry support for invalid apply_patch.
///
/// This is the core retry loop, extracted for testability. In production,
/// the `stream_factory` creates EventSource instances from HTTP requests.
/// In tests, it can provide in-memory mock streams.
///
/// # Arguments
/// * `tx` - Channel sender for output SSE events to the downstream client
/// * `initial_body` - The initial Anthropic Messages API request body
/// * `stream_factory` - Factory that creates SSE event streams from a request body
/// * `echo` - Echo-back fields from the original request
/// * `namespace_registry` - Shared namespace registry from request conversion
/// * `signature_cache` - Shared signature cache
async fn run_streaming_with_retry(
    tx: mpsc::Sender<Result<axum::body::Bytes, std::convert::Infallible>>,
    initial_body: serde_json::Value,
    mut stream_factory: SseStreamFactory,
    echo: EchoFields,
    namespace_registry: std::sync::Arc<std::sync::Mutex<crate::conversion::NamespaceRegistry>>,
    signature_cache: std::sync::Arc<crate::conversion::SignatureCache>,
) {
    use crate::conversion::response::StreamingState;
    use crate::sse::anthropic::{AnthropicEvent, parse_sse_events};
    use crate::sse::responses::{ResponsesEvent, format_done, format_responses_event};

    let mut current_body = initial_body;
    let mut streaming_state: Option<StreamingState> = None;
    let mut done_sent = false;

    'retry_loop: loop {
        let mut event_stream = match stream_factory(&current_body) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to create EventSource");
                let _ = tx
                    .send(Ok(axum::body::Bytes::from(
                        "event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Failed to connect to upstream\"}\n\n",
                    )))
                    .await;
                let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
                return;
            }
        };

        // Inner SSE streaming loop.
        while let Some(event_result) = event_stream.next().await {
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
                                    echo.model.clone(),
                                    ns_reg,
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                    echo.tool_choice.clone(),
                                    echo.instructions.clone(),
                                    echo.parallel_tool_calls,
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
                                let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
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
                        let events =
                            state.process_event(crate::sse::anthropic::AnthropicEvent::Error {
                                error: error_val,
                            });
                        for r_event in events {
                            if matches!(r_event, ResponsesEvent::Done) {
                                done_sent = true;
                                let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
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

        // Drop the event stream (releases any upstream connection).
        drop(event_stream);

        // Drain signatures from this response.
        if let Some(state) = streaming_state.as_mut() {
            let sigs = state.drain_signatures();
            for (reasoning_id, signature) in sigs {
                signature_cache.insert(reasoning_id, signature);
            }
        }

        // Check if retry is needed.
        if let Some(state) = streaming_state.as_mut()
            && state.is_apply_patch_invalid()
        {
            let captured = state.take_content_block_capture();
            let (toolu_id, _) = state.take_failed_apply_patch_info();
            let error_msg = format_apply_patch_error();

            // Build retry body using current_body (accumulates context across retries).
            current_body = build_retry_body(&current_body, captured, &toolu_id, error_msg);

            state.prepare_for_retry();
            tracing::info!(toolu_id = %toolu_id, "retrying apply_patch with format error feedback");
            continue 'retry_loop;
        }

        break 'retry_loop;
    }

    // Send final [DONE] only if not already sent.
    if !done_sent {
        let _ = tx.send(Ok(axum::body::Bytes::from(format_done()))).await;
    }
    drop(tx);
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
    let parallel_tool_calls_echo = body_json
        .get("parallel_tool_calls")
        .and_then(|v| v.as_bool());

    // -- Step 4: Create ConversionTask and convert request --
    let mut task = crate::conversion::ConversionTask::new(route_info.upstream_base_url.clone());
    let anthropic_body = crate::conversion::request::convert_request(&mut task, body_json)
        .map_err(|e| {
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
    let namespace_registry = std::sync::Arc::new(std::sync::Mutex::new(task.namespace_registry));
    let signature_cache = task.signature_cache.clone();

    // Spawn a task to consume the upstream SSE stream, with retry support for
    // invalid apply_patch format.
    let spawn_factory: SseStreamFactory = Box::new(move |body: &serde_json::Value| {
        let request_builder = retry_client
            .post(&retry_upstream_url)
            .header("x-api-key", &retry_api_key)
            .header("anthropic-version", &retry_anthropic_version)
            .header("content-type", "application/json")
            .json(body);

        let event_source = reqwest_eventsource::EventSource::new(request_builder)?;
        Ok(Box::pin(event_source) as SseStream)
    });

    tokio::spawn(run_streaming_with_retry(
        tx,
        retry_anthropic_body,
        spawn_factory,
        EchoFields {
            model,
            tool_choice: tool_choice_echo,
            instructions: instructions_echo,
            parallel_tool_calls: parallel_tool_calls_echo,
        },
        namespace_registry,
        signature_cache,
    ));

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
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"]
            .as_array()
            .unwrap();
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
        assert_eq!(
            content.len(),
            1,
            "User message content must be an array with one block"
        );
    }

    #[test]
    fn build_retry_body_tool_result_has_is_error_true() {
        let original = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
        });
        let body = build_retry_body(&original, vec![], "toolu_err", "some error");
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"]
            .as_array()
            .unwrap()[0];
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
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"]
            .as_array()
            .unwrap()[0];
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
        let tool_result = &body["messages"].as_array().unwrap()[2]["content"]
            .as_array()
            .unwrap()[0];
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
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"]
            .as_array()
            .unwrap();
        assert_eq!(assistant_content.len(), 3);
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert_eq!(
            assistant_content[0]["thinking"],
            "Let me reason about this..."
        );
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
        let captured =
            vec![serde_json::json!({"type": "redacted_thinking", "data": "base64encodeddata=="})];
        let body = build_retry_body(&original, captured, "toolu_1", "err");
        let assistant_content = body["messages"].as_array().unwrap()[1]["content"]
            .as_array()
            .unwrap();
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

    // =======================================================================
    // Retry loop integration tests
    // =======================================================================

    use crate::conversion::SignatureCache;

    /// Helper: create a reqwest_eventsource::Event::Message from event type and data.
    fn sse_msg(event: &str, data: String) -> reqwest_eventsource::Event {
        reqwest_eventsource::Event::Message(eventsource_stream::Event {
            event: event.to_string(),
            data,
            id: String::new(),
            retry: None,
        })
    }

    /// Helper: build a complete valid Anthropic SSE stream (text-only, no tool use).
    /// Produces: message_start -> content_block_start(text) -> content_block_delta ->
    ///   content_block_stop -> message_delta -> message_stop
    fn valid_text_only_sse_events(text: &str) -> Vec<reqwest_eventsource::Event> {
        let msg_id = "msg_test001";
        vec![
            sse_msg(
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": msg_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "claude-sonnet-4-20250514",
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 10, "output_tokens": 0}
                    }
                })
                .to_string(),
            ),
            sse_msg(
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                })
                .to_string(),
            ),
            sse_msg(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text}
                })
                .to_string(),
            ),
            sse_msg(
                "content_block_stop",
                serde_json::json!({
                    "type": "content_block_stop",
                    "index": 0
                })
                .to_string(),
            ),
            sse_msg(
                "message_delta",
                serde_json::json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 5}
                })
                .to_string(),
            ),
            sse_msg(
                "message_stop",
                serde_json::json!({"type": "message_stop"}).to_string(),
            ),
        ]
    }

    /// Helper: build SSE events for an apply_patch with invalid format.
    /// The patch text does NOT start with "*** Begin Patch" and has no diff markers.
    fn invalid_apply_patch_sse_events(
        msg_id: &str,
        text_before: Option<&str>,
        toolu_id: &str,
        patch_text: &str,
    ) -> Vec<reqwest_eventsource::Event> {
        let mut events = Vec::new();

        // message_start
        events.push(sse_msg(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": msg_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-4-20250514",
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            })
            .to_string(),
        ));

        let mut block_index = 0;

        // Optional text block before the apply_patch
        if let Some(text) = text_before {
            events.push(sse_msg(
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {"type": "text", "text": ""}
                })
                .to_string(),
            ));
            events.push(sse_msg(
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "text_delta", "text": text}
                })
                .to_string(),
            ));
            events.push(sse_msg(
                "content_block_stop",
                serde_json::json!({
                    "type": "content_block_stop",
                    "index": block_index
                })
                .to_string(),
            ));
            block_index += 1;
        }

        // apply_patch tool_use block (invalid format)
        events.push(sse_msg(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": toolu_id,
                    "name": "apply_patch",
                    "input": {}
                }
            })
            .to_string(),
        ));
        let patch_json = serde_json::json!({"patch": patch_text}).to_string();
        events.push(sse_msg(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {"type": "input_json_delta", "partial_json": patch_json}
            })
            .to_string(),
        ));
        events.push(sse_msg(
            "content_block_stop",
            serde_json::json!({
                "type": "content_block_stop",
                "index": block_index
            })
            .to_string(),
        ));

        events.push(sse_msg(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 50}
            })
            .to_string(),
        ));
        events.push(sse_msg(
            "message_stop",
            serde_json::json!({"type": "message_stop"}).to_string(),
        ));

        events
    }

    /// Helper: build SSE events for a valid apply_patch (correct Codex freeform format).
    fn valid_apply_patch_sse_events(
        msg_id: &str,
        toolu_id: &str,
        patch_text: &str,
    ) -> Vec<reqwest_eventsource::Event> {
        let mut events = Vec::new();

        events.push(sse_msg(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": msg_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-4-20250514",
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            })
            .to_string(),
        ));

        events.push(sse_msg(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": toolu_id,
                    "name": "apply_patch",
                    "input": {}
                }
            })
            .to_string(),
        ));
        let patch_json = serde_json::json!({"patch": patch_text}).to_string();
        events.push(sse_msg(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": patch_json}
            })
            .to_string(),
        ));
        events.push(sse_msg(
            "content_block_stop",
            serde_json::json!({
                "type": "content_block_stop",
                "index": 0
            })
            .to_string(),
        ));

        events.push(sse_msg(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 30}
            })
            .to_string(),
        ));
        events.push(sse_msg(
            "message_stop",
            serde_json::json!({"type": "message_stop"}).to_string(),
        ));

        events
    }

    /// Helper: collect all output bytes from the retry loop into a single String.
    async fn collect_retry_output(
        rx: &mut tokio::sync::mpsc::Receiver<Result<axum::body::Bytes, std::convert::Infallible>>,
    ) -> String {
        let mut output = String::new();
        while let Some(chunk) = rx.recv().await {
            match chunk {
                Ok(bytes) => output.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => unreachable!("infallible"),
            }
        }
        output
    }

    /// Helper: check if the SSE output contains a specific event type.
    fn sse_output_contains(output: &str, event_type: &str) -> bool {
        output.contains(&format!("event: {}\n", event_type))
    }

    /// Helper: count occurrences of an SSE event type in the output.
    fn count_sse_events(output: &str, event_type: &str) -> usize {
        output.matches(&format!("event: {}\n", event_type)).count()
    }

    /// Helper: create default retry loop parameters.
    fn default_retry_params() -> (
        EchoFields,
        std::sync::Arc<std::sync::Mutex<crate::conversion::NamespaceRegistry>>,
        std::sync::Arc<SignatureCache>,
    ) {
        (
            EchoFields {
                model: "claude-sonnet-4-20250514".to_string(),
                tool_choice: "auto".to_string(),
                instructions: None,
                parallel_tool_calls: None,
            },
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::conversion::NamespaceRegistry::new(),
            )),
            std::sync::Arc::new(SignatureCache::new(std::time::Duration::from_secs(3600))),
        )
    }

    // --- Scenario 1: Valid response (no retry) ---

    #[tokio::test]
    async fn retry_loop_valid_response_no_retry() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        let events = valid_text_only_sse_events("Hello world");

        // Factory that returns one valid stream, then would fail if called again.
        let events_clone = events.clone();
        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            assert!(
                call_count <= 1,
                "factory should only be called once for valid response"
            );
            let stream = futures::stream::iter(events_clone.clone().into_iter().map(Ok));
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({"model": "test", "messages": [], "max_tokens": 1024});

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have the text content events and [DONE]
        assert!(
            sse_output_contains(&output, "response.created"),
            "should have response.created, got: {}",
            &output[..output.len().min(500)]
        );
        assert!(
            sse_output_contains(&output, "response.output_text.delta"),
            "should have text delta, got: {}",
            &output[..output.len().min(500)]
        );
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE], got: {}",
            &output[..output.len().min(500)]
        );
        // Should have response.completed (from message_stop -> ResponseCompleted)
        assert!(
            sse_output_contains(&output, "response.completed"),
            "should have response.completed"
        );
        // Should NOT have retry-related error events
        assert!(
            !sse_output_contains(&output, "error"),
            "should not have error events"
        );
    }

    // --- Scenario 2: Invalid apply_patch triggers retry, second stream succeeds ---

    #[tokio::test]
    async fn retry_loop_invalid_patch_triggers_retry_then_succeeds() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // First call: invalid patch. Second call: valid text response.
        let invalid_events =
            invalid_apply_patch_sse_events("msg_retry1", None, "toolu_bad1", "bad patch content");
        let valid_events = valid_text_only_sse_events("Fixed!");

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            if call_count == 1 {
                let stream = futures::stream::iter(invalid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            } else {
                assert_eq!(call_count, 2, "should only need 2 calls");
                let stream = futures::stream::iter(valid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            }
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should end with [DONE]
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE], got: {}",
            &output[..output.len().min(500)]
        );

        // Should have response.created (from the first or second stream)
        assert!(
            sse_output_contains(&output, "response.created"),
            "should have response.created"
        );

        // Should have the text from the second stream
        assert!(
            sse_output_contains(&output, "response.output_text.delta"),
            "should have text delta from retry stream"
        );

        // Should NOT have any error events (the invalid patch is silently retried)
        assert!(
            !sse_output_contains(&output, "error"),
            "should not have error events"
        );
    }

    // --- Scenario 3: Double retry (two invalid patches, third succeeds) ---

    #[tokio::test]
    async fn retry_loop_double_retry_then_succeeds() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        let invalid1 =
            invalid_apply_patch_sse_events("msg_retry1", None, "toolu_bad1", "still bad");
        let invalid2 = invalid_apply_patch_sse_events("msg_retry2", None, "toolu_bad2", "also bad");
        let valid = valid_text_only_sse_events("Finally works!");

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            let events = match call_count {
                1 => invalid1.clone(),
                2 => invalid2.clone(),
                3 => valid.clone(),
                _ => panic!("factory called too many times: {}", call_count),
            };
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        assert!(output.contains("data: [DONE]"), "should end with [DONE]");
        // Should have text from the third (successful) stream
        assert!(
            sse_output_contains(&output, "response.output_text.delta"),
            "should have text delta from final successful stream"
        );
    }

    // --- Scenario 4: EventSource creation failure ---

    #[tokio::test]
    async fn retry_loop_event_source_creation_failure() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        let factory: SseStreamFactory =
            Box::new(move |_body: &serde_json::Value| Err("connection refused".into()));

        let initial_body = serde_json::json!({"model": "test", "messages": [], "max_tokens": 1024});

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have an error event
        assert!(
            sse_output_contains(&output, "error"),
            "should have error event for creation failure"
        );
        assert!(
            output.contains("Failed to connect to upstream"),
            "error message should mention connection failure"
        );
        // Should still end with [DONE]
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE] even on error"
        );
    }

    // --- Scenario 5: SSE stream error mid-stream (StreamEnded) ---

    #[tokio::test]
    async fn retry_loop_sse_stream_ends_prematurely() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // Stream that starts with message_start then ends with StreamEnded error.
        // In the retry loop, "Stream ended" is treated as a normal stream end.
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            let msg_start = Ok(sse_msg(
                "message_start",
                serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_miderr",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "claude-sonnet-4-20250514",
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 10, "output_tokens": 0}
                    }
                })
                .to_string(),
            ));
            let err = reqwest_eventsource::Error::StreamEnded;
            let stream = futures::stream::iter(vec![msg_start, Err(err)]);
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({"model": "test", "messages": [], "max_tokens": 1024});

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have response.created (from message_start)
        assert!(
            sse_output_contains(&output, "response.created"),
            "should have response.created before stream end"
        );
        // Should end with [DONE]
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE] after stream end"
        );
    }

    // --- Scenario 5b: SSE stream error before message_start ---

    #[tokio::test]
    async fn retry_loop_sse_stream_error_before_message_start() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // Stream that errors immediately, before any message_start.
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            let err = reqwest_eventsource::Error::InvalidLastEventId("bad-id".to_string());
            let stream = futures::stream::iter(vec![Err(err)]);
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({"model": "test", "messages": [], "max_tokens": 1024});

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have error event
        assert!(
            sse_output_contains(&output, "error"),
            "should have error event for stream error before message_start"
        );
        // Should end with [DONE]
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE] after error"
        );
    }

    // --- Scenario 6: Text forwarded before retry ---

    #[tokio::test]
    async fn retry_loop_text_forwarded_before_retry() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // First stream: text "I will fix it" + invalid apply_patch
        let invalid_events = invalid_apply_patch_sse_events(
            "msg_txt_retry",
            Some("I will fix it"),
            "toolu_txt_bad",
            "not a valid patch",
        );
        // Second stream: valid text response
        let valid_events = valid_text_only_sse_events("Done!");

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            if call_count == 1 {
                let stream = futures::stream::iter(invalid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            } else {
                let stream = futures::stream::iter(valid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            }
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have text from both streams
        // First stream should have forwarded "I will fix it"
        assert!(
            output.contains("I will fix it"),
            "text from first stream should be forwarded before retry"
        );
        // Second stream should have "Done!"
        assert!(
            output.contains("Done!"),
            "text from retry stream should be forwarded"
        );
        // Should end with [DONE]
        assert!(output.contains("data: [DONE]"), "should end with [DONE]");

        // Count response.created events -- should have at least one
        let created_count = count_sse_events(&output, "response.created");
        assert!(
            created_count >= 1,
            "should have at least one response.created"
        );
    }

    // --- Scenario: Valid apply_patch succeeds without retry ---

    #[tokio::test]
    async fn retry_loop_valid_apply_patch_no_retry() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // A valid apply_patch (starts with *** Begin Patch) should succeed without retry.
        let valid_patch = "*** Begin Patch\n*** Update File: foo.rs\n-old\n+new\n*** End Patch\n";
        let events = valid_apply_patch_sse_events("msg_valid_patch", "toolu_valid", valid_patch);

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            assert_eq!(
                call_count, 1,
                "factory should only be called once for valid patch"
            );
            let stream = futures::stream::iter(events.clone().into_iter().map(Ok));
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have response.created
        assert!(
            sse_output_contains(&output, "response.created"),
            "should have response.created"
        );
        // Should have custom_tool_call output (the apply_patch was valid)
        assert!(
            output.contains("custom_tool_call"),
            "should have custom_tool_call for valid apply_patch, got: {}",
            &output[..output.len().min(500)]
        );
        // Should end with [DONE]
        assert!(output.contains("data: [DONE]"), "should end with [DONE]");
        // Should NOT have error events
        assert!(
            !sse_output_contains(&output, "error"),
            "should not have error events for valid patch"
        );
    }

    // --- Scenario 35: EventSource factory called exactly 2 times (invalid then valid) ---

    #[tokio::test]
    async fn retry_loop_factory_called_exactly_twice() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // First call: invalid patch. Second call: valid text response.
        let invalid_events =
            invalid_apply_patch_sse_events("msg_factory1", None, "toolu_f1", "bad patch content");
        let valid_events = valid_text_only_sse_events("Factory works!");

        // Use Arc<AtomicUsize> so the factory (moved into closure) can count calls,
        // and we can read the final count after the retry loop completes.
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            let prev = call_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if prev == 0 {
                let stream = futures::stream::iter(invalid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            } else {
                let stream = futures::stream::iter(valid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            }
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Factory should have been called exactly 2 times
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "factory should be called exactly 2 times (once for invalid, once for retry), got: {}",
            call_count.load(std::sync::atomic::Ordering::SeqCst)
        );

        // Should end with [DONE]
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE], got: {}",
            &output[..output.len().min(500)]
        );

        // Should have text from the second (retry) stream
        assert!(
            output.contains("Factory works!"),
            "should have text from retry stream"
        );
    }

    // --- Scenario 48: StreamingState created lazily (non-message_start events first) ---

    #[tokio::test]
    async fn retry_loop_lazy_state_creation_with_ping_then_message() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // Stream: ping event, then an Open event, then a valid text response.
        // The ping and Open events should be handled without error (no state creation yet),
        // then message_start creates state and text is processed normally.
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            let events: Vec<reqwest_eventsource::Event> = vec![
                // Ping event (non-message_start, no state created)
                sse_msg("ping", serde_json::json!({"type": "ping"}).to_string()),
                // Open event (handled but no action)
                reqwest_eventsource::Event::Open,
                // Now the real message starts
                sse_msg(
                    "message_start",
                    serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg_lazy",
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": "claude-sonnet-4-20250514",
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": {"input_tokens": 10, "output_tokens": 0}
                        }
                    })
                    .to_string(),
                ),
                sse_msg(
                    "content_block_start",
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "text", "text": ""}
                    })
                    .to_string(),
                ),
                sse_msg(
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": "Lazy state works"}
                    })
                    .to_string(),
                ),
                sse_msg(
                    "content_block_stop",
                    serde_json::json!({
                        "type": "content_block_stop",
                        "index": 0
                    })
                    .to_string(),
                ),
                sse_msg(
                    "message_delta",
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"output_tokens": 5}
                    })
                    .to_string(),
                ),
                sse_msg(
                    "message_stop",
                    serde_json::json!({"type": "message_stop"}).to_string(),
                ),
            ];
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream) as SseStream)
        });

        let initial_body = serde_json::json!({"model": "test", "messages": [], "max_tokens": 1024});

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have response.created (from message_start after ping)
        assert!(
            sse_output_contains(&output, "response.created"),
            "should have response.created after ping, got: {}",
            &output[..output.len().min(500)]
        );

        // Should have the text content
        assert!(
            output.contains("Lazy state works"),
            "should have text content after lazy state creation, got: {}",
            &output[..output.len().min(500)]
        );

        // Should have response.completed
        assert!(
            sse_output_contains(&output, "response.completed"),
            "should have response.completed"
        );

        // Should end with [DONE]
        assert!(output.contains("data: [DONE]"), "should end with [DONE]");

        // Should NOT have error events
        assert!(
            !sse_output_contains(&output, "error"),
            "ping events before message_start should not cause errors"
        );
    }

    // --- Scenario 4b: EventSource creation failure on retry (not first call) ---

    #[tokio::test]
    async fn retry_loop_creation_failure_on_retry_attempt() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // First call succeeds with invalid patch, second call fails
        let invalid_events =
            invalid_apply_patch_sse_events("msg_retry_fail", None, "toolu_rfail", "bad patch");

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            if call_count == 1 {
                let stream = futures::stream::iter(invalid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            } else {
                Err("retry connection failed".into())
            }
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            sig_cache,
        ));

        let output = collect_retry_output(&mut rx).await;

        // Should have error from the retry failure
        assert!(
            sse_output_contains(&output, "error"),
            "should have error event for retry creation failure"
        );
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE] even on retry failure"
        );
    }

    // --- Scenario: Signatures from a failed response are drained and cached across retry ---

    #[tokio::test]
    async fn retry_loop_signatures_drained_and_cached_across_retry() {
        let (tx, mut rx) = mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(64);
        let (echo, ns_reg, sig_cache) = default_retry_params();

        // Build first stream: thinking block with a known signature, then invalid apply_patch.
        let mut first_events = Vec::new();

        // message_start
        first_events.push(sse_msg(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_sig_test",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-4-20250514",
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 10, "output_tokens": 0}
                }
            })
            .to_string(),
        ));

        // thinking block (index 0)
        first_events.push(sse_msg(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            })
            .to_string(),
        ));
        first_events.push(sse_msg(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "I am reasoning"}
            })
            .to_string(),
        ));
        // signature_delta with known value
        first_events.push(sse_msg(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "rs_test_signature_123"}
            })
            .to_string(),
        ));
        first_events.push(sse_msg(
            "content_block_stop",
            serde_json::json!({
                "type": "content_block_stop",
                "index": 0
            })
            .to_string(),
        ));

        // invalid apply_patch block (index 1)
        first_events.push(sse_msg(
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_sig_bad",
                    "name": "apply_patch",
                    "input": {}
                }
            })
            .to_string(),
        ));
        let bad_patch = serde_json::json!({"patch": "not a valid patch"}).to_string();
        first_events.push(sse_msg(
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": bad_patch}
            })
            .to_string(),
        ));
        first_events.push(sse_msg(
            "content_block_stop",
            serde_json::json!({
                "type": "content_block_stop",
                "index": 1
            })
            .to_string(),
        ));

        first_events.push(sse_msg(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 50}
            })
            .to_string(),
        ));
        first_events.push(sse_msg(
            "message_stop",
            serde_json::json!({"type": "message_stop"}).to_string(),
        ));

        // Second stream: simple valid text response (the retry succeeds)
        let valid_events = valid_text_only_sse_events("Retry successful");

        let mut call_count = 0;
        let factory: SseStreamFactory = Box::new(move |_body: &serde_json::Value| {
            call_count += 1;
            if call_count == 1 {
                let stream = futures::stream::iter(first_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            } else {
                assert_eq!(call_count, 2, "should only need 2 calls");
                let stream = futures::stream::iter(valid_events.clone().into_iter().map(Ok));
                Ok(Box::pin(stream) as SseStream)
            }
        });

        let initial_body = serde_json::json!({
            "model": "test",
            "messages": [{"role": "user", "content": "fix it"}],
            "max_tokens": 1024
        });

        tokio::spawn(run_streaming_with_retry(
            tx,
            initial_body,
            factory,
            echo,
            ns_reg,
            // Clone sig_cache for the spawned task; we retain the original to inspect after.
            sig_cache.clone(),
        ));

        let output = collect_retry_output(&mut rx).await;

        // The retry should succeed.
        assert!(
            output.contains("data: [DONE]"),
            "should end with [DONE], got: {}",
            &output[..output.len().min(500)]
        );

        // Extract the reasoning_id from the SSE output.
        // The response.reasoning_summary_text.done event includes "item_id": "rs_..."
        let reasoning_id = output
            .lines()
            .filter_map(|line| {
                let data = line.strip_prefix("data: ")?;
                let json: serde_json::Value = serde_json::from_str(data).ok()?;
                json.get("item_id")
                    .and_then(|v| v.as_str())
                    .filter(|id| id.starts_with("rs_"))
                    .map(|id| id.to_string())
            })
            .next()
            .expect("should find a reasoning item_id (rs_...) in the SSE output");

        // Verify the signature from the first (failed) response is in the cache.
        let cached_sig = sig_cache.get(&reasoning_id);
        assert_eq!(
            cached_sig,
            Some("rs_test_signature_123".to_string()),
            "signature from the first (failed) response should be cached under reasoning_id {}",
            reasoning_id
        );
    }
}
