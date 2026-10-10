//! Approval wiring: the live gate (policy + handler) and the mode switch.
//!
//! Approval in agent-base is **two layers, both required**:
//! - [`ToolPolicy`] (the *gate*): decides whether a call needs approval at all
//!   and assigns its `risk_level`. Returning `None` auto-approves; returning
//!   `Some(ApprovalRequest)` defers to the handler.
//! - [`ApprovalHandler`] (the *decision*): `AllowOnce` / `AllowAlways` / `Deny`.
//!
//! [`LiveApprovalGate`] is phimint's single implementation of both layers,
//! sharing one [`ApprovalModeSwitch`] so Shift+Tab can flip behaviour mid-
//! session with no rewiring:
//!
//! | mode | policy | handler |
//! |------|--------|---------|
//! | `auto` | everything `None` (auto-approved) | never consulted |
//! | `ask` | writes / risky shell → request | enqueue for the TUI popup |
//! | `deny` | writes / risky shell → request | `Deny` |
//!
//! `--approval` only sets the switch's **initial** value (deny is a CLI
//! startup mode, not a Shift+Tab stop). Reads always pass: `ask` gates
//! mutations, not `ls`.
//!
//! History: phimint previously wired *only* a handler (`Auto`/`DenyAll`), so
//! `--approval deny` was a silent no-op — with no policy, `process_approval`
//! short-circuits and never consults the handler. The policy is what makes
//! deny real, which is why the live gate always carries one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use phi_agent::{
    AgentResult, ApprovalDecision, ApprovalHandler, ApprovalRequest, QueuedApprovalHandler,
    RiskLevel, ToolPolicy,
};
use serde_json::Value;
use tokio::sync::mpsc;

// The pending-request type lives in the framework (phi_agent::cli::approval,
// sunk down in the distillation plan); re-exported here because phimint's
// approval module stays the product's approval façade (ui/ consumes it).
pub use phi_agent::ApprovalItem;

// ── Command risk classification ─────────────────────────────────────────────

/// Read-only / build commands that are safe to auto-approve.
///
/// `cargo check/build/test/run/…` is the agent's build loop — it must stay
/// frictionless or the coding loop stalls on every compile. Destructive
/// patterns are matched *first*, so `cargo install` (see below) still prompts.
const SAFE_PREFIXES: &[&str] = &[
    "cargo check",
    "cargo build",
    "cargo test",
    "cargo run",
    "cargo clippy",
    "cargo fmt",
    "cargo doc",
    "cargo bench",
    "cargo metadata",
    "cargo tree",
    "git status",
    "git diff",
    "git log",
    "git show",
    "ls ",
    "ls",
    "cat ",
    "cd ",
    "rg ",
    "grep ",
    "find ",
    "pwd",
    "echo ",
    "head ",
    "tail ",
    "wc ",
    "which ",
    "tree ",
    "sed -n ",
];

/// Destructive / out-of-workspace mutations — always prompt with a red badge.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm ",
    "rm -",
    "sudo",
    "mkfs",
    "fdisk",
    "dd ",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "chmod",
    "chown",
    "kill -9",
    "pkill -9",
    "git reset",
    "git clean",
    "git push --force",
    "cargo publish",
    "cargo install",
    "pip install",
    "pip3 install",
    "npm install",
    "> /dev/sd",
];

/// Classify a shell command into a risk level.
///
/// Order matters: destructive patterns are matched *before* safe prefixes, so
/// e.g. `cd / && rm -rf *` is Destructive, not Safe-by-`cd`.
pub fn classify_command(command: &str) -> RiskLevel {
    let cmd = command.trim();
    if cmd.is_empty() {
        return RiskLevel::Safe;
    }
    let lower = cmd.to_ascii_lowercase();

    if DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return RiskLevel::Destructive;
    }
    if SAFE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return RiskLevel::Safe;
    }
    RiskLevel::Sensitive
}

/// A one-line description of a file mutation for the approval prompt.
fn describe_write(tool_name: &str, args: &Value) -> String {
    let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
    match tool_name {
        "write_file" => format!("Write file: {path}"),
        "edit_file" => format!("Edit file: {path}"),
        other => other.to_string(),
    }
}

/// Truncate a command for the approval title (keep it scannable).
fn short_title(command: &str) -> String {
    let trimmed = command.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 60 {
        trimmed.to_string()
    } else {
        chars.into_iter().take(57).collect::<String>() + "..."
    }
}

/// A scoped `action_key` for a shell command: the full command, whitespace-
/// normalised.
///
/// `action_key` is what `AllowAlways` caches (`tool_engine.rs` caches the key
/// verbatim and later skips approval on an exact match). Scoping it to the
/// concrete command — rather than the tool name — means "allow always" grants a
/// *narrow* standing approval (this exact command), not carte blanche for every
/// `execute_command`. Mirrors codex's `prefix_rule` intent: authorise the
/// concrete action, not the whole tool. Commands that `classify_command` deems
/// Safe never reach here, so only Sensitive/Destructive commands get scoped.
fn command_action_key(command: &str) -> String {
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("execute_command:{normalized}")
}

/// The ask-gate: auto-approve reads, prompt on writes and risky shell.
///
/// Pure decision function shared by the live gate's Ask/Deny modes — reads and
/// safe shell return `None` (never reach the handler), mutations return the
/// request the handler decides on.
fn classify_request(tool_name: &str, args: &Value) -> Option<ApprovalRequest> {
    match tool_name {
        // Read-only context tools — always auto-approved.
        "read_file" | "list_files" | "search_content" | "repo_map" => None,

        // File mutations — prompt (Sensitive). `action_key` is scoped to the
        // path so `AllowAlways` grants a narrow standing approval (this file
        // only), not every write_file/edit_file.
        "write_file" | "edit_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            Some(ApprovalRequest {
                title: tool_name.to_string(),
                message: describe_write(tool_name, args),
                action_key: Some(format!("{tool_name}:{path}")),
                risk_level: RiskLevel::Sensitive,
                raw: Some(args.clone()),
                source: None,
            })
        }

        // Shell — classify the command; Safe commands auto-approve.
        "execute_command" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            match classify_command(command) {
                RiskLevel::Safe => None,
                level => Some(ApprovalRequest {
                    title: format!("execute_command: {}", short_title(command)),
                    message: command.to_string(),
                    action_key: Some(command_action_key(command)),
                    risk_level: level,
                    raw: Some(args.clone()),
                    source: None,
                }),
            }
        }

        _ => None,
    }
}

// ── The live mode switch (Shift+Tab) ────────────────────────────────────────

/// Runtime approval mode — what the next tool call does.
///
/// The UI (Shift+Tab) flips it through [`ApprovalModeSwitch`]; the gate reads
/// it on every tool call, so a flip takes effect immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RuntimeApprovalMode {
    /// Every tool call auto-approves (the policy short-circuits to `None`).
    Auto = 0,
    /// Writes and risky shell prompt; reads pass. The interactive default.
    Ask = 1,
    /// Writes and risky shell are rejected. CLI startup mode only — Shift+Tab
    /// leaves it (first press enters `ask`), it is not a cycle stop.
    Deny = 2,
}

impl RuntimeApprovalMode {
    /// Parse a CLI `--approval` value. Unknown values fall back to `auto`
    /// (matches the historical `build_approval` fallback).
    pub fn parse(s: &str) -> Self {
        match s {
            "ask" => Self::Ask,
            "deny" => Self::Deny,
            _ => Self::Auto,
        }
    }

    /// The status-bar badge for this mode (ASCII — chrome stays width-safe).
    pub fn badge(self) -> &'static str {
        match self {
            Self::Auto => "[auto]",
            Self::Ask => "[ask]",
            Self::Deny => "[deny]",
        }
    }
}

/// Shared live switch between the TUI (Shift+Tab) and the approval gate.
///
/// One `Arc` handed to both sides: the UI flips it, the gate reads it per
/// tool call. No locks in the hot path (`AtomicU8`), no channel — the mode is
/// a single byte of shared state.
#[derive(Debug)]
pub struct ApprovalModeSwitch {
    mode: AtomicU8,
}

impl ApprovalModeSwitch {
    pub fn new(initial: RuntimeApprovalMode) -> Self {
        Self {
            mode: AtomicU8::new(initial as u8),
        }
    }

    pub fn get(&self) -> RuntimeApprovalMode {
        match self.mode.load(Ordering::SeqCst) {
            1 => RuntimeApprovalMode::Ask,
            2 => RuntimeApprovalMode::Deny,
            _ => RuntimeApprovalMode::Auto,
        }
    }

    pub fn set(&self, mode: RuntimeApprovalMode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    /// Shift+Tab cycle: `auto` ⇄ `ask`. `deny` is a CLI startup mode, not a
    /// cycle stop — the first press leaves it for `ask`.
    pub fn cycle_auto_ask(&self) -> RuntimeApprovalMode {
        let next = match self.get() {
            RuntimeApprovalMode::Auto => RuntimeApprovalMode::Ask,
            RuntimeApprovalMode::Ask => RuntimeApprovalMode::Auto,
            RuntimeApprovalMode::Deny => RuntimeApprovalMode::Ask,
        };
        self.set(next);
        next
    }
}

// ── The live gate: policy + handler behind one switch ───────────────────────

/// phimint's single approval wiring: one object implementing both layers.
///
/// Both layers consult the same [`ApprovalModeSwitch`], so Shift+Tab changes
/// the next tool call with no rewiring and no queue rebuild. The queue is
/// created once at startup and lives for the whole TUI lifetime — in `auto`
/// mode nothing is ever pushed to it, in `ask` mode every prompt goes through
/// it. Sub-agents inherit this gate via the parent-policy delegation chain
/// (see `agent.rs`), so they follow the live mode too.
pub struct LiveApprovalGate {
    mode: Arc<ApprovalModeSwitch>,
    queued: QueuedApprovalHandler,
}

impl LiveApprovalGate {
    pub fn new(
        mode: Arc<ApprovalModeSwitch>,
        queue_tx: mpsc::UnboundedSender<ApprovalItem>,
    ) -> Self {
        Self {
            mode,
            queued: QueuedApprovalHandler::new(queue_tx),
        }
    }

    /// The switch handle, for the UI (Shift+Tab) and the status-bar badge.
    pub fn mode_switch(&self) -> Arc<ApprovalModeSwitch> {
        Arc::clone(&self.mode)
    }
}

#[async_trait]
impl ToolPolicy for LiveApprovalGate {
    async fn evaluate_approval(&self, tool_name: &str, args: &Value) -> Option<ApprovalRequest> {
        match self.mode.get() {
            // auto: nothing needs approval — the handler is never consulted.
            RuntimeApprovalMode::Auto => None,
            // ask/deny share the gate; the handler decides allow vs deny.
            RuntimeApprovalMode::Ask | RuntimeApprovalMode::Deny => {
                classify_request(tool_name, args)
            }
        }
    }
}

#[async_trait]
impl ApprovalHandler for LiveApprovalGate {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        match self.mode.get() {
            // Defensive: the policy auto-passes in auto mode. If a request
            // still arrives (mode flipped between evaluate and approve), allow
            // it — auto means "don't block the run".
            RuntimeApprovalMode::Auto => Ok(ApprovalDecision::AllowOnce),
            RuntimeApprovalMode::Deny => Ok(ApprovalDecision::Deny),
            RuntimeApprovalMode::Ask => self.queued.approve(request, cancel_token).await,
        }
    }
}

// ── Wiring ──────────────────────────────────────────────────────────────────

/// `build_live_approval` output: handler, policy, the TUI-side queue receiver,
/// and the shared mode switch (for Shift+Tab + the status-bar badge).
pub type LiveApprovalBuild = (
    Arc<dyn ApprovalHandler>,
    Option<Arc<dyn ToolPolicy>>,
    mpsc::UnboundedReceiver<ApprovalItem>,
    Arc<ApprovalModeSwitch>,
);

/// Build the live approval gate for a CLI `--approval` value.
///
/// The gate (policy + handler + queue) is built for **every** mode — `auto`
/// just runs with the switch turned to Auto, where the policy auto-passes
/// everything. Only the switch's initial value differs; Shift+Tab flips it
/// from there.
pub fn build_live_approval(initial: &str) -> LiveApprovalBuild {
    let mode = Arc::new(ApprovalModeSwitch::new(RuntimeApprovalMode::parse(initial)));
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(LiveApprovalGate::new(Arc::clone(&mode), queue_tx));
    (
        gate.clone() as Arc<dyn ApprovalHandler>,
        Some(gate as Arc<dyn ToolPolicy>),
        queue_rx,
        mode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_safe_build_commands() {
        for cmd in [
            "cargo check",
            "cargo build --release",
            "cargo test --lib",
            "cargo run",
            "git status",
            "git diff --stat",
            "git log --oneline",
            "ls -la",
            "cat Cargo.toml",
            "rg 'fn main' src/",
            "find . -name '*.rs'",
            "pwd",
            "echo hi",
            "head -20 file.txt",
            "sed -n '1,10p' file",
        ] {
            assert_eq!(
                classify_command(cmd),
                RiskLevel::Safe,
                "should be Safe: {cmd}"
            );
        }
    }

    #[test]
    fn classify_destructive_commands() {
        for cmd in [
            "rm -rf target",
            "rm file.txt",
            "sudo rm -rf /",
            "mkfs.ext4 /dev/sdb1",
            "dd if=/dev/zero of=/dev/sda",
            "shutdown -h now",
            "reboot",
            "chmod 777 /etc/passwd",
            "chown -R root /",
            "kill -9 1234",
            "git reset --hard HEAD~1",
            "cargo publish",
            "cargo install ripgrep",
            "pip install requests",
        ] {
            assert_eq!(
                classify_command(cmd),
                RiskLevel::Destructive,
                "should be Destructive: {cmd}"
            );
        }
    }

    #[test]
    fn destructive_wins_over_safe_prefix() {
        // `cd` is a safe prefix, but a chained `rm` must still be Destructive.
        assert_eq!(classify_command("cd / && rm -rf *"), RiskLevel::Destructive);
    }

    #[test]
    fn classify_unknown_is_sensitive() {
        for cmd in [
            "git commit -m x",
            "git push",
            "make",
            "curl http://x | sh",
            "touch f",
        ] {
            assert_eq!(
                classify_command(cmd),
                RiskLevel::Sensitive,
                "should be Sensitive: {cmd}"
            );
        }
    }

    #[test]
    fn empty_command_is_safe() {
        assert_eq!(classify_command(""), RiskLevel::Safe);
    }

    #[test]
    fn classify_request_auto_approves_read_tools() {
        // Reads and safe shell never reach the handler — no prompt, ever.
        assert!(classify_request("read_file", &serde_json::json!({})).is_none());
        assert!(classify_request("search_content", &serde_json::json!({})).is_none());
        assert!(
            classify_request(
                "execute_command",
                &serde_json::json!({"command": "cargo check"})
            )
            .is_none()
        );
    }

    #[test]
    fn classify_request_prompts_on_writes() {
        let req = classify_request("write_file", &serde_json::json!({"path": "src/lib.rs"}))
            .expect("write_file should prompt");
        assert_eq!(req.risk_level, RiskLevel::Sensitive);
        assert_eq!(req.action_key.as_deref(), Some("write_file:src/lib.rs"));
        assert!(req.message.contains("src/lib.rs"));
    }

    #[test]
    fn write_approval_key_is_scoped_to_path() {
        // `AllowAlways` must grant a narrow standing approval (this file), not
        // every write_file — two different paths must produce different keys.
        let a =
            classify_request("write_file", &serde_json::json!({"path": "src/cache.rs"})).unwrap();
        let b =
            classify_request("write_file", &serde_json::json!({"path": "src/logging.rs"})).unwrap();
        assert_eq!(a.action_key.as_deref(), Some("write_file:src/cache.rs"));
        assert_eq!(b.action_key.as_deref(), Some("write_file:src/logging.rs"));
        assert_ne!(a.action_key, b.action_key);
    }

    #[test]
    fn command_approval_key_is_scoped_to_command() {
        let req = classify_request(
            "execute_command",
            &serde_json::json!({"command": "rm -rf /tmp/x"}),
        )
        .expect("rm should prompt");
        assert_eq!(req.risk_level, RiskLevel::Destructive);
        assert_eq!(
            req.action_key.as_deref(),
            Some("execute_command:rm -rf /tmp/x")
        );
        // Whitespace is normalised so `touch X` and `touch   X` share a key.
        let a = classify_request(
            "execute_command",
            &serde_json::json!({"command": "touch  X"}),
        )
        .unwrap();
        let b = classify_request(
            "execute_command",
            &serde_json::json!({"command": "touch X"}),
        )
        .unwrap();
        assert_eq!(a.action_key, b.action_key);
    }

    #[test]
    fn classify_request_classifies_shell() {
        // Destructive command → Destructive risk; unknown → Sensitive.
        let req = classify_request(
            "execute_command",
            &serde_json::json!({"command": "rm -rf /tmp/x"}),
        )
        .expect("rm should prompt");
        assert_eq!(req.risk_level, RiskLevel::Destructive);
        let req = classify_request("execute_command", &serde_json::json!({"command": "make"}))
            .expect("make should prompt");
        assert_eq!(req.risk_level, RiskLevel::Sensitive);
    }

    #[test]
    fn mode_switch_cycle_is_auto_ask_with_deny_exit() {
        // Shift+Tab: auto ⇄ ask; deny (CLI startup mode) leaves for ask.
        let sw = ApprovalModeSwitch::new(RuntimeApprovalMode::Auto);
        assert_eq!(sw.cycle_auto_ask(), RuntimeApprovalMode::Ask);
        assert_eq!(sw.cycle_auto_ask(), RuntimeApprovalMode::Auto);

        let sw = ApprovalModeSwitch::new(RuntimeApprovalMode::Deny);
        assert_eq!(sw.cycle_auto_ask(), RuntimeApprovalMode::Ask);
        assert_eq!(sw.cycle_auto_ask(), RuntimeApprovalMode::Auto);
    }

    #[test]
    fn parse_falls_back_to_auto() {
        assert_eq!(RuntimeApprovalMode::parse("ask"), RuntimeApprovalMode::Ask);
        assert_eq!(
            RuntimeApprovalMode::parse("deny"),
            RuntimeApprovalMode::Deny
        );
        assert_eq!(
            RuntimeApprovalMode::parse("auto"),
            RuntimeApprovalMode::Auto
        );
        assert_eq!(
            RuntimeApprovalMode::parse("bogus"),
            RuntimeApprovalMode::Auto
        );
    }

    #[test]
    fn build_live_approval_wires_every_mode() {
        // The gate is always built (policy present) — the switch's initial
        // value is the only per-mode difference. This is what makes Shift+Tab
        // able to leave any mode, including deny.
        for (arg, want) in [
            ("auto", RuntimeApprovalMode::Auto),
            ("ask", RuntimeApprovalMode::Ask),
            ("deny", RuntimeApprovalMode::Deny),
            ("bogus", RuntimeApprovalMode::Auto),
        ] {
            let (_handler, policy, _rx, switch) = build_live_approval(arg);
            assert!(policy.is_some(), "{arg}: the live gate must carry a policy");
            assert_eq!(switch.get(), want, "{arg}: initial mode");
        }
    }

    #[tokio::test]
    async fn auto_mode_policy_passes_everything() {
        let (_handler, policy, _rx, switch) = build_live_approval("auto");
        let policy = policy.expect("policy present");
        assert!(
            policy
                .evaluate_approval("write_file", &serde_json::json!({"path": "src/lib.rs"}))
                .await
                .is_none(),
            "auto mode must never prompt — writes included"
        );
        assert!(
            policy
                .evaluate_approval(
                    "execute_command",
                    &serde_json::json!({"command": "rm -rf /tmp/x"})
                )
                .await
                .is_none(),
            "auto mode must never prompt — destructive shell included"
        );
        // Switching to ask (Shift+Tab) makes the very same call prompt.
        switch.set(RuntimeApprovalMode::Ask);
        assert!(
            policy
                .evaluate_approval("write_file", &serde_json::json!({"path": "src/lib.rs"}))
                .await
                .is_some(),
            "the flip must take effect on the next evaluate"
        );
    }

    #[tokio::test]
    async fn ask_mode_queues_prompt_and_roundtrips() {
        let (handler, _policy, mut queue_rx, switch) = build_live_approval("ask");
        assert_eq!(switch.get(), RuntimeApprovalMode::Ask);

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn({
            let handler = handler.clone();
            let cancel = cancel.clone();
            async move {
                handler
                    .approve(
                        ApprovalRequest {
                            title: "write_file".to_string(),
                            message: "m".to_string(),
                            action_key: None,
                            risk_level: RiskLevel::Sensitive,
                            raw: None,
                            source: None,
                        },
                        cancel,
                    )
                    .await
            }
        });

        let item = queue_rx.recv().await.expect("request should be queued");
        assert_eq!(item.request.title, "write_file");
        item.decision_tx
            .send(ApprovalDecision::Deny)
            .expect("UI should be able to answer");
        let decision = handle.await.expect("handler task").expect("approve");
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn deny_mode_denies_without_queueing() {
        let (handler, _policy, mut queue_rx, _switch) = build_live_approval("deny");
        let cancel = tokio_util::sync::CancellationToken::new();
        let decision = handler
            .approve(
                ApprovalRequest {
                    title: "write_file".to_string(),
                    message: "m".to_string(),
                    action_key: None,
                    risk_level: RiskLevel::Sensitive,
                    raw: None,
                    source: None,
                },
                cancel,
            )
            .await
            .expect("approve");
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(
            queue_rx.try_recv().is_err(),
            "deny must decide locally, never prompt"
        );
    }
}
