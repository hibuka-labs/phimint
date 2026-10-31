//! M2 dogfood, scripted (test-before-manual): the model drives `code_cell`
//! through the real phimint loop — two steps, one resident session. The
//! second step reads a variable the first one bound, so a session-reset
//! regression between calls shows up here and not first in a live run. The
//! audit row must land with the session's own artifacts.
#![cfg(feature = "code-exec")]

use std::sync::{Arc, Mutex};

use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderInfo, StreamChunk,
    UsageInfo,
};
use phimint::agent::TokenBudgetOptions;

/// Mock LLM: `stream()` plays back the script and records what each request
/// carried to the model; `chat()` is the judge path and never runs here.
struct ScriptedProvider {
    script: Mutex<std::vec::IntoIter<Vec<StreamChunk>>>,
    requests: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_text(&self) -> String {
        self.requests.lock().unwrap().join("\n")
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream(
        &self,
        request: ChatRequest,
    ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
        // Serialize the message list — the tool-result text the model will
        // see is in here, and it is what the state-survival asserts read.
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.messages).unwrap_or_default());
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

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Vec<StreamChunk> {
    vec![
        StreamChunk::ToolCall(serde_json::json!({
            "delta": {
                "tool_calls": [{
                    "id": id,
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string()
                    }
                }]
            }
        })),
        StreamChunk::Stop {
            finish_reason: Some("tool_calls".to_string()),
        },
    ]
}

fn cells(items: &[(&str, &str)]) -> serde_json::Value {
    let cells: Vec<serde_json::Value> = items
        .iter()
        .map(|(intent, code)| serde_json::json!({"intent": intent, "code": code}))
        .collect();
    serde_json::json!({ "cells": cells })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_steps_code_cells_and_the_session_survives_between_them() {
    // Step 1 binds `x`, touches the shell and writes a file purely in Python;
    // step 2 multiplies `x` — it only prints 82 if the resident session
    // survived the tool-call boundary. The pure write must cross the worker
    // audit hook and leave an `open` call row (layer 2 mediation).
    let base = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let note = workspace.path().join("note.txt");
    let write_code = format!(
        "from pathlib import Path; Path({:?}).write_text(\"tidings\")",
        note.display().to_string()
    );
    let script = vec![
        tool_call(
            "call_cells_1",
            "code_cell",
            cells(&[
                ("bind", "x = 41"),
                ("print", "print(x + 1)"),
                ("write", write_code.as_str()),
                ("shell", "sh.run(\"echo dogfood\")"),
            ]),
        ),
        tool_call(
            "call_cells_2",
            "code_cell",
            cells(&[("reuse", "print(x * 2)")]),
        ),
        vec![
            StreamChunk::Text("42 and 82 confirmed.".to_string()),
            StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            },
        ],
    ];
    let provider = Arc::new(ScriptedProvider::new(script));

    let (approval, policy, _approval_rx, _mode) =
        phimint::approval::build_live_approval("auto", None);
    let session_id = "code-cell-e2e".to_string();
    let (agent, _resolver, _telemetry, _bg, _canvas) = phimint::agent::build(
        provider.clone() as Arc<dyn LlmProvider>,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![],
        Some(TokenBudgetOptions {
            budget: 210_000,
            retention_days: 30,
            base_dir: base.path().to_path_buf(),
            session_id: session_id.clone(),
        }),
    )
    .await
    .unwrap();
    let session = agent.create_session().await;

    let result = agent
        .run_turn(
            session,
            "compute 41+1 and 41*2 via code cells",
            |_ev| Ok(()),
        )
        .await;
    assert!(result.is_ok(), "run_turn must succeed: {:?}", result.err());

    // The observation stream reached the model: step 1 printed 42, step 2
    // printed 82 from the variable step 1 bound.
    let seen = provider.request_text();
    assert!(
        seen.contains("42"),
        "step 1's output must reach the model:\n{seen}"
    );
    assert!(
        seen.contains("82"),
        "step 2 must see the state step 1 left behind:\n{seen}"
    );
    assert!(
        seen.contains("x(int, 41)"),
        "the namespace snapshot must reach the model:\n{seen}"
    );

    // 能记: every submitted cell is on the record — even the pure-Python ones
    // that never cross the host-call seam — and every side effect journals its
    // own call row, next to the session's artifacts. The pure write is
    // mediated (layer 2): its `open` sits on the ledger like `sh.run` does.
    let journal = base
        .path()
        .join("sessions")
        .join(&session_id)
        .join("code_journal.jsonl");
    let rows = std::fs::read_to_string(&journal).unwrap_or_default();
    assert_eq!(
        rows.matches("\"kind\":\"cell\"").count(),
        5,
        "4 + 1 submitted cells, 5 cell rows at {journal:?}:\n{rows}"
    );
    assert_eq!(
        rows.matches("\"kind\":\"call\"").count(),
        2,
        "two side effects, two call rows:\n{rows}"
    );
    assert!(
        rows.contains("\"fn_name\":\"sh.run\""),
        "the side effect must be journaled at {journal:?}:\n{rows}"
    );
    assert!(
        rows.contains("\"fn_name\":\"open\""),
        "the pure-Python write must be mediated at {journal:?}:\n{rows}"
    );
    assert_eq!(
        std::fs::read_to_string(&note).unwrap_or_default(),
        "tidings",
        "the artifact the mediated open produced"
    );
}
