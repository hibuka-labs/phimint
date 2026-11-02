//! Mock-LLM integration test for the browser tool-face loading (design note
//! "canvas / CDP 工具面优化").
//!
//! Drives the real `build()` → `run_turn()` request path with a scripted
//! provider and asserts on what the model actually receives in `ChatRequest`:
//!
//!   Turn 1 (canvas closed, no browser work yet): the browser starter set +
//!   the idle `## Browser Tools` note
//!   → the canvas opens mid-session (a live `phi_canvas` server)
//!   Next LLM call: exactly the 5 canvas tools + the canvas `## Browser Tools` note
//!   Turn 2 (canvas still live): the constrained face is still what is sent
//!   → the canvas closes
//!   Next LLM call: the session's steady face is back (idle here — this
//!   scripted session never called a `browser_*` tool; once one appears the
//!   steady face is the full suite, pinned in `browser_face`'s middleware
//!   tests) and the canvas note is gone
//!
//! This is the request-path proof behind the design note's §7.3 / §9.1 open
//! question: the tool face can change mid-session, and the model is told.

use std::sync::{Arc, Mutex};

use phi_agent::llm_trait::{
    Capabilities, ChatMessage, ChatRequest, ChatResponse, ChatStream, FinishReason, LlmError,
    LlmProvider, ProviderInfo, StreamChunk, UsageInfo,
};

/// One captured LLM request: the tools block and system message as sent.
#[derive(Clone, Debug)]
struct CapturedRequest {
    tool_names: Vec<String>,
    system: String,
}

/// Side-channel LLM calls the harness must answer without eating the agent
/// script (the same classification the PTY mock applies): title generation and
/// the guard's completion judge.
const TITLE_MARK: &str = "You are a title generator";
const JUDGE_MARK: &str = "You are a task completion judge";

/// Mock provider that records every request and plays back scripted turns.
struct ScriptedProvider {
    script: Mutex<std::vec::IntoIter<Vec<StreamChunk>>>,
    captured: Mutex<Vec<CapturedRequest>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter()),
            captured: Mutex::new(Vec::new()),
        }
    }

    /// Side calls get a canned answer and never reach the capture list or the
    /// script — they are not part of the tool-face story.
    fn side_call_reply(request: &ChatRequest) -> Option<Vec<StreamChunk>> {
        let blob = serde_json::to_string(&request.messages).unwrap_or_default();
        if blob.contains(TITLE_MARK) {
            Some(vec![
                StreamChunk::Text("tool face session".to_string()),
                StreamChunk::Stop {
                    finish_reason: Some("stop".to_string()),
                },
            ])
        } else if blob.contains(JUDGE_MARK) {
            Some(vec![
                StreamChunk::Text(r#"{"done": true, "reason": "scripted"}"#.to_string()),
                StreamChunk::Stop {
                    finish_reason: Some("stop".to_string()),
                },
            ])
        } else {
            None
        }
    }

    fn capture(&self, request: &ChatRequest) {
        let tool_names = request
            .tools
            .iter()
            .map(|t| {
                t["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let system = request
            .messages
            .iter()
            .find_map(|m| match m {
                ChatMessage::System { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.captured
            .lock()
            .unwrap()
            .push(CapturedRequest { tool_names, system });
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let chunks: Vec<Result<StreamChunk, LlmError>> = match Self::side_call_reply(&request) {
            Some(reply) => reply.into_iter().map(Ok).collect(),
            None => {
                self.capture(&request);
                self.script
                    .lock()
                    .unwrap()
                    .next()
                    .unwrap_or_default()
                    .into_iter()
                    .map(Ok)
                    .collect()
            }
        };
        Ok(ChatStream::new(Box::pin(futures_util::stream::iter(
            chunks,
        ))))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if Self::side_call_reply(&request).is_none() {
            self.capture(&request);
        }
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

/// Browser stub: the canvas URL is "delivered" without spawning anything.
struct AlwaysLaunch;

impl phi_canvas::BrowserLauncher for AlwaysLaunch {
    fn launch(&self, _url: &str) -> phi_canvas::LaunchDecision {
        phi_canvas::LaunchDecision::Launched
    }
}

fn browser_names(tools: &[String]) -> Vec<&str> {
    tools
        .iter()
        .map(String::as_str)
        .filter(|n| n.starts_with("browser_"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_open_narrows_the_face_mid_session_and_close_restores_it() {
    let store = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    // One tool call per canvas verb, one text to end each turn.
    let script = vec![
        // Turn 1: open the canvas (tool) → then answer.
        vec![
            StreamChunk::ToolCall(serde_json::json!({
                "delta": {
                    "tool_calls": [{
                        "id": "call_open",
                        "function": {"name": "canvas_open", "arguments": "{}"}
                    }]
                }
            })),
            StreamChunk::Stop {
                finish_reason: Some("tool_calls".to_string()),
            },
        ],
        vec![
            StreamChunk::Text("canvas is open".to_string()),
            StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            },
        ],
        // Turn 2: a plain answer while the canvas is live.
        vec![
            StreamChunk::Text("still on the canvas".to_string()),
            StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            },
        ],
        // Turn 3: close the canvas (tool) → then answer.
        vec![
            StreamChunk::ToolCall(serde_json::json!({
                "delta": {
                    "tool_calls": [{
                        "id": "call_close",
                        "function": {"name": "canvas_close", "arguments": "{}"}
                    }]
                }
            })),
            StreamChunk::Stop {
                finish_reason: Some("tool_calls".to_string()),
            },
        ],
        vec![
            StreamChunk::Text("canvas closed".to_string()),
            StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            },
        ],
    ];

    let provider = Arc::new(ScriptedProvider::new(script));
    let (approval, policy, _approval_rx, _mode) =
        phimint::approval::build_live_approval("auto", None);
    let (agent, _resolver, _telemetry, _bg_registry, canvas_activity) = phimint::agent::build(
        provider.clone() as Arc<dyn LlmProvider>,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![],
        None,
    )
    .await
    .unwrap();

    // The canvas side of a session, wired exactly like `run()` does: the
    // three canvas tools on the root registry, the session's CanvasTools
    // bound to the tool-face probe.
    let canvas_tools = phimint::tools::canvas::new_canvas_tools(
        store.path(),
        "sess-face-1",
        Arc::new(AlwaysLaunch),
    );
    canvas_activity.bind(canvas_tools.clone()).await;
    {
        let registry = agent.runtime().tools_mut();
        for tool in phimint::tools::canvas::canvas_tool_adapters(canvas_tools.clone()) {
            registry.write().await.register_arc(Arc::new(tool));
        }
    }

    let session = agent.create_session().await;

    // Turn 1 — the model opens the canvas.
    agent
        .run_turn(session.clone(), "open the canvas", |_ev| Ok(()))
        .await
        .expect("turn 1 must succeed");
    assert!(canvas_activity.is_canvas_live().await, "canvas is live");

    // Turn 2 — still on the canvas.
    agent
        .run_turn(session.clone(), "and now?", |_ev| Ok(()))
        .await
        .expect("turn 2 must succeed");

    // Turn 3 — the model closes the canvas.
    agent
        .run_turn(session.clone(), "close it", |_ev| Ok(()))
        .await
        .expect("turn 3 must succeed");
    assert!(!canvas_activity.is_canvas_live().await, "canvas is closed");

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 5, "one LLM call per script entry: {reqs:?}");

    // Call 1 (turn 1): the idle starter face — this session has not touched
    // the browser, so the starter five ride instead of the 22-tool suite
    // (T12 menu slimming) and the narrowing is announced.
    let before = &reqs[0];
    assert_eq!(
        browser_names(&before.tool_names).len(),
        5,
        "starter five before any browser work: {:?}",
        before.tool_names
    );
    assert!(
        before.system.contains("## Browser Tools"),
        "the idle narrowing is announced"
    );
    assert!(
        before
            .system
            .contains("the full browser toolset arrives the next turn"),
        "the note names the growth path: {}",
        before.system
    );

    // Call 2 (turn 1, after `canvas_open` ran) and call 3 (turn 2): the
    // constrained face + the announcement — mid-session, same session. The
    // tools array arrives in registry-lookup order, so compare as a set.
    let expected = vec![
        "browser_navigate",
        "browser_click",
        "browser_screenshot",
        "browser_snapshot",
        "browser_close",
    ];
    for (i, label) in [(1usize, "call 2"), (2, "call 3")] {
        let req = &reqs[i];
        let mut names = browser_names(&req.tool_names);
        names.sort_unstable();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort_unstable();
        assert_eq!(
            names, expected_sorted,
            "{label}: canvas minimum is what the model sees, nothing more"
        );
        assert!(
            req.tool_names.iter().any(|n| n == "read_file"),
            "{label}: non-browser tools are untouched"
        );
        assert!(
            req.system.contains("## Browser Tools"),
            "{label}: the change is announced in the system prompt"
        );
        assert!(
            req.system.contains("only the canvas minimum"),
            "{label}: the note names the constraint"
        );
    }

    // Call 4 (turn 3, before `canvas_close` runs): still the constrained face.
    assert_eq!(browser_names(&reqs[3].tool_names).len(), 5);

    // Call 5 (turn 3, after `canvas_close` ran): the session's steady face is
    // back and the canvas note is gone — the face restores itself mid-session
    // too. This scripted session never called a `browser_*` tool, so steady
    // is the idle starter face (with its own note), not the full suite; the
    // full-suite restoration after real browser work is pinned in
    // `browser_face`'s middleware tests.
    let after = &reqs[4];
    assert_eq!(browser_names(&after.tool_names).len(), 5);
    assert!(
        after.system.contains("## Browser Tools"),
        "the idle face carries its own note"
    );
    assert!(
        !after.system.contains("only the canvas minimum"),
        "the canvas note is removed with the constraint"
    );
}
