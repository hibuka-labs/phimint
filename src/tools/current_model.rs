//! `current_model` — the identity-question answer source.
//!
//! Feedback 2026-11-07: the weaker backends (mimo-v2.6-pro) answer "which
//! model are you" from conversation history even when the stamp names the
//! live model — 2 of 8 minimal replays echoed an earlier name, and copy
//! hardening did not fix it. Measured fix: this tool, which the stamp
//! routes identity questions to. A tool result is fresh context that
//! history cannot pre-fill — 8 of 8 measured replays called it and
//! answered with its value.
//!
//! The answer is the serving model name — the live provider's own
//! report when one is in hand, then the store's serving resolver, then
//! the `main` tier entry. The stamp (`crate::model::identity`) resolves
//! through the same chain, so its fallback name and this tool's result
//! can never disagree.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::llm_trait::LlmProvider;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::model::store::ModelStore;

/// Answers "which model are you" from the live store (see module docs).
pub struct CurrentModel {
    store: Arc<Mutex<ModelStore>>,
}

impl CurrentModel {
    /// `store` = the live tier table; the tool reads its `main`.
    pub fn new(store: Arc<Mutex<ModelStore>>) -> Self {
        Self { store }
    }

    /// The store-resolved serving name: the serving resolver's answer
    /// when one is bound, else the live `main` tier entry.
    pub async fn live_main(&self) -> String {
        let store = self.store.lock().await;
        store
            .serving_model()
            .unwrap_or_else(|| store.tier_model("main").to_string())
    }

    /// The answer for one call: a live provider report in hand beats the
    /// store's resolver, which beats the tier table.
    pub async fn answer(&self, llm_client: Option<&Arc<dyn LlmProvider>>) -> String {
        if let Some(model) = llm_client.map(|p| p.info().model).filter(|m| !m.is_empty()) {
            return model;
        }
        self.live_main().await
    }
}

#[async_trait]
impl Tool for CurrentModel {
    fn name(&self) -> &'static str {
        "current_model"
    }

    fn description(&self) -> &'static str {
        "The model serving this session. Call before answering any question about which model you are."
    }

    fn schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "The model serving this session.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, _args: &Value, ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        Ok(vec![Content::text(
            self.answer(ctx.llm_client.as_ref()).await,
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{ModelConfig, TierConfig};
    use phi_agent::llm_trait::{
        Capabilities, ChatRequest, ChatResponse, ChatStream, LlmError, LlmProvider, ProviderInfo,
    };

    struct StubProvider;

    #[async_trait]
    impl LlmProvider for StubProvider {
        async fn stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
            Err(LlmError::llm("stub"))
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::llm("stub"))
        }
        fn capabilities(&self) -> Capabilities {
            Default::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "stub".to_string(),
                model: "stub-model".to_string(),
                version: None,
            }
        }
    }

    fn store(main: &str) -> ModelStore {
        ModelStore::new(
            ModelConfig {
                base_url: Some("https://api.example.com/v1".to_string()),
                api_key: Some("sk-test".to_string()),
                protocol: Some("openai".to_string()),
                main: TierConfig::Simple(main.to_string()),
                lite: None,
                advanced: None,
                model: None,
                scene_tiers: None,
                update: Default::default(),
                ui: Default::default(),
                code_exec: Default::default(),
            },
            Arc::new(StubProvider),
        )
    }

    /// protects: the tool's wire name is exactly the name the stamp points
    /// at, it takes no input, and it asks to be called before any identity
    /// answer. fails_when: the name diverges from the stamp copy or the
    /// schema grows required fields. no_claim: the stamp's wording.
    #[test]
    fn the_tool_declares_what_the_stamp_points_at() {
        let tool = CurrentModel::new(Arc::new(Mutex::new(store("alpha-model"))));
        assert_eq!(tool.name(), "current_model");
        assert!(
            tool.description()
                .contains("Call before answering any question about which model you are"),
            "{}",
            tool.description()
        );
        assert_eq!(
            tool.schema(),
            serde_json::json!({ "type": "object", "properties": {} })
        );
    }

    /// protects: the answer tracks the live main model — a swap is visible
    /// immediately, with no history or persistence in between. fails_when:
    /// the tool reports a pre-swap name after `set_tier_model`. no_claim:
    /// what the model does with the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_answer_is_the_live_main_model() {
        let store = Arc::new(Mutex::new(store("alpha-model")));
        let tool = CurrentModel::new(Arc::clone(&store));
        assert_eq!(tool.live_main().await, "alpha-model");

        store
            .lock()
            .await
            .set_tier_model("main", "beta-model".to_string());
        assert_eq!(tool.live_main().await, "beta-model");
    }

    struct NamedProvider(String);

    #[async_trait]
    impl LlmProvider for NamedProvider {
        async fn stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
            Err(LlmError::llm("stub"))
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::llm("stub"))
        }
        fn capabilities(&self) -> Capabilities {
            Default::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "stub".to_string(),
                model: self.0.clone(),
                version: None,
            }
        }
    }

    /// protects: the answer names the model the live provider itself
    /// reports, even while the tier table names a different model — a
    /// staged (deferred) engine swap cannot make the tool lie; and with
    /// no provider in hand the store chain decides. fails_when: `answer`
    /// returns the tier name while a provider report is in hand.
    /// no_claim: what the model does with the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_provider_report_beats_the_tier_table() {
        let store = Arc::new(Mutex::new(store("selected-model")));
        let tool = CurrentModel::new(Arc::clone(&store));
        let provider: Arc<dyn LlmProvider> = Arc::new(NamedProvider("serving-model".to_string()));
        assert_eq!(tool.answer(Some(&provider)).await, "serving-model");
        assert_eq!(tool.answer(None).await, "selected-model");
    }

    /// protects: the store's serving resolver beats the tier table — the
    /// same chain the stamp resolves through — and a resolver that cannot
    /// name a model falls back to the tier entry. fails_when: `live_main`
    /// reports the tier name while a bound resolver answers a different
    /// one. no_claim: the stamp's wording.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_serving_resolver_beats_the_tier_table() {
        let store = Arc::new(Mutex::new(store("selected-model")));
        store
            .lock()
            .await
            .bind_serving_name(Arc::new(|| Some("serving-model".to_string())));
        let tool = CurrentModel::new(Arc::clone(&store));
        assert_eq!(tool.live_main().await, "serving-model");

        store.lock().await.bind_serving_name(Arc::new(|| None));
        assert_eq!(tool.live_main().await, "selected-model");
    }
}
