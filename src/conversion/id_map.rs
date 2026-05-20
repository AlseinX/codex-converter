use std::collections::HashMap;
use uuid::Uuid;

/// Bidirectional ID map between OpenAI call_id and Anthropic toolu_id.
///
/// - OpenAI uses `call_xxx` (or `ws_xxx`, `fs_xxx`, etc. for built-in tools).
/// - Anthropic uses `toolu_xxx`.
/// - The proxy generates `toolu_` prefixed IDs and maintains the mapping.
#[derive(Debug, Default)]
pub struct IdMap {
    call_to_toolu: HashMap<String, String>,
    toolu_to_call: HashMap<String, String>,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.call_to_toolu.len()
    }

    /// Register a call_id and generate a new toolu_id for it.
    /// Returns the generated toolu_id.
    pub fn insert_call(&mut self, call_id: String) -> String {
        let toolu_id = format!("toolu_{}", Uuid::new_v4().simple());
        self.call_to_toolu
            .insert(call_id.clone(), toolu_id.clone());
        self.toolu_to_call.insert(toolu_id.clone(), call_id);
        toolu_id
    }

    /// Register a mapping with an explicit toolu_id (for built-in tool IDs like ws_xxx -> toolu_ws_xxx).
    pub fn insert_with_toolu(&mut self, call_id: String, toolu_id: String) {
        self.call_to_toolu
            .insert(call_id.clone(), toolu_id.clone());
        self.toolu_to_call.insert(toolu_id, call_id);
    }

    /// Look up the toolu_id for a given call_id.
    pub fn get_toolu_for_call(&self, call_id: &str) -> Option<&String> {
        self.call_to_toolu.get(call_id)
    }

    /// Look up the call_id for a given toolu_id.
    pub fn get_call_for_toolu(&self, toolu_id: &str) -> Option<&String> {
        self.toolu_to_call.get(toolu_id)
    }
}
