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
