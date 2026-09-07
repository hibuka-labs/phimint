//! Per-turn system prompt refresh for skill catalog hot-reload.
//!
//! Replaces the first `ChatMessage::System` in the messages list with a
//! freshly composed prompt (base + catalog) before each LLM call, so
//! edited/added/removed SKILL.md files take effect mid-session without restart.
//!
//! Design: skill-injection M3a. Uses `agent_base::Middleware::on_pre_llm`
//! which runs before every LLM call — zero cross-repo changes needed.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::llm_trait::ChatMessage;
use phi_agent::{
    AgentResult, Middleware, PreLlmCtx,
};

use crate::skills::{SkillResolver, render_catalog};

/// Middleware that refreshes the system prompt (base + skills catalog) before
/// each LLM call. This lets the model see skill catalog changes (additions,
/// removals, description edits) without restarting the session.
///
/// The resolver is `Arc`-shared with the TUI's `/` slash path and the
/// `SkillTool` — all three read the same immutable skill list. To pick up
/// filesystem changes, the resolver would need to be rebuilt (future work);
/// this middleware handles the *wiring* half: ensuring the prompt the model
/// sees always reflects the resolver's current state.
pub(crate) struct SkillCatalogRefreshMiddleware {
    resolver: Arc<SkillResolver>,
}

impl SkillCatalogRefreshMiddleware {
    pub fn new(resolver: Arc<SkillResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl Middleware for SkillCatalogRefreshMiddleware {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        // Only touch the system message when there are skills to inject.
        // Without skills, the builder's original prompt is left byte-identical.
        if let Some(catalog) = render_catalog(&self.resolver) {
            if let Some(first) = ctx.messages.first_mut() {
                if let ChatMessage::System { content, .. } = first {
                    *content = format!("{content}\n\n{catalog}");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillResolver;
    use phi_agent::llm_trait::ChatMessage;
    use phi_agent::SessionId;
    use std::path::Path;

    fn make_skill_dir(tmp: &Path, name: &str, body: &str) {
        let dir = tmp.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\nuser-invocable: true\n---\n\n{body}"),
        )
        .unwrap();
    }

    fn make_pre_llm_ctx(system_content: &str) -> PreLlmCtx {
        PreLlmCtx {
            session_id: SessionId {
                id: 1,
                external_id: None,
            },
            messages: vec![ChatMessage::System {
                content: system_content.to_string(),
                ephemeral: false,
            }],
            tools: vec![],
            emit_fn: None,
            turn_count: 1,
            max_turns: 10,
        }
    }

    #[tokio::test]
    async fn refresh_replaces_system_message_with_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        make_skill_dir(tmp.path(), "alpha", "alpha body");
        let resolver = Arc::new(SkillResolver::from_dirs(&[tmp.path().join("skills")]));

        let mw = SkillCatalogRefreshMiddleware::new(resolver);
        let mut ctx = make_pre_llm_ctx("original prompt");

        mw.on_pre_llm(&mut ctx).await.unwrap();

        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert!(content.starts_with("original prompt"), "base prompt must be preserved");
                assert!(content.contains("## Skills"), "catalog must be appended");
                assert!(content.contains("- alpha: test"), "skill must appear");
            }
            _ => panic!("expected System message"),
        }
    }

    #[tokio::test]
    async fn refresh_without_skills_keeps_original_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = Arc::new(SkillResolver::from_dirs(&[tmp.path().join("nonexistent")]));

        let mw = SkillCatalogRefreshMiddleware::new(resolver);
        let original = "base prompt only";
        let mut ctx = make_pre_llm_ctx(original);

        mw.on_pre_llm(&mut ctx).await.unwrap();

        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert_eq!(content, original, "must be byte-identical without skills");
            }
            _ => panic!("expected System message"),
        }
    }

    #[tokio::test]
    async fn refresh_reflects_newly_added_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = Arc::new(SkillResolver::from_dirs(&[tmp.path().join("skills")]));
        let mw = SkillCatalogRefreshMiddleware::new(resolver.clone());

        // First turn: no skills
        let mut ctx = make_pre_llm_ctx("prompt");
        mw.on_pre_llm(&mut ctx).await.unwrap();
        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert!(!content.contains("## Skills"));
            }
            _ => panic!(),
        }

        // Add a skill and rebuild resolver
        make_skill_dir(tmp.path(), "new-skill", "new body");
        let resolver2 = Arc::new(SkillResolver::from_dirs(&[tmp.path().join("skills")]));
        let mw2 = SkillCatalogRefreshMiddleware::new(resolver2);

        // Second turn: skill appears
        let mut ctx = make_pre_llm_ctx("prompt");
        mw2.on_pre_llm(&mut ctx).await.unwrap();
        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("- new-skill: test"), "new skill must appear");
            }
            _ => panic!(),
        }
    }
}
