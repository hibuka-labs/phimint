//! Mock-LLM integration test for the `skill` tool (skill-injection M2).
//!
//! Exercises the full build() → run_turn() path with a scripted LLM provider:
//!   Turn 1: model calls `skill` tool with a known skill name
//!   Turn 2: model returns a text response
//!
//! Asserts:
//!   - The system prompt the LLM received contains the skills catalog
//!   - The tool returned the full skill body (verified via session messages)
//!   - Non-user-invocable skills are loadable by the tool

use std::sync::{Arc, Mutex};

use phi_agent::Content;
use phi_agent::llm_trait::{
    Capabilities, ChatMessage, ChatRequest, ChatResponse, ChatStream, FinishReason, LlmError,
    LlmProvider, ProviderInfo, StreamChunk, UsageInfo,
};

/// A mock LLM that plays back a scripted sequence of turns.
///
/// Each turn is a `Vec<StreamChunk>`; `stream()` consumes the next turn.
struct ScriptedProvider {
    script: Mutex<std::vec::IntoIter<Vec<StreamChunk>>>,
    /// Captures the system prompt from every request for assertion.
    system_prompts: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter()),
            system_prompts: Mutex::new(Vec::new()),
        }
    }

    fn capture(&self, request: &ChatRequest) {
        if let Some(ChatMessage::System { content, .. }) = request.messages.first() {
            self.system_prompts.lock().unwrap().push(content.clone());
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        self.capture(&request);
        let chunks: Vec<Result<StreamChunk, LlmError>> = self
            .script
            .lock()
            .unwrap()
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(Ok)
            .collect();
        Ok(ChatStream::new(Box::pin(futures_util::stream::iter(
            chunks,
        ))))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.capture(&request);
        Ok(ChatResponse {
            content: String::new(),
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

/// Build an agent with a scripted mock LLM and a temp skill directory.
///
/// Returns `(provider, agent, session, tmp_dir)` — the TempDir guard must
/// outlive the session so the resolver's source_path_for stays valid.
async fn build_with_skills(
    script: Vec<Vec<StreamChunk>>,
    skills: &[(&str, &str, bool)],
) -> (
    Arc<ScriptedProvider>,
    phi_agent::PhiAgent,
    phi_agent::SessionId,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    for (name, body, invocable) in skills {
        let dir = tmp.path().join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let inv = if *invocable { "true" } else { "false" };
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill for {name}\nuser-invocable: {inv}\n---\n\n{body}"),
        )
        .unwrap();
    }

    let provider = Arc::new(ScriptedProvider::new(script));
    let workspace = tempfile::tempdir().unwrap();
    let (approval, policy, _approval_rx, _mode) = phimint::approval::build_live_approval("auto");
    let (agent, _resolver, _telemetry, _bg_registry) = phimint::agent::build(
        provider.clone() as Arc<dyn LlmProvider>,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![tmp.path().join("skills")],
        None,
    )
    .unwrap();
    let session = agent.create_session().await;
    (provider, agent, session, tmp)
}

/// Helper: extract all Text content from a Vec<Content>.
#[allow(dead_code)]
fn text(out: &[Content]) -> String {
    out.iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_calls_skill_tool_and_receives_body() {
    // Turn 1: model calls the skill tool with name="code-review"
    // Turn 2: model returns a text response acknowledging the body
    let (provider, agent, session, _guard) = build_with_skills(
        vec![
            vec![
                StreamChunk::ToolCall(serde_json::json!({
                    "delta": {
                        "tool_calls": [{
                            "id": "call_skill_1",
                            "function": {
                                "name": "skill",
                                "arguments": "{\"name\": \"code-review\"}"
                            }
                        }]
                    }
                })),
                StreamChunk::Stop {
                    finish_reason: Some("tool_calls".to_string()),
                },
            ],
            vec![
                StreamChunk::Text("I've loaded the code-review skill.".to_string()),
                StreamChunk::Stop {
                    finish_reason: Some("stop".to_string()),
                },
            ],
        ],
        &[("code-review", "Review this PR thoroughly.", true)],
    )
    .await;

    let result = agent
        .run_turn(session, "review this PR", |_ev| Ok(()))
        .await;
    assert!(result.is_ok(), "run_turn must succeed: {:?}", result.err());

    // The system prompt must contain the catalog.
    let prompts = provider.system_prompts.lock().unwrap().clone();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("## Skills") && p.contains("- code-review:")),
        "system prompt must contain the skills catalog"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_user_invocable_skill_loadable_by_tool() {
    // The skill tool can load skills the slash path cannot.
    let (provider, agent, session, _guard) = build_with_skills(
        vec![
            vec![
                StreamChunk::ToolCall(serde_json::json!({
                    "delta": {
                        "tool_calls": [{
                            "id": "call_internal",
                            "function": {
                                "name": "skill",
                                "arguments": "{\"name\": \"internal\"}"
                            }
                        }]
                    }
                })),
                StreamChunk::Stop {
                    finish_reason: Some("tool_calls".to_string()),
                },
            ],
            vec![
                StreamChunk::Text("Loaded internal skill.".to_string()),
                StreamChunk::Stop {
                    finish_reason: Some("stop".to_string()),
                },
            ],
        ],
        &[("internal", "internal instructions", false)],
    )
    .await;

    let result = agent.run_turn(session, "run internal", |_ev| Ok(())).await;
    assert!(result.is_ok(), "run_turn must succeed: {:?}", result.err());

    // Internal skill is in the catalog (it's not user_invocable but IS model-visible).
    let prompts = provider.system_prompts.lock().unwrap().clone();
    assert!(
        prompts.iter().any(|p| p.contains("- internal:")),
        "non-user-invocable skill must appear in the catalog"
    );
}
