use serde_json::{json, Value};

/// Convert a Responses API user content block to an Anthropic content block.
///
/// Supported types:
/// - `input_text` -> `text`
/// - `input_image` with URL string -> `image` with `url` source
/// - `input_image` with data URI -> `image` with `base64` source
/// - `input_file` -> dropped (no Anthropic equivalent)
///
/// `cache_control` is passed through if present.
///
/// Returns `None` for unsupported types (caller should skip or log).
pub fn convert_user_content(input: &Value) -> Option<Value> {
    let obj = input.as_object()?;
    let content_type = obj.get("type")?.as_str()?;

    let mut block = match content_type {
        "input_text" => {
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            json!({"type": "text", "text": text})
        }
        "input_image" => convert_input_image(obj)?,
        "input_file" => {
            // Anthropic has no generic file input. Drop silently.
            tracing::debug!("dropping input_file content block (no Anthropic equivalent)");
            return None;
        }
        other => {
            tracing::warn!(
                content_type = other,
                "dropping unknown user content block type"
            );
            return None;
        }
    };

    // Pass through cache_control if present.
    if let Some(cc) = obj.get("cache_control") {
        block
            .as_object_mut()
            .unwrap()
            .insert("cache_control".to_string(), cc.clone());
    }

    Some(block)
}

/// Convert an `input_image` content block to Anthropic `image` block.
fn convert_input_image(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let image_url_field = obj.get("image_url")?;

    // Case 1: image_url is a plain string (URL).
    if let Some(url_str) = image_url_field.as_str() {
        return Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url_str
            }
        }));
    }

    // Case 2: image_url is an object with a "url" field (data URI).
    let url_value = image_url_field.get("url")?.as_str()?;

    if let Some(rest) = url_value.strip_prefix("data:") {
        // Parse data URI: "data:image/png;base64,<data>"
        let parts: Vec<&str> = rest.splitn(2, ',').collect();
        if parts.len() != 2 {
            tracing::warn!("malformed data URI in input_image");
            return None;
        }
        let mime = parts[0].trim_end_matches(";base64");
        let data = parts[1];

        Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": mime,
                "data": data
            }
        }))
    } else {
        // Not a data URI, treat as regular URL.
        Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url_value
            }
        }))
    }
}

/// Convert an array of Responses API user content blocks to Anthropic content blocks.
/// Skips unsupported types with a warning log.
pub fn convert_user_content_array(inputs: &[Value]) -> Vec<Value> {
    let mut result = Vec::with_capacity(inputs.len());
    for input in inputs {
        if let Some(block) = convert_user_content(input) {
            result.push(block);
        }
    }
    result
}

/// Convert `tool_result.content` from function_call_output.
///
/// The output can be:
/// - A plain string -> Anthropic `tool_result.content` as plain string.
/// - An array of content items -> Anthropic `tool_result.content` as array of content blocks.
pub fn convert_tool_result_content(output: &Value) -> Value {
    if output.is_string() {
        output.clone()
    } else if output.is_array() {
        let blocks: Vec<Value> = output
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| convert_user_content(item))
            .collect();
        Value::Array(blocks)
    } else {
        // Fallback: serialize as JSON string.
        json!(output.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_text_to_text() {
        let input = json!({"type": "input_text", "text": "Hello"});
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "Hello");
    }

    #[test]
    fn input_image_url_to_image_url_source() {
        let input = json!({
            "type": "input_image",
            "image_url": "https://example.com/image.png"
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://example.com/image.png");
    }

    #[test]
    fn input_image_base64_to_base64_source() {
        let input = json!({
            "type": "input_image",
            "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn input_image_jpeg_base64() {
        let input = json!({
            "type": "input_image",
            "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        assert_eq!(block["source"]["data"], "/9j/4AAQ");
    }

    #[test]
    fn input_file_dropped() {
        let input = json!({
            "type": "input_file",
            "file_url": "https://example.com/doc.pdf"
        });
        let result = convert_user_content(&input);
        assert!(result.is_none());
    }

    #[test]
    fn cache_control_passthrough() {
        let input = json!({
            "type": "input_text",
            "text": "cached text",
            "cache_control": {"type": "ephemeral"}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn unknown_content_type_returns_none() {
        let input = json!({"type": "input_video", "url": "..."});
        let result = convert_user_content(&input);
        assert!(result.is_none());
    }
}
