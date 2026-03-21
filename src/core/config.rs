use std::path::Path;

use serde::{Deserialize, Serialize};

const DEFAULT_MODEL: &str = "gpt-4.1-mini";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    OpenaiCompatible,
    Anthropic,
    ChatgptOauth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub provider: ProviderType,
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    /// Price per 1M input tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_1m: Option<f64>,
    /// Price per 1M output tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_1m: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Maximum tool-use iterations before forcing a text response.
    #[serde(default = "AgentSettings::default_max_iterations")]
    pub max_iterations: usize,
    /// Conversation message count that triggers auto-summarization.
    #[serde(default = "AgentSettings::default_summary_threshold")]
    pub summary_threshold: usize,
    /// Git heartbeat interval in seconds.
    #[serde(default = "AgentSettings::default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    /// Enable vector-based tool selection (RAG for tools).
    /// Requires OPENAI_API_KEY for embeddings.
    #[serde(default = "AgentSettings::default_tool_selection")]
    pub tool_selection: bool,
    /// Number of top-K tools to include (besides always-available).
    #[serde(default = "AgentSettings::default_tool_selection_top_k")]
    pub tool_selection_top_k: usize,
    /// Enable auto-extraction of facts from user messages to long-term memory.
    #[serde(default = "AgentSettings::default_auto_extract")]
    pub auto_extract: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_iterations: Self::default_max_iterations(),
            summary_threshold: Self::default_summary_threshold(),
            heartbeat_interval_secs: Self::default_heartbeat_interval_secs(),
            tool_selection: Self::default_tool_selection(),
            tool_selection_top_k: Self::default_tool_selection_top_k(),
            auto_extract: Self::default_auto_extract(),
        }
    }
}

impl AgentSettings {
    fn default_max_iterations() -> usize { 15 }
    fn default_summary_threshold() -> usize { 80 }
    fn default_heartbeat_interval_secs() -> u64 { 300 }
    fn default_tool_selection() -> bool { true }
    fn default_tool_selection_top_k() -> usize { 8 }
    fn default_auto_extract() -> bool { true }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub settings: AgentSettings,
}

pub fn save_models_config(memory_dir: &Path, config: &ModelsConfig) {
    let path = memory_dir.join("models.json");
    let tmp_path = memory_dir.join("models.json.tmp");

    let content = match serde_json::to_string_pretty(config) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to serialize models config: {e}");
            return;
        }
    };

    if let Err(e) = std::fs::write(&tmp_path, &content) {
        log::error!("Failed to write temp models config file: {e}");
        return;
    }

    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        log::error!("Failed to rename models config file: {e}");
    }
}

pub fn load_models_config(memory_dir: &Path) -> ModelsConfig {
    let path = memory_dir.join("models.json");

    if let Ok(content) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<ModelsConfig>(&content) {
            Ok(config) if !config.models.is_empty() => return config,
            Ok(_) => log::warn!("models.json has empty models array, falling back to env vars"),
            Err(e) => log::warn!("Failed to parse models.json: {e}, falling back to env vars"),
        }
    }

    // Fallback: create config from environment variables
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    ModelsConfig {
        models: vec![ModelEntry {
            id: model.clone(),
            name: model,
            provider: ProviderType::OpenaiCompatible,
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: None,
            api_key: None,
            is_default: true,
            input_price_per_1m: None,
            output_price_per_1m: None,
        }],
        settings: Default::default(),
    }
}
