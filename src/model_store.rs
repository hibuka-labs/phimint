//! Model store for multi-model support.
//!
//! `ModelStore` manages the mapping from tier names (e.g., "main", "lite",
//! "advanced") to actual model names and their providers. It supports:
//! - Runtime tier switching (e.g., `/lite mimo-v2.5`)
//! - Lazy provider creation (only create providers when needed)
//! - Fallback to the main model when a tier is not configured

use std::collections::HashMap;
use std::sync::Arc;

use phi_agent::llm_trait::{LlmError, LlmProvider};

use crate::config::ModelConfig;

/// Model store that manages tier → model → provider mappings.
///
/// This is an application-layer structure used by phimint to manage
/// multiple model tiers. The framework layer (agent-base) doesn't
/// know about this - it only sees `Arc<dyn LlmProvider>`.
pub struct ModelStore {
    /// Tier name → (model, base_url, api_key, protocol) mapping
    tiers: HashMap<String, (String, String, String, String)>,

    /// Model name → provider cache
    providers: HashMap<String, Arc<dyn LlmProvider>>,

    /// The default provider (created from main tier config)
    default_provider: Arc<dyn LlmProvider>,

    /// Default model name
    default_model: String,

    /// Base configuration for creating new providers
    config: ModelConfig,
}

impl ModelStore {
    /// Create a new model store from configuration.
    ///
    /// The default provider is created from `config.main`.
    /// Other tiers are initialized from their config or fall back to main.
    pub fn new(
        config: ModelConfig,
        default_provider: Arc<dyn LlmProvider>,
    ) -> Self {
        let default_model = config.main.model().to_string();

        let mut tiers = HashMap::new();
        // Resolve all tiers
        let default_url = config.base_url.as_deref().unwrap_or("");
        let default_key = config.api_key.as_deref().unwrap_or("");
        let default_protocol = config.protocol.as_deref().unwrap_or("");

        // Main tier
        let (model, url, key, proto) = config.main.resolve(default_url, default_key, default_protocol);
        tiers.insert("main".to_string(), (model, url, key, proto));

        // Lite tier (falls back to main if not configured)
        let lite_config = config.lite.as_ref().unwrap_or(&config.main);
        let (model, url, key, proto) = lite_config.resolve(default_url, default_key, default_protocol);
        tiers.insert("lite".to_string(), (model, url, key, proto));

        // Advanced tier (falls back to main if not configured)
        let advanced_config = config.advanced.as_ref().unwrap_or(&config.main);
        let (model, url, key, proto) = advanced_config.resolve(default_url, default_key, default_protocol);
        tiers.insert("advanced".to_string(), (model, url, key, proto));

        Self {
            tiers,
            providers: HashMap::new(),
            default_provider,
            default_model,
            config,
        }
    }

    /// Get the model name for a tier.
    ///
    /// Falls back to the default model if the tier is not configured.
    pub fn tier_model(&self, tier: &str) -> &str {
        self.tiers
            .get(tier)
            .map(|(model, _, _, _)| model.as_str())
            .unwrap_or(&self.default_model)
    }

    /// Set the model name for a tier.
    ///
    /// This doesn't create the provider yet - it's created lazily
    /// when `get_or_create_provider` is called.
    pub fn set_tier_model(&mut self, tier: &str, model: String) {
        if let Some(entry) = self.tiers.get_mut(tier) {
            entry.0 = model;
        } else {
            // Create new entry with default url/key/protocol
            let default_url = self.config.base_url.as_deref().unwrap_or("");
            let default_key = self.config.api_key.as_deref().unwrap_or("");
            let default_protocol = self.config.protocol.as_deref().unwrap_or("");
            self.tiers.insert(tier.to_string(), (model, default_url.to_string(), default_key.to_string(), default_protocol.to_string()));
        }
    }

    /// Get or create a provider for a tier.
    ///
    /// If the tier uses the default model, returns the default provider.
    /// Otherwise, creates a new provider (cached for future use).
    pub fn get_or_create_provider(&mut self, tier: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
        let (model, base_url, api_key, protocol) = self.tiers.get(tier)
            .cloned()
            .unwrap_or_else(|| {
                let default_url = self.config.base_url.as_deref().unwrap_or("");
                let default_key = self.config.api_key.as_deref().unwrap_or("");
                let default_protocol = self.config.protocol.as_deref().unwrap_or("");
                (self.default_model.clone(), default_url.to_string(), default_key.to_string(), default_protocol.to_string())
            });

        // If using the default model, return the default provider
        if model == self.default_model {
            return Ok(self.default_provider.clone());
        }

        // Check cache
        if let Some(provider) = self.providers.get(&model) {
            return Ok(provider.clone());
        }

        // Create new provider
        let provider = self.create_provider(&model, &base_url, &api_key, &protocol)?;
        self.providers.insert(model, provider.clone());
        Ok(provider)
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Arc<dyn LlmProvider> {
        self.default_provider.clone()
    }

    /// List all configured tiers and their models.
    pub fn list_tiers(&self) -> Vec<(&str, &str)> {
        self.tiers
            .iter()
            .map(|(k, (model, _, _, _))| (k.as_str(), model.as_str()))
            .collect()
    }

    /// Create a provider for a specific model.
    fn create_provider(&self, model: &str, base_url: &str, api_key: &str, protocol: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
        // Use the unified provider factory
        let provider = phi_agent::create_provider(&phi_agent::llm_trait::LlmConfig {
            model: model.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            protocol: if protocol.is_empty() {
                None
            } else {
                protocol.parse::<phi_agent::llm_trait::Protocol>().ok()
            },
            ..Default::default()
        })?;
        Ok(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TierConfig;

    fn mock_config() -> ModelConfig {
        ModelConfig {
            base_url: Some("http://localhost:3200".to_string()),
            api_key: Some("sk-test".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("mimo-v2.5-pro".to_string()),
            lite: Some(TierConfig::Simple("mimo-v2.5".to_string())),
            advanced: None,
            scene_tiers: None,
        }
    }

    #[test]
    fn tier_model_returns_configured_model() {
        let config = mock_config();
        let store = ModelStore::new(config, Arc::new(MockProvider));

        assert_eq!(store.tier_model("main"), "mimo-v2.5-pro");
        assert_eq!(store.tier_model("lite"), "mimo-v2.5");
        // advanced falls back to default since not configured
        assert_eq!(store.tier_model("advanced"), "mimo-v2.5-pro");
    }

    #[test]
    fn tier_model_falls_back_to_default() {
        let config = mock_config();
        let store = ModelStore::new(config, Arc::new(MockProvider));

        assert_eq!(store.tier_model("unknown"), "mimo-v2.5-pro");
    }

    #[test]
    fn set_tier_model_updates_mapping() {
        let config = mock_config();
        let mut store = ModelStore::new(config, Arc::new(MockProvider));

        store.set_tier_model("lite", "qwen-turbo".to_string());
        assert_eq!(store.tier_model("lite"), "qwen-turbo");
    }

    #[test]
    fn list_tiers_returns_all_configured() {
        let config = mock_config();
        let store = ModelStore::new(config, Arc::new(MockProvider));

        let tiers = store.list_tiers();
        assert_eq!(tiers.len(), 3);
    }

    // Mock provider for testing
    struct MockProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn stream(&self, _request: phi_agent::llm_trait::ChatRequest) -> Result<phi_agent::llm_trait::ChatStream, LlmError> {
            Err(LlmError::llm("mock"))
        }

        async fn chat(&self, _request: phi_agent::llm_trait::ChatRequest) -> Result<phi_agent::llm_trait::ChatResponse, LlmError> {
            Err(LlmError::llm("mock"))
        }

        fn capabilities(&self) -> phi_agent::llm_trait::Capabilities {
            Default::default()
        }

        fn info(&self) -> phi_agent::llm_trait::ProviderInfo {
            phi_agent::llm_trait::ProviderInfo {
                name: "mock".to_string(),
                model: "mock-model".to_string(),
                version: None,
            }
        }
    }
}
