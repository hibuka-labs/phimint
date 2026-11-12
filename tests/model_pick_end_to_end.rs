//! `/model` pick end to end (notes/spec-model-picker.md): the command action
//! the shell dispatches, the pick modal's answer, the live engine's hot swap,
//! and the wire that proves where the picked endpoint actually goes.
//!
//! Eight cards, seam-free on the product side (the only doubling is the LLM
//! endpoint itself — a real TCP stub):
//!   1. pick → confirm → the engine's next chat() hits the stub with the
//!      picked model name on the wire;
//!   2. Full-config endpoint wins on the wire (same model name, the request
//!      lands on the Full tier's host, never the default one);
//!   3. Esc cancels: nothing changes, the engine keeps the boot provider;
//!   4. a pick persists the swap to the config file behind the store —
//!      surgical (2026-11-07 feedback: a session-only pick reverted on
//!      restart);
//!   5. the model-tier action commands are all listed in the `/` picker;
//!   6. `models` stays typed-only (the picker already lists the models);
//!   7. after a swap, the next request's system message names the new model
//!      (the live-model identity stamp);
//!   8. the stamp tracks every swap along the live session's road: three
//!      tiers, the boot lite warm-up, the pick modal, and the default-provider
//!      shortcut (feedback 2026-11-07 session 3c72b717).

use std::sync::{Arc, Mutex as StdMutex};

use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderInfo, StreamChunk,
};
use phi_agent_tui::{PickAnswer, PickItem, ShellFlow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

use phimint::model::config::{ModelConfig, TierConfig};
use phimint::model::router::PhimintRouter;
use phimint::model::store::ModelStore;
use phimint::shell::commands::command_table;

/// The loud note a switch earns when the store has no config file behind it
/// (the harness stores never do — the persistence road is card 4's).
const NO_CONFIG_NOTE: &str =
    "\n! config.json not saved: no config file found (pick is session-only)";

// ── fixtures ─────────────────────────────────────────────────────────────────

/// The boot provider: never talks to the wire (the pick swaps in a real
/// endpoint provider before any probe), but its `info()` name proves the
/// engine kept or dropped it. It does record the requests it serves — the
/// default-provider shortcut (picking the boot tuple back) runs turns through
/// it, and those requests carry the identity stamp under test too.
struct BootProvider {
    seen: Arc<StdMutex<Vec<Capture>>>,
}

#[async_trait::async_trait]
impl LlmProvider for BootProvider {
    async fn stream(
        &self,
        request: ChatRequest,
    ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|message| {
                let (role, content) = match message {
                    phi_agent::llm_trait::ChatMessage::System { content, .. } => {
                        ("system", content.clone())
                    }
                    phi_agent::llm_trait::ChatMessage::User { content, .. } => {
                        ("user", content.clone())
                    }
                    phi_agent::llm_trait::ChatMessage::Assistant { content, .. } => {
                        ("assistant", content.clone().unwrap_or_default())
                    }
                    _ => ("user", String::new()),
                };
                serde_json::json!({"role": role, "content": content})
            })
            .collect();
        self.seen.lock().unwrap().push(Capture {
            path: "(boot provider)".to_string(),
            authorization: String::new(),
            body: serde_json::json!({"messages": messages}),
        });
        let chunks: Vec<Result<StreamChunk, phi_agent::llm_trait::LlmError>> = vec![
            Ok(StreamChunk::Text(stub_answer())),
            Ok(StreamChunk::Stop {
                finish_reason: Some("stop".to_string()),
            }),
        ];
        Ok(phi_agent::llm_trait::ChatStream::new(Box::pin(
            futures_util::stream::iter(chunks),
        )))
    }

    async fn chat(
        &self,
        _request: ChatRequest,
    ) -> Result<ChatResponse, phi_agent::llm_trait::LlmError> {
        Err(phi_agent::llm_trait::LlmError::llm(
            "boot provider never chats",
        ))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "boot".to_string(),
            model: "boot-model".to_string(),
            version: None,
        }
    }
}

/// The stub's canned streaming answer — deliberately long (the guard's
/// judge-skip threshold is 256 chars of output): a short text-only reply
/// drops under it, the judge makes its own LLM call, and that call joins the
/// captured requests under the stamp assertions. It names no model: the
/// stamped system message carries the whole identity claim under test.
fn stub_answer() -> String {
    format!(
        "{}{}",
        "the stub's canned reply names no model. ".repeat(8),
        "over and out."
    )
}

/// One captured HTTP request at the stub: where it landed and with what.
#[derive(Debug)]
struct Capture {
    path: String,
    authorization: String,
    body: serde_json::Value,
}

/// A real TCP LLM endpoint: every request is captured and answered with a
/// minimal chat.completion. Returns `(base_url, captures)`.
async fn stub_endpoint() -> (String, Arc<StdMutex<Vec<Capture>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let captures: Arc<StdMutex<Vec<Capture>>> = Arc::new(StdMutex::new(Vec::new()));
    let seen = Arc::clone(&captures);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                // Read the request head, then exactly Content-Length bytes.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break pos + 4;
                            }
                        }
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + content_length {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buf[head_end..head_end + content_length])
                        .unwrap_or(serde_json::Value::Null);
                let wants_stream = body
                    .get("stream")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
                let path = head.lines().next().unwrap_or_default().to_string();
                let authorization = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_string())
                    })
                    .unwrap_or_default();
                seen.lock().unwrap().push(Capture {
                    path,
                    authorization,
                    body,
                });

                // The engine streams (and `probe_wire` chats): answer each
                // in its own wire shape.
                let (content_type, reply) = if wants_stream {
                    let reply = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({
                            "id": "chatcmpl-stub",
                            "object": "chat.completion.chunk",
                            "choices": [{
                                "index": 0,
                                "delta": {"content": stub_answer()},
                                "finish_reason": serde_json::Value::Null
                            }]
                        }),
                        serde_json::json!({
                            "id": "chatcmpl-stub",
                            "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                    );
                    ("text/event-stream", reply)
                } else {
                    let reply = serde_json::json!({
                        "id": "chatcmpl-stub",
                        "object": "chat.completion",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string();
                    ("application/json", reply)
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                socket.write_all(head.as_bytes()).await.ok();
                socket.write_all(reply.as_bytes()).await.ok();
            });
        }
    });
    (format!("http://{addr}"), captures)
}

/// The product stack around one config: real agent (boot provider, with the
/// live-model identity stamp mounted exactly as production builds it), real
/// store, real command table — plus the pick queue the shell would own.
struct Harness {
    agent: Arc<phi_agent::PhiAgent>,
    model_store: Arc<Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
    pick_tx: mpsc::UnboundedSender<PickItem>,
    pick_rx: mpsc::UnboundedReceiver<PickItem>,
    /// The requests the boot provider served (the default-provider shortcut).
    boot_seen: Arc<StdMutex<Vec<Capture>>>,
    _workspace: tempfile::TempDir,
    _skills: tempfile::TempDir,
}

async fn harness(config: ModelConfig) -> Harness {
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    let (approval, policy, _approval_rx, _mode) =
        phimint::approval::build_live_approval("auto", None);
    let boot_seen: Arc<StdMutex<Vec<Capture>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(BootProvider {
        seen: Arc::clone(&boot_seen),
    });
    let model_store = Arc::new(Mutex::new(ModelStore::new(config, Arc::clone(&provider))));
    let (agent, _resolver, _telemetry, _bg, _canvas) = phimint::agent::build(
        provider,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        "boot-model".to_string(),
        vec![skills.path().join("skills")],
        None,
        Some(Arc::clone(&model_store)),
        None,
    )
    .await
    .expect("agent build");
    let router = Arc::new(PhimintRouter::new());
    let (pick_tx, pick_rx) = mpsc::unbounded_channel();
    Harness {
        agent: Arc::new(agent),
        model_store,
        router,
        pick_tx,
        pick_rx,
        boot_seen,
        _workspace: workspace,
        _skills: skills,
    }
}

/// Drive bare `/model` through the command table's action (the exact closure
/// the shell dispatches) and answer the pushed pick the way the human would.
/// Returns the command's settled text.
async fn pick_main_model(
    h: &mut Harness,
    answer: impl FnOnce(Vec<phi_agent_tui::PickRow>) -> PickAnswer,
) -> String {
    let external_dir = tempfile::tempdir().unwrap();
    let (gate, _policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let external = Arc::new(Mutex::new(phimint::external::state::ExternalState::load(
        external_dir.path(),
        external_dir.path(),
        gate,
    )));
    let commands = command_table(
        Arc::clone(&h.agent),
        Arc::clone(&h.model_store),
        Arc::clone(&h.router),
        external,
        Some(h.pick_tx.clone()),
    );
    let action = commands
        .iter()
        .find(|spec| spec.name == "model")
        .expect("the model command is in the table")
        .action
        .clone();
    let command = tokio::spawn(async move {
        match action("/model".to_string()).await {
            ShellFlow::Answer(text) => text,
            ShellFlow::Unhandled => panic!("bare /model must be answered by the model face"),
        }
    });

    let item = h.pick_rx.recv().await.expect("the pick modal is pushed");
    assert_eq!(item.title, "Model");
    item.answer_tx
        .send(answer(item.rows))
        .expect("the command action is parked on the answer");
    command.await.unwrap()
}

/// Drive a typed command through the command table's action (the exact
/// closure the shell dispatches on Enter). Returns the settled text.
async fn typed(h: &Harness, input: &str) -> String {
    let external_dir = tempfile::tempdir().unwrap();
    let (gate, _policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let external = Arc::new(Mutex::new(phimint::external::state::ExternalState::load(
        external_dir.path(),
        external_dir.path(),
        gate,
    )));
    let commands = command_table(
        Arc::clone(&h.agent),
        Arc::clone(&h.model_store),
        Arc::clone(&h.router),
        external,
        Some(h.pick_tx.clone()),
    );
    let name = input
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    let action = commands
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("{name} missing from the command table"))
        .action
        .clone();
    match action(input.to_string()).await {
        ShellFlow::Answer(text) => text,
        ShellFlow::Unhandled => panic!("{input} must be answered by the model face"),
    }
}

/// Run one real turn against the live engine — the request goes out through
/// whatever provider the store installed, so the stub captures it.
async fn run_turn(h: &Harness, session: &phi_agent::SessionId, prompt: &str) {
    h.agent
        .run_turn(session.clone(), prompt, |_ev| Ok(()))
        .await
        .expect("the turn runs");
}

/// The system message of one captured request (the first message with the
/// system role — the identity stamp lives there).
fn system_of(capture: &Capture) -> String {
    capture
        .body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter().find_map(|msg| {
                (msg.get("role").and_then(|r| r.as_str()) == Some("system")).then(|| {
                    msg.get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string()
                })
            })
        })
        .unwrap_or_default()
}

/// The stamped system messages among the captured requests. A guard judge
/// call rides the same provider with its own prompt and is not one of these.
fn stamped_systems(seen: &[Capture]) -> Vec<String> {
    seen.iter()
        .map(system_of)
        .filter(|system| system.contains("## Current model"))
        .collect()
}

/// Probe the LIVE engine: whatever provider the swap installed is the one
/// that answers this — the wire proves it.
async fn probe_wire(agent: &phi_agent::PhiAgent) {
    let provider = agent.runtime().llm_engine().get_provider();
    let response = provider
        .chat(ChatRequest::new(vec![
            phi_agent::llm_trait::ChatMessage::user("hi"),
        ]))
        .await
        .expect("the swapped provider answers");
    assert_eq!(response.content, "pong");
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

fn config(base_url: &str, main: TierConfig, lite: Option<TierConfig>) -> ModelConfig {
    ModelConfig {
        base_url: Some(base_url.to_string()),
        api_key: Some("sk-test".to_string()),
        protocol: Some("openai".to_string()),
        main,
        lite,
        advanced: None,
        model: None,
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    }
}

// ── cards ────────────────────────────────────────────────────────────────────

/// protects: every model-tier ACTION command is listed in the `/` picker —
/// a command the picker does not show is a command nobody finds (feedback
/// #15 lived this: `/mo` in the picker matched only the `modernize` skill).
/// fails_when: an action command drops out of the picker face or loses its
/// help row. no_claim: the picker's rendering, ordering, or filter policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_tier_actions_are_discoverable_in_the_slash_picker() {
    let (base_url, _captures) = stub_endpoint().await;
    let h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        None,
    ))
    .await;
    let external_dir = tempfile::tempdir().unwrap();
    let (gate, _policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let external = Arc::new(Mutex::new(phimint::external::state::ExternalState::load(
        external_dir.path(),
        external_dir.path(),
        gate,
    )));
    let commands = command_table(
        h.agent,
        h.model_store,
        h.router,
        external,
        Some(h.pick_tx.clone()),
    );
    for name in ["model", "lite", "advanced", "focus"] {
        let spec = commands
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("{name} missing from the command table"));
        assert!(spec.in_picker, "{name} must be listed in the / picker");
        assert!(!spec.help.is_empty(), "{name} needs a help row");
    }
}

/// protects: `models` stays runnable by typing while staying out of the
/// picker rows — the picker already lists the models themselves, so a
/// second listing command among the rows is noise (feedback 2026-11-07:
/// `/mo` showed `model` and `models` side by side). fails_when: `models`
/// disappears from the command table (typing it would become a model turn)
/// or creeps back into the picker. no_claim: the listing's wording beyond
/// naming the configured models and the focus mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_stays_typable_but_out_of_the_picker() {
    let (base_url, _captures) = stub_endpoint().await;
    let h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        None,
    ))
    .await;

    let text = typed(&h, "/models").await;
    assert!(text.contains("alpha-model"), "{text}");
    assert!(text.contains("Focus mode"), "{text}");

    let external_dir = tempfile::tempdir().unwrap();
    let (gate, _policy, _rx, _mode) = phimint::approval::build_live_approval("auto", None);
    let external = Arc::new(Mutex::new(phimint::external::state::ExternalState::load(
        external_dir.path(),
        external_dir.path(),
        gate,
    )));
    let commands = command_table(
        h.agent,
        h.model_store,
        h.router,
        external,
        Some(h.pick_tx.clone()),
    );
    let spec = commands
        .iter()
        .find(|spec| spec.name == "models")
        .expect("models stays in the command table");
    assert!(!spec.in_picker, "models is typed-only");
    assert!(!spec.help.is_empty(), "models keeps its help row");
}

/// protects: a picked row swaps the live engine's provider and the wire
/// carries the picked model name to the configured endpoint — pick, confirm,
/// and traffic are one chain. fails_when: the confirmation arrives but the
/// engine keeps the old provider, or the wire carries a different name.
/// no_claim: the modal's rendering (phi-agent-tui's cards own it) or what the
/// endpoint does with the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pick_swaps_the_live_engine_and_the_wire_carries_the_picked_model() {
    let (base_url, captures) = stub_endpoint().await;
    let mut h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        Some(TierConfig::Simple("beta-model".to_string())),
    ))
    .await;

    let text = pick_main_model(&mut h, |rows| {
        assert!(
            rows.iter().any(|row| row.id == "beta-model"),
            "the picker lists the configured models: {rows:?}"
        );
        PickAnswer::Chosen {
            id: "beta-model".to_string(),
            literal: false,
        }
    })
    .await;
    assert_eq!(
        text,
        format!("Main model set to: beta-model (was alpha-model){NO_CONFIG_NOTE}"),
        "both names are unknown to the registry, so there are no capability bits to diff"
    );

    probe_wire(&h.agent).await;
    let seen = captures.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one request landed: {seen:?}");
    assert!(
        seen[0].path.contains("chat/completions"),
        "path: {}",
        seen[0].path
    );
    assert_eq!(
        seen[0].body.get("model").and_then(|m| m.as_str()),
        Some("beta-model")
    );
    assert_eq!(seen[0].authorization, "Bearer sk-test");
}

/// protects: the endpoint travels with the name — two tiers on one model name
/// with different endpoints collapse to one shelf row, that row carries the
/// Full tier's endpoint, and a pick of it lands the request there (and
/// nowhere else). The merged name here is deliberately NOT the current
/// main's: on the current name the live tuple wins, so a same-name row never
/// silently swaps the endpoint (the design's「同名行元组=生效元组赢」face —
/// pinned in `model::picker_rows` and `model_pointer_end_to_end`).
/// fails_when: the pick installs the Simple tier's inherited endpoint while
/// the row advertised the Full one, or the merged name duplicates on the
/// shelf. no_claim: which endpoint a product should prefer beyond "Full
/// wins", or what a pick of the current row does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_config_endpoint_wins_on_the_wire() {
    let (default_url, default_captures) = stub_endpoint().await;
    let (full_url, full_captures) = stub_endpoint().await;
    let mut model_config = config(
        &default_url,
        TierConfig::Simple("alpha-model".to_string()),
        Some(TierConfig::Simple("dup-model".to_string())),
    );
    model_config.advanced = Some(TierConfig::Full {
        model: "dup-model".to_string(),
        base_url: Some(full_url.clone()),
        api_key: Some("sk-other".to_string()),
        protocol: Some("openai".to_string()),
    });
    let mut h = harness(model_config).await;

    let text = pick_main_model(&mut h, |rows| {
        assert_eq!(
            rows.iter().filter(|row| row.id == "dup-model").count(),
            1,
            "one name, one row: {rows:?}"
        );
        PickAnswer::Chosen {
            id: "dup-model".to_string(),
            literal: false,
        }
    })
    .await;
    assert_eq!(
        text,
        format!("Main model set to: dup-model (was alpha-model){NO_CONFIG_NOTE}"),
        "the Full endpoint rides the merged row's name"
    );

    probe_wire(&h.agent).await;
    let full_seen = full_captures.lock().unwrap();
    assert_eq!(full_seen.len(), 1, "the Full endpoint gets the traffic");
    assert_eq!(
        full_seen[0].authorization, "Bearer sk-other",
        "the Full tier's key travels with the endpoint"
    );
    assert!(
        default_captures.lock().unwrap().is_empty(),
        "the default endpoint must see nothing"
    );
}

/// protects: Esc is a true cancel — the engine keeps the boot provider and
/// the store keeps its tuples. fails_when: a cancelled pick half-applies or
/// the command settles with a success copy. no_claim: what happens to the
/// turn the command ran on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_pick_leaves_the_engine_untouched() {
    let (base_url, _captures) = stub_endpoint().await;
    let mut h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        Some(TierConfig::Simple("beta-model".to_string())),
    ))
    .await;

    let text = pick_main_model(&mut h, |_| PickAnswer::Cancelled).await;
    assert_eq!(text, "model unchanged");
    assert_eq!(
        h.agent.runtime().llm_engine().get_provider().info().model,
        "boot-model",
        "no swap happened"
    );
    assert_eq!(h.model_store.lock().await.tier_model("main"), "alpha-model");
}

/// protects: a pick persists to the config file behind the store — the
/// `model` selection pointer carries the picked name (effective main =
/// pointer) while the `main` definition, the comments and the sibling keys
/// survive byte-for-byte, the confirm marks the save, and no stray tmp file
/// is left behind (feedback 2026-11-07: a session-only pick reverted on
/// restart and the model's self-introduction drifted). fails_when: the pick
/// path touches a byte outside the `model` member, rewrites the `main`
/// definition (the「model list shrinks」regression), leaves the save
/// unmarked, or leaves a sibling file. no_claim: the splice shapes
/// (`model::persist` fixtures own those).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pick_persists_the_swap_to_the_config_file() {
    let (base_url, _captures) = stub_endpoint().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.json");
    std::fs::write(
        &config_path,
        "{\n  // keep me\n  \"main\": \"alpha-model\"\n}\n",
    )
    .unwrap();

    let mut h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        Some(TierConfig::Simple("beta-model".to_string())),
    ))
    .await;
    h.model_store
        .lock()
        .await
        .set_config_path(Some(config_path.clone()));
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "beta-model".to_string(),
        literal: false,
    })
    .await;
    assert!(text.starts_with("Main model set to: beta-model"), "{text}");
    assert!(
        text.contains("\n  saved to config.json"),
        "the confirm marks the save on its own line: {text}"
    );

    let after = std::fs::read_to_string(&config_path).unwrap();
    let parsed: ModelConfig = json5::from_str(&after).unwrap();
    assert_eq!(
        parsed.tier_config("main").unwrap().0,
        "beta-model",
        "effective main = the `model` pointer: {after}"
    );
    assert_eq!(
        parsed.model.as_ref().map(|m| m.model()),
        Some("beta-model"),
        "the pick landed in the `model` pointer: {after}"
    );
    assert_eq!(
        parsed.main.model(),
        "alpha-model",
        "the configured `main` definition never moves: {after}"
    );
    assert!(after.contains("// keep me"), "{after}");
    let entries: Vec<_> = std::fs::read_dir(config_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("config.json")],
        "the splice leaves no sibling file (the tmp rename consumes it)"
    );
}

/// protects: the outgoing request names the live main model — after a
/// `/model` swap, the next request's system message carries a `## Current
/// model` section naming the new model, the old name is gone from it, the
/// stamp never accumulates copies, and the `current_model` tool the stamp
/// routes identity questions to is on the request's tool list (feedback
/// 2026-11-07: the swap landed on the wire but the model answered "what
/// model are you" with the previous name, from stale history). fails_when:
/// the section is missing from a request, still names the previous model
/// after a swap, stacks copies, or the tool is absent from the wire.
/// no_claim: the section's wording beyond naming the model, or what the
/// endpoint does with the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_model_swap_the_next_request_names_the_new_model() {
    let (base_url, captures) = stub_endpoint().await;
    let h = harness(config(
        &base_url,
        TierConfig::Simple("alpha-model".to_string()),
        Some(TierConfig::Simple("beta-model".to_string())),
    ))
    .await;

    // Swap through the real command path, then ask: the request names the
    // live model.
    let text = typed(&h, "/model beta-model").await;
    assert_eq!(
        text,
        format!("Main model set to: beta-model (was alpha-model){NO_CONFIG_NOTE}"),
        "both names are unknown to the registry, so there are no capability bits to diff"
    );
    let session = h.agent.create_session().await;
    let before = captures.lock().unwrap().len();
    run_turn(&h, &session, "what model are you?").await;
    {
        let seen = captures.lock().unwrap();
        let stamped = stamped_systems(&seen[before..]);
        assert!(
            !stamped.is_empty(),
            "the request carries the stamp: {seen:?}"
        );
        for system in stamped {
            assert!(system.contains("`beta-model`"), "{system}");
            assert_eq!(system.matches("## Current model").count(), 1, "{system}");
        }
        // The identity tool the stamp points at rides the request's tool
        // list (a name match in the system copy cannot fake this — it only
        // matches inside `tools`).
        for capture in &seen[before..] {
            let names: Vec<String> = capture
                .body
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            t.get("name")
                                .and_then(|n| n.as_str())
                                .or_else(|| t.pointer("/function/name").and_then(|n| n.as_str()))
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                names.iter().any(|n| n == "current_model"),
                "the `current_model` tool is on the wire: {names:?}"
            );
        }
    }

    // Swap mid-session and re-ask the same question in the same session.
    // `gamma-model` is a typed literal nothing vouches for, so the pick
    // carries the spelling warning.
    let text = typed(&h, "/model gamma-model").await;
    assert_eq!(
        text,
        format!(
            "Main model set to: gamma-model (was beta-model){NO_CONFIG_NOTE}\n\
             ! `gamma-model` is not a declared or known model name — double-check the spelling"
        )
    );
    let before = captures.lock().unwrap().len();
    run_turn(&h, &session, "what model are you?").await;
    {
        let seen = captures.lock().unwrap();
        let stamped = stamped_systems(&seen[before..]);
        assert!(
            !stamped.is_empty(),
            "the request carries the stamp: {seen:?}"
        );
        for system in &stamped {
            assert!(
                system.contains("`gamma-model`"),
                "after the swap every stamped request names the new model: {system}"
            );
            assert!(
                !system.contains("beta-model"),
                "and the stale name is gone: {system}"
            );
            assert_eq!(
                system.matches("## Current model").count(),
                1,
                "one stamp, never two: {system}"
            );
        }
    }
}

/// protects: the stamp tracks every swap along the live session's exact road
/// — three configured tiers (main/lite/advanced), the boot lite warm-up the
/// title generator performs, the pick modal's answer path, and the
/// default-provider shortcut when the boot tuple is picked back (feedback
/// 2026-11-07 session 3c72b717 walked this shape: pick lite's model → ask ✓,
/// pick the boot model back → ask ✓, pick advanced's model → ask and the
/// answer named the lite model instead). Each request after a swap names the
/// picked model — whatever provider served it (warmed cache, boot default, or
/// a fresh factory provider) — one region, no previous name. fails_when: any
/// post-swap request carries an earlier pick's name or a second stamp region.
/// no_claim: what a live LLM answers (the stubs' canned text names no model).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_swap_is_named_by_the_next_request_across_the_live_tier_shape() {
    let (base_url, captures) = stub_endpoint().await;
    let mut config = config(
        &base_url,
        TierConfig::Simple("tier-main".to_string()),
        Some(TierConfig::Simple("tier-lite".to_string())),
    );
    config.advanced = Some(TierConfig::Simple("tier-advanced".to_string()));
    let mut h = harness(config).await;

    // The title generator warms the lite tier's provider at boot (main.rs
    // runs this before the first turn), so the first pick lands on a cached
    // provider.
    h.model_store
        .lock()
        .await
        .get_or_create_provider("lite")
        .ok();
    let session = h.agent.create_session().await;

    // Swap A: the lite tier's model — served by the warmed provider (wire).
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "tier-lite".to_string(),
        literal: false,
    })
    .await;
    assert_eq!(
        text,
        format!("Main model set to: tier-lite (was tier-main){NO_CONFIG_NOTE}")
    );
    let before = captures.lock().unwrap().len();
    run_turn(&h, &session, "what model are you?").await;
    {
        let seen = captures.lock().unwrap();
        let stamped = stamped_systems(&seen[before..]);
        assert!(
            !stamped.is_empty(),
            "the request carries the stamp: {seen:?}"
        );
        for system in &stamped {
            assert!(system.contains("`tier-lite`"), "{system}");
            assert_eq!(system.matches("## Current model").count(), 1, "{system}");
        }
    }

    // Swap B: the boot tuple back — the default provider serves this turn,
    // and its request carries the stamp too.
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "tier-main".to_string(),
        literal: false,
    })
    .await;
    assert_eq!(
        text,
        format!("Main model set to: tier-main (was tier-lite){NO_CONFIG_NOTE}")
    );
    let before = h.boot_seen.lock().unwrap().len();
    run_turn(&h, &session, "what model are you?").await;
    {
        let seen = h.boot_seen.lock().unwrap();
        let stamped = stamped_systems(&seen[before..]);
        assert!(
            !stamped.is_empty(),
            "the request carries the stamp: {seen:?}"
        );
        for system in &stamped {
            assert!(system.contains("`tier-main`"), "{system}");
            assert!(
                !system.contains("tier-lite"),
                "the stale name is gone: {system}"
            );
            assert_eq!(system.matches("## Current model").count(), 1, "{system}");
        }
    }

    // Swap C: the advanced tier's model — the live session's failing moment.
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "tier-advanced".to_string(),
        literal: false,
    })
    .await;
    assert_eq!(
        text,
        format!("Main model set to: tier-advanced (was tier-main){NO_CONFIG_NOTE}")
    );
    let before = captures.lock().unwrap().len();
    run_turn(&h, &session, "what model are you?").await;
    {
        let seen = captures.lock().unwrap();
        let stamped = stamped_systems(&seen[before..]);
        assert!(
            !stamped.is_empty(),
            "the request carries the stamp: {seen:?}"
        );
        for system in &stamped {
            assert!(
                system.contains("`tier-advanced`"),
                "after the third swap the request names the advanced model: {system}"
            );
            assert!(
                !system.contains("tier-main") && !system.contains("tier-lite"),
                "and no earlier pick's name lingers: {system}"
            );
            assert_eq!(
                system.matches("## Current model").count(),
                1,
                "one stamp, never two: {system}"
            );
        }
    }
}
