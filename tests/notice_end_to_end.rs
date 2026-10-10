//! End-to-end notify chain (Batch E) — the automated substitute for the
//! manual TUI test.
//!
//! One test drives the real path, seam-free:
//!   DefaultGuard judge fail-open (mock LLM returns unparseable content)
//!   → `NoticeHandle` (injected by `RuntimeCore::new` via `set_notice`)
//!   → engine pump stamps `session_id` → `RuntimeEvent::UserEvent::Notice`
//!   → phimint `App` renders the persistent red line without settling the turn.
//!
//! The judge shares the scripted mock LLM with the agent: `stream()` plays
//! back the agent's turns, `chat()` (judge-only) returns empty content —
//! unparseable JSON → one retry → fail-open → Warning notice.

use std::sync::{Arc, Mutex};

use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderInfo, StreamChunk,
    UsageInfo,
};
use phi_agent::{RuntimeEvent, SessionId, UserEvent};
use phi_tui::lines::LineKind;
use phimint::ui::app::{AgentStatus, App, TuiEvent};

/// Mock LLM: `stream()` plays back the script; `chat()` returns empty content
/// so the completion judge always hits the unparseable → fail-open path.
struct ScriptedProvider {
    script: Mutex<std::vec::IntoIter<Vec<StreamChunk>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter()),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream(
        &self,
        _request: ChatRequest,
    ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
        let chunks: Vec<Result<StreamChunk, phi_agent::llm_trait::LlmError>> = self
            .script
            .lock()
            .unwrap()
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(Ok)
            .collect();
        Ok(phi_agent::llm_trait::ChatStream::new(Box::pin(
            futures_util::stream::iter(chunks),
        )))
    }

    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, phi_agent::llm_trait::LlmError> {
        // Judge-only path: empty content → parse_judge_response fails.
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

/// Build the real phimint agent stack around the mock LLM plus one temp skill
/// (so turn 1 has a real tool to call and `run_has_tool_calls` becomes true).
async fn build_agent(
    script: Vec<Vec<StreamChunk>>,
) -> (phi_agent::PhiAgent, SessionId, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills").join("noop-skill");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: noop-skill\ndescription: test skill\nuser-invocable: true\n---\n\nbody\n",
    )
    .unwrap();

    let provider = Arc::new(ScriptedProvider::new(script));
    let workspace = tempfile::tempdir().unwrap();
    let (approval, policy) = phimint::approval::build_approval("auto");
    let (agent, _resolver, _telemetry, _bg) = phimint::agent::build(
        provider as Arc<dyn LlmProvider>,
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
    (agent, session, tmp)
}

/// Judge degradation → pump-stamped notice → TUI red line, end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn judge_degradation_renders_red_line_end_to_end() {
    // Turn 1: real tool call (so the run counts as tool-using).
    // Turn 2: 5-char text reply — under judge_skip_threshold (256) — so the
    //   completion judge runs, sees the mock's empty content, fails to parse
    //   (with one retry) and fails OPEN, emitting the Warning notice.
    let (agent, session, _skills) = build_agent(vec![
        vec![
            StreamChunk::ToolCall(serde_json::json!({
                "delta": {
                    "tool_calls": [{
                        "id": "call_skill_1",
                        "function": {
                            "name": "skill",
                            "arguments": "{\"name\": \"noop-skill\"}"
                        }
                    }]
                }
            })),
            StreamChunk::Stop {
                finish_reason: Some("tool_calls".to_string()),
            },
        ],
        vec![
            StreamChunk::Text("done.".to_string()),
            StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            },
        ],
    ])
    .await;

    let events: Arc<Mutex<Vec<RuntimeEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    let result = agent
        .run_turn(
            session.clone(),
            "use the skill tool, then reply done",
            move |ev| {
                events_clone.lock().unwrap().push(ev);
                Ok(())
            },
        )
        .await;
    assert!(result.is_ok(), "run must succeed: {:?}", result.err());

    let delivered = events.lock().unwrap().clone();

    // ── Engine side: the pump delivered a stamped Notice before the terminal ──
    let notice_pos = delivered
        .iter()
        .position(|e| {
            matches!(
                e,
                RuntimeEvent::UserEvent {
                    event: UserEvent::Notice { .. },
                    ..
                }
            )
        })
        .expect("judge fail-open must deliver a notice through the pump");
    let finished_pos = delivered
        .iter()
        .position(|e| matches!(e, RuntimeEvent::RunFinished { .. }))
        .expect("RunFinished must be emitted");
    assert!(
        notice_pos < finished_pos,
        "notice ({notice_pos}) must arrive before RunFinished ({finished_pos})"
    );

    let notice_event = delivered[notice_pos].clone();
    match &notice_event {
        RuntimeEvent::UserEvent {
            session_id,
            event: UserEvent::Notice { kind, source, text },
            ..
        } => {
            assert_eq!(*session_id, session, "pump must stamp the run's session id");
            assert_eq!(*kind, phi_agent::NoticeKind::Warning);
            assert_eq!(source, "guard");
            assert_eq!(text, "guard judge unparsed — treating as complete");
        }
        other => panic!("expected UserEvent::Notice, got: {other:?}"),
    }

    // ── UI side: the exact event the engine produced renders as the red line ──
    let mut app = App::new();
    // Mid-turn precondition — only then is "does not settle the turn" visible.
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "skill".to_string(),
        args_json: "{}".to_string(),
        agent_id: None,
        trace_id: None,
    }));
    assert!(
        matches!(app.status, AgentStatus::Running { .. }),
        "precondition: mid-turn"
    );
    let lines_before = app.transcript.output.len();

    app.handle_event(TuiEvent::Runtime(notice_event));

    assert_eq!(
        app.transcript.output.len(),
        lines_before + 1,
        "exactly one red line emitted"
    );
    let line = app.transcript.output.last().expect("line appended");
    assert_eq!(line.kind, LineKind::Error, "warning renders as red line");
    // The event carries the cause-specific English fact (asserted above); the
    // TUI shows the product-layer user copy — no "judge" jargon on screen.
    assert_eq!(
        line.text, "❌ Answer accepted without verification.",
        "red line = error glyph + user-facing copy"
    );
    assert!(
        matches!(app.status, AgentStatus::Running { .. }),
        "the notice must not settle the turn, got: {:?}",
        app.status
    );
}
