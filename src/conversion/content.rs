use serde_json::{json, Value};

use crate::conversion::namespace::NamespaceRegistry;

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

// ---------------------------------------------------------------------------
// Response direction: Anthropic content block → Responses API output item
// ---------------------------------------------------------------------------

/// Convert an Anthropic content block to a Responses API output item.
///
/// Returns `(output_item, reasoning_id)`.
/// `reasoning_id` is populated only for `thinking` and `redacted_thinking` blocks.
/// `fc_counter` is incremented for each `tool_use` block to generate sequential `fc_N` IDs.
///
/// ID generation:
/// - reasoning: `rs_{uuid_v4}` (proxy-generated)
/// - function_call: `fc_{sequential}` with `call_{sequential}` call_id (proxy-generated)
/// - message: no generated ID (uses response ID from caller)
pub fn convert_content_block_to_output(
    block: &Value,
    namespace_registry: &NamespaceRegistry,
    _output_index: usize,
    fc_counter: &mut u64,
) -> (Value, String) {
    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match block_type {
        "text" => {
            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let item = json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }]
            });
            (item, String::new())
        }
        "thinking" => {
            let thinking_text = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
            let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
            let item = json!({
                "type": "reasoning",
                "id": &reasoning_id,
                "summary": [{
                    "type": "summary_text",
                    "text": thinking_text,
                }],
                // encrypted_content and content fields are OMITTED (not null, not empty).
            });
            (item, reasoning_id)
        }
        "redacted_thinking" => {
            let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
            let item = json!({
                "type": "reasoning",
                "id": &reasoning_id,
                "summary": [],
                "encrypted_content": data,
            });
            (item, reasoning_id)
        }
        "tool_use" => {
            let _toolu_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let raw_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = block.get("input").cloned().unwrap_or(json!({}));

            // Serialize input to JSON string for arguments field.
            let arguments = serde_json::to_string(&input).unwrap_or_default();

            // Generate sequential IDs.
            let fc_id = format!("fc_{}", fc_counter);
            let call_id = format!("call_{}", fc_counter);
            *fc_counter += 1;

            let mut item = json!({
                "type": "function_call",
                "id": fc_id,
                "call_id": call_id,
                "name": raw_name,
                "status": "completed",
                "arguments": arguments,
            });

            // Check namespace registry for the tool name.
            if let Some(entry) = namespace_registry.lookup(raw_name) {
                item["name"] = json!(entry.tool_name);
                item["namespace"] = json!(entry.namespace);
            }
            // Registry miss -> plain function_call without namespace field.

            (item, String::new())
        }
        _ => {
            tracing::warn!(
                block_type = block_type,
                "unknown Anthropic content block type in response"
            );
            (json!({}), String::new())
        }
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

    // --- Response direction tests ---

    #[test]
    fn text_to_message_output_item() {
        let block = json!({"type": "text", "text": "Hello world"});
        let (item, reasoning_id) =
            convert_content_block_to_output(&block, &NamespaceRegistry::new(), 0, &mut 0);
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "Hello world");
        assert_eq!(item["content"][0]["annotations"], json!([]));
        assert!(reasoning_id.is_empty());
    }

    #[test]
    fn thinking_to_reasoning_output_item() {
        let block = json!({"type": "thinking", "thinking": "Let me analyze..."});
        let mut fc_counter = 0u64;
        let (item, reasoning_id) =
            convert_content_block_to_output(&block, &NamespaceRegistry::new(), 1, &mut fc_counter);
        assert_eq!(item["type"], "reasoning");
        assert!(
            reasoning_id.starts_with("rs_"),
            "reasoning ID should start with rs_, got {}",
            reasoning_id
        );
        assert_eq!(item["id"], reasoning_id);
        assert_eq!(item["summary"][0]["type"], "summary_text");
        assert_eq!(item["summary"][0]["text"], "Let me analyze...");
        assert!(
            item.get("encrypted_content").is_none(),
            "encrypted_content should be absent"
        );
        assert!(
            item.get("content").is_none(),
            "content field should be absent"
        );
    }

    #[test]
    fn redacted_thinking_to_reasoning_with_encrypted_content() {
        let block = json!({"type": "redacted_thinking", "data": "ENCRYPTED_BLOB"});
        let mut fc_counter = 0u64;
        let (item, reasoning_id) =
            convert_content_block_to_output(&block, &NamespaceRegistry::new(), 2, &mut fc_counter);
        assert_eq!(item["type"], "reasoning");
        assert!(reasoning_id.starts_with("rs_"));
        assert_eq!(item["encrypted_content"], "ENCRYPTED_BLOB");
        assert_eq!(item["summary"], json!([]));
    }

    #[test]
    fn tool_use_to_function_call_no_namespace() {
        let reg = NamespaceRegistry::new();
        // No registration -- tool not in namespace registry.
        let block = json!({"type": "tool_use", "id": "toolu_01ABC", "name": "exec_command", "input": {"command": "ls"}});
        let mut fc_counter = 0u64;
        let (item, _) = convert_content_block_to_output(&block, &reg, 3, &mut fc_counter);
        assert_eq!(item["type"], "function_call");
        assert!(
            item["id"].as_str().unwrap().starts_with("fc_"),
            "function call ID should start with fc_"
        );
        assert!(item["call_id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(item["name"], "exec_command");
        assert_eq!(item["arguments"], "{\"command\":\"ls\"}");
        assert!(
            item.get("namespace").is_none(),
            "no namespace when not in registry"
        );
        assert_eq!(item["status"], "completed");
        assert_eq!(fc_counter, 1);
    }

    #[test]
    fn tool_use_to_function_call_with_namespace() {
        let mut reg = NamespaceRegistry::new();
        reg.register(
            "mcp__memory__search".to_string(),
            "mcp__memory__".to_string(),
            "search".to_string(),
        );
        let block = json!({"type": "tool_use", "id": "toolu_01ABC", "name": "mcp__memory__search", "input": {"query": "test"}});
        let mut fc_counter = 0u64;
        let (item, _) = convert_content_block_to_output(&block, &reg, 4, &mut fc_counter);
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "search");
        assert_eq!(item["namespace"], "mcp__memory__");
        assert_eq!(item["arguments"], "{\"query\":\"test\"}");
    }

    #[test]
    fn fc_counter_increments_per_tool_use() {
        let reg = NamespaceRegistry::new();
        let block1 = json!({"type": "tool_use", "id": "toolu_01", "name": "a", "input": {}});
        let block2 = json!({"type": "tool_use", "id": "toolu_02", "name": "b", "input": {}});
        let mut fc_counter = 0u64;
        let (item1, _) = convert_content_block_to_output(&block1, &reg, 0, &mut fc_counter);
        let (item2, _) = convert_content_block_to_output(&block2, &reg, 1, &mut fc_counter);
        assert_eq!(fc_counter, 2);
        assert_eq!(item1["id"], "fc_0");
        assert_eq!(item2["id"], "fc_1");
    }

    #[test]
    fn rs_ids_are_unique_uuids() {
        let block1 = json!({"type": "thinking", "thinking": "a"});
        let block2 = json!({"type": "thinking", "thinking": "b"});
        let mut fc_counter = 0u64;
        let (_, id1) =
            convert_content_block_to_output(&block1, &NamespaceRegistry::new(), 0, &mut fc_counter);
        let (_, id2) =
            convert_content_block_to_output(&block2, &NamespaceRegistry::new(), 1, &mut fc_counter);
        assert_ne!(id1, id2, "reasoning IDs must be unique");
    }
}
