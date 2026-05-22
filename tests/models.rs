//! Live integration tests for Models API: HTTP client → proxy → real Anthropic API.
//!
//! Each test starts its own in-process proxy and sends HTTP requests directly to it.
//! The proxy forwards to the real Anthropic API using credentials from
//! `~/.claude/settings.json` or environment variables.
//!
//! Run with: `cargo test --test models -- --test-threads=1`

use std::net::SocketAddr;
use std::path::PathBuf;

use codex_conv::catalog::{
    ConfigShellToolType, InputModality, ModelInfo, ModelVisibility, ModelsResponse,
    ReasoningSummary, TruncationPolicyConfig, TruncationPolicyMode, WebSearchToolType,
};
use codex_conv::config::AppConfig;
use codex_conv::router::{AppState, build_router};

// ---------------------------------------------------------------------------
// Credentials (shared with integrations.rs)
// ---------------------------------------------------------------------------

fn get_credential(settings_key: &str, env_name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_name)
        && !v.is_empty()
    {
        return Some(v);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".claude/settings.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&content).ok()?;
    settings
        .get("env")
        .and_then(|e| e.get(settings_key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn api_key() -> String {
    get_credential("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN")
        .expect("ANTHROPIC_AUTH_TOKEN not found in ~/.claude/settings.json or env")
}

fn base_url() -> String {
    get_credential("ANTHROPIC_BASE_URL", "ANTHROPIC_BASE_URL")
        .expect("ANTHROPIC_BASE_URL not found in ~/.claude/settings.json or env")
}

fn upstream_host() -> String {
    let url = base_url();
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// A model ID that exists on the real upstream API.
const KNOWN_MODEL: &str = "glm-5.1";

// ---------------------------------------------------------------------------
// In-process proxy helpers
// ---------------------------------------------------------------------------

async fn start_proxy(catalog: Option<ModelsResponse>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind proxy listener");
    let addr = listener.local_addr().unwrap();

    let config = AppConfig::default();
    let state = AppState { config, catalog };
    let app = build_router(state);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {e}");
        }
    });

    (addr, handle)
}

fn minimal_model_info(slug: &str) -> ModelInfo {
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

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build HTTP client")
}

fn models_list_url(proxy_addr: SocketAddr) -> String {
    let host = upstream_host();
    format!("http://{}/https/{}/models", proxy_addr, host)
}

fn models_get_url(proxy_addr: SocketAddr, model_id: &str) -> String {
    let host = upstream_host();
    format!("http://{}/https/{}/models/{}", proxy_addr, host, model_id)
}

// ===========================================================================
// Mode 1 Tests (no catalog — standard OpenAI format conversion)
// ===========================================================================

#[tokio::test]
async fn mode1_list_models_returns_openai_format() {
    let (addr, _proxy) = start_proxy(None).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_list_url(addr))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let json: serde_json::Value = resp.json().await.expect("failed to parse response");
    assert_eq!(json["object"], "list", "response must have object=list");

    let data = json["data"]
        .as_array()
        .expect("response must have data array");
    assert!(
        !data.is_empty(),
        "upstream should return at least one model"
    );

    let first = &data[0];
    assert!(first.get("id").is_some(), "each model must have 'id'");
    assert_eq!(first["object"], "model");
    assert_eq!(first["owned_by"], "anthropic");
    assert!(
        first["created"].as_i64().unwrap_or(0) > 0,
        "created must be a positive Unix epoch timestamp"
    );
}

#[tokio::test]
async fn mode1_list_models_preserves_all_upstream_models() {
    let (addr, _proxy) = start_proxy(None).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_list_url(addr))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = resp.json().await.expect("failed to parse response");
    let data = json["data"].as_array().expect("must have data array");

    // Collect all IDs and verify known model exists
    let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(
        ids.iter().any(|id| id.contains("glm")),
        "upstream should return glm models, got: {ids:?}"
    );
}

#[tokio::test]
async fn mode1_get_nonexistent_model_returns_404() {
    let (addr, _proxy) = start_proxy(None).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_get_url(addr, "this-model-does-not-exist-xyz-12345"))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mode1_missing_auth_returns_401() {
    let (addr, _proxy) = start_proxy(None).await;
    let client = http_client();

    let resp = client
        .get(models_list_url(addr))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mode1_invalid_route_returns_400() {
    let (addr, _proxy) = start_proxy(None).await;
    let client = http_client();
    let key = api_key();
    let host = upstream_host();

    let url = format!("http://{}/https/{}/models/a/b", addr, host);
    let resp = client
        .get(&url) // & needed for format!()-produced String
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Mode 2 Tests (with catalog — filtered Codex catalog format)
// ===========================================================================

#[tokio::test]
async fn mode2_list_models_filtered_by_upstream() {
    // Use a model that exists on the real upstream and one that doesn't.
    let catalog = ModelsResponse {
        models: vec![
            minimal_model_info(KNOWN_MODEL),
            minimal_model_info("this-does-not-exist-on-upstream"),
        ],
    };

    let (addr, _proxy) = start_proxy(Some(catalog)).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_list_url(addr))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let json: serde_json::Value = resp.json().await.expect("failed to parse response");
    let models = json["models"]
        .as_array()
        .expect("Mode 2 response must have 'models' array");

    let slugs: Vec<&str> = models.iter().map(|m| m["slug"].as_str().unwrap()).collect();
    assert!(
        slugs.contains(&KNOWN_MODEL),
        "{KNOWN_MODEL} should appear (exists on upstream)"
    );
    assert!(
        !slugs.contains(&"this-does-not-exist-on-upstream"),
        "nonexistent model must be filtered out"
    );
}

#[tokio::test]
async fn mode2_list_no_overlap_falls_back_to_openai_format() {
    // Catalog contains only models that don't exist on upstream.
    let catalog = ModelsResponse {
        models: vec![minimal_model_info("catalog-only-nonexistent-model")],
    };

    let (addr, _proxy) = start_proxy(Some(catalog)).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_list_url(addr))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let json: serde_json::Value = resp.json().await.expect("failed to parse response");
    // With zero overlap, proxy should fall back to standard OpenAI list format.
    assert_eq!(json["object"], "list");
    assert!(
        json.get("models").is_none(),
        "should not have 'models' key in fallback"
    );
}

#[tokio::test]
async fn mode2_get_nonexistent_model_returns_404() {
    let catalog = ModelsResponse {
        models: vec![minimal_model_info(KNOWN_MODEL)],
    };

    let (addr, _proxy) = start_proxy(Some(catalog)).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_get_url(addr, "this-model-does-not-exist-xyz-12345"))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mode2_missing_auth_returns_401() {
    let catalog = ModelsResponse {
        models: vec![minimal_model_info(KNOWN_MODEL)],
    };

    let (addr, _proxy) = start_proxy(Some(catalog)).await;
    let client = http_client();

    let resp = client
        .get(models_list_url(addr))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mode2_priority_ordering() {
    // Use real model IDs to ensure overlap with upstream.
    let mut m_low = minimal_model_info(KNOWN_MODEL);
    m_low.priority = 10;
    let mut m_high = minimal_model_info("glm-4.7");
    m_high.priority = 1;

    let catalog = ModelsResponse {
        models: vec![m_low, m_high],
    };

    let (addr, _proxy) = start_proxy(Some(catalog)).await;
    let client = http_client();
    let key = api_key();

    let resp = client
        .get(models_list_url(addr))
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let json: serde_json::Value = resp.json().await.expect("failed to parse response");
    let models = json["models"].as_array().expect("must have models array");

    let priorities: Vec<i64> = models
        .iter()
        .map(|m| m["priority"].as_i64().unwrap())
        .collect();
    assert_eq!(
        priorities,
        vec![1, 10],
        "models should be sorted by priority ascending"
    );
}
