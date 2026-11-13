//! Model store for multi-model support.
//!
//! `ModelStore` manages the mapping from tier names (e.g., "main", "lite",
//! "advanced") to actual model names and their providers. It supports:
//! - Runtime tier switching (e.g., `/lite mimo-v2.5`)
//! - Lazy provider creation (only create providers when needed)
//! - Fallback to the main model when a tier is not configured

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use phi_agent::llm_trait::{LlmError, LlmProvider};

use crate::model::config::{EndpointTuple, ModelConfig};

/// Resolves the name of the model the engine is serving *right now*.
///
/// Carried as a closure (wired up after the agent is built) so the engine's
/// provider-handle type — a phi-agent engine type phimint cannot name —
/// never appears in this module's signatures. Returns `None` when no
/// resolver is bound or nothing is serving yet.
pub type ServingNameFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Model store that manages tier → model → provider mappings.
///
/// This is an application-layer structure used by phimint to manage
/// multiple model tiers. The framework layer (agent-base) doesn't
/// know about this - it only sees `Arc<dyn LlmProvider>`.
pub struct ModelStore {
    /// Tier name → (model, base_url, api_key, protocol) mapping
    tiers: HashMap<String, EndpointTuple>,

    /// Model name → provider cache
    providers: HashMap<String, Arc<dyn LlmProvider>>,

    /// The default provider (created from the boot effective main)
    default_provider: Arc<dyn LlmProvider>,

    /// Default model name
    default_model: String,

    /// The endpoint tuple the default provider was built from. The default
    /// provider is handed out only when a tier's FULL tuple matches — same
    /// model name on a different endpoint is a different provider.
    default_tuple: EndpointTuple,

    /// Base configuration for creating new providers
    config: ModelConfig,

    /// The config file this store was loaded from, if any — a `/model` pick
    /// is persisted there (see [`crate::model::persist`]).
    config_path: Option<PathBuf>,

    /// Resolver for the name of the model the engine is serving right now
    /// (see [`ServingNameFn`]). The tier table names the *selection*, which
    /// moves before a staged engine swap lands — the identity stamp and the
    /// `current_model` tool read this instead so they cannot lie.
    serving_name: Option<ServingNameFn>,
}

impl ModelStore {
    /// Create a new model store from configuration.
    ///
    /// The tier table is built through THE one resolution rule
    /// ([`ModelConfig::tier_config`]): the effective `main` is the `model`
    /// pointer when set, else the configured `main`; unconfigured lite/
    /// advanced follow the effective `main`. The default provider was built
    /// from that same effective main (the boot provider in `main.rs`), so
    /// `default_tuple` records it — boot and session can never disagree
    /// because both read the one rule.
    pub fn new(config: ModelConfig, default_provider: Arc<dyn LlmProvider>) -> Self {
        let mut tiers = HashMap::new();
        for tier in ["main", "lite", "advanced"] {
            if let Some(tuple) = config.tier_config(tier) {
                tiers.insert(tier.to_string(), tuple);
            }
        }
        let default_tuple = config
            .tier_config("main")
            .expect("main tier always resolves");
        let default_model = default_tuple.0.clone();

        Self {
            tiers,
            providers: HashMap::new(),
            default_provider,
            default_model,
            default_tuple,
            config,
            config_path: None,
            serving_name: None,
        }
    }

    /// Remember the config file behind this store (set once at load time).
    pub fn set_config_path(&mut self, path: Option<PathBuf>) {
        self.config_path = path;
    }

    /// The config file behind this store, if any.
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// The base configuration (for picker enumeration).
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// The full endpoint tuple of a tier: `(model, base_url, api_key,
    /// protocol)`. Unknown tiers fall back to the default tuple.
    pub fn tier_tuple(&self, tier: &str) -> EndpointTuple {
        self.tiers
            .get(tier)
            .cloned()
            .unwrap_or_else(|| self.default_tuple.clone())
    }

    /// The tuple「clear selection」rolls the main tier back to: the configured
    /// `main` definition (selections — the pointer and `--model` — ride on
    /// top of it and never move it).
    pub fn configured_main(&self) -> EndpointTuple {
        self.config.configured_main()
    }

    /// Drop the in-memory selection pointer (part of「clear selection」): the
    /// `model` slot in the loaded config is a selection, not a definition.
    pub fn clear_pointer(&mut self) {
        self.config.model = None;
    }

    /// Bind the serving-name resolver (wired to the live engine's provider
    /// slot once the agent is built). Rebinding replaces it.
    pub fn bind_serving_name(&mut self, resolve: ServingNameFn) {
        self.serving_name = Some(resolve);
    }

    /// The name of the model the engine is serving right now: the resolver's
    /// answer when one is bound and it answers, else `None` — callers fall
    /// back to the tier table. This is the truth source behind the identity
    /// stamp and the `current_model` tool.
    pub fn serving_model(&self) -> Option<String> {
        self.serving_name.as_ref().and_then(|resolve| resolve())
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
            // New tier: ride the default tuple's endpoint (the documented
            // unknown-tier fallback), carrying only the new name.
            self.tiers.insert(
                tier.to_string(),
                (
                    model,
                    self.default_tuple.1.clone(),
                    self.default_tuple.2.clone(),
                    self.default_tuple.3.clone(),
                ),
            );
        }
    }

    /// Get or create a provider for a tier.
    ///
    /// If the tier's full endpoint tuple matches the default provider's, the
    /// default provider is returned; otherwise a provider is created (cached
    /// by endpoint tuple for future use).
    pub fn get_or_create_provider(&mut self, tier: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
        let (model, base_url, api_key, protocol) = self.tier_tuple(tier);

        self.provider_for(&model, &base_url, &api_key, &protocol)
    }

    /// Install a picked main model (the `/model` swap): create the provider
    /// FIRST — an error leaves the store untouched — then point the main tier
    /// at the picked endpoint tuple and cache the provider. An unconfigured
    /// lite/advanced tier follows the switch (the one resolution rule: an
    /// unconfigured tier sits on the effective `main`). In-memory here; the
    /// caller persists the pick to `config.json`'s `model` pointer
    /// (`model::persist`) so the next boot loads the same selection.
    pub fn apply_picked(
        &mut self,
        model: String,
        base_url: String,
        api_key: String,
        protocol: String,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
        let provider = self.provider_for(&model, &base_url, &api_key, &protocol)?;
        let tuple = (model, base_url, api_key, protocol);
        self.tiers.insert("main".to_string(), tuple.clone());
        for tier in ["lite", "advanced"] {
            if self.config.tier_def(tier).is_none() {
                self.tiers.insert(tier.to_string(), tuple.clone());
            }
        }
        Ok(provider)
    }

    /// The provider for one endpoint tuple: the default provider when the
    /// tuple is its own, the cached one when seen before, else a fresh
    /// creation. One place decides — a tier, a pick, and a re-pick of the
    /// original model all resolve the same way.
    fn provider_for(
        &mut self,
        model: &str,
        base_url: &str,
        api_key: &str,
        protocol: &str,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
        // The default provider is the one built from the boot tuple — same
        // model name on a different endpoint is a different provider.
        let boot = (
            self.default_tuple.0.as_str(),
            self.default_tuple.1.as_str(),
            self.default_tuple.2.as_str(),
            self.default_tuple.3.as_str(),
        );
        if (model, base_url, api_key, protocol) == boot {
            return Ok(self.default_provider.clone());
        }

        let key = format!("{model}|{base_url}|{protocol}|{api_key}");
        if let Some(provider) = self.providers.get(&key) {
            return Ok(provider.clone());
        }

        let provider = self.create_provider(model, base_url, api_key, protocol)?;
        self.providers.insert(key, provider.clone());
        Ok(provider)
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Arc<dyn LlmProvider> {
        self.default_provider.clone()
    }

    /// The boot endpoint tuple — the same-endpoint anchor for the spawn
    /// resolver: a child's client was built from this tuple (not from the
    /// live `main` tier), so a tier only lands a model override when its
    /// endpoint matches.
    pub fn default_tuple(&self) -> EndpointTuple {
        self.default_tuple.clone()
    }

    /// List all configured tiers and their models.
    pub fn list_tiers(&self) -> Vec<(&str, &str)> {
        self.tiers
            .iter()
            .map(|(k, (model, _, _, _))| (k.as_str(), model.as_str()))
            .collect()
    }

    /// Create a provider for a specific model.
    fn create_provider(
        &self,
        model: &str,
        base_url: &str,
        api_key: &str,
        protocol: &str,
    ) -> Result<Arc<dyn LlmProvider>, LlmError> {
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
        // Watched like the boot provider: a `/model` pick or a tier that
        // gets rejected has to end in the same notice — naming the key this
        // provider actually travels under, not the boot one.
        Ok(crate::model::auth_watch::watch(
            provider, model, base_url, api_key,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::TierConfig;

    fn mock_config() -> ModelConfig {
        ModelConfig {
            base_url: Some("http://localhost:3200".to_string()),
            api_key: Some("sk-test".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("mimo-v2.5-pro".to_string()),
            lite: Some(TierConfig::Simple("mimo-v2.5".to_string())),
            advanced: None,
            model: None,
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
            code_exec: Default::default(),
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

    /// protects: a pick installs the FULL picked endpoint tuple on the main
    /// tier and the provider that tuple creates. fails_when: the swap lands
    /// on the wrong tuple or the tier keeps serving the old provider.
    /// no_claim: whether the model exists at the provider.
    #[test]
    fn apply_picked_installs_the_picked_tuple_and_its_provider() {
        let mut store = ModelStore::new(mock_config(), Arc::new(MockProvider));
        let provider = store
            .apply_picked(
                "qwen-plus".to_string(),
                "https://api.example.com/v1".to_string(),
                "sk-x".to_string(),
                "openai".to_string(),
            )
            .expect("provider creation succeeds");
        assert_eq!(provider.info().model, "qwen-plus");
        assert_eq!(
            store.tier_tuple("main"),
            (
                "qwen-plus".to_string(),
                "https://api.example.com/v1".to_string(),
                "sk-x".to_string(),
                "openai".to_string(),
            )
        );
        // The main tier now resolves to that exact provider (cache, not a
        // second construction).
        let again = store.get_or_create_provider("main").expect("cached");
        assert!(Arc::ptr_eq(&provider, &again));
    }

    /// protects: the provider identity is the endpoint tuple, not the name —
    /// the same model name on a different endpoint is a different provider,
    /// never the boot default. fails_when: a name match silently hands back
    /// the default provider on the wrong endpoint. no_claim: how the
    /// endpoints differ in behavior.
    #[test]
    fn same_name_on_a_different_endpoint_is_not_the_default_provider() {
        let mut store = ModelStore::new(mock_config(), Arc::new(MockProvider));
        let swapped = store
            .apply_picked(
                "mimo-v2.5-pro".to_string(),
                "https://other.example.com/v1".to_string(),
                "sk-o".to_string(),
                "openai".to_string(),
            )
            .expect("provider creation succeeds");
        assert!(
            !Arc::ptr_eq(&swapped, &store.default_provider()),
            "same name, different endpoint: not the default provider"
        );
        let again = store.get_or_create_provider("main").expect("cached");
        assert!(Arc::ptr_eq(&swapped, &again));
    }

    /// protects: the default provider is handed out only when the FULL tuple
    /// matches the one it was built from. fails_when: a same-name
    /// different-endpoint tier is answered with the default provider.
    /// no_claim: which endpoint a product boots on.
    #[test]
    fn the_default_provider_comes_back_only_on_a_full_tuple_match() {
        let mut store = ModelStore::new(mock_config(), Arc::new(MockProvider));
        let boot = store.get_or_create_provider("main").expect("boot tuple");
        assert!(Arc::ptr_eq(&boot, &store.default_provider()));

        // Re-picking the exact boot tuple is the default provider again.
        let repicked = store
            .apply_picked(
                "mimo-v2.5-pro".to_string(),
                "http://localhost:3200".to_string(),
                "sk-test".to_string(),
                "openai".to_string(),
            )
            .expect("provider creation succeeds");
        assert!(Arc::ptr_eq(&repicked, &store.default_provider()));
    }

    /// protects: the serving resolver — not the tier table — names the model
    /// the engine is serving, and rebinding replaces it; an unbound store
    /// reports no serving name so callers can fall back to the tier table.
    /// fails_when: `serving_model` reports the tier name, survives a rebind,
    /// or returns a name while unbound. no_claim: when the engine lands a
    /// staged swap.
    #[test]
    fn the_serving_resolver_names_the_serving_model() {
        let mut store = ModelStore::new(mock_config(), Arc::new(MockProvider));
        assert_eq!(store.serving_model(), None);
        store.bind_serving_name(Arc::new(|| Some("serving-model".to_string())));
        assert_eq!(store.serving_model().as_deref(), Some("serving-model"));
        store.bind_serving_name(Arc::new(|| Some("other-model".to_string())));
        assert_eq!(store.serving_model().as_deref(), Some("other-model"));
    }

    // Mock provider for testing
    struct MockProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn stream(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatStream, LlmError> {
            Err(LlmError::llm("mock"))
        }

        async fn chat(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatResponse, LlmError> {
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

    /// A local endpoint that rejects every request with 401 - a real
    /// socket, so the provider fails the way a wrong key really fails.
    async fn rejecting_endpoint() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let body = "{\"error\":{\"message\":\"invalid api key\"}}";
                    let reply = format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.flush().await;
                });
            }
        });
        format!("http://{addr}/v1")
    }

    /// protects: a provider the store builds - a `/model` pick, a tier - is
    /// watched like the boot provider, so a rejection on any of them ends in
    /// the same notice. fails_when: the store hands out unwrapped providers
    /// and the rejection is silent. no_claim: the boot provider (main.rs
    /// installs it).
    #[tokio::test]
    async fn a_provider_the_store_builds_reports_its_rejection_to_the_watch() {
        let _guard = crate::model::auth_watch::watch_test_lock().lock().await;
        crate::model::auth_watch::reset();
        crate::model::auth_watch::set_context(crate::model::auth_watch::AuthContext {
            config_path: std::path::PathBuf::from("/tmp/phimint-config.json"),
            masked_key: "sk-t\u{2026}000".to_string(),
            config_base_url: "https://api.openai.com/v1".to_string(),
            key_source: None,
        });

        let base_url = rejecting_endpoint().await;
        let mut store = ModelStore::new(mock_config(), Arc::new(MockProvider));
        let provider = store
            .apply_picked(
                "qwen-plus".to_string(),
                base_url,
                "sk-pick-abcdef123456".to_string(),
                "openai".to_string(),
            )
            .expect("provider creation succeeds");

        let request = phi_agent::llm_trait::ChatRequest::new(Vec::new());
        let err = match provider.chat(request).await {
            Ok(_) => panic!("the stub rejects every request"),
            Err(err) => err,
        };
        assert_eq!(err.status(), Some(401), "the rejection reaches the caller");

        let notice = crate::model::auth_watch::take_notice().expect("the rejection is announced");
        assert!(notice.contains("401"), "names the status: {notice}");
        // The notice names the key the store-built provider actually used
        // (masked `sk-p…456`), with the boot key marked as the boot key —
        // not the other way around.
        assert!(
            notice.contains("sk-p\u{2026}456"),
            "shows the key the request used: {notice}"
        );
        assert!(
            notice.contains("boot key is sk-t\u{2026}000"),
            "the boot key is named as the boot key: {notice}"
        );
    }
}
