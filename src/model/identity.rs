//! The live model's identity stamp on the outgoing system message.
//!
//! Feedback 2026-11-07: after a `/model` swap the agent still answered "what
//! model are you" with the previous name. The swap lands on the wire (the
//! request's `model` field) and on `config.json` (see
//! [`crate::model::persist`]), but neither is in the model's context: the
//! confirm line is UI-only and never enters history, and the only in-context
//! name was the agent's own earlier answer — which the weaker backends echo
//! back even when contradicted (mimo-v2.6-pro answered from history on 2 of
//! 8 minimal replays; copy hardening did not fix it).
//!
//! [`ModelIdentityStamp`] stamps a `## Current model` section onto the
//! request-side system message before every LLM call, naming the model the
//! engine is serving (the same chain [`ModelStore::serving_model`] resolves —
//! the tier table names the *selection*, which moves before a staged engine
//! swap lands) and routing identity questions to the `current_model`
//! tool (measured 8/8 correct on the same replays). The section is a
//! current-state statement (refreshed in place each turn, the same contract
//! as the skills catalog and the browser tool face): an LLM call has no
//! memory beyond the messages it is given, so a name that is not in the
//! prompt of *every* call after the swap is a name the model never sees. It
//! rides the request clone only — the session's stored history is never
//! rewritten, and a resumed session picks the stamp up again on its next
//! request.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, ChatMessage, Middleware, PreLlmCtx};
use tokio::sync::Mutex;

use crate::model::store::ModelStore;

/// Header of the current-model section in the system prompt.
const STAMP_HEADER: &str = "## Current model";

/// Byte sequence marking the start of a stamp region: the `\n\n` separator
/// the renderer prepends plus the header line (the same anchor shape the
/// browser tool face uses).
const STAMP_ANCHOR: &str = "\n\n## Current model\n\n";

/// A `## ` section start — the boundary a stamp region may never cross (the
/// same mark the browser face and the skills catalog surgery use, so the
/// regions can sit in one prompt without seeing each other).
const NEXT_HEADING_MARK: &str = "\n\n## ";

/// The stamp section naming `model`. The copy is the variant measured 8/8
/// correct on the history-interference replays (2026-11-07 probe): name the
/// model, route identity questions to the `current_model` tool, and forbid
/// echoing names from earlier in the conversation.
fn render_section(model: &str) -> String {
    format!(
        "\n\n{STAMP_HEADER}\n\n\
         The model serving this conversation is `{model}`. If asked which model you are, \
         call `current_model` and answer with its result. Never repeat a model name from \
         earlier in this conversation — the session may have switched away from it.\n"
    )
}

/// Locate one stamp region: from the anchor through the next `## ` heading
/// (or end of content).
fn find_stamp_region(content: &str) -> Option<(usize, usize)> {
    let start = content.find(STAMP_ANCHOR)?;
    let search_from = start + STAMP_ANCHOR.len();
    let end = content[search_from..]
        .find(NEXT_HEADING_MARK)
        .map_or(content.len(), |rel| search_from + rel);
    Some((start, end))
}

/// Swap the `## Current model` region for `section` (append when absent).
///
/// Steady state — one region, same section text — is a fixed point, so an
/// unchanged model keeps the prompt byte-identical for provider-side
/// caching. Content outside the region is preserved byte-for-byte.
fn refresh_stamp_section(content: &str, section: &str) -> String {
    match find_stamp_region(content) {
        Some((start, end)) => format!("{}{}{}", &content[..start], section, &content[end..]),
        None => format!("{content}{section}"),
    }
}

/// Per-turn current-model stamp (see the module docs).
pub struct ModelIdentityStamp {
    store: Arc<Mutex<ModelStore>>,
}

impl ModelIdentityStamp {
    /// Stamp the live main model of `store` onto every outgoing request.
    pub fn new(store: Arc<Mutex<ModelStore>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Middleware for ModelIdentityStamp {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        // The serving name is the truth: after a deferred (staged) engine
        // swap the tier table already names the pick while the old provider
        // is still serving the turn, and the stamp must not lie about it.
        let store = self.store.lock().await;
        let model = store
            .serving_model()
            .unwrap_or_else(|| store.tier_model("main").to_string());
        let section = render_section(&model);
        // The system prompt is the first non-ephemeral System message — the
        // same definition `set_system_prompt` uses. Ephemeral systems are
        // per-turn hints and are never rewritten; a request with no system
        // prompt at all (a stripped resume) gets a fresh one at the front.
        let target = ctx.messages.iter().position(|m| {
            matches!(
                m,
                ChatMessage::System {
                    ephemeral: false,
                    ..
                }
            )
        });
        match target {
            Some(idx) => {
                if let ChatMessage::System { content, .. } = &mut ctx.messages[idx] {
                    *content = refresh_stamp_section(content, &section);
                }
            }
            None => ctx.messages.insert(0, ChatMessage::system(section)),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{ModelConfig, TierConfig};
    use phi_agent::SessionId;
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

    fn pre_llm_ctx(messages: Vec<ChatMessage>) -> PreLlmCtx {
        PreLlmCtx {
            session_id: SessionId::new(1),
            messages,
            tools: Vec::new(),
            emit_fn: None,
            turn_count: 1,
            max_turns: 8,
        }
    }

    /// protects: the stamp replaces itself in place — a swap names the new
    /// model, the old name is gone, and content outside the region is
    /// byte-preserved; the steady state is a fixed point (provider-side
    /// caching). fails_when: the region accumulates copies, a swap leaves a
    /// stale name, or neighbor sections move. no_claim: the copy beyond
    /// naming the model.
    #[test]
    fn the_stamp_replaces_itself_in_place_and_keeps_the_rest() {
        let base = "You are phimint.\n\n## Active Skills\n\n- one\n";
        let once = refresh_stamp_section(base, &render_section("alpha-model"));
        assert!(once.starts_with(base), "the base is preserved: {once}");
        assert!(once.contains("`alpha-model`"), "{once}");

        let twice = refresh_stamp_section(&once, &render_section("beta-model"));
        assert!(twice.starts_with(base), "{twice}");
        assert!(twice.contains("`beta-model`"), "{twice}");
        assert!(!twice.contains("alpha-model"), "{twice}");
        assert_eq!(
            twice.matches(STAMP_HEADER).count(),
            1,
            "one region, never two: {twice}"
        );
        assert_eq!(
            refresh_stamp_section(&twice, &render_section("beta-model")),
            twice,
            "the steady state is a fixed point"
        );

        // A section appended after the stamp bounds its region: the swap
        // stays in place and the later section survives byte-for-byte.
        let later = format!("{twice}\n\n## Later\n\nmore\n");
        let swapped = refresh_stamp_section(&later, &render_section("gamma-model"));
        assert!(swapped.contains("`gamma-model`"), "{swapped}");
        assert!(!swapped.contains("beta-model"), "{swapped}");
        assert!(
            swapped.ends_with("\n\n## Later\n\nmore\n"),
            "the later section is untouched: {swapped}"
        );
    }

    /// protects: every request names the store's live main model, routes
    /// identity questions to the `current_model` tool, and forbids echoing
    /// names from earlier in the conversation; a swap is named by the very
    /// next request, and the other messages are not rewritten. fails_when:
    /// the stamp goes stale after a swap or lands anywhere but the system
    /// message. no_claim: the copy beyond carrying the name and the routing
    /// instruction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_request_names_the_live_model() {
        let store = Arc::new(Mutex::new(store("alpha-model")));
        let stamp = ModelIdentityStamp::new(Arc::clone(&store));
        let mut ctx = pre_llm_ctx(vec![
            ChatMessage::system("You are phimint."),
            ChatMessage::user("what model are you?"),
        ]);

        stamp.on_pre_llm(&mut ctx).await.unwrap();
        let ChatMessage::System { content, .. } = &ctx.messages[0] else {
            panic!("the system message stays first: {:?}", ctx.messages);
        };
        assert!(content.contains("`alpha-model`"), "{content}");
        assert!(content.contains("call `current_model`"), "{content}");
        assert!(
            content.contains("Never repeat a model name from earlier"),
            "{content}"
        );

        store
            .lock()
            .await
            .set_tier_model("main", "beta-model".to_string());
        stamp.on_pre_llm(&mut ctx).await.unwrap();
        let ChatMessage::System { content, .. } = &ctx.messages[0] else {
            panic!("the system message stays first: {:?}", ctx.messages);
        };
        assert!(content.contains("`beta-model`"), "{content}");
        assert!(!content.contains("alpha-model"), "{content}");
        assert!(
            matches!(&ctx.messages[1], ChatMessage::User { content, .. } if content == "what model are you?"),
            "the rest of the request is untouched: {:?}",
            ctx.messages
        );
    }

    /// protects: a request with no system prompt (a stripped resume) gets a
    /// stamped one at the front, and an ephemeral per-turn hint is never
    /// mistaken for the system prompt. fails_when: the stamp lands on the
    /// hint or a prompt-less request ships without it. no_claim: how a
    /// session loses its system message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_promptless_request_gets_a_stamped_system_message() {
        let stamp = ModelIdentityStamp::new(Arc::new(Mutex::new(store("alpha-model"))));

        let mut ctx = pre_llm_ctx(vec![ChatMessage::user("hi")]);
        stamp.on_pre_llm(&mut ctx).await.unwrap();
        assert!(
            matches!(&ctx.messages[0], ChatMessage::System { content, ephemeral: false }
                if content.contains("`alpha-model`")),
            "a fresh stamped system message leads: {:?}",
            ctx.messages
        );

        let mut ctx = pre_llm_ctx(vec![
            ChatMessage::system_ephemeral("turn hint"),
            ChatMessage::user("hi"),
        ]);
        stamp.on_pre_llm(&mut ctx).await.unwrap();
        assert!(
            matches!(&ctx.messages[1], ChatMessage::System { content, ephemeral: true }
                if content == "turn hint"),
            "the hint is never rewritten: {:?}",
            ctx.messages
        );
        assert!(
            matches!(&ctx.messages[0], ChatMessage::System { content, ephemeral: false }
                if content.contains("`alpha-model`")),
            "the stamp gets its own system message: {:?}",
            ctx.messages
        );
    }

    /// protects: the stamp names the model the engine is serving, not the
    /// tier-table selection — a staged (deferred) swap moves the table first
    /// while the old provider still serves, and the stamp must not claim
    /// otherwise. fails_when: the stamped region names the tier selection
    /// while the serving resolver answers a different name. no_claim: when
    /// the engine lands the staged swap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_stamp_names_the_serving_model_not_the_tier_selection() {
        let store = Arc::new(Mutex::new(store("selected-model")));
        let stamp = ModelIdentityStamp::new(Arc::clone(&store));
        store
            .lock()
            .await
            .bind_serving_name(Arc::new(|| Some("serving-model".to_string())));

        let mut ctx = pre_llm_ctx(vec![ChatMessage::user("hi")]);
        stamp.on_pre_llm(&mut ctx).await.unwrap();
        let ChatMessage::System { content, .. } = &ctx.messages[0] else {
            panic!("the system message stays first: {:?}", ctx.messages);
        };
        assert!(content.contains("`serving-model`"), "{content}");
        assert!(!content.contains("selected-model"), "{content}");
    }
}
