use serde_json::{json, Value};

/// Result of reasoning -> thinking conversion.
pub struct ThinkingConfig {
    /// The `thinking` field for the Anthropic request (if reasoning was present and effort != "none").
    pub thinking: Option<Value>,
    /// The `output_config.effort` value (if reasoning was present and effort != "none").
    pub effort: Option<String>,
    /// Whether thinking is enabled (controls temperature omission).
    pub thinking_enabled: bool,
}

/// Convert Responses API `reasoning` field to Anthropic `thinking` + `output_config.effort`.
///
/// - `reasoning.effort` -> `output_config.effort` (direct string forwarding, with mappings)
/// - `reasoning.summary` -> `thinking.display`
/// - `reasoning.effort: "none"` -> omit both `thinking` and `output_config`
/// - `reasoning` absent -> omit both
pub fn convert_reasoning(reasoning: Option<&Value>) -> ThinkingConfig {
    let Some(reasoning) = reasoning else {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    };

    let effort_val = reasoning
        .get("effort")
        .and_then(|v| v.as_str())
        .unwrap_or("high");
    let summary_val = reasoning.get("summary").and_then(|v| v.as_str());

    // effort "none" means no thinking.
    if effort_val == "none" {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    }

    // Map effort value.
    let mapped_effort = match effort_val {
        "minimal" => "low", // Anthropic has no "minimal" level.
        other => other,     // "low", "medium", "high", "xhigh" pass through directly.
    };

    // Map summary to thinking.display.
    let display = match summary_val {
        Some("none") => "omitted",
        _ => "summarized", // absent, "auto", "concise", "detailed" all map to "summarized".
    };

    ThinkingConfig {
        thinking: Some(json!({
            "type": "adaptive",
            "display": display,
        })),
        effort: Some(mapped_effort.to_string()),
        thinking_enabled: true,
    }
}

/// Determine the Anthropic content block for a reasoning input item.
///
/// - If `encrypted_content` is present -> `redacted_thinking`
/// - Otherwise -> `thinking` with summary text and cached or empty signature
pub fn convert_reasoning_input(
    summary: &[Value],
    encrypted_content: Option<&str>,
    signature_cache: &crate::conversion::SignatureCache,
    reasoning_id: &str,
) -> Value {
    if let Some(data) = encrypted_content {
        json!({
            "type": "redacted_thinking",
            "data": data
        })
    } else {
        // Concatenate summary texts.
        let thinking_text = summary
            .iter()
            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");

        let signature = signature_cache.get(reasoning_id).unwrap_or_default();

        json!({
            "type": "thinking",
            "thinking": thinking_text,
            "signature": signature
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reasoning_returns_empty_config() {
        let config = convert_reasoning(None);
        assert!(config.thinking.is_none());
        assert!(config.effort.is_none());
        assert!(!config.thinking_enabled);
    }

    #[test]
    fn reasoning_high_effort_auto_summary() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "auto"})));
        assert!(config.thinking_enabled);
        assert_eq!(config.effort.as_deref(), Some("high"));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["type"], "adaptive");
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_effort_none_omits_all() {
        let config = convert_reasoning(Some(&json!({"effort": "none"})));
        assert!(config.thinking.is_none());
        assert!(config.effort.is_none());
        assert!(!config.thinking_enabled);
    }

    #[test]
    fn reasoning_summary_none_display_omitted() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "none"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "omitted");
    }

    #[test]
    fn reasoning_minimal_maps_to_low() {
        let config = convert_reasoning(Some(&json!({"effort": "minimal", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("low"));
    }

    #[test]
    fn reasoning_summary_absent_defaults_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "medium"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_summary_concise_maps_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "concise"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_summary_detailed_maps_to_summarized() {
        let config = convert_reasoning(Some(&json!({"effort": "high", "summary": "detailed"})));
        let thinking = config.thinking.unwrap();
        assert_eq!(thinking["display"], "summarized");
    }

    #[test]
    fn reasoning_xhigh_passthrough() {
        let config = convert_reasoning(Some(&json!({"effort": "xhigh", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn reasoning_low_passthrough() {
        let config = convert_reasoning(Some(&json!({"effort": "low", "summary": "auto"})));
        assert_eq!(config.effort.as_deref(), Some("low"));
    }

    #[test]
    fn reasoning_input_encrypted_content_becomes_redacted_thinking() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "some summary"})],
            Some("ENCRYPTED_DATA"),
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "redacted_thinking");
        assert_eq!(result["data"], "ENCRYPTED_DATA");
    }

    #[test]
    fn reasoning_input_summary_only_empty_signature() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "I thought about it"})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["thinking"], "I thought about it");
        assert_eq!(result["signature"], "");
    }

    #[test]
    fn reasoning_input_summary_cached_signature() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "cached_sig_123".to_string());
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "thinking..."})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["signature"], "cached_sig_123");
    }

    #[test]
    fn reasoning_input_multiple_summaries_concatenated() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(
            &[
                json!({"type": "summary_text", "text": "part1"}),
                json!({"type": "summary_text", "text": "part2"}),
            ],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["thinking"], "part1part2");
    }

    #[test]
    fn reasoning_input_empty_summary() {
        let cache = crate::conversion::SignatureCache::new(std::time::Duration::from_secs(3600));
        let result = convert_reasoning_input(&[], None, &cache, "rs_001");
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["thinking"], "");
    }
}
