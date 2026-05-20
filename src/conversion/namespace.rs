use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Registry for MCP namespace flattening.
///
/// Maps flattened tool names (e.g., "mcp__memory__search") to their
/// original (namespace, tool_name) components.
#[derive(Debug, Default, Clone)]
pub struct NamespaceRegistry {
    /// flat_name -> (namespace, tool_name)
    entries: HashMap<String, NamespaceEntry>,
}

#[derive(Debug, Clone)]
pub struct NamespaceEntry {
    pub namespace: String,
    pub tool_name: String,
}

impl NamespaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Register a flattened tool name with its original components.
    pub fn register(&mut self, flat_name: String, namespace: String, tool_name: String) {
        self.entries.insert(
            flat_name,
            NamespaceEntry {
                namespace,
                tool_name,
            },
        );
    }

    /// Look up a flattened tool name to get its original components.
    pub fn lookup(&self, flat_name: &str) -> Option<&NamespaceEntry> {
        self.entries.get(flat_name)
    }

    /// Flatten an MCP tool name: `mcp__{server}__{tool}`.
    /// Applies truncation + hash suffix for names exceeding 64 characters.
    /// Returns the flattened name. Also registers the truncated alias if truncation was applied.
    ///
    /// `namespace` should include the `mcp__` prefix and trailing separator,
    /// e.g. `"mcp__memory__"`. The flat name is formed as `{namespace}{tool_name}`
    /// (trailing underscores on namespace are preserved).
    pub fn flatten_and_register(&mut self, namespace: &str, tool_name: &str) -> String {
        // Trim trailing underscores from namespace (Codex sends "mcp__memory__").
        let ns = namespace.trim_end_matches('_');
        let flat = format!("{}__{}", ns, tool_name);

        let registered = if flat.len() <= 64 {
            flat.clone()
        } else {
            // Truncate + 12 hex char hash suffix.
            truncate_and_hash(&flat)
        };

        self.register(
            registered.clone(),
            namespace.to_string(),
            tool_name.to_string(),
        );

        // Also register the original (un-truncated) name so lookups work both ways.
        if registered != flat {
            self.register(flat, namespace.to_string(), tool_name.to_string());
        }

        registered
    }
}

/// Truncate a tool name to fit within 64 characters, appending a 12-char hex hash.
/// Format: first 51 chars + "_" + 12 hex chars = 64 chars total.
fn truncate_and_hash(name: &str) -> String {
    let hash = Sha256::digest(name.as_bytes());
    let suffix = hex::encode(&hash[..6]); // 12 hex chars

    // 64 - 1 (underscore) - 12 (hash) = 51 chars of the original name.
    let max_prefix = 64 - 1 - 12;
    let prefix: String = name.chars().take(max_prefix).collect();
    format!("{}_{}", prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_basic() {
        let mut reg = NamespaceRegistry::new();
        let flat = reg.flatten_and_register("mcp__memory__", "search");
        assert_eq!(flat, "mcp__memory__search");
    }

    #[test]
    fn flatten_long_name_truncated() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        let flat = reg.flatten_and_register("mcp__svr__", &long_tool);
        assert!(
            flat.len() <= 64,
            "truncated name must be <= 64 chars, got {}",
            flat.len()
        );
        assert!(flat.starts_with("mcp__svr__a"));
    }

    #[test]
    fn lookup_truncated_name() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        let flat = reg.flatten_and_register("mcp__svr__", &long_tool);
        let entry = reg.lookup(&flat).unwrap();
        assert_eq!(entry.namespace, "mcp__svr__");
        assert_eq!(entry.tool_name, long_tool);
    }

    #[test]
    fn lookup_original_name_also_works() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        reg.flatten_and_register("mcp__svr__", &long_tool);
        // The original (un-truncated) name should also be in the registry.
        let original = format!("mcp__svr__{}", long_tool);
        let entry = reg.lookup(&original).unwrap();
        assert_eq!(entry.tool_name, long_tool);
    }
}
