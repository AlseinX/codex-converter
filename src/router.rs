use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::Router;

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

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: crate::config::AppConfig,
}

/// Build the main axum router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Catch-all route that accepts any path ending in /responses.
        .route("/*path", post(handle_responses))
        .with_state(state)
}

/// Main handler for Responses API requests.
async fn handle_responses(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, (StatusCode, axum::Json<serde_json::Value>)> {
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

    tracing::debug!(path = %path, upstream = %route_info.upstream_base_url, "routed request");

    // TODO (Task 5+): Create ConversionTask and delegate.
    let _ = (state, route_info);
    Ok(Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Body::from("not yet implemented"))
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
}
