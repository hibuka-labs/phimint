//! The `model` selection pointer end to end (notes/model-pointer-design.md):
//! what a restart sees, what a switch moves, and what「clear selection」leaves
//! behind. The seam is file ↔ store ↔ commands ↔ file — the splice shapes
//! themselves are `model::persist`'s fixture cards, and the picker row-set
//! rules are `model::picker_rows`' unit cards; this file proves the three
//! compose the way the product runs them.
//!
//! Four cards, seam-free on the product side (the only doubling is the LLM
//! endpoint itself — real TCP stubs):
//!   1. boot-with-pointer serves the picked endpoint (wire: model name +
//!      host) and names it in the identity stamp;「clear selection」then
//!      rolls back to the configured `main` and a reboot returns there;
//!   2. N switches move only the `model` member — the main/lite/advanced
//!      definitions and the comments survive every switch byte-for-byte
//!      (the「model list shrinks」regression card, SC1);
//!   3. re-picking the current row is a tuple-equivalence skip: no swap
//!      copy, no save, and the file is byte-identical.
//!   4. a CLI `--model` override is session-only: the boot composition
//!      serves the CLI name over a file pointer while the config file on
//!      disk stays byte-identical (nothing persists the override).

use std::sync::{Arc, Mutex as StdMutex};

use phi_agent_tui::{PickAnswer, PickItem, ShellFlow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

use phimint::model::config::ModelConfig;
use phimint::model::picker_rows::CLEAR_SELECTION_ID;
use phimint::model::router::PhimintRouter;
use phimint::model::store::ModelStore;
use phimint::shell::commands::command_table;

// ── fixtures ─────────────────────────────────────────────────────────────────

/// The stub's canned streaming answer — deliberately long (the guard's
/// judge-skip threshold is 256 chars of output): a short text-only reply
/// drops under it, the judge makes its own LLM call, and that call joins the
/// captured requests under the stamp assertions.
fn stub_answer() -> String {
    format!(
        "{}{}",
        "the stub's canned reply names no model. ".repeat(8),
        "over and over."
    )
}

/// One captured HTTP request at a stub: where it landed and with what.
#[derive(Debug)]
struct Capture {
    path: String,
    body: serde_json::Value,
}

/// A real TCP LLM endpoint: every request is captured and answered with a
/// minimal chat.completion (streaming and non-streaming shapes). Returns
/// `(base_url, captures)`.
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
                seen.lock().unwrap().push(Capture { path, body });

                // The engine streams: answer in the matching wire shape.
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
                let content_type = if wants_stream {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let reply = if wants_stream {
                    reply
                } else {
                    serde_json::json!({
                        "id": "chatcmpl-stub",
                        "object": "chat.completion",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "pong"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string()
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

/// The boot provider main.rs builds: a real provider for the effective main
/// (the `model` pointer when set, else the configured `main`) — the same
/// `create_provider` call on the same resolution rule.
fn boot_provider(config: &ModelConfig) -> Arc<dyn phi_agent::llm_trait::LlmProvider> {
    let (model, base_url, api_key, protocol) = config
        .tier_config("main")
        .expect("main tier always resolves");
    phi_agent::create_provider(&phi_agent::llm_trait::LlmConfig {
        model,
        base_url,
        api_key,
        protocol: if protocol.is_empty() {
            None
        } else {
            protocol.parse::<phi_agent::llm_trait::Protocol>().ok()
        },
        ..Default::default()
    })
    .expect("boot provider builds")
}

/// The product stack around one config FILE, booted the way a restart boots:
/// parse the file (the exact json5 parse main.rs runs), build the store and
/// the boot provider from its effective main, and wire the command table's
/// pick queue. The file sits behind the store, so picks persist for real.
struct Harness {
    agent: Arc<phi_agent::PhiAgent>,
    model_store: Arc<Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
    pick_tx: mpsc::UnboundedSender<PickItem>,
    pick_rx: mpsc::UnboundedReceiver<PickItem>,
    _workspace: tempfile::TempDir,
    _skills: tempfile::TempDir,
}

async fn boot(config_path: &std::path::Path) -> Harness {
    let raw = std::fs::read_to_string(config_path).unwrap();
    let config: ModelConfig = json5::from_str(&raw).unwrap();
    let boot_model = config.tier_config("main").unwrap().0;
    let workspace = tempfile::tempdir().unwrap();
    let skills = tempfile::tempdir().unwrap();
    let (approval, policy, _approval_rx, _mode) =
        phimint::approval::build_live_approval("auto", None);
    let provider = boot_provider(&config);
    let model_store = Arc::new(Mutex::new(ModelStore::new(config, Arc::clone(&provider))));
    model_store
        .lock()
        .await
        .set_config_path(Some(config_path.to_path_buf()));
    let (agent, _resolver, _telemetry, _bg, _canvas) = phimint::agent::build(
        provider,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        boot_model,
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

/// Run one real turn through the live engine — the request goes out through
/// whatever provider the store installed, so the stubs capture it.
async fn run_turn(h: &Harness, prompt: &str) {
    let session = h.agent.create_session().await;
    h.agent
        .run_turn(session, prompt, |_ev| Ok(()))
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

/// The `model` member of a config text (the selection pointer's value),
/// stripped — `None` when the file has no such member.
fn pointer_value(raw: &str) -> Option<String> {
    let parsed: ModelConfig = json5::from_str(raw).unwrap();
    parsed.model.as_ref().map(|m| m.model().to_string())
}

/// The switch report every pick settles on: which model, and that the file
/// was saved.
fn assert_switch_report(text: &str, want: &str) {
    assert!(
        text.starts_with(&format!("Main model set to: {want}")),
        "{text}"
    );
    assert!(text.contains("\n  saved to config.json"), "{text}");
}

/// SC1's span-stability at the seam: after a switch the file is the original
/// bytes plus exactly the one `model` member — the main/lite/advanced
/// definitions, the defaults and the comments never move. Remove the
/// member's chunk and the original comes back byte-for-byte.
fn assert_only_the_model_member_moved(path: &std::path::Path, original: &str, want: &str) {
    let after = std::fs::read_to_string(path).unwrap();
    for definition in [
        "\"main\": \"alpha-model\"",
        "\"lite\": \"beta-model\"",
        "\"advanced\": \"gamma-model\"",
    ] {
        assert!(
            after.contains(definition),
            "after {want}, the definition moved: {after}"
        );
    }
    assert!(after.contains("// keep me"), "after {want}: {after}");
    assert_eq!(
        after.matches("\"model\":").count(),
        1,
        "exactly one `model` member: {after}"
    );
    assert_eq!(
        pointer_value(&after).as_deref(),
        Some(want),
        "the pointer tracks the pick: {after}"
    );
    let chunk = format!("\n  \"model\": \"{want}\",");
    assert_eq!(
        after.matches(&chunk).count(),
        1,
        "the member lands with its separator and comma: {after}"
    );
    assert_eq!(
        after.replace(&chunk, ""),
        original,
        "nothing but the `model` member moved"
    );
}

// ── cards ────────────────────────────────────────────────────────────────────

/// protects: SC2 — a boot over a pointer file serves the picked endpoint:
/// the wire carries the picked model name and lands on the picked host (the
/// default host sees nothing), and the identity stamp names the picked
/// model.「clear selection」then deletes the pointer, rolls the effective
/// main back to the configured definition, and a reboot returns to the
/// default host. fails_when: the boot ignores the pointer, the pointer
/// endpoint is silently dropped, the clear leaves the pointer behind, or a
/// reboot still serves the picked model. no_claim: restart timing (the
/// rebuild here IS the restart).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pointer_boot_serves_the_picked_endpoint_and_a_clear_boot_returns_to_default() {
    let (default_url, default_seen) = stub_endpoint().await;
    let (pointer_url, pointer_seen) = stub_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        format!(
            "{{\n  // keep me\n  \"main\": \"alpha-model\",\n  \"base_url\": \"{default_url}\",\n  \"api_key\": \"sk-test\",\n  \"protocol\": \"openai\",\n  \"model\": {{ \"model\": \"pointer-model\", \"base_url\": \"{pointer_url}\" }}\n}}\n"
        ),
    )
    .unwrap();

    // Boot one: the pointer rules.
    let mut h = boot(&path).await;
    {
        let store = h.model_store.lock().await;
        assert_eq!(store.tier_model("main"), "pointer-model");
        assert_eq!(
            store.tier_tuple("main"),
            (
                "pointer-model".to_string(),
                pointer_url.clone(),
                "sk-test".to_string(),
                "openai".to_string(),
            ),
            "the delta field rides the pointer, the rest inherits the defaults"
        );
    }
    run_turn(&h, "hello").await;
    {
        let seen = pointer_seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the picked host served the turn: {seen:?}");
        assert_eq!(
            seen[0].body.get("model").and_then(|m| m.as_str()),
            Some("pointer-model"),
            "the wire names the picked model"
        );
        assert!(
            seen[0].path.contains("chat/completions"),
            "the request is a chat call: {}",
            seen[0].path
        );
        let system = system_of(&seen[0]);
        assert!(system.contains("`pointer-model`"), "{system}");
    }
    assert!(
        default_seen.lock().unwrap().is_empty(),
        "the default host sees nothing while the pointer rules"
    );

    //「Clear selection」from the picker: back to the configured main.
    let text = pick_main_model(&mut h, |rows| {
        assert!(
            rows.iter().any(|row| row.id == CLEAR_SELECTION_ID),
            "the clear row is offered while a selection rides: {rows:?}"
        );
        PickAnswer::Chosen {
            id: CLEAR_SELECTION_ID.to_string(),
            literal: false,
        }
    })
    .await;
    assert_eq!(
        text, "Main model set to: alpha-model (was pointer-model)\n  saved to config.json",
        "both names are unknown to the registry, so there are no capability bits to diff"
    );

    // Reboot over the cleared file: the configured main rules again.
    let h = boot(&path).await;
    assert_eq!(h.model_store.lock().await.tier_model("main"), "alpha-model");
    run_turn(&h, "hello").await;
    {
        let seen = default_seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the default host serves the turn: {seen:?}");
        assert_eq!(
            seen[0].body.get("model").and_then(|m| m.as_str()),
            Some("alpha-model")
        );
        assert!(system_of(&seen[0]).contains("`alpha-model`"));
    }
    assert!(
        pointer_seen.lock().unwrap().len() == 1,
        "the picked host saw only the pointer boot"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("// keep me"), "{after}");
    assert!(
        after.contains("\"main\": \"alpha-model\""),
        "the definition survives: {after}"
    );
    assert!(
        !after.contains("\"model\""),
        "the pointer member is gone: {after}"
    );
    assert!(pointer_value(&after).is_none());
}

/// protects: SC1 — N switches move only the `model` member: the main, lite
/// and advanced definitions and the surrounding comments survive every
/// switch byte-for-byte (the「model list shrinks」regression), and the file
/// keeps exactly one `model` member tracking the pick. fails_when: any
/// switch rewrites or deletes a tier definition, or a second `model` member
/// appears. no_claim: the splice corner cases (comment-adjacent commas,
/// quoted keys — `model::persist` fixtures own those) or the confirm copy
/// (`shell::commands` cards own it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_switches_move_only_the_model_member() {
    let (base_url, _seen) = stub_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let original = format!(
        "{{\n  // keep me\n  \"main\": \"alpha-model\",\n  \"lite\": \"beta-model\",\n  \"advanced\": \"gamma-model\",\n  \"base_url\": \"{base_url}\",\n  \"api_key\": \"sk-test\",\n  \"protocol\": \"openai\"\n}}\n"
    );
    std::fs::write(&path, &original).unwrap();

    let mut h = boot(&path).await;

    // Switch 1 — modal pick of a declared row.
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "beta-model".to_string(),
        literal: false,
    })
    .await;
    assert_switch_report(&text, "beta-model");
    assert_only_the_model_member_moved(&path, &original, "beta-model");

    // Switch 2 — modal pick of another declared row.
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "gamma-model".to_string(),
        literal: false,
    })
    .await;
    assert_switch_report(&text, "gamma-model");
    assert_only_the_model_member_moved(&path, &original, "gamma-model");

    // Switch 3 — a typed literal through the command line face.
    let text = typed(&h, "/model delta-model").await;
    assert_switch_report(&text, "delta-model");
    assert_only_the_model_member_moved(&path, &original, "delta-model");

    // Switch 4 — back to a declared row: the member is replaced in place.
    let text = pick_main_model(&mut h, |_| PickAnswer::Chosen {
        id: "beta-model".to_string(),
        literal: false,
    })
    .await;
    assert_switch_report(&text, "beta-model");
    assert_only_the_model_member_moved(&path, &original, "beta-model");

    let after = std::fs::read_to_string(&path).unwrap();
    let parsed: ModelConfig = json5::from_str(&after).unwrap();
    assert_eq!(parsed.tier_config("main").unwrap().0, "beta-model");
    assert_eq!(parsed.main.model(), "alpha-model");
    assert_eq!(parsed.lite.as_ref().unwrap().model(), "beta-model");
    assert_eq!(parsed.advanced.as_ref().unwrap().model(), "gamma-model");
}

/// protects: under a pointer whose name is the configured `main` but whose
/// endpoint differs (a face only the pointer introduces), the picker's
/// current row carries the EFFECTIVE tuple — re-picking it is a
/// tuple-equivalence skip: no swap copy, no save, and the file is
/// byte-identical. fails_when: the re-pick silently swaps the endpoint back
/// to the declaration or rewrites the file. no_claim: how the pointer came
/// to rename the endpoint of the configured name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_picking_the_current_row_is_a_tuple_equivalence_skip() {
    let (default_url, _default_seen) = stub_endpoint().await;
    let (pointer_url, _pointer_seen) = stub_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        format!(
            "{{\n  // keep me\n  \"main\": \"alpha-model\",\n  \"base_url\": \"{default_url}\",\n  \"api_key\": \"sk-test\",\n  \"protocol\": \"openai\",\n  \"model\": {{ \"model\": \"alpha-model\", \"base_url\": \"{pointer_url}\" }}\n}}\n"
        ),
    )
    .unwrap();

    let mut h = boot(&path).await;
    assert_eq!(
        h.model_store.lock().await.tier_tuple("main").1,
        pointer_url,
        "the pointer's endpoint is the live one"
    );
    let before = std::fs::read_to_string(&path).unwrap();

    let text = pick_main_model(&mut h, |rows| {
        assert_eq!(rows[0].id, "alpha-model", "the current row floats top");
        assert!(rows[0].current, "{rows:?}");
        PickAnswer::Chosen {
            id: "alpha-model".to_string(),
            literal: false,
        }
    })
    .await;
    assert_eq!(
        text, "Main model unchanged: alpha-model",
        "re-picking the current row changes nothing and saves nothing"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "the file is byte-identical: no switch, no write"
    );
    assert_eq!(
        h.model_store.lock().await.tier_tuple("main").1,
        pointer_url,
        "the endpoint did not fall back to the declaration"
    );
}

/// protects: the CLI `--model` override is session-only — the boot
/// composition (parse the file → apply the CLI slot → build store and boot
/// provider) serves the CLI name over a file pointer while the config file
/// on disk stays byte-identical. fails_when: any CLI-apply step writes the
/// file (a sticky `--model`), or the CLI name loses to the pointer.
/// no_claim: the picker's persist path (a real pick writes by design) and
/// `--lite_model`/`--advanced_model` (unchanged semantics).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cli_override_serves_the_cli_name_and_never_writes_the_file() {
    let (default_url, _default_seen) = stub_endpoint().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let original = format!(
        "{{\n  // keep me\n  \"main\": \"alpha-model\",\n  \"base_url\": \"{default_url}\",\n  \"api_key\": \"sk-test\",\n  \"protocol\": \"openai\",\n  \"model\": \"pointer-model\"\n}}\n"
    );
    std::fs::write(&path, &original).unwrap();

    // The boot composition main.rs runs, with the CLI override applied
    // after the parse.
    let mut config: ModelConfig =
        json5::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config.apply_cli_model(Some("cli-model"));
    let provider = boot_provider(&config);
    let store = ModelStore::new(config, provider);

    assert_eq!(
        store.tier_model("main"),
        "cli-model",
        "the CLI name outranks the file pointer"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "the config file is byte-identical: the override is session-only"
    );
}
