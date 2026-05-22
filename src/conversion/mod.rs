pub mod content;
pub mod error;
pub mod id_map;
pub mod models;
pub mod namespace;
pub mod request;
pub mod response;
pub mod signature_cache;
pub mod thinking;

pub use id_map::IdMap;
pub use namespace::NamespaceRegistry;
pub use signature_cache::SignatureCache;

/// Per-request conversion state machine.
///
/// Lifecycle:
/// 1. `new()` — create with upstream base URL.
/// 2. `convert_request()` — parse Responses API request, build namespace registry, ID maps,
///    convert to Anthropic Messages API request. (Task 9)
/// 3. Stream SSE events from upstream, converting each in real-time. (Part 2)
/// 4. On stream end, write accumulated signatures to signature cache. (Part 2)
/// 5. Drop — all in-flight state released.
pub struct ConversionTask {
    /// The upstream Anthropic API base URL (no /v1).
    pub upstream_base_url: String,

    /// Bidirectional ID map: call_id <-> toolu_id.
    pub id_map: IdMap,

    /// Namespace registry: flat_name -> (namespace, tool_name).
    pub namespace_registry: NamespaceRegistry,

    /// Shared signature cache reference (TTL-based, shared across tasks).
    pub signature_cache: std::sync::Arc<SignatureCache>,
}

impl ConversionTask {
    /// Create a new conversion task for the given upstream base URL.
    pub fn new(upstream_base_url: String) -> Self {
        Self {
            upstream_base_url,
            id_map: IdMap::new(),
            namespace_registry: NamespaceRegistry::new(),
            signature_cache: std::sync::Arc::new(SignatureCache::new(
                std::time::Duration::from_secs(3 * 3600),
            )),
        }
    }

    /// Create with a shared signature cache.
    pub fn with_signature_cache(
        upstream_base_url: String,
        signature_cache: std::sync::Arc<SignatureCache>,
    ) -> Self {
        Self {
            upstream_base_url,
            id_map: IdMap::new(),
            namespace_registry: NamespaceRegistry::new(),
            signature_cache,
        }
    }

    /// Full upstream URL for the Anthropic Messages API endpoint.
    pub fn upstream_messages_url(&self) -> String {
        format!("{}/v1/messages", self.upstream_base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_has_empty_state() {
        let task = ConversionTask::new("https://api.anthropic.com".to_string());
        assert!(task.upstream_base_url == "https://api.anthropic.com");
        assert!(task.id_map.is_empty());
        assert!(task.namespace_registry.is_empty());
    }

    #[test]
    fn id_map_round_trip() {
        let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
        let toolu = task.id_map.insert_call("call_001".to_string());
        assert!(toolu.starts_with("toolu_"));
        let resolved = task.id_map.get_toolu_for_call("call_001").unwrap();
        assert_eq!(resolved, &toolu);
        let call = task.id_map.get_call_for_toolu(&toolu).unwrap();
        assert_eq!(call, "call_001");
    }

    #[test]
    fn namespace_register_and_lookup() {
        let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
        task.namespace_registry.register(
            "mcp__memory__search".to_string(),
            "mcp__memory__".to_string(),
            "search".to_string(),
        );
        let entry = task
            .namespace_registry
            .lookup("mcp__memory__search")
            .unwrap();
        assert_eq!(entry.namespace, "mcp__memory__");
        assert_eq!(entry.tool_name, "search");
    }

    #[test]
    fn upstream_messages_url() {
        let task = ConversionTask::new("https://api.anthropic.com".to_string());
        assert_eq!(
            task.upstream_messages_url(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn with_shared_signature_cache() {
        let cache = std::sync::Arc::new(SignatureCache::new(std::time::Duration::from_secs(60)));
        let task = ConversionTask::with_signature_cache(
            "https://api.example.com".to_string(),
            std::sync::Arc::clone(&cache),
        );
        assert!(task.upstream_base_url == "https://api.example.com");
        assert!(task.id_map.is_empty());
        assert!(task.namespace_registry.is_empty());
        // Verify the shared cache reference
        task.signature_cache
            .insert("rs_test".to_string(), "sig123".to_string());
        assert_eq!(cache.get("rs_test"), Some("sig123".to_string()));
    }
}
