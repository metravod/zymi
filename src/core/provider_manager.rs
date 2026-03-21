use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, RwLock};

use super::anthropic::AnthropicProvider;
use super::chatgpt::ChatgptProvider;
use super::config::{save_models_config, ModelEntry, ModelsConfig, ProviderType};
use super::openai::OpenAiProvider;
use super::{LlmError, LlmProvider, LlmResponse, Message, StreamEvent, ToolDefinition};

#[derive(Debug, Clone)]
pub struct ModelSelectorEntry {
    pub id: String,
    pub name: String,
}

pub struct ProviderManager {
    inner: RwLock<Arc<dyn LlmProvider>>,
    current_model_id: RwLock<String>,
    config: RwLock<ModelsConfig>,
    memory_dir: PathBuf,
}

impl ProviderManager {
    pub fn new(config: ModelsConfig, memory_dir: PathBuf) -> Result<Self, LlmError> {
        let default_entry = config
            .models
            .iter()
            .find(|m| m.is_default)
            .or_else(|| config.models.first())
            .ok_or_else(|| LlmError::ApiError("No models configured".to_string()))?;

        log::info!(
            "Initializing provider manager: default_model={}, total_models={}",
            default_entry.id,
            config.models.len()
        );

        let provider = create_provider(default_entry, &memory_dir)?;
        let model_id = default_entry.id.clone();

        Ok(Self {
            inner: RwLock::new(provider),
            current_model_id: RwLock::new(model_id),
            config: RwLock::new(config),
            memory_dir,
        })
    }

    pub async fn switch_model(&self, model_id: &str) -> Result<(), LlmError> {
        log::info!("Switching model to '{}'", model_id);
        let config = self.config.read().await;
        let entry = config
            .models
            .iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| {
                log::error!("Unknown model requested: {}", model_id);
                LlmError::ApiError(format!("Unknown model: {model_id}"))
            })?;

        let provider = create_provider(entry, &self.memory_dir)?;

        *self.inner.write().await = provider;
        *self.current_model_id.write().await = model_id.to_string();
        log::info!("Model switched to '{}'", model_id);

        Ok(())
    }

    pub async fn current_model_id(&self) -> String {
        self.current_model_id.read().await.clone()
    }

    pub async fn available_models(&self) -> Vec<ModelSelectorEntry> {
        let config = self.config.read().await;
        config
            .models
            .iter()
            .map(|m| ModelSelectorEntry {
                id: m.id.clone(),
                name: m.name.clone(),
            })
            .collect()
    }

    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }

    pub async fn add_model(&self, entry: ModelEntry) -> Result<(), LlmError> {
        let mut config = self.config.write().await;
        config.models.push(entry);
        save_models_config(&self.memory_dir, &config);
        Ok(())
    }
}

fn create_provider(entry: &ModelEntry, memory_dir: &Path) -> Result<Arc<dyn LlmProvider>, LlmError> {
    log::info!(
        "Creating provider: model={}, provider={:?}, base_url={:?}",
        entry.id,
        entry.provider,
        entry.base_url
    );
    let direct_key = entry.api_key.clone().unwrap_or_default();
    let api_key = if !direct_key.is_empty() {
        direct_key
    } else {
        std::env::var(&entry.api_key_env).unwrap_or_default()
    };

    match entry.provider {
        ProviderType::OpenaiCompatible => {
            if api_key.is_empty() && entry.base_url.is_none() {
                Ok(Arc::new(OpenAiProvider::new(&entry.id)))
            } else {
                Ok(Arc::new(OpenAiProvider::with_config(
                    &entry.id,
                    &api_key,
                    entry.base_url.as_deref(),
                )))
            }
        }
        ProviderType::Anthropic => {
            if api_key.is_empty() {
                return Err(LlmError::ApiError(format!(
                    "API key env var '{}' is not set for model '{}'",
                    entry.api_key_env, entry.id
                )));
            }
            let provider = AnthropicProvider::new(
                &entry.id,
                &api_key,
                entry.base_url.as_deref(),
            )
            .map_err(LlmError::ApiError)?;
            Ok(Arc::new(provider))
        }
        ProviderType::ChatgptOauth => {
            let tokens = crate::auth::storage::load_tokens(memory_dir).ok_or_else(|| {
                LlmError::ApiError(
                    "No OAuth tokens found. Run `zymi login` first.".to_string(),
                )
            })?;
            Ok(Arc::new(ChatgptProvider::new(&entry.id, memory_dir, tokens)))
        }
    }
}

#[async_trait]
impl LlmProvider for ProviderManager {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError> {
        let provider = self.inner.read().await.clone();
        provider.chat(messages, tools).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let provider = self.inner.read().await.clone();
        provider.chat_stream(messages, tools, tx).await
    }
}
