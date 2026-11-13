//! Lite-pinned judge plumbing (multi-model routing decision 3).
//!
//! The guard judge is cheap classification work: it stays on the `lite` tier
//! and must not follow `/model` picks — except by the one designed route: an
//! unconfigured `lite` resolves to the effective `main` (the design's "lite
//! 未配置时跟 main", so the judge still serves where the session serves).
//! Two pieces, both zero-framework:
//!
//! - [`LitePinnedClient`] — a delegating provider that resolves the tier the
//!   scene table names **per call**, so a tier change takes effect on the
//!   judge's very next call. No build-time snapshot can go stale.
//! - [`LitePinnedGuard`] — wraps the real guard and drops
//!   `set_provider_source`. The framework offers LLM-calling guards the
//!   engine's live provider handle as an anti-stale measure (session
//!   20261107_27dc727a), but accepting it would swap the pinned client out
//!   for the engine's provider — the exact `/model` coupling the pin exists
//!   to prevent. The wrapper takes the hit instead: the inner guard keeps
//!   the client it was built with.

use std::sync::Arc;

use agent_base::{GuardCtx, GuardDecision, NoticeHandle, ReactLoopGuard};
use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, ChatStream, LlmError, LlmProvider, ProviderInfo,
};

use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;

/// The scene whose tier pins the judge — the scene table is the single
/// adjudication surface (review A-1): re-pinning is a table edit, not code.
pub const SCENE_GUARD: &str = "guard";

type StoreHandle = Arc<tokio::sync::Mutex<ModelStore>>;

/// A delegating provider pinned to the scene table's `guard` tier.
///
/// Resolves per call through [`PhimintRouter::route`] + the live model store;
/// when either handle is absent (tests) or the tier cannot produce a
/// provider, the built fallback client serves — the judge degrades to the
/// main model instead of going dark (routing decision 3: unconfigured
/// `lite` already resolves to the effective `main` inside the store).
pub struct LitePinnedClient {
    fallback: Arc<dyn LlmProvider>,
    store: Option<StoreHandle>,
    router: Option<Arc<PhimintRouter>>,
}

impl LitePinnedClient {
    pub fn new(
        fallback: Arc<dyn LlmProvider>,
        store: Option<StoreHandle>,
        router: Option<Arc<PhimintRouter>>,
    ) -> Self {
        Self {
            fallback,
            store,
            router,
        }
    }

    /// The provider the judge should serve through right now.
    ///
    /// Public so tests can observe per-call re-resolution without ever
    /// letting a call reach a real endpoint.
    pub async fn pinned_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        let (store, router) = (self.store.as_ref()?, self.router.as_ref()?);
        let tier = router.route(SCENE_GUARD).to_string();
        let mut store = store.lock().await;
        match store.get_or_create_provider(&tier) {
            Ok(provider) => Some(provider),
            Err(err) => {
                tracing::warn!(
                    "lite pin: tier `{}` has no provider ({}); the judge \
                     falls back to its built client",
                    tier,
                    err
                );
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for LitePinnedClient {
    async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let target = self
            .pinned_provider()
            .await
            .unwrap_or_else(|| Arc::clone(&self.fallback));
        target.stream(request).await
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let target = self
            .pinned_provider()
            .await
            .unwrap_or_else(|| Arc::clone(&self.fallback));
        target.chat(request).await
    }

    /// Display metadata only — the wire model is stamped by the resolved
    /// provider's own request building, so the fallback's identity is the
    /// honest one for these sync accessors.
    fn capabilities(&self) -> Capabilities {
        self.fallback.capabilities()
    }

    fn info(&self) -> ProviderInfo {
        self.fallback.info()
    }
}

/// Wraps a guard so the engine's provider handle never replaces its client.
///
/// Everything passes through to the inner guard except
/// [`ReactLoopGuard::set_provider_source`], which the trait's default empty
/// implementation swallows (the pin — see the module docs).
pub struct LitePinnedGuard {
    inner: Arc<dyn ReactLoopGuard>,
}

impl LitePinnedGuard {
    /// `Arc` in so a caller (or a test) can hold the inner guard alongside
    /// the wrapper.
    pub fn new(inner: Arc<dyn ReactLoopGuard>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl ReactLoopGuard for LitePinnedGuard {
    async fn on_turn(&self, ctx: &GuardCtx) -> GuardDecision {
        self.inner.on_turn(ctx).await
    }

    async fn on_tool_call(&self, ctx: &GuardCtx) -> GuardDecision {
        self.inner.on_tool_call(ctx).await
    }

    fn set_notice(&self, handle: NoticeHandle) {
        self.inner.set_notice(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use agent_base::ProviderHandle;
    use agent_base::types::{FinishReason, SessionId};

    use crate::model::config::{ModelConfig, TierConfig};

    struct RecordingProvider {
        model: &'static str,
        calls: AtomicUsize,
    }

    impl RecordingProvider {
        fn new(model: &'static str) -> Arc<RecordingProvider> {
            Arc::new(Self {
                model,
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for RecordingProvider {
        async fn stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
            Err(LlmError::api(500, "recording stub"))
        }

        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LlmError::api(500, "recording stub"))
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "recording".to_string(),
                model: self.model.to_string(),
                version: None,
            }
        }
    }

    fn config(lite: Option<&str>, advanced: Option<&str>) -> ModelConfig {
        ModelConfig {
            base_url: Some("https://api.openai.com/v1".to_string()),
            api_key: Some("test-key".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("gpt-4".to_string()),
            lite: lite.map(|m| TierConfig::Simple(m.to_string())),
            advanced: advanced.map(|m| TierConfig::Simple(m.to_string())),
            model: None,
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
            code_exec: Default::default(),
        }
    }

    fn client_with(
        store: Option<StoreHandle>,
        router: Option<Arc<PhimintRouter>>,
        fallback: Arc<dyn LlmProvider>,
    ) -> LitePinnedClient {
        LitePinnedClient::new(fallback, store, router)
    }

    fn as_provider(p: &Arc<RecordingProvider>) -> Arc<dyn LlmProvider> {
        p.clone()
    }

    #[tokio::test]
    async fn unconfigured_lite_resolves_to_the_effective_main() {
        // Routing decision 3's degradation clause: with only one model
        // configured, the judge serves the boot provider — no extra config
        // is ever required.
        let boot = RecordingProvider::new("boot-model");
        let store = Arc::new(tokio::sync::Mutex::new(ModelStore::new(
            config(None, None),
            as_provider(&boot),
        )));
        let client = client_with(
            Some(store),
            Some(Arc::new(PhimintRouter::new())),
            RecordingProvider::new("fallback-model"),
        );
        let pinned = client.pinned_provider().await.expect("resolves");
        assert!(
            Arc::ptr_eq(&pinned, &as_provider(&boot)),
            "unconfigured lite must land on the boot provider"
        );
    }

    #[tokio::test]
    async fn per_call_resolution_follows_the_tier_table() {
        // No snapshot: a tier change is served by the very next call (the
        // "换档 → 下一调用换靶" row of the test review).
        let boot = RecordingProvider::new("boot-model");
        let store = Arc::new(tokio::sync::Mutex::new(ModelStore::new(
            config(Some("lite-model-a"), None),
            boot,
        )));
        let client = client_with(
            Some(Arc::clone(&store)),
            Some(Arc::new(PhimintRouter::new())),
            RecordingProvider::new("fallback-model"),
        );
        let first = client.pinned_provider().await.expect("resolves");
        assert_eq!(first.info().model, "lite-model-a");

        store
            .lock()
            .await
            .set_tier_model("lite", "lite-model-b".to_string());
        let second = client.pinned_provider().await.expect("resolves");
        assert_eq!(second.info().model, "lite-model-b");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "the target must be re-resolved, not snapshotted"
        );
    }

    #[tokio::test]
    async fn the_judge_does_not_follow_model_picks() {
        // The pin's whole point: `/model` moves `main`; the judge stays.
        let boot = RecordingProvider::new("boot-model");
        let store = Arc::new(tokio::sync::Mutex::new(ModelStore::new(
            config(Some("lite-model-a"), None),
            boot,
        )));
        let client = client_with(
            Some(Arc::clone(&store)),
            Some(Arc::new(PhimintRouter::new())),
            RecordingProvider::new("fallback-model"),
        );
        assert_eq!(
            client.pinned_provider().await.expect("res").info().model,
            "lite-model-a"
        );

        {
            let mut store = store.lock().await;
            store
                .apply_picked(
                    "gpt-x".to_string(),
                    "https://api.openai.com/v1".to_string(),
                    "test-key".to_string(),
                    "openai".to_string(),
                )
                .expect("pick applies");
            assert_eq!(store.tier_model("main"), "gpt-x", "the pick landed");
        }
        assert_eq!(
            client.pinned_provider().await.expect("res").info().model,
            "lite-model-a"
        );
    }

    #[tokio::test]
    async fn the_scene_table_decides_which_tier_pins_the_judge() {
        // A-1: the router is the single adjudication surface — a custom
        // table re-pins the judge without code changes.
        let boot = RecordingProvider::new("boot-model");
        let store = Arc::new(tokio::sync::Mutex::new(ModelStore::new(
            config(Some("lite-model-a"), Some("adv-model")),
            boot,
        )));
        let mut table = std::collections::HashMap::new();
        table.insert(SCENE_GUARD.to_string(), "advanced".to_string());
        let router = Arc::new(PhimintRouter::with_scene_tiers(table));
        let client = client_with(
            Some(store),
            Some(router),
            RecordingProvider::new("fallback-model"),
        );
        assert_eq!(
            client.pinned_provider().await.expect("res").info().model,
            "adv-model"
        );
    }

    #[tokio::test]
    async fn the_fallback_client_serves_without_a_store() {
        let fallback = RecordingProvider::new("fallback-model");
        let client = client_with(None, None, as_provider(&fallback));
        assert!(
            client.pinned_provider().await.is_none(),
            "nothing to pin to"
        );
        let err = client
            .chat(ChatRequest::new(vec![]))
            .await
            .expect_err("stub");
        assert!(err.to_string().contains("recording stub"));
        assert_eq!(
            fallback.calls.load(Ordering::SeqCst),
            1,
            "the fallback must serve the judge's call"
        );
    }

    // Value: protects=the judge degrades to its built client instead of
    // going dark when the pinned tier cannot produce a provider (a broken
    // tier endpoint: empty base_url → "LLM_BASE_URL is required");
    // fails_when=pinned_provider propagates the failure or the judge is left
    // without any client, so the completion judge stops judging;
    // why_new=the Err branch of pinned_provider is uncovered — the existing
    // tests only exercise a missing store and a healthy tier;
    // seam=none; no_claim=the healthy-tier resolution rules (proven above)
    // and the booted TUI (pty scenario 14).
    #[tokio::test]
    async fn a_tier_without_a_provider_falls_back_to_the_built_client() {
        let boot = RecordingProvider::new("boot-model");
        let mut cfg = config(Some("lite-model-a"), None);
        // A tier whose endpoint is unusable: the provider factory refuses it.
        cfg.lite = Some(TierConfig::Full {
            model: "lite-model-a".to_string(),
            base_url: Some(String::new()),
            api_key: Some("test-key".to_string()),
            protocol: Some("openai".to_string()),
        });
        let store = Arc::new(tokio::sync::Mutex::new(ModelStore::new(cfg, boot)));
        let fallback = RecordingProvider::new("fallback-model");
        let client = client_with(
            Some(store),
            Some(Arc::new(PhimintRouter::new())),
            as_provider(&fallback),
        );

        assert!(
            client.pinned_provider().await.is_none(),
            "the broken tier yields no provider"
        );
        let err = client
            .chat(ChatRequest::new(vec![]))
            .await
            .expect_err("stub");
        assert!(err.to_string().contains("recording stub"));
        assert_eq!(
            fallback.calls.load(Ordering::SeqCst),
            1,
            "the built client must serve the judge when the tier fails"
        );
    }

    // ── the guard wrapper (the pin itself) ───────────────────────────────

    struct FakeGuard {
        provider_source: AtomicBool,
        notice: AtomicBool,
    }

    impl FakeGuard {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                provider_source: AtomicBool::new(false),
                notice: AtomicBool::new(false),
            })
        }
    }

    #[async_trait::async_trait]
    impl ReactLoopGuard for FakeGuard {
        async fn on_turn(&self, _ctx: &GuardCtx) -> GuardDecision {
            GuardDecision::Fail {
                error: "turn-marker".to_string(),
            }
        }

        async fn on_tool_call(&self, _ctx: &GuardCtx) -> GuardDecision {
            GuardDecision::Fail {
                error: "tool-marker".to_string(),
            }
        }

        fn set_notice(&self, _handle: NoticeHandle) {
            self.notice.store(true, Ordering::SeqCst);
        }

        fn set_provider_source(&self, _handle: ProviderHandle) {
            self.provider_source.store(true, Ordering::SeqCst);
        }
    }

    fn test_ctx() -> GuardCtx {
        GuardCtx {
            session_id: SessionId::new(1),
            turn_count: 1,
            user_input: "test".to_string(),
            model_response: "response".to_string(),
            finish_reason: FinishReason::Stop,
            available_tools: vec![],
            reasoning_only_strikes: 0,
            empty_response_strikes: 0,
            run_has_actions: false,
            last_tool_calls_invalid: false,
            all_user_inputs: vec!["test".to_string()],
            is_reasoning_only: false,
            is_empty_response: false,
            is_text_only: false,
            awaiting_human: false,
            thinking_disabled: false,
            original_thinking_enabled: true,
            remaining_turns: 50,
        }
    }

    #[tokio::test]
    async fn the_wrapper_drops_provider_source_and_passes_the_rest() {
        let inner = FakeGuard::new();
        let guard = LitePinnedGuard::new(Arc::clone(&inner) as Arc<dyn ReactLoopGuard>);

        // The engine offers its live provider handle — the wrapper swallows
        // it so the pinned client inside the inner guard survives.
        guard.set_provider_source(ProviderHandle::new(RecordingProvider::new("engine-model")));
        assert!(
            !inner.provider_source.load(Ordering::SeqCst),
            "set_provider_source must never reach the inner guard"
        );

        let (notice, _rx) = NoticeHandle::new_channel();
        guard.set_notice(notice);
        assert!(
            inner.notice.load(Ordering::SeqCst),
            "set_notice passes through"
        );

        match guard.on_turn(&test_ctx()).await {
            GuardDecision::Fail { error } => assert_eq!(error, "turn-marker"),
            _ => panic!("on_turn must pass through"),
        }
        match guard.on_tool_call(&test_ctx()).await {
            GuardDecision::Fail { error } => assert_eq!(error, "tool-marker"),
            _ => panic!("on_tool_call must pass through"),
        }
    }
}
