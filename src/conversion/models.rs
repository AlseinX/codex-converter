use serde_json::{Value, json};

pub fn anthropic_to_openai_model(anthropic_model: &Value) -> Value {
    let id = anthropic_model
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Upstream may return "created" as an integer (already OpenAI format) or
    // "created_at" as an ISO 8601 string (Anthropic native format).
    let created = if let Some(v) = anthropic_model.get("created") {
        v.as_i64().unwrap_or(0)
    } else {
        anthropic_model
            .get("created_at")
            .and_then(|v| v.as_str())
            .map(parse_iso8601_to_epoch)
            .unwrap_or(0)
    };

    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": "anthropic"
    })
}

pub fn anthropic_list_to_openai_list(anthropic_body: &Value) -> Value {
    let models = anthropic_body
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(anthropic_to_openai_model)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if anthropic_body
        .get("has_more")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        tracing::warn!("Anthropic model list is truncated (has_more=true)");
    }

    json!({
        "object": "list",
        "data": models
    })
}

fn parse_iso8601_to_epoch(s: &str) -> i64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp();
    }
    tracing::warn!(input = %s, "failed to parse ISO 8601 timestamp, using 0");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_model_conversion() {
        let anthropic = json!({
            "type": "model",
            "id": "claude-sonnet-4-20250514",
            "display_name": "Claude Sonnet 4",
            "created_at": "2025-02-19T00:00:00Z"
        });
        let openai = anthropic_to_openai_model(&anthropic);
        assert_eq!(openai["id"], "claude-sonnet-4-20250514");
        assert_eq!(openai["object"], "model");
        assert_eq!(openai["owned_by"], "anthropic");
        assert!(openai["created"].is_i64());
        assert!(openai["created"].as_i64().unwrap() > 0);
    }

    #[test]
    fn timestamp_valid_iso8601() {
        let epoch = parse_iso8601_to_epoch("2025-02-19T00:00:00Z");
        assert!(epoch > 0, "valid ISO 8601 should produce non-zero epoch");
    }

    #[test]
    fn timestamp_invalid_string_returns_zero() {
        let epoch = parse_iso8601_to_epoch("not-a-date");
        assert_eq!(epoch, 0, "invalid timestamp should return 0");
    }

    #[test]
    fn list_conversion_wraps_in_object_list() {
        let anthropic = json!({
            "data": [
                { "type": "model", "id": "m1", "display_name": "M1", "created_at": "2025-01-01T00:00:00Z" },
                { "type": "model", "id": "m2", "display_name": "M2", "created_at": "2025-06-01T00:00:00Z" }
            ],
            "has_more": false
        });
        let openai = anthropic_list_to_openai_list(&anthropic);
        assert_eq!(openai["object"], "list");
        let data = openai["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "m1");
        assert_eq!(data[1]["id"], "m2");
    }

    #[test]
    fn model_missing_fields_uses_defaults() {
        let anthropic = json!({});
        let openai = anthropic_to_openai_model(&anthropic);
        assert_eq!(openai["id"], "");
        assert_eq!(openai["created"], 0);
        assert_eq!(openai["owned_by"], "anthropic");
    }

    #[test]
    fn display_name_discarded() {
        let anthropic = json!({
            "type": "model",
            "id": "m1",
            "display_name": "Should Not Appear",
            "created_at": "2025-01-01T00:00:00Z"
        });
        let openai = anthropic_to_openai_model(&anthropic);
        assert!(openai.get("display_name").is_none());
    }
}
