use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConfigShellToolType {
    #[default]
    Default,
    Local,
    UnifiedExec,
    Disabled,
    ShellCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibility {
    #[default]
    List,
    Hide,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    #[default]
    Auto,
    None,
    Concise,
    Detailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ApplyPatchToolType {
    Freeform,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum WebSearchToolType {
    #[default]
    Text,
    TextAndImage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TruncationPolicyConfig {
    pub mode: TruncationPolicyMode,
    pub limit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TruncationPolicyMode {
    Bytes,
    Tokens,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningEffortPreset {
    pub effort: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelServiceTier {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelAvailabilityNux {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfoUpgrade {
    pub model: String,
    pub migration_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMessages {
    pub instructions_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub supported_reasoning_levels: Vec<ReasoningEffortPreset>,
    pub shell_type: ConfigShellToolType,
    pub visibility: ModelVisibility,
    pub supported_in_api: bool,
    pub priority: i32,
    pub base_instructions: String,
    pub supports_reasoning_summaries: bool,
    pub support_verbosity: bool,
    pub default_verbosity: Option<Verbosity>,
    pub apply_patch_tool_type: Option<ApplyPatchToolType>,
    pub truncation_policy: TruncationPolicyConfig,
    pub supports_parallel_tool_calls: bool,
    pub experimental_supported_tools: Vec<String>,

    #[serde(default)]
    pub default_reasoning_summary: ReasoningSummary,
    #[serde(default)]
    pub default_reasoning_level: Option<ReasoningEffort>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
    #[serde(default)]
    pub service_tiers: Vec<ModelServiceTier>,
    #[serde(default)]
    pub supports_image_detail_original: bool,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub max_context_window: Option<i64>,
    #[serde(default)]
    pub auto_compact_token_limit: Option<i64>,
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: i64,
    #[serde(default = "default_input_modalities")]
    pub input_modalities: Vec<InputModality>,
    #[serde(default)]
    pub supports_search_tool: bool,
    #[serde(default)]
    pub web_search_tool_type: WebSearchToolType,
    #[serde(default)]
    pub availability_nux: Option<ModelAvailabilityNux>,
    #[serde(default)]
    pub upgrade: Option<ModelInfoUpgrade>,
    #[serde(default)]
    pub model_messages: Option<ModelMessages>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

fn default_effective_context_window_percent() -> i64 {
    95
}

fn default_input_modalities() -> Vec<InputModality> {
    vec![InputModality::Text, InputModality::Image]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelsResponse {
    pub models: Vec<ModelInfo>,
}

pub fn load_catalog_file(path: &Path) -> Result<ModelsResponse, CatalogError> {
    let content =
        std::fs::read_to_string(path).map_err(|e| CatalogError::Io(path.to_path_buf(), e))?;
    let catalog: ModelsResponse =
        yaml_serde::from_str(&content).map_err(|e| CatalogError::Parse(path.to_path_buf(), e))?;
    Ok(catalog)
}

pub fn load_and_merge_catalogs(
    paths: &[std::path::PathBuf],
    config_dir: &Path,
) -> Result<ModelsResponse, CatalogError> {
    let resolved: Vec<std::path::PathBuf> = paths
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                config_dir.join(p)
            }
        })
        .collect();

    if resolved.is_empty() {
        return Ok(ModelsResponse { models: vec![] });
    }

    let mut catalogs: Vec<ModelsResponse> = Vec::with_capacity(resolved.len());
    for path in &resolved {
        catalogs.push(load_catalog_file(path)?);
    }

    let mut merged: HashMap<String, ModelInfo> = HashMap::new();

    // Iterate in reverse so earlier (higher-priority) files overwrite later ones.
    for catalog in catalogs.iter().rev() {
        for model in &catalog.models {
            if let Some(existing) = merged.get_mut(&model.slug) {
                overlay_model_info(existing, model);
            } else {
                merged.insert(model.slug.clone(), model.clone());
            }
        }
    }

    let models: Vec<ModelInfo> = merged.into_values().collect();
    if models.is_empty() {
        return Err(CatalogError::Empty);
    }

    Ok(ModelsResponse { models })
}

fn overlay_model_info(target: &mut ModelInfo, higher_priority: &ModelInfo) {
    target.display_name = higher_priority.display_name.clone();
    target.priority = higher_priority.priority;
    target.supported_in_api = higher_priority.supported_in_api;
    target.base_instructions = higher_priority.base_instructions.clone();
    target.supports_reasoning_summaries = higher_priority.supports_reasoning_summaries;
    target.support_verbosity = higher_priority.support_verbosity;
    target.supports_parallel_tool_calls = higher_priority.supports_parallel_tool_calls;
    target.shell_type = higher_priority.shell_type.clone();
    target.visibility = higher_priority.visibility.clone();
    target.truncation_policy = higher_priority.truncation_policy.clone();

    if higher_priority.description.is_some() {
        target.description = higher_priority.description.clone();
    }
    if higher_priority.default_verbosity.is_some() {
        target.default_verbosity = higher_priority.default_verbosity.clone();
    }
    if higher_priority.apply_patch_tool_type.is_some() {
        target.apply_patch_tool_type = higher_priority.apply_patch_tool_type.clone();
    }
    if higher_priority.default_reasoning_level.is_some() {
        target.default_reasoning_level = higher_priority.default_reasoning_level.clone();
    }
    if higher_priority.context_window.is_some() {
        target.context_window = higher_priority.context_window;
    }
    if higher_priority.max_context_window.is_some() {
        target.max_context_window = higher_priority.max_context_window;
    }
    if higher_priority.auto_compact_token_limit.is_some() {
        target.auto_compact_token_limit = higher_priority.auto_compact_token_limit;
    }
    if higher_priority.availability_nux.is_some() {
        target.availability_nux = higher_priority.availability_nux.clone();
    }
    if higher_priority.upgrade.is_some() {
        target.upgrade = higher_priority.upgrade.clone();
    }
    if higher_priority.model_messages.is_some() {
        target.model_messages = higher_priority.model_messages.clone();
    }

    target.supported_reasoning_levels = higher_priority.supported_reasoning_levels.clone();
    target.experimental_supported_tools = higher_priority.experimental_supported_tools.clone();
    target.additional_speed_tiers = higher_priority.additional_speed_tiers.clone();
    target.service_tiers = higher_priority.service_tiers.clone();
    target.input_modalities = higher_priority.input_modalities.clone();

    target.supports_image_detail_original = higher_priority.supports_image_detail_original;
    target.supports_search_tool = higher_priority.supports_search_tool;
    target.web_search_tool_type = higher_priority.web_search_tool_type.clone();
    target.default_reasoning_summary = higher_priority.default_reasoning_summary.clone();
    target.effective_context_window_percent = higher_priority.effective_context_window_percent;
}

pub fn validate_and_load_catalog(
    config: &crate::config::AppConfig,
    config_file_path: Option<&Path>,
) -> Result<ModelsResponse, CatalogError> {
    if config.model_catalog.is_empty() {
        return Ok(ModelsResponse { models: vec![] });
    }
    let config_dir = config_file_path
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."));
    load_and_merge_catalogs(&config.model_catalog, config_dir)
}

#[derive(Debug)]
pub enum CatalogError {
    Io(std::path::PathBuf, std::io::Error),
    Parse(std::path::PathBuf, yaml_serde::Error),
    Empty,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Io(path, e) => {
                write!(f, "model_catalog file not found: {}", path.display())?;
                if e.kind() != std::io::ErrorKind::NotFound {
                    write!(f, " ({})", e)?;
                }
                Ok(())
            }
            CatalogError::Parse(path, e) => {
                write!(f, "model_catalog parse error in {}: {}", path.display(), e)
            }
            CatalogError::Empty => write!(f, "model_catalog has no models after merging all files"),
        }
    }
}

impl std::error::Error for CatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn write_temp_catalog(
        dir: &std::path::Path,
        filename: &str,
        content: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(filename);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn minimal_model_info_json(slug: &str) -> serde_json::Value {
        serde_json::json!({
            "slug": slug,
            "display_name": slug,
            "description": null,
            "supported_reasoning_levels": [],
            "shell_type": "default",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 0,
            "base_instructions": "",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10000},
            "supports_parallel_tool_calls": false,
            "experimental_supported_tools": []
        })
    }

    #[test]
    fn load_single_catalog_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = serde_json::json!({
            "models": [minimal_model_info_json("claude-sonnet-4-20250514")]
        })
        .to_string();
        let path = write_temp_catalog(dir.path(), "models.json", &content);
        let catalog = load_catalog_file(&path).unwrap();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].slug, "claude-sonnet-4-20250514");
    }

    #[test]
    fn load_missing_file_returns_error() {
        let result = load_catalog_file(std::path::Path::new("/nonexistent/models.json"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("model_catalog file not found"));
    }

    #[test]
    fn load_malformed_json_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_catalog(dir.path(), "bad.json", "{not valid json");
        let result = load_catalog_file(&path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("model_catalog parse error"));
    }

    #[test]
    fn empty_models_array_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_temp_catalog(dir.path(), "empty.json", r#"{"models": []}"#);
        let result = load_and_merge_catalogs(&[path], dir.path());
        assert!(matches!(result, Err(CatalogError::Empty)));
    }

    #[test]
    fn merge_two_files_different_slugs_union() {
        let dir = tempfile::tempdir().unwrap();
        let file1 = serde_json::json!({"models": [minimal_model_info_json("model-a")]});
        let file2 = serde_json::json!({"models": [minimal_model_info_json("model-b")]});
        let p1 = write_temp_catalog(dir.path(), "a.json", &file1.to_string());
        let p2 = write_temp_catalog(dir.path(), "b.json", &file2.to_string());
        let merged = load_and_merge_catalogs(&[p1, p2], dir.path()).unwrap();
        assert_eq!(merged.models.len(), 2);
        let slugs: Vec<&str> = merged.models.iter().map(|m| m.slug.as_str()).collect();
        assert!(slugs.contains(&"model-a"));
        assert!(slugs.contains(&"model-b"));
    }

    #[test]
    fn merge_same_slug_earlier_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        let mut model_a1 = minimal_model_info_json("model-a");
        model_a1["display_name"] = serde_json::json!("Name from file 1");
        model_a1["priority"] = serde_json::json!(5);
        let file1 = serde_json::json!({"models": [model_a1]});

        let mut model_a2 = minimal_model_info_json("model-a");
        model_a2["display_name"] = serde_json::json!("Name from file 2");
        model_a2["priority"] = serde_json::json!(10);
        let file2 = serde_json::json!({"models": [model_a2]});

        let p1 = write_temp_catalog(dir.path(), "high.json", &file1.to_string());
        let p2 = write_temp_catalog(dir.path(), "low.json", &file2.to_string());
        let merged = load_and_merge_catalogs(&[p1, p2], dir.path()).unwrap();
        assert_eq!(merged.models.len(), 1);
        assert_eq!(merged.models[0].display_name, "Name from file 1");
        assert_eq!(merged.models[0].priority, 5);
    }

    #[test]
    fn merge_option_fallthrough() {
        let dir = tempfile::tempdir().unwrap();
        let mut model_a1 = minimal_model_info_json("model-a");
        model_a1.as_object_mut().unwrap().remove("context_window");
        let file1 = serde_json::json!({"models": [model_a1]});

        let mut model_a2 = minimal_model_info_json("model-a");
        model_a2["context_window"] = serde_json::json!(200000);
        let file2 = serde_json::json!({"models": [model_a2]});

        let p1 = write_temp_catalog(dir.path(), "high.json", &file1.to_string());
        let p2 = write_temp_catalog(dir.path(), "low.json", &file2.to_string());
        let merged = load_and_merge_catalogs(&[p1, p2], dir.path()).unwrap();
        assert_eq!(merged.models[0].context_window, Some(200000));
    }

    #[test]
    fn relative_path_resolved_to_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("catalogs");
        std::fs::create_dir_all(&subdir).unwrap();
        let content = serde_json::json!({"models": [minimal_model_info_json("m1")]});
        write_temp_catalog(&subdir, "models.json", &content.to_string());
        let relative = std::path::PathBuf::from("catalogs/models.json");
        let merged = load_and_merge_catalogs(&[relative], dir.path()).unwrap();
        assert_eq!(merged.models.len(), 1);
    }

    #[test]
    fn merge_vec_fields_complete_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut model_a1 = minimal_model_info_json("model-a");
        model_a1["experimental_supported_tools"] = serde_json::json!(["tool_x"]);
        let file1 = serde_json::json!({"models": [model_a1]});

        let mut model_a2 = minimal_model_info_json("model-a");
        model_a2["experimental_supported_tools"] = serde_json::json!(["tool_a", "tool_b"]);
        let file2 = serde_json::json!({"models": [model_a2]});

        let p1 = write_temp_catalog(dir.path(), "high.json", &file1.to_string());
        let p2 = write_temp_catalog(dir.path(), "low.json", &file2.to_string());
        let merged = load_and_merge_catalogs(&[p1, p2], dir.path()).unwrap();
        assert_eq!(
            merged.models[0].experimental_supported_tools,
            vec!["tool_x"]
        );
    }
}
