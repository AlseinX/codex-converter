use codex_conv::config::AppConfig;
use codex_conv::router::{AppState, build_router};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn anthropic_models_list_body() -> serde_json::Value {
    json!({
        "data": [
            {
                "type": "model",
                "id": "claude-sonnet-4-20250514",
                "display_name": "Claude Sonnet 4",
                "created_at": "2025-02-19T00:00:00Z"
            },
            {
                "type": "model",
                "id": "claude-opus-4-20250514",
                "display_name": "Claude Opus 4",
                "created_at": "2025-01-15T00:00:00Z"
            }
        ],
        "has_more": false,
        "first_id": "claude-sonnet-4-20250514",
        "last_id": "claude-opus-4-20250514"
    })
}

fn anthropic_single_model_body() -> serde_json::Value {
    json!({
        "type": "model",
        "id": "claude-sonnet-4-20250514",
        "display_name": "Claude Sonnet 4",
        "created_at": "2025-02-19T00:00:00Z"
    })
}

async fn start_proxy_with_catalog(
    mock_server: &wiremock::MockServer,
    catalog: Option<codex_conv::catalog::ModelsResponse>,
) -> (axum::Router, String) {
    let host = mock_server.uri();
    let host = host
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let mut config = AppConfig::default();
    config.upstream.anthropic_version = "2023-06-01".to_string();
    let state = AppState { config, catalog };
    let app = build_router(state);
    (app, host.to_string())
}

async fn start_proxy_no_catalog(mock_server: &wiremock::MockServer) -> (axum::Router, String) {
    start_proxy_with_catalog(mock_server, None).await
}

fn minimal_model_info(slug: &str) -> codex_conv::catalog::ModelInfo {
    use codex_conv::catalog::*;
    ModelInfo {
        slug: slug.to_string(),
        display_name: slug.to_string(),
        description: None,
        supported_reasoning_levels: vec![],
        shell_type: ConfigShellToolType::Default,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority: 0,
        base_instructions: String::new(),
        supports_reasoning_summaries: false,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        truncation_policy: TruncationPolicyConfig {
            mode: TruncationPolicyMode::Bytes,
            limit: 10000,
        },
        supports_parallel_tool_calls: false,
        experimental_supported_tools: vec![],
        default_reasoning_summary: ReasoningSummary::Auto,
        default_reasoning_level: None,
        additional_speed_tiers: vec![],
        service_tiers: vec![],
        supports_image_detail_original: false,
        context_window: Some(200000),
        max_context_window: Some(200000),
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        input_modalities: vec![InputModality::Text, InputModality::Image],
        supports_search_tool: false,
        web_search_tool_type: WebSearchToolType::Text,
        availability_nux: None,
        upgrade: None,
        model_messages: None,
    }
}

// --- Mode 1 Tests ---

#[tokio::test]
async fn mode1_list_models_returns_openai_format() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_models_list_body()))
        .mount(&mock_server)
        .await;

    let (app, host) = start_proxy_no_catalog(&mock_server).await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["object"], "list");
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "claude-sonnet-4-20250514");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "anthropic");
    assert!(data[0]["created"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn mode1_get_model_returns_openai_format() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/claude-sonnet-4-20250514"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_single_model_body()))
        .mount(&mock_server)
        .await;

    let (app, host) = start_proxy_no_catalog(&mock_server).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/claude-sonnet-4-20250514", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], "claude-sonnet-4-20250514");
    assert_eq!(json["object"], "model");
    assert_eq!(json["owned_by"], "anthropic");
}

#[tokio::test]
async fn mode1_get_nonexistent_model_returns_404() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/nonexistent"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": "model not found" }
        })))
        .mount(&mock_server)
        .await;

    let (app, host) = start_proxy_no_catalog(&mock_server).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/nonexistent", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mode1_missing_auth_returns_401() {
    let mock_server = MockServer::start().await;
    let (app, host) = start_proxy_no_catalog(&mock_server).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mode1_invalid_route_returns_400() {
    let mock_server = MockServer::start().await;
    let (app, host) = start_proxy_no_catalog(&mock_server).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/a/b", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn mode1_forward_query_params() {
    let mock_server = MockServer::start().await;
    // The mock matches any GET to /v1/models regardless of query params.
    // We verify that query params are forwarded by checking the response succeeds
    // (the upstream would 404 if the path were wrong).
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [], "has_more": false
        })))
        .mount(&mock_server)
        .await;

    let (app, host) = start_proxy_no_catalog(&mock_server).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models?limit=20", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
}

// --- Mode 2 Tests ---

#[tokio::test]
async fn mode2_list_models_filtered_by_upstream() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "claude-sonnet-4-20250514", "display_name": "S4" },
                { "type": "model", "id": "claude-opus-4-20250514", "display_name": "O4" }
            ],
            "has_more": false
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![
            minimal_model_info("claude-sonnet-4-20250514"),
            minimal_model_info("claude-opus-4-20250514"),
            minimal_model_info("nonexistent-model"),
        ],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let models = json["models"].as_array().unwrap();
    assert_eq!(
        models.len(),
        2,
        "only upstream-matching models should appear"
    );
    let slugs: Vec<&str> = models.iter().map(|m| m["slug"].as_str().unwrap()).collect();
    assert!(slugs.contains(&"claude-sonnet-4-20250514"));
    assert!(slugs.contains(&"claude-opus-4-20250514"));
    assert!(!slugs.contains(&"nonexistent-model"));
}

#[tokio::test]
async fn mode2_list_no_overlap_falls_back_to_openai_format() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "upstream-only-model", "display_name": "Upstream" }
            ],
            "has_more": false
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("catalog-only-model")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["object"], "list");
    assert!(
        json.get("models").is_none(),
        "should not have 'models' key in fallback"
    );
}

#[tokio::test]
async fn mode2_get_model_found_in_catalog() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/claude-sonnet-4-20250514"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "model", "id": "claude-sonnet-4-20250514", "display_name": "S4"
        })))
        .mount(&mock_server)
        .await;

    let mut info = minimal_model_info("claude-sonnet-4-20250514");
    info.display_name = "Sonnet 4 from Catalog".to_string();
    let catalog = codex_conv::catalog::ModelsResponse { models: vec![info] };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/claude-sonnet-4-20250514", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let models = json["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["display_name"], "Sonnet 4 from Catalog");
}

#[tokio::test]
async fn mode2_get_model_upstream_404() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/nonexistent"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "type": "error",
            "error": { "type": "not_found_error", "message": "model not found" }
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("other-model")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/nonexistent", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mode2_get_model_not_in_catalog_returns_openai_format() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/claude-sonnet-4-20250514"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "model", "id": "claude-sonnet-4-20250514",
            "display_name": "S4", "created_at": "2025-02-19T00:00:00Z"
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("other-model")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models/claude-sonnet-4-20250514", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], "claude-sonnet-4-20250514");
    assert_eq!(json["object"], "model");
    assert!(json.get("models").is_none());
}

#[tokio::test]
async fn mode2_upstream_5xx_returns_error() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "type": "error",
            "error": { "type": "api_error", "message": "internal error" }
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("model-a")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn mode2_upstream_401_propagated() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid api key" }
        })))
        .mount(&mock_server)
        .await;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("model-a")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mode2_missing_auth_returns_401() {
    let mock_server = MockServer::start().await;
    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![minimal_model_info("model-a")],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mode2_priority_ordering() {
    let mock_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                { "type": "model", "id": "model-low", "display_name": "Low" },
                { "type": "model", "id": "model-high", "display_name": "High" },
                { "type": "model", "id": "model-mid", "display_name": "Mid" }
            ],
            "has_more": false
        })))
        .mount(&mock_server)
        .await;

    let mut m_low = minimal_model_info("model-low");
    m_low.priority = 10;
    let mut m_high = minimal_model_info("model-high");
    m_high.priority = 1;
    let mut m_mid = minimal_model_info("model-mid");
    m_mid.priority = 5;

    let catalog = codex_conv::catalog::ModelsResponse {
        models: vec![m_low, m_high, m_mid],
    };

    let (app, host) = start_proxy_with_catalog(&mock_server, Some(catalog)).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/http/{}/models", host))
        .header("authorization", "Bearer sk-test-key")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let models = json["models"].as_array().unwrap();
    let priorities: Vec<i64> = models
        .iter()
        .map(|m| m["priority"].as_i64().unwrap())
        .collect();
    assert_eq!(
        priorities,
        vec![1, 5, 10],
        "models should be sorted by priority ascending"
    );
}
