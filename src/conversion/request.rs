use crate::conversion::content;
use crate::conversion::thinking;
use crate::conversion::ConversionTask;
use serde_json::{json, Value};

/// Convert a Responses API request body to an Anthropic Messages API request body.
///
/// This function drives the entire request conversion phase of the state machine:
/// 1. Parse top-level parameters.
/// 2. Build namespace registry from namespace tools.
/// 3. Convert input items to Anthropic messages with alternation merging.
/// 4. Build ID map entries for all call_ids.
/// 5. Return the complete Anthropic request body.
pub fn convert_request(
    task: &mut ConversionTask,
    input: Value,
) -> Result<Value, RequestConversionError> {
    let obj = input.as_object().ok_or_else(|| {
        RequestConversionError::InvalidRequest("request body must be a JSON object".to_string())
    })?;

    // -- Top-level parameters --
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Stream is always true (proxy requires streaming from upstream).
    let stream = true;

    // -- System construction --
    let instructions = obj.get("instructions").and_then(|v| v.as_str());
    let system = build_system(instructions, obj.get("input"));

    // -- Reasoning -> thinking --
    let thinking_cfg = thinking::convert_reasoning(obj.get("reasoning"));

    // -- Tools (namespace flattening + function tools) --
    let tools = convert_tools(task, obj.get("tools"));

    // -- tool_choice --
    let tool_choice = convert_tool_choice(obj.get("tool_choice"), obj.get("parallel_tool_calls"));

    // -- Input items -> Messages --
    let messages = convert_input_items(task, obj.get("input"))?;

    // -- Build result --
    let mut result = json!({
        "model": model,
        "stream": stream,
        "messages": messages,
    });

    // Conditionally add fields.
    if let Some(sys) = system {
        result["system"] = sys;
    }

    result["tool_choice"] = tool_choice;

    if let Some(tools_arr) = tools {
        result["tools"] = tools_arr;
    }

    // Thinking + output_config.
    if let Some(th) = thinking_cfg.thinking {
        result["thinking"] = th;
    }
    if let Some(effort) = thinking_cfg.effort {
        // Merge into existing output_config or create new.
        if let Some(oc) = result.get_mut("output_config") {
            oc["effort"] = json!(effort);
        } else {
            result["output_config"] = json!({"effort": effort});
        }
    }

    // Temperature: pass through only when thinking not enabled.
    if !thinking_cfg.thinking_enabled {
        if let Some(temp) = obj.get("temperature") {
            result["temperature"] = temp.clone();
        }
    }

    // top_p: pass through.
    if let Some(top_p) = obj.get("top_p") {
        result["top_p"] = top_p.clone();
    }

    // max_output_tokens -> max_tokens.
    if let Some(max) = obj.get("max_output_tokens") {
        result["max_tokens"] = max.clone();
    }

    // metadata: forward only user_id.
    if let Some(meta) = obj.get("metadata") {
        if let Some(user_id) = meta.get("user_id") {
            result["metadata"] = json!({"user_id": user_id});
        }
    }

    // service_tier.
    if let Some(st) = obj.get("service_tier").and_then(|v| v.as_str()) {
        match st {
            "auto" => {
                result["service_tier"] = json!("auto");
            }
            "default" => {
                result["service_tier"] = json!("standard_only");
            }
            _ => {} // "flex", "priority", "scale" -> omit (no Anthropic equivalent).
        }
    }

    // text.format -> output_config.format.
    if let Some(text_fmt) = obj.get("text").and_then(|t| t.get("format")) {
        let fmt_type = text_fmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match fmt_type {
            "json_schema" => {
                let schema = text_fmt.get("schema").cloned().unwrap_or(json!({}));
                if let Some(oc) = result.get_mut("output_config") {
                    oc["format"] = json!({"type": "json_schema", "schema": schema});
                } else {
                    result["output_config"] =
                        json!({"format": {"type": "json_schema", "schema": schema}});
                }
            }
            "json_object" => {
                if let Some(oc) = result.get_mut("output_config") {
                    oc["format"] = json!({"type": "json_object"});
                } else {
                    result["output_config"] = json!({"format": {"type": "json_object"}});
                }
            }
            _ => {} // "text" type or unknown -> omit output_config.format.
        }
    }

    Ok(result)
}

/// Build the `system` parameter from `instructions` + system input items.
/// Returns None if both are empty.
fn build_system(instructions: Option<&str>, input: Option<&Value>) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    if let Some(text) = instructions {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    }

    // Extract system messages from input.
    if let Some(input_arr) = input.and_then(|v| v.as_array()) {
        for item in input_arr {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "message" && role == "system" {
                if let Some(content) = item.get("content") {
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if let Some(converted) = content::convert_user_content(block) {
                                blocks.push(converted);
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
            }
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(Value::Array(blocks))
    }
}

/// Convert the `tools` array, flattening namespace tools and renaming `parameters` -> `input_schema`.
fn convert_tools(task: &mut ConversionTask, tools: Option<&Value>) -> Option<Value> {
    let tools_arr = tools.and_then(|v| v.as_array())?;
    if tools_arr.is_empty() {
        return None;
    }

    let mut result = Vec::new();

    for tool in tools_arr {
        let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match tool_type {
            "namespace" => {
                // Flatten namespace tools.
                let namespace = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let children = tool.get("tools").and_then(|v| v.as_array());
                if let Some(children) = children {
                    for child in children {
                        let child_name = child.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let flat_name = task
                            .namespace_registry
                            .flatten_and_register(namespace, child_name);

                        let mut anth_tool = json!({
                            "type": "custom",
                            "name": flat_name,
                        });

                        if let Some(params) = child.get("parameters") {
                            anth_tool["input_schema"] = params.clone();
                        }

                        result.push(anth_tool);
                    }
                }
            }
            "function" => {
                let name = tool
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut anth_tool = json!({
                    "type": "custom",
                    "name": name,
                });
                if let Some(params) = tool.get("parameters") {
                    anth_tool["input_schema"] = params.clone();
                }
                result.push(anth_tool);
            }
            // Built-in tool types -> convert to custom tools with derived schemas.
            other => {
                if let Some(converted) = convert_builtin_tool(other, tool) {
                    result.push(converted);
                }
                // If convert_builtin_tool returns None, the tool type is unsupported and dropped.
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(Value::Array(result))
    }
}

/// Convert a built-in Responses API tool to an Anthropic custom tool with a derived input_schema.
fn convert_builtin_tool(tool_type: &str, tool: &Value) -> Option<Value> {
    let name = match tool_type {
        "mcp" => {
            // Server-side hosted MCP tool -- cannot convert.
            tracing::debug!("dropping mcp tool type (server-side hosted, cannot proxy)");
            return None;
        }
        other => other.to_string(),
    };

    // The derived schemas mirror the exact structure of the corresponding output item's call parameters.
    let schema = match name.as_str() {
        "web_search" | "web_search_preview" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["search", "open_page", "find_in_page"]},
                "query": {"type": "string"},
                "queries": {"type": "array", "items": {"type": "string"}},
                "sources": {"type": "array", "items": {"type": "object", "properties": {"type": {"type": "string"}, "url": {"type": "string"}}}},
                "url": {"type": "string"},
                "pattern": {"type": "string"}
            },
            "required": ["type"]
        }),
        "file_search" => json!({
            "type": "object",
            "properties": {
                "queries": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["queries"]
        }),
        "code_interpreter" => json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "container_id": {"type": "string"}
            },
            "required": ["code"]
        }),
        "computer_use_preview" | "computer" | "computer_use" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["screenshot", "click", "double_click", "drag", "keypress", "move", "scroll", "type", "wait"]},
                "button": {"type": "string", "enum": ["left", "right", "wheel", "back", "forward"]},
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "text": {"type": "string"},
                "keys": {"type": "array", "items": {"type": "string"}},
                "path": {"type": "array", "items": {"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}}}},
                "scroll_x": {"type": "integer"},
                "scroll_y": {"type": "integer"}
            },
            "required": ["type"]
        }),
        "image_generation" => json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"}
            },
            "required": ["prompt"]
        }),
        "local_shell" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["exec"]},
                "command": {"type": "array", "items": {"type": "string"}},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "timeout_ms": {"type": "integer"},
                "user": {"type": "string"},
                "working_directory": {"type": "string"}
            },
            "required": ["type", "command"]
        }),
        "shell" => json!({
            "type": "object",
            "properties": {
                "commands": {"type": "array", "items": {"type": "string"}},
                "timeout_ms": {"type": "integer"},
                "max_output_length": {"type": "integer"}
            },
            "required": ["commands"]
        }),
        "apply_patch" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["create_file", "update_file", "delete_file"]},
                "path": {"type": "string"},
                "diff": {"type": "string"}
            },
            "required": ["type", "path"]
        }),
        "tool_search" => json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string"}
            },
            "required": ["goal"]
        }),
        "custom" => {
            // Custom tools: use parameters if present, otherwise minimal schema.
            let tool_name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("custom");
            if let Some(params) = tool.get("parameters") {
                return Some(json!({
                    "type": "custom",
                    "name": tool_name,
                    "input_schema": params
                }));
            } else if tool.get("format").is_some() {
                // Freeform/grammar tool -- provide minimal schema accepting raw string.
                return Some(json!({
                    "type": "custom",
                    "name": tool_name,
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"}
                        },
                        "required": ["input"]
                    }
                }));
            } else {
                return None;
            }
        }
        _ => {
            tracing::warn!(tool_type = tool_type, "dropping unknown built-in tool type");
            return None;
        }
    };

    // Normalize the tool name for computer_use_preview -> computer_use.
    let anth_name = match name.as_str() {
        "computer_use_preview" => "computer_use",
        "computer" => "computer_use",
        "web_search_preview" => "web_search",
        other => other,
    };

    Some(json!({
        "type": "custom",
        "name": anth_name,
        "input_schema": schema
    }))
}

/// Convert `tool_choice` from Responses API to Anthropic format.
///
/// Responses API sends bare strings ("auto", "required", "none", or a tool name).
/// Anthropic requires structured objects.
///
/// `parallel_tool_calls: false` -> `disable_parallel_tool_use: true` inside the tool_choice object.
fn convert_tool_choice(tool_choice: Option<&Value>, parallel_tool_calls: Option<&Value>) -> Value {
    let disable_parallel = parallel_tool_calls
        .and_then(|v| v.as_bool())
        .map(|b| !b) // Invert: false -> disable=true.
        .unwrap_or(false);

    let mut choice = match tool_choice {
        None => {
            // Default to "auto" when absent.
            json!({"type": "auto"})
        }
        Some(v) if v.is_string() => match v.as_str().unwrap() {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            tool_name => json!({"type": "tool", "name": tool_name}),
        },
        Some(v) if v.is_object() => {
            let obj = v.as_object().unwrap();
            match obj.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "function" => {
                    let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    json!({"type": "tool", "name": name})
                }
                "allowed_tools" => {
                    // allowed_tools has no Anthropic equivalent, default to auto.
                    json!({"type": "auto"})
                }
                other => {
                    // Passthrough for unknown object forms.
                    json!({"type": other})
                }
            }
        }
        _ => json!({"type": "auto"}),
    };

    // Embed disable_parallel_tool_use unless tool_choice is "none" (no tools to parallelize).
    let tc_type = choice.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if disable_parallel && tc_type != "none" {
        choice["disable_parallel_tool_use"] = json!(true);
    }

    choice
}

/// Convert the `input` array to Anthropic `messages` array.
///
/// Key behaviors:
/// - System messages are extracted to the `system` parameter (done in build_system).
/// - Consecutive same-role items are merged (alternation enforcement).
/// - function_call -> assistant tool_use, function_call_output -> user tool_result.
/// - reasoning -> assistant thinking/redacted_thinking.
/// - compaction/compaction_trigger/context_compaction -> dropped.
/// - Unknown items -> dropped with warning log.
fn convert_input_items(
    task: &mut ConversionTask,
    input: Option<&Value>,
) -> Result<Value, RequestConversionError> {
    let input_arr = match input.and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Ok(json!([])),
    };

    let mut messages: Vec<Value> = Vec::new();

    for item in input_arr {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Skip system messages (handled by build_system).
        if item_type == "message" {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "system" {
                continue;
            }
        }

        // Determine the role and content blocks for this input item.
        let (role, content_blocks) = convert_single_input_item(task, item)?;

        // Skip items that produce no content (dropped types).
        let role = match role {
            Some(r) => r,
            None => continue,
        };

        // Merge with previous message if same role (alternation enforcement).
        if let Some(last) = messages.last_mut() {
            let last_role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if last_role == role {
                if let Some(arr) = last.get_mut("content").and_then(|v| v.as_array_mut()) {
                    arr.extend(content_blocks);
                }
                continue;
            }
        }

        messages.push(json!({
            "role": role,
            "content": content_blocks,
        }));
    }

    Ok(Value::Array(messages))
}

/// Convert a single input item into (role, content_blocks).
///
/// Returns (None, []) for items that should be dropped.
fn convert_single_input_item(
    task: &mut ConversionTask,
    item: &Value,
) -> Result<(Option<String>, Vec<Value>), RequestConversionError> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        "message" => {
            let role = item
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = item.get("content").cloned().unwrap_or(json!([]));
            let blocks = convert_message_content(&content);
            Ok((Some(role), blocks))
        }
        "function_call" | "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Parse arguments from JSON string.
            let raw_args = match item_type {
                "function_call" => item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}"),
                "custom_tool_call" => item.get("input").and_then(|v| v.as_str()).unwrap_or("{}"),
                _ => "{}",
            };
            let parsed_input: Value = serde_json::from_str(raw_args).map_err(|e| {
                RequestConversionError::InvalidRequest(format!(
                    "failed to parse arguments for call_id '{}': {}",
                    call_id, e
                ))
            })?;

            // Register in ID map.
            let toolu_id = task.id_map.insert_call(call_id.clone());

            let tool_use = json!({
                "type": "tool_use",
                "id": toolu_id,
                "name": name,
                "input": parsed_input,
            });

            Ok((Some("assistant".to_string()), vec![tool_use]))
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let output = item.get("output").cloned().unwrap_or(json!(""));

            // Look up toolu_id from the ID map.
            let toolu_id = task
                .id_map
                .get_toolu_for_call(&call_id)
                .cloned()
                .unwrap_or_else(|| {
                    // If the call_id was not registered (e.g., function_call was in a previous
                    // turn), generate a new toolu_id for it.
                    tracing::warn!(call_id = %call_id, "function_call_output references unregistered call_id, generating new toolu_id");
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    task.id_map.insert_with_toolu(call_id.clone(), id.clone());
                    id
                });

            let content = content::convert_tool_result_content(&output);

            let tool_result = json!({
                "type": "tool_result",
                "tool_use_id": toolu_id,
                "content": content,
            });

            Ok((Some("user".to_string()), vec![tool_result]))
        }
        "mcp_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output_obj = item.get("output").cloned().unwrap_or(json!({}));

            let toolu_id = task
                .id_map
                .get_toolu_for_call(&call_id)
                .cloned()
                .unwrap_or_else(|| {
                    tracing::warn!(call_id = %call_id, "mcp_tool_call_output references unregistered call_id");
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    task.id_map.insert_with_toolu(call_id.clone(), id.clone());
                    id
                });

            // Serialize the CallToolResult to a JSON string for tool_result.content.
            let content_str = serde_json::to_string(&output_obj).unwrap_or_default();

            let mut tool_result = json!({
                "type": "tool_result",
                "tool_use_id": toolu_id,
                "content": content_str,
            });

            // Check isError from CallToolResult.
            if let Some(is_error) = output_obj.get("isError").and_then(|v| v.as_bool()) {
                if is_error {
                    tool_result["is_error"] = json!(true);
                }
                // When false or absent, omit is_error entirely.
            }

            Ok((Some("user".to_string()), vec![tool_result]))
        }
        "reasoning" => {
            let reasoning_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let summary = item
                .get("summary")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let encrypted_content = item.get("encrypted_content").and_then(|v| v.as_str());

            // The `content` field (raw reasoning text) is dropped -- only summary forwarded.
            let block = thinking::convert_reasoning_input(
                &summary,
                encrypted_content,
                &task.signature_cache,
                &reasoning_id,
            );

            Ok((Some("assistant".to_string()), vec![block]))
        }
        // Dropped items.
        "compaction" | "context_compaction" | "compaction_trigger" => {
            tracing::debug!(item_type = item_type, "dropping compaction-related item");
            Ok((None, vec![]))
        }
        "tool_search_output" => {
            tracing::debug!("dropping tool_search_output (no Anthropic equivalent)");
            Ok((None, vec![]))
        }
        _ => {
            tracing::warn!(item_type = item_type, "dropping unknown input item type");
            Ok((None, vec![]))
        }
    }
}

/// Convert message content from Responses API format to Anthropic content blocks.
fn convert_message_content(content: &Value) -> Vec<Value> {
    if let Some(arr) = content.as_array() {
        content::convert_user_content_array(arr)
    } else if let Some(text) = content.as_str() {
        vec![json!({"type": "text", "text": text})]
    } else {
        vec![]
    }
}

/// Error type for request conversion failures.
#[derive(Debug)]
pub enum RequestConversionError {
    InvalidRequest(String),
}

impl std::fmt::Display for RequestConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestConversionError::InvalidRequest(msg) => {
                write!(f, "invalid request: {}", msg)
            }
        }
    }
}

impl std::error::Error for RequestConversionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_task() -> ConversionTask {
        ConversionTask::new("https://api.anthropic.com".to_string())
    }

    // =========================================================================
    // Top-level parameter tests
    // =========================================================================

    #[test]
    fn top_level_passthrough() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn stream_always_true() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": false,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn instructions_to_system() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "You are helpful.",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are helpful.");
    }

    #[test]
    fn instructions_plus_system_input_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "Base instructions.",
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Extra system."}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "Base instructions.");
        assert_eq!(system[1]["text"], "Extra system.");
    }

    #[test]
    fn no_system_when_empty() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("system").is_none());
    }

    #[test]
    fn empty_instructions_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(
            result.get("system").is_none(),
            "empty instructions should not produce a system field"
        );
    }

    // =========================================================================
    // tool_choice tests
    // =========================================================================

    #[test]
    fn tool_choice_auto_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
    }

    #[test]
    fn tool_choice_required_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "required",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "any");
    }

    #[test]
    fn tool_choice_none_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "none",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "none");
    }

    #[test]
    fn tool_choice_function_object() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": {"type": "function", "name": "my_tool"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "tool");
        assert_eq!(result["tool_choice"]["name"], "my_tool");
    }

    #[test]
    fn tool_choice_named_by_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "my_tool",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "tool");
        assert_eq!(result["tool_choice"]["name"], "my_tool");
    }

    #[test]
    fn tool_choice_absent_defaults_to_auto() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
    }

    #[test]
    fn tool_choice_allowed_tools_to_auto() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": {"type": "allowed_tools", "tools": ["tool_a"]},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
    }

    // =========================================================================
    // parallel_tool_calls tests
    // =========================================================================

    #[test]
    fn parallel_tool_calls_false_embeds_disable() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
        assert_eq!(result["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn parallel_tool_calls_true_no_disable() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
        assert!(result["tool_choice"]
            .get("disable_parallel_tool_use")
            .is_none());
    }

    #[test]
    fn parallel_tool_calls_false_with_none_no_change() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "none",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "none");
        // "none" means no tools -- disable_parallel_tool_use is irrelevant, omit it.
        assert!(result["tool_choice"]
            .get("disable_parallel_tool_use")
            .is_none());
    }

    #[test]
    fn parallel_tool_calls_false_with_required() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "required",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "any");
        assert_eq!(result["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn parallel_tool_calls_false_with_named_tool() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "my_tool",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "tool");
        assert_eq!(result["tool_choice"]["name"], "my_tool");
        assert_eq!(result["tool_choice"]["disable_parallel_tool_use"], true);
    }

    // =========================================================================
    // Input items -> Messages
    // =========================================================================

    #[test]
    fn user_message_converted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn assistant_message_converted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi there"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
    }

    #[test]
    fn function_call_creates_assistant_tool_use() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "{\"command\":\"ls\"}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["name"], "exec_command");
        assert_eq!(messages[0]["content"][0]["input"]["command"], "ls");
        // ID map should have been populated.
        assert!(task.id_map.get_toolu_for_call("call_001").is_some());
    }

    #[test]
    fn function_call_output_creates_user_tool_result() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "file1.txt\nfile2.txt"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        // function_call -> assistant, function_call_output -> user
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn function_call_arguments_parse_failure_rejects() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "not-valid-json{"}
            ]
        });
        let result = convert_request(&mut task, input);
        assert!(result.is_err());
    }

    // =========================================================================
    // Message alternation merge
    // =========================================================================

    #[test]
    fn consecutive_function_calls_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_002", "name": "tool_b", "arguments": "{}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "consecutive function_calls should merge into one assistant message"
        );
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn consecutive_function_call_outputs_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "result_a"},
                {"type": "function_call_output", "call_id": "call_001", "output": "result_b"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        // assistant (function_call) + user (merged outputs)
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reasoning_plus_function_call_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "thinking..."}]},
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "reasoning + function_call should merge into one assistant message"
        );
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        // First block is thinking, second is tool_use.
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn user_then_assistant_alternation() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    // =========================================================================
    // Reasoning input
    // =========================================================================

    #[test]
    fn reasoning_with_summary_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "I thought about it"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        let content = &messages[0]["content"][0];
        assert_eq!(content["type"], "thinking");
        assert_eq!(content["thinking"], "I thought about it");
        // Empty signature when no cached signature exists.
        assert_eq!(content["signature"], "");
    }

    #[test]
    fn reasoning_with_encrypted_content() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [], "encrypted_content": "ENCRYPTED_DATA_HERE"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let content = &result["messages"].as_array().unwrap()[0]["content"][0];
        assert_eq!(content["type"], "redacted_thinking");
        assert_eq!(content["data"], "ENCRYPTED_DATA_HERE");
    }

    #[test]
    fn reasoning_content_field_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "summary"}], "content": [{"type": "reasoning_text", "text": "raw text"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let content = &result["messages"].as_array().unwrap()[0]["content"][0];
        // The raw content field is dropped; only summary is forwarded.
        assert_eq!(content["thinking"], "summary");
        assert!(content.get("content").is_none());
    }

    // =========================================================================
    // Dropped input items
    // =========================================================================

    #[test]
    fn compaction_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "compaction", "encrypted_content": "..."},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "compaction should be dropped");
    }

    #[test]
    fn context_compaction_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "context_compaction", "encrypted_content": "..."},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn compaction_trigger_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "compaction_trigger"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn tool_search_output_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "tool_search_output", "call_id": "ts_001", "tools": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn unknown_item_dropped_with_warning() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "future_unknown_type", "data": "..."},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "unknown items should be dropped");
    }

    // =========================================================================
    // Thinking/reasoning params
    // =========================================================================

    #[test]
    fn reasoning_effort_maps_to_output_config() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "auto"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["thinking"]["type"], "adaptive");
        assert_eq!(result["output_config"]["effort"], "high");
    }

    #[test]
    fn reasoning_summary_none_sets_display_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "none"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["thinking"]["display"], "omitted");
    }

    #[test]
    fn reasoning_effort_none_omits_thinking() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "none"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("thinking").is_none());
        assert!(result.get("output_config").is_none());
    }

    #[test]
    fn reasoning_minimal_maps_to_low() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "minimal", "summary": "auto"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["effort"], "low");
    }

    #[test]
    fn no_reasoning_no_thinking_field() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("thinking").is_none());
        assert!(result.get("output_config").is_none());
    }

    // =========================================================================
    // Temperature
    // =========================================================================

    #[test]
    fn temperature_omitted_when_thinking_enabled() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "auto"},
            "temperature": 0.7,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(
            result.get("temperature").is_none(),
            "temperature must be omitted when thinking enabled"
        );
    }

    #[test]
    fn temperature_passed_through_when_no_thinking() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "temperature": 0.7,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["temperature"], 0.7);
    }

    // =========================================================================
    // max_output_tokens
    // =========================================================================

    #[test]
    fn max_output_tokens_to_max_tokens() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "max_output_tokens": 4096,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["max_tokens"], 4096);
        assert!(result.get("max_output_tokens").is_none());
    }

    // =========================================================================
    // top_p
    // =========================================================================

    #[test]
    fn top_p_passthrough() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "top_p": 0.9,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["top_p"], 0.9);
    }

    #[test]
    fn top_p_absent_when_not_provided() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("top_p").is_none());
    }

    // =========================================================================
    // metadata
    // =========================================================================

    #[test]
    fn metadata_forwards_user_id_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "metadata": {"user_id": "user123", "extra_key": "extra_value"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["metadata"]["user_id"], "user123");
        assert!(result["metadata"].get("extra_key").is_none());
    }

    #[test]
    fn metadata_without_user_id_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "metadata": {"extra_key": "value"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("metadata").is_none());
    }

    // =========================================================================
    // service_tier
    // =========================================================================

    #[test]
    fn service_tier_auto() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "auto",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["service_tier"], "auto");
    }

    #[test]
    fn service_tier_default_to_standard_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "default",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["service_tier"], "standard_only");
    }

    #[test]
    fn service_tier_flex_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "flex",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("service_tier").is_none());
    }

    #[test]
    fn service_tier_priority_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "priority",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("service_tier").is_none());
    }

    // =========================================================================
    // text.format -> output_config.format
    // =========================================================================

    #[test]
    fn text_format_json_schema() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "json_schema", "name": "codex_output_schema", "schema": {"type": "object"}, "strict": true}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            result["output_config"]["format"]["schema"]["type"],
            "object"
        );
        // name and strict should be dropped.
        assert!(result["output_config"]["format"].get("name").is_none());
        assert!(result["output_config"]["format"].get("strict").is_none());
    }

    #[test]
    fn text_format_text_omits_output_config() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "text"}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        if let Some(oc) = result.get("output_config") {
            assert!(oc.get("format").is_none());
        }
    }

    #[test]
    fn text_format_json_object() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "json_object"}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["format"]["type"], "json_object");
    }

    #[test]
    fn text_format_merges_with_reasoning_output_config() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "auto"},
            "text": {"format": {"type": "json_schema", "schema": {"type": "string"}}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        // Both effort and format should be present in output_config.
        assert_eq!(result["output_config"]["effort"], "high");
        assert_eq!(result["output_config"]["format"]["type"], "json_schema");
    }

    // =========================================================================
    // MCP namespace tools
    // =========================================================================

    #[test]
    fn namespace_tools_flattened() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__memory__",
                    "tools": [
                        {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
                    ]
                }
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "mcp__memory__search");
        assert_eq!(tools[0]["type"], "custom");
        assert!(tools[0]["input_schema"].is_object());
        // Registry should have the mapping.
        let entry = task
            .namespace_registry
            .lookup("mcp__memory__search")
            .unwrap();
        assert_eq!(entry.namespace, "mcp__memory__");
        assert_eq!(entry.tool_name, "search");
    }

    #[test]
    fn namespace_tools_multiple_children() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__svr__",
                    "tools": [
                        {"type": "function", "name": "tool_a", "parameters": {"type": "object"}},
                        {"type": "function", "name": "tool_b", "parameters": {"type": "object"}}
                    ]
                }
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "mcp__svr__tool_a");
        assert_eq!(tools[1]["name"], "mcp__svr__tool_b");
    }

    // =========================================================================
    // function tools
    // =========================================================================

    #[test]
    fn function_tools_parameters_to_input_schema() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "exec_command");
        assert!(tools[0].get("input_schema").is_some());
        assert!(tools[0].get("parameters").is_none());
    }

    #[test]
    fn function_tools_type_is_custom() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {"type": "function", "name": "my_tool", "parameters": {"type": "object"}}
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "custom");
    }

    #[test]
    fn empty_tools_array_omits_tools() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("tools").is_none());
    }

    // =========================================================================
    // custom_tool_call and custom_tool_call_output
    // =========================================================================

    #[test]
    fn custom_tool_call_same_as_function_call() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_001", "name": "apply_patch", "input": "{\"patch\":\"...\"}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["name"], "apply_patch");
    }

    #[test]
    fn custom_tool_call_output_same_as_function_call_output() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_001", "name": "apply_patch", "input": "{}"},
                {"type": "custom_tool_call_output", "call_id": "call_001", "output": "patch applied"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["content"], "patch applied");
    }

    // =========================================================================
    // mcp_tool_call_output
    // =========================================================================

    #[test]
    fn mcp_tool_call_output_with_error() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "mcp__memory__search", "arguments": "{}"},
                {"type": "mcp_tool_call_output", "call_id": "call_001", "output": {"content": [{"type": "text", "text": "error"}], "isError": true}}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        let tool_result = &messages[1]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["is_error"], true);
    }

    #[test]
    fn mcp_tool_call_output_without_error() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "mcp__memory__search", "arguments": "{}"},
                {"type": "mcp_tool_call_output", "call_id": "call_001", "output": {"content": [{"type": "text", "text": "ok"}]}}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let tool_result = &result["messages"].as_array().unwrap()[1]["content"][0];
        assert!(
            tool_result.get("is_error").is_none(),
            "is_error should be omitted when not an error"
        );
    }

    // =========================================================================
    // phase stripping
    // =========================================================================

    #[test]
    fn phase_field_stripped_from_messages() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}], "phase": "commentary"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert!(messages[0].get("phase").is_none());
    }

    // =========================================================================
    // Built-in tool conversion
    // =========================================================================

    #[test]
    fn builtin_web_search_tool() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "web_search"}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["type"], "custom");
        assert_eq!(tools[0]["name"], "web_search");
        assert!(tools[0]["input_schema"].is_object());
    }

    #[test]
    fn builtin_computer_use_preview_normalizes_name() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "computer_use_preview"}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "computer_use");
    }

    #[test]
    fn builtin_computer_normalizes_name() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "computer"}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "computer_use");
    }

    #[test]
    fn builtin_web_search_preview_normalizes_name() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "web_search_preview"}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "web_search");
    }

    #[test]
    fn builtin_mcp_tool_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "mcp", "name": "server"}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("tools").is_none());
    }

    #[test]
    fn builtin_custom_tool_with_parameters() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "custom", "name": "my_custom", "parameters": {"type": "object"}}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "my_custom");
        assert_eq!(tools[0]["type"], "custom");
        assert!(tools[0]["input_schema"].is_object());
    }

    #[test]
    fn builtin_custom_tool_with_format() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [{"type": "custom", "name": "apply_patch", "format": {"type": "lark", "grammar": "..."}}],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "apply_patch");
        // Should have minimal schema with single string input field.
        let schema = &tools[0]["input_schema"];
        assert_eq!(schema["properties"]["input"]["type"], "string");
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn empty_input_array_produces_empty_messages() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn missing_input_produces_empty_messages() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514"
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn invalid_request_body_rejected() {
        let mut task = make_task();
        let input = json!("not an object");
        let result = convert_request(&mut task, input);
        assert!(result.is_err());
    }

    #[test]
    fn full_round_trip_request() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "You are a helpful assistant.",
            "reasoning": {"effort": "high", "summary": "auto"},
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ],
            "tool_choice": "auto",
            "max_output_tokens": 8192,
            "temperature": 0.5,
            "metadata": {"user_id": "test_user"},
            "service_tier": "auto",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check."}]},
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "I need to list files"}]},
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "file1.txt\nfile2.txt"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();

        // Verify all key fields.
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["stream"], true);
        assert!(result.get("system").is_some());
        assert_eq!(result["thinking"]["type"], "adaptive");
        assert_eq!(result["output_config"]["effort"], "high");
        assert_eq!(result["max_tokens"], 8192);
        // temperature should be omitted (thinking enabled).
        assert!(result.get("temperature").is_none());
        assert_eq!(result["metadata"]["user_id"], "test_user");
        assert_eq!(result["service_tier"], "auto");
        assert_eq!(result["tool_choice"]["type"], "auto");
        assert!(result["tools"].as_array().unwrap().len() == 1);

        // Verify messages: user, assistant(merged reasoning+tool_use), user(tool_result)
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        // Merged: thinking block + tool_use block.
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn id_map_populated_for_function_calls() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_002", "name": "tool_b", "arguments": "{}"}
            ]
        });
        convert_request(&mut task, input).unwrap();
        assert_eq!(task.id_map.len(), 2);
        assert!(task.id_map.get_toolu_for_call("call_001").is_some());
        assert!(task.id_map.get_toolu_for_call("call_002").is_some());
    }

    #[test]
    fn tool_result_references_correct_toolu_id() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "done"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        let toolu_id = messages[0]["content"][0]["id"].as_str().unwrap();
        let tool_use_id = messages[1]["content"][0]["tool_use_id"].as_str().unwrap();
        assert_eq!(
            toolu_id, tool_use_id,
            "tool_result must reference the same toolu_id as the tool_use"
        );
    }

    #[test]
    fn string_content_in_message() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": "Hello world"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "Hello world");
    }

    #[test]
    fn cache_control_preserved_in_user_content() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "cached", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let block = &result["messages"].as_array().unwrap()[0]["content"][0];
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn multiple_system_messages_extracted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "First system.",
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Second system."}]},
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Third system."}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], "First system.");
        assert_eq!(system[1]["text"], "Second system.");
        assert_eq!(system[2]["text"], "Third system.");
        // System messages should NOT appear in messages.
        assert_eq!(result["messages"].as_array().unwrap().len(), 0);
    }
}
