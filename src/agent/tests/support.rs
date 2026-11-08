use super::super::prompt::*;
use super::super::*;
use crate::model::config::CodeExecMode;
use phi_agent::llm_trait::{
    Capabilities, ChatMessage, ChatRequest, ChatResponse, ChatStream, FinishReason, LlmError,
    LlmProvider, ProviderInfo, StreamChunk, UsageInfo,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// The base prompt this build teaches (layer ②): the tool-road prompt
/// with the engine, the same prompt minus the `code_cell` line without it
/// — an engine-less binary must not teach a tool it cannot run. The
/// compose and wiring tests assert against this so "the catalog is the
/// only addition" holds in both builds. (The code-exec base is the tool
/// road under its child-kit rewrite — `tool_road_prompt_…` pins that
/// rewrite to literals.)
#[cfg(feature = "code-exec")]
pub(super) fn base_prompt() -> String {
    code_road_prompt(CodeExecMode::Tool, "")
}

#[cfg(not(feature = "code-exec"))]
pub(super) fn base_prompt() -> String {
    SYSTEM_PROMPT.replace(CODE_CELL_BULLET, "")
}

/// LLM stub that records the system prompt and tool list of every request
/// it receives and answers with a tool-free "done" (the turn ends
/// immediately).
pub(super) struct CapturingMockProvider {
    pub(super) system_prompts: Mutex<Vec<String>>,
    pub(super) tools: Mutex<Vec<Value>>,
}

impl CapturingMockProvider {
    fn record(&self, request: &ChatRequest) {
        if let Some(ChatMessage::System { content, .. }) = request.messages.first() {
            self.system_prompts.lock().unwrap().push(content.clone());
        }
        self.tools
            .lock()
            .unwrap()
            .extend(request.tools.iter().cloned());
    }
}

#[async_trait::async_trait]
impl LlmProvider for CapturingMockProvider {
    async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        self.record(&request);
        let chunks: Vec<Result<StreamChunk, LlmError>> = vec![
            Ok(StreamChunk::Text("done".to_string())),
            Ok(StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            }),
        ];
        Ok(ChatStream::new(Box::pin(futures_util::stream::iter(
            chunks,
        ))))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.record(&request);
        Ok(ChatResponse {
            content: "done".to_string(),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: vec![],
            usage: UsageInfo::default(),
            finish_reason: FinishReason::Stop,
            raw: None,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_streaming: true,
            supports_tools: true,
            ..Default::default()
        }
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock".to_string(),
            model: "mock-model".to_string(),
            version: None,
        }
    }
}

/// Fixture: one skill in `<tmp>/skills/catalog-skill/SKILL.md`.
pub(super) fn fixture_skill_dir(tmp: &std::path::Path) -> std::path::PathBuf {
    let dir = tmp.join("skills").join("catalog-skill");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: catalog-skill\ndescription: guard fixture skill\n\
         user-invocable: true\n---\n\nfixture body",
    )
    .unwrap();
    tmp.join("skills")
}

/// Build a real agent via `build()` against a self-managed workspace and
/// run one turn against the capturing mock, returning every system prompt
/// the LLM received.
pub(super) async fn system_prompts_received(skill_dirs: Vec<std::path::PathBuf>) -> Vec<String> {
    let workspace = tempfile::tempdir().unwrap();
    system_prompts_received_in(workspace.path(), skill_dirs).await
}

/// Same as [`system_prompts_received`] but with a caller-owned workspace —
/// needed when an assertion must reference the exact workspace the agent
/// was built against (e.g. the memory root slug, Phase 9c).
pub(super) async fn system_prompts_received_in(
    workspace: &std::path::Path,
    skill_dirs: Vec<std::path::PathBuf>,
) -> Vec<String> {
    let provider = Arc::new(CapturingMockProvider {
        system_prompts: Mutex::new(Vec::new()),
        tools: Mutex::new(Vec::new()),
    });
    let (approval, policy, _approval_rx, _mode) =
        crate::approval::build_live_approval("auto", None);
    let (agent, _resolver, _telemetry, _bg_registry, _canvas_activity) = build(
        provider.clone() as Arc<dyn LlmProvider>,
        approval,
        policy,
        1_000,
        workspace.to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        skill_dirs,
        None,
        None,
    )
    .await
    .unwrap();
    let session = agent.create_session().await;
    agent
        .run_turn(session, "hello", |_ev| Ok(()))
        .await
        .unwrap();

    let prompts = provider.system_prompts.lock().unwrap().clone();
    assert!(
        !prompts.is_empty(),
        "mock provider must receive at least one request"
    );
    prompts
}

/// The browser-tool names, taken from phi-tools' registry rather than
/// listed here: a tool added over there must show up in the exclusion
/// assertions without anyone remembering to extend a list. The registry
/// (and its grouping) is the suite — `browser_tools` constructs exactly
/// these, and phi-tools' own tests pin the two together.
pub(super) fn browser_tool_names() -> Vec<String> {
    phi_tools::browser_tool_names(&[phi_tools::BrowserToolGroup::All])
        .into_iter()
        .map(str::to_string)
        .collect()
}
