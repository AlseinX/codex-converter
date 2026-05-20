use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// TTL-based cache for Anthropic thinking signatures.
///
/// Keyed by proxy-generated reasoning item ID (`rs_xxx`).
/// Shared across all conversion tasks via `Arc<SignatureCache>`.
pub struct SignatureCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

struct CacheEntry {
    signature: String,
    expires_at: Instant,
}

impl SignatureCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Store a signature for the given reasoning item ID.
    pub fn insert(&self, reasoning_id: String, signature: String) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            reasoning_id,
            CacheEntry {
                signature,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Retrieve a signature for the given reasoning item ID.
    /// Returns None if not found or expired.
    pub fn get(&self, reasoning_id: &str) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get(reasoning_id)?;
        if Instant::now() > entry.expires_at {
            entries.remove(reasoning_id);
            None
        } else {
            Some(entry.signature.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_retrieve() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_abc".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_abc".to_string()));
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = SignatureCache::new(Duration::from_millis(1));
        cache.insert("rs_001".to_string(), "sig_abc".to_string());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get("rs_001"), None);
    }

    #[test]
    fn missing_entry_returns_none() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        assert_eq!(cache.get("rs_nonexistent"), None);
    }

    #[test]
    fn overwrite_existing() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_old".to_string());
        cache.insert("rs_001".to_string(), "sig_new".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_new".to_string()));
    }

    #[test]
    fn multiple_entries_independent() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_a".to_string());
        cache.insert("rs_002".to_string(), "sig_b".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_a".to_string()));
        assert_eq!(cache.get("rs_002"), Some("sig_b".to_string()));
    }
}
