//! tool-socket P3 command-level integration tests (`add_tool` end-to-end + the
//! command face driving the socket board).
//!
//! These take the real product path: `AddTool::call` → `ExternalState::add_tool_from_agent` →
//! **the TUI queue behind [`LiveApprovalGate::confirm_always`]** (the test plays
//! the UI: it receives [`ApprovalItem`] and answers on `decision_tx`) → persist +
//! register. The assertions pin three product promises:
//!
//! 1. The confirm box shows the full picture (complete argv + absolute path +
//!    timeout) — nothing is persisted until the human nods, and the tool is
//!    callable immediately;
//! 2. Human denial = zero persist, zero registration; a literal secret is
//!    rejected **before it ever reaches the confirm box**;
//! 3. The command face (`/tools` panel, per-tool toggles, removal, usage hints)
//!    answers the semantics body with graceful copy.

use std::sync::{Arc, Mutex};

use phi_agent::{
    AgentError, AgentResult, ApprovalDecision, ApprovalRequest, Content, McpServerConfig, Tool,
    ToolContext, ToolMetadata, ToolRegistry,
};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use phimint::approval::build_live_approval;
use phimint::external::commands::{self, ExternalCommand};
use phimint::external::state::{Confirm, ExternalState, ToolHost};
use phimint::tools::add_tool::AddTool;

// ── Test fakes ────────────────────────────────────────────────────────────────

/// No-op tool: just occupies a name in the tool table (for the command-face
/// toggle tests).
struct NamedNoop(&'static str);

#[async_trait::async_trait]
impl Tool for NamedNoop {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "noop"
    }
    fn schema(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.0.to_string(),
            description: "noop".into(),
            origin: "test".into(),
            version: "0".into(),
            requirements: vec![],
        }
    }
    async fn call(&self, _args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        Ok(vec![])
    }
}

/// In-memory host: one tool table; MCP is bookkeeping only (command-face tests
/// touch no network).
struct FakeHost {
    registry: Arc<RwLock<ToolRegistry>>,
    attached: Mutex<Vec<String>>,
    detached: Mutex<Vec<String>>,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(ToolRegistry::default())),
            attached: Mutex::new(Vec::new()),
            detached: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ToolHost for FakeHost {
    fn registry(&self) -> Arc<RwLock<ToolRegistry>> {
        Arc::clone(&self.registry)
    }
    async fn attach_mcp(&self, config: McpServerConfig) -> Result<(), String> {
        self.attached.lock().unwrap().push(config.name.clone());
        Ok(())
    }
    async fn detach_mcp(&self, name: &str) {
        self.detached.lock().unwrap().push(name.to_string());
    }
}

/// Always allows (the command-face `/tools` path never prompts; the real prompt
/// goes through the queue, see the two add_tool tests).
struct NoConfirm;

#[async_trait::async_trait]
impl Confirm for NoConfirm {
    async fn confirm(&self, _request: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::AllowOnce
    }
}

fn text_of(out: Vec<Content>) -> String {
    out.into_iter()
        .map(|c| match c {
            Content::Text { text } => text,
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 1. add_tool end-to-end: full-picture confirm → persist + register ──────────────────────────────

/// Automated form of UX acceptance checklist item 3: the confirm box shows
/// **the complete argv + absolute path + timeout**; after the nod tools.json is
/// written and `script.deploy` is callable immediately.
#[tokio::test]
async fn add_tool_end_to_end_registers_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _policy, mut approval_rx, _mode) = build_live_approval("auto", None);
    let state = Arc::new(AsyncMutex::new(ExternalState::load(
        dir.path(),
        dir.path(),
        gate,
    )));
    let registry = Arc::new(RwLock::new(ToolRegistry::default()));
    let tool = AddTool::new(Arc::clone(&state), Arc::downgrade(&registry));

    // The test plays the UI: receive the approval item, record the copy, answer
    // allow. A recv timeout = the product never showed the box → fail loudly
    // instead of hanging the test (build-time-rejected entries must never get here).
    let seen: Arc<Mutex<Vec<ApprovalRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let ui = {
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            let item = tokio::time::timeout(std::time::Duration::from_secs(5), approval_rx.recv())
                .await
                .expect("timed out waiting for the confirm box")
                .expect("add_tool must ask the human");
            seen.lock().unwrap().push(item.request);
            item.decision_tx
                .send(ApprovalDecision::AllowOnce)
                .expect("add_tool is waiting on the decision");
        })
    };

    let args = serde_json::json!({
        "name": "deploy",
        "description": "ship the release",
        "run": ["/bin/echo", "shipping"],
        "timeout_secs": 120,
        "env": {"DEPLOY_TOKEN": "${ENV:HOME}"},
    });
    let out = text_of(tool.call(&args, &ToolContext::for_test()).await.unwrap());
    ui.await.unwrap();

    assert!(out.contains("✓ tool added as script.deploy"), "got: {out}");
    // Callable via the registry immediately.
    assert!(registry.read().await.get("script.deploy").is_some());
    // Persisted to disk.
    let tools_json = phimint::external::tools_path(dir.path());
    let on_disk = std::fs::read_to_string(&tools_json).expect("tools.json written");
    assert!(on_disk.contains("\"deploy\""), "got: {on_disk}");

    // Full-picture confirm: complete argv + absolute path + timeout (UX checklist
    // item 3).
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let review = &seen[0];
    assert!(
        review.title.contains("script.deploy"),
        "title: {}",
        review.title
    );
    assert!(
        review.message.contains("add script tool `deploy`"),
        "{}",
        review.message
    );
    assert!(
        review.message.contains("/bin/echo"),
        "argv missing: {}",
        review.message
    );
    assert!(
        review.message.contains("shipping"),
        "argv missing: {}",
        review.message
    );
    assert!(
        review.message.contains("timeout: 120s"),
        "timeout missing: {}",
        review.message
    );
    // env full picture = each entry quoted (not just the key name), and the quote
    // itself carries no literal secret.
    assert!(
        review.message.contains("DEPLOY_TOKEN = ${ENV:HOME}"),
        "{}",
        review.message
    );
    assert!(
        !review.message.contains("sk-"),
        "secret must never appear in the review"
    );
}

// ── 2. Human denial = zero persist, zero registration ────────────────────────────────────────────────

#[tokio::test]
async fn add_tool_denied_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _policy, mut approval_rx, _mode) = build_live_approval("auto", None);
    let state = Arc::new(AsyncMutex::new(ExternalState::load(
        dir.path(),
        dir.path(),
        gate,
    )));
    let registry = Arc::new(RwLock::new(ToolRegistry::default()));
    let tool = AddTool::new(Arc::clone(&state), Arc::downgrade(&registry));

    let ui = tokio::spawn(async move {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("timed out waiting for the confirm box")
            .expect("add_tool must ask the human");
        assert!(
            item.request.message.contains("argv"),
            "full facts before the decision"
        );
        item.decision_tx
            .send(ApprovalDecision::Deny)
            .expect("add_tool is waiting on the decision");
    });

    let args = serde_json::json!({
        "name": "deploy",
        "description": "ship the release",
        "run": ["/bin/echo", "shipping"],
    });
    let out = text_of(tool.call(&args, &ToolContext::for_test()).await.unwrap());
    ui.await.unwrap();

    assert!(
        out.contains("not added (declined — nothing written)"),
        "got: {out}"
    );
    assert!(registry.read().await.get("script.deploy").is_none());
    assert!(
        !phimint::external::tools_path(dir.path()).exists(),
        "deny must not touch tools.json"
    );
}

// ── 3. A literal secret is refused before the confirm box ──────────────────────────────────────

/// "Illegal entries never prompt": an entry refused by the secret rule must not
/// even open the confirm box (`try_recv` stays empty).
#[tokio::test]
async fn add_tool_literal_secret_refused_before_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _policy, mut approval_rx, _mode) = build_live_approval("auto", None);
    let state = Arc::new(AsyncMutex::new(ExternalState::load(
        dir.path(),
        dir.path(),
        gate,
    )));
    let registry = Arc::new(RwLock::new(ToolRegistry::default()));
    let tool = AddTool::new(Arc::clone(&state), Arc::downgrade(&registry));

    let args = serde_json::json!({
        "name": "leaky",
        "description": "should never mount",
        "run": ["/bin/echo", "hi"],
        "env": {"TOKEN": "sk-live-literal"},
    });
    let out = text_of(tool.call(&args, &ToolContext::for_test()).await.unwrap());

    assert!(out.contains("rejected"), "got: {out}");
    assert!(
        out.contains("${ENV:VAR}"),
        "the error must teach the fix: {out}"
    );
    assert!(
        approval_rx.try_recv().is_err(),
        "an illegal entry must never reach the confirm box"
    );
    assert!(registry.read().await.get("script.leaky").is_none());
    assert!(!phimint::external::tools_path(dir.path()).exists());
}

// ── 4. The command face drives the socket board ────────────────────────────────────────────────────

/// The whole seam `parse_command` → `ExternalState::handle`: panel, per-tool
/// toggles, removal, usage hints on a namespace miss — all answer with graceful
/// copy and never go to the model as a turn.
#[tokio::test]
async fn command_face_drives_socket_board() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _policy, _approval_rx, _mode) = build_live_approval("auto", None);
    let mut state = ExternalState::load(dir.path(), dir.path(), gate);
    let host = FakeHost::new();
    host.registry()
        .write()
        .await
        .register_arc(Arc::new(NamedNoop("read_file")));

    // /tools: source/namespace/tier/toggle all visible on one screen.
    let cmd = commands::parse_command("/tools").expect("parse /tools");
    assert_eq!(cmd, ExternalCommand::ToolsList);
    let panel = state.handle(&host, &NoConfirm, cmd).await;
    assert!(panel.contains("read_file"), "panel: {panel}");
    assert!(
        panel.contains("on "),
        "panel must show the toggle state: {panel}"
    );
    assert!(
        panel.contains("toggle: /tools on|off <key>"),
        "panel: {panel}"
    );

    // One-key per-tool off (takes effect immediately) → back on.
    let cmd = commands::parse_command("/tools off read_file").expect("parse off");
    let msg = state.handle(&host, &NoConfirm, cmd).await;
    assert!(msg.contains("✓ read_file off (immediate"), "got: {msg}");
    assert!(host.registry().write().await.get("read_file").is_none());

    let cmd = commands::parse_command("/tools on read_file").expect("parse on");
    let msg = state.handle(&host, &NoConfirm, cmd).await;
    assert!(msg.contains("✓ read_file on"), "got: {msg}");
    assert!(host.registry().write().await.get("read_file").is_some());

    // Graceful errors: remove/uninstall of a nonexistent name — all "✗ …" copy,
    // neither a crash nor silence.
    let cmd = commands::parse_command("/tools remove nope").expect("parse remove");
    let msg = state.handle(&host, &NoConfirm, cmd).await;
    assert!(msg.starts_with('✗'), "got: {msg}");

    let cmd = commands::parse_command("/plugin remove nope").expect("parse plugin remove");
    let msg = state.handle(&host, &NoConfirm, cmd).await;
    assert!(msg.starts_with('✗'), "got: {msg}");

    // Namespace miss (`/plugin frobnicate x`) → usage hint, not a turn.
    let usage = commands::usage_for("/plugin frobnicate x").expect("namespace miss → usage");
    assert!(usage.contains("/plugin add"), "usage: {usage}");
    assert!(commands::usage_for("just chatting").is_none());
}

// ── 5. Socket-board addendum: law rejections (relative paths) also never reach the confirm box ───────────

/// The path law fails closed: a relative `run[0]` is rejected before the confirm
/// box (the agent cannot sneak a cwd-relative script in behind "a human reviews
/// it first").
#[tokio::test]
async fn add_tool_relative_program_refused_before_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (gate, _policy, mut approval_rx, _mode) = build_live_approval("auto", None);
    let state = Arc::new(AsyncMutex::new(ExternalState::load(
        dir.path(),
        dir.path(),
        gate,
    )));
    let registry = Arc::new(RwLock::new(ToolRegistry::default()));
    let tool = AddTool::new(Arc::clone(&state), Arc::downgrade(&registry));

    // `ScriptToolEntry::new` already runs `check_laws` — the relative path is
    // bounced here with `ToolArgsInvalid`, never reaching ExternalState.
    let args = serde_json::json!({
        "name": "sneaky",
        "description": "relative program",
        "run": ["./deploy.sh"],
    });
    let err = tool
        .call(&args, &ToolContext::for_test())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AgentError::ToolArgsInvalid { .. }),
        "got: {err:?}"
    );
    assert!(approval_rx.try_recv().is_err());
    assert!(!phimint::external::tools_path(dir.path()).exists());
}
