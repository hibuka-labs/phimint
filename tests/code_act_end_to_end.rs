//! M3 dogfood, scripted (test-before-manual): the model ACTS in code — text
//! turns of `<intent>/<code>` blocks through the real phimint loop with the
//! CodeAct backend mounted (T10d act road). Two steps, one resident session:
//! the second reads a variable the first bound, so a session-reset regression
//! between steps shows up here and not first in a live run. The menu on this
//! road carries no `code_cell` — code is the action itself — and the prompt
//! teaches the code protocol instead of the tool. The engine policy is the
//! human gate here (`sh.run` suspends for consent; auto answers it), and the
//! audit row must land with the session's own artifacts.
#![cfg(feature = "code-exec")]

use std::sync::{Arc, Mutex};

use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderInfo, StreamChunk,
    UsageInfo,
};
use phi_agent_tui::InteractSlot;
use phimint::agent::TokenBudgetOptions;
use phimint::model::config::CodeExecMode;

/// Mock LLM: `stream()` plays back the script and records what each request
/// carried to the model (messages AND the tool menu); `chat()` is the judge
/// path and never runs here.
struct ScriptedProvider {
    script: Mutex<std::vec::IntoIter<Vec<StreamChunk>>>,
    requests: Mutex<Vec<String>>,
    menus: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter()),
            requests: Mutex::new(Vec::new()),
            menus: Mutex::new(Vec::new()),
        }
    }

    fn request_text(&self) -> String {
        self.requests.lock().unwrap().join("\n")
    }

    fn request_at(&self, i: usize) -> String {
        self.requests.lock().unwrap()[i].clone()
    }

    fn menu_text(&self) -> String {
        self.menus.lock().unwrap().join("\n")
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
    async fn stream(
        &self,
        request: ChatRequest,
    ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
        // Serialize the message list — the shape-A pair the model will see is
        // in here, and it is what the state-survival asserts read. The tool
        // menu rides along so the act road's missing `code_cell` is visible.
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.messages).unwrap_or_default());
        self.menus
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.tools).unwrap_or_default());
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

/// One act-road step: the wire format the code protocol teaches — blocks
/// joined by newlines, run as one batch against the resident session.
fn step(blocks: &[(&str, &str)]) -> Vec<StreamChunk> {
    let text: Vec<String> = blocks
        .iter()
        .map(|(intent, code)| format!("<intent>{intent}</intent>\n<code>{code}</code>"))
        .collect();
    vec![
        StreamChunk::Text(text.join("\n")),
        StreamChunk::Stop {
            finish_reason: Some("stop".to_string()),
        },
    ]
}

fn final_text(s: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::Text(s.to_string()),
        StreamChunk::Stop {
            finish_reason: Some("stop".to_string()),
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_acts_in_code_and_the_session_survives_between_steps() {
    // Step 1 binds `x`, prints it, writes a file purely in Python and touches
    // the shell; step 2 multiplies `x` — it only prints 82 if the resident
    // session survived the step boundary. The shell call is the engine
    // policy's Ask (Process) on this road: consent suspends through the
    // interaction slot and auto answers it. The pure write must cross the
    // worker audit hook and leave an `open` call row (layer 2 mediation).
    let base = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let note = workspace.path().join("note.txt");
    let write_code = format!(
        "from pathlib import Path; Path({:?}).write_text(\"tidings\")",
        note.display().to_string()
    );
    let script = vec![
        step(&[
            ("bind", "x = 41"),
            ("print", "print(x + 1)"),
            ("write", write_code.as_str()),
            ("shell", "sh.run(\"echo dogfood\")"),
        ]),
        step(&[("reuse", "print(x * 2)")]),
        final_text("42 and 82 confirmed."),
    ];
    let provider = Arc::new(ScriptedProvider::new(script));

    let (gate, policy, _approval_rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let slot = Arc::new(InteractSlot::new(
        Arc::clone(&gate) as Arc<dyn phi_agent::ApprovalHandler>
    ));
    let session_id = "code-act-e2e".to_string();
    let (agent, _resolver, _telemetry, _bg, _canvas) = phimint::agent::build_with_code_exec(
        CodeExecMode::Act,
        Some(slot),
        provider.clone() as Arc<dyn LlmProvider>,
        gate,
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
        None,
        None,
        None, // writes_possible: no approval switch in tests, so always writable
    )
    .await
    .unwrap();
    let session = agent.create_session().await;

    let result = agent
        .run_turn(session, "compute 41+1 and 41*2 by acting in code", |_ev| {
            Ok(())
        })
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

    // Shape A on the wire: the batch re-renders as its blocks and the
    // observation pane lands as the message that follows — both visible to
    // the next step's request.
    let after_step_1 = provider.request_at(1);
    assert!(
        after_step_1.contains("<intent>bind</intent>"),
        "the blocks must re-inject as history:\n{after_step_1}"
    );
    assert!(
        after_step_1.contains("[observation]"),
        "the observation pane must follow the blocks:\n{after_step_1}"
    );
    assert!(
        provider.request_at(2).contains("82"),
        "the final call must see step 2's output:\n{seen}"
    );

    // Prompt switching on the wire: the act road teaches the code protocol,
    // not the code_cell tool line.
    let first = provider.request_at(0);
    assert!(
        first.contains("## Code protocol (CodeAct)"),
        "the act road's protocol section must be on the wire:\n{first}"
    );
    assert!(
        !first.contains("run code cells in a resident Python session"),
        "the code_cell bullet must be gone on the act road:\n{first}"
    );

    // The menu is the other half of the swap: no `code_cell` tool on this
    // road — code is the action — while both file vehicles ride it
    // (file_face, 2026-11-01: the triad is the single-file path, code steps
    // are the chain path, and the prompt teaches the dispatch rule).
    let menu = provider.menu_text();
    assert!(
        !menu.contains("\"code_cell\""),
        "the act road's menu must not carry code_cell:\n{menu}"
    );
    for name in ["\"read_file\"", "\"list_files\"", "\"write_file\""] {
        assert!(
            menu.contains(name),
            "the file triad must ride the act road's menu ({name}):\n{menu}"
        );
    }
    assert!(
        menu.contains("\"edit_file\""),
        "ordinary tools must stay on the menu:\n{menu}"
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
        "the consented side effect must be journaled at {journal:?}:\n{rows}"
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

/// 判别门 (OV-2, T11): the M2 dogfood file chain — 8 sources → report +
/// totals + renames — lands WHOLE in ONE code step, with no `code_cell`
/// anywhere in the menu. The M2 shape could only do this by registering a
/// `code_cell` tool (schema tax and all); here code is the action itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_chain_lands_whole_in_one_step_with_no_code_cell_in_the_menu() {
    let base = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let ws = workspace.path();
    // The M2 fixture shape: data01..08.txt, 5 integers each (i, i+1, …).
    let mut expected_report = String::new();
    let mut grand = 0i64;
    for i in 1..=8 {
        let nums: Vec<i64> = (0..5).map(|j| (i * 10 + j) as i64).collect();
        grand += nums.iter().sum::<i64>();
        let body: Vec<String> = nums.iter().map(|n| n.to_string()).collect();
        std::fs::write(ws.join(format!("data{i:02}.txt")), body.join("\n")).unwrap();
        expected_report.push_str(&format!(
            "data{i:02}.txt {} {}\n",
            nums.iter().sum::<i64>(),
            nums.iter().max().unwrap()
        ));
    }

    // One cell, the whole chain: read 8, write 2, rename 8. Print the grand
    // total so the observation carries the answer (the observation is the
    // only channel back).
    let chain = format!(
        "from pathlib import Path\n\
         ws = Path({ws:?})\n\
         report = []\n\
         grand = 0\n\
         for i in range(1, 9):\n\
         \x20   p = ws / f\"data{{i:02d}}.txt\"\n\
         \x20   nums = [int(x) for x in p.read_text().split()]\n\
         \x20   grand += sum(nums)\n\
         \x20   report.append(f\"{{p.name}} {{sum(nums)}} {{max(nums)}}\")\n\
         (ws / \"report.txt\").write_text(\"\\n\".join(report) + \"\\n\")\n\
         (ws / \"totals.txt\").write_text(str(grand) + \"\\n\")\n\
         for i in range(1, 9):\n\
         \x20   p = ws / f\"data{{i:02d}}.txt\"\n\
         \x20   p.rename(ws / f\"{{p.name}}.done\")\n\
         print(grand)",
        ws = ws.display().to_string()
    );
    let script = vec![
        step(&[("the whole file chain", chain.as_str())]),
        final_text("done — report, totals and renames are in place."),
    ];
    let provider = Arc::new(ScriptedProvider::new(script));

    let (gate, policy, _approval_rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let slot = Arc::new(InteractSlot::new(
        Arc::clone(&gate) as Arc<dyn phi_agent::ApprovalHandler>
    ));
    let session_id = "code-act-gate".to_string();
    let (agent, _resolver, _telemetry, _bg, _canvas) = phimint::agent::build_with_code_exec(
        CodeExecMode::Act,
        Some(slot),
        provider.clone() as Arc<dyn LlmProvider>,
        gate,
        policy,
        1_000,
        ws.to_path_buf(),
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
        None,
        None,
        None, // writes_possible: no approval switch in tests, so always writable
    )
    .await
    .unwrap();
    let session = agent.create_session().await;

    let result = agent
        .run_turn(
            session,
            "process the data files into report/totals and rename them",
            |_ev| Ok(()),
        )
        .await;
    assert!(result.is_ok(), "run_turn must succeed: {:?}", result.err());

    // Artifacts: the chain's whole output, verified independently of the model.
    assert_eq!(
        std::fs::read_to_string(ws.join("report.txt")).unwrap_or_default(),
        expected_report,
        "report.txt must carry name/sum/max per source"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("totals.txt")).unwrap_or_default(),
        format!("{grand}\n"),
        "totals.txt must carry the grand total"
    );
    for i in 1..=8 {
        assert!(
            !ws.join(format!("data{i:02}.txt")).exists(),
            "data{i:02}.txt must be renamed away"
        );
        assert!(
            ws.join(format!("data{i:02}.txt.done")).exists(),
            "data{i:02}.txt.done must exist"
        );
    }

    // One action step: one code reply, then the closing text. The M2 shape
    // needed a registered tool call to start; this road never does.
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "one code step + one closing text = 2 LLM calls"
    );
    let menu = provider.menu_text();
    assert!(
        !menu.contains("\"code_cell\""),
        "判别门: the menu must not carry code_cell:\n{menu}"
    );
    // The triad rides this menu too (file_face, 2026-11-01) — the whole chain
    // landed in one code step by the prompt's dispatch rule, not because the
    // wire withheld the single-file tools.
    for name in ["\"read_file\"", "\"list_files\"", "\"write_file\""] {
        assert!(
            menu.contains(name),
            "判别门: the file triad rides the menu beside the code road ({name}):\n{menu}"
        );
    }

    // 能记: one submitted cell (one attempt row) and one call row per
    // mediated effect — 8 reads + 2 writes + 8 renames = 18 — so the whole
    // chain's effect account is greppable next to the artifacts.
    let journal = base
        .path()
        .join("sessions")
        .join(&session_id)
        .join("code_journal.jsonl");
    let rows = std::fs::read_to_string(&journal).unwrap_or_default();
    assert_eq!(
        rows.matches("\"kind\":\"cell\"").count(),
        1,
        "判别门 = 1 step = 1 cell row at {journal:?}:\n{rows}"
    );
    assert_eq!(
        rows.matches("\"kind\":\"call\"").count(),
        18,
        "8 reads + 2 writes + 8 renames = 18 call rows:\n{rows}"
    );
    assert_eq!(
        rows.matches("\"fn_name\":\"os.rename\"").count(),
        8,
        "every rename must be on the ledger:\n{rows}"
    );
    let seen = provider.request_text();
    assert!(
        seen.contains(&grand.to_string()),
        "the grand total must reach the model through the observation:\n{seen}"
    );
}

/// 税 2 baseline (T11): the per-request tool-schema byte account. Every
/// request on the tool road carries the full menu including `code_cell`'s
/// schema; the act road never lists it. Pins the relationship (the absolute
/// numbers move whenever the menu moves) and prints the account the dogfood
/// notes record and T12's menu slimming cuts against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_menu_schema_tax_account_names_the_per_request_bytes() {
    let script = vec![final_text("accounted.")];

    // Tool road: the M2 shape, `code_cell` on the menu.
    let tool_base = tempfile::tempdir().unwrap();
    let tool_ws = tempfile::tempdir().unwrap();
    let tool_provider = Arc::new(ScriptedProvider::new(script.clone()));
    let (gate, policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let (tool_agent, ..) = phimint::agent::build_with_code_exec(
        CodeExecMode::Tool,
        None,
        tool_provider.clone() as Arc<dyn LlmProvider>,
        gate,
        policy,
        1_000,
        tool_ws.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![],
        Some(TokenBudgetOptions {
            budget: 210_000,
            retention_days: 30,
            base_dir: tool_base.path().to_path_buf(),
            session_id: "menu-tax-tool".to_string(),
        }),
        None,
        None,
        None, // writes_possible
    )
    .await
    .unwrap();
    let session = tool_agent.create_session().await;
    let result = tool_agent
        .run_turn(session, "count the bytes", |_ev| Ok(()))
        .await;
    assert!(
        result.is_ok(),
        "tool-road turn must succeed: {:?}",
        result.err()
    );

    // Act road: identical assembly minus `code_cell`.
    let act_base = tempfile::tempdir().unwrap();
    let act_ws = tempfile::tempdir().unwrap();
    let act_provider = Arc::new(ScriptedProvider::new(script));
    let (gate, policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let (act_agent, ..) = phimint::agent::build_with_code_exec(
        CodeExecMode::Act,
        Some(Arc::new(InteractSlot::new(
            Arc::clone(&gate) as Arc<dyn phi_agent::ApprovalHandler>
        ))),
        act_provider.clone() as Arc<dyn LlmProvider>,
        gate,
        policy,
        1_000,
        act_ws.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![],
        Some(TokenBudgetOptions {
            budget: 210_000,
            retention_days: 30,
            base_dir: act_base.path().to_path_buf(),
            session_id: "menu-tax-act".to_string(),
        }),
        None,
        None,
        None, // writes_possible
    )
    .await
    .unwrap();
    let session = act_agent.create_session().await;
    let result = act_agent
        .run_turn(session, "count the bytes", |_ev| Ok(()))
        .await;
    assert!(
        result.is_ok(),
        "act-road turn must succeed: {:?}",
        result.err()
    );

    // Per-request menu bytes as they ride the wire (one request per road).
    let tool_menu = tool_provider.menu_text();
    let act_menu = act_provider.menu_text();
    assert!(
        tool_menu.contains("\"code_cell\""),
        "the tool road must carry code_cell:\n{tool_menu}"
    );
    assert!(
        !act_menu.contains("\"code_cell\""),
        "the act road must not:\n{act_menu}"
    );

    // Per-tool shares: the account T12 cuts against (fattest schema first).
    let mut shares: Vec<(String, usize)> = serde_json::from_str::<serde_json::Value>(&tool_menu)
        .expect("tool menu is a JSON array")
        .as_array()
        .expect("tool menu is a JSON array")
        .iter()
        .map(|t| {
            // Wire shape is OpenAI-style: {"function": {"name": …, …}, …}.
            let name = t
                .get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| t.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            (name, t.to_string().len())
        })
        .collect();
    shares.sort_by_key(|a| std::cmp::Reverse(a.1));
    let tool_bytes = tool_menu.len();
    let act_bytes = act_menu.len();
    let tax = tool_bytes - act_bytes;
    let cell_bytes = shares
        .iter()
        .find(|(n, _)| n == "code_cell")
        .map(|(_, b)| *b)
        .expect("code_cell is on the tool road's menu");
    let act_tools = serde_json::from_str::<serde_json::Value>(&act_menu)
        .expect("act menu is a JSON array")
        .as_array()
        .expect("act menu is a JSON array")
        .len();
    println!(
        "menu schema account: tool={tool_bytes}B ({} tools) act={act_bytes}B ({act_tools} tools) \
         delta={tax}B (code_cell alone — both roads carry the file face)",
        shares.len()
    );
    for (name, bytes) in &shares {
        println!("  schema {bytes:>5}B  {name}");
    }

    // The relationship (absolute bytes move with the menu; the delta is the
    // code_cell entry plus its separator — both roads carry the file face,
    // so the file triad cancels out of the delta). `--nocapture` prints the
    // per-tool account the dogfood notes record.
    assert!(
        act_bytes < tool_bytes,
        "the act road pays strictly less per request"
    );
    assert!(
        tax >= cell_bytes,
        "the tax must cover code_cell's whole entry: tax={tax} entry={cell_bytes}"
    );
}
