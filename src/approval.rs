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
//! # External tools (tool-socket P1): the ledger is above the mode
//!
//! Identity keys with a `mcp.` / `script.` / `plugin.` prefix are **never**
//! short-circuited by the mode switch. The standing tier lives in
//! `~/.phimint/approvals.json` (the *approval ledger*) and decides first
//! (contract: `agent-works/docs/tool-contract.md` §2):
//!
//! | tier \\ mode | auto | ask | deny |
//! |---|---|---|---|
//! | `ask each time` (default) | **prompt** | prompt | deny (brake) |
//! | `always allow` | allow | allow | deny (brake) |
//! | `deny` | deny | deny | deny |
//!
//! Three hard rules:
//! 1. **fail-closed** — `classify_request`'s fallback answers "confirm" for
//!    the three prefixes, and `auto` never auto-approves them;
//! 2. **single memory owner** — `AllowAlways` is recorded only in the ledger;
//!    the engine's `action_key` cache (builtin precise actions) gets the
//!    decision demoted to `AllowOnce` ([`engine_decision_for_external`]);
//! 3. **SC6 test criterion** — an `ask each time` external tool call prompts even
//!    under Shift+Tab=auto.
//!
//! History: phimint previously wired *only* a handler (`Auto`/`DenyAll`), so
//! `--approval deny` was a silent no-op — with no policy, `process_approval`
//! short-circuits and never consults the handler. The policy is what makes
//! deny real, which is why the live gate always carries one.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use phi_agent::approval_ledger::{
    ApprovalLedger, ApprovalMode as LedgerMode, ApprovalTier, ExternalGate,
    engine_decision_for_external, gate_decision, is_external_identity_key,
};
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

/// Description of an external (three-prefix) tool call for the approval prompt.
///
/// The confirmation dialog must show the full picture (name + args) — a bare
/// name is not an approval surface (contract: tool-contract.md §2).
fn describe_external(tool_name: &str, args: &Value) -> String {
    format!(
        "External tool `{tool_name}` requests a call:\n{}",
        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
    )
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

        _ if is_external_identity_key(tool_name) => {
            // Three prefixes (`mcp.` / `script.` / `plugin.`): fail-closed —
            // the fallback must answer "confirm", never auto-approve. The
            // live gate consults the ledger *before* this classifier, so this
            // arm is the last line of defence (contract hard rule #1).
            Some(ApprovalRequest {
                title: tool_name.to_string(),
                message: describe_external(tool_name, args),
                // Carrier: the identity key. `action_key` is what the handler
                // matches on and what the ledger remembers; it must NOT leak
                // into the engine's `AllowAlways` cache (demoted in approve()).
                action_key: Some(tool_name.to_string()),
                risk_level: RiskLevel::Sensitive,
                raw: Some(args.clone()),
                source: None,
            })
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
    /// Claude Code style wording: `<mode> mode on`.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Auto => "auto mode on",
            Self::Ask => "ask mode on",
            Self::Deny => "deny mode on",
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

/// The approval ledger behind the gate: standing tiers for external tools.
///
/// Memory owner for `always allow` (contract hard rule #2) — the engine's
/// `action_key` cache never sees `AllowAlways` for external keys. Storage is
/// injected: `~/.phimint/approvals.json` is the product's choice (`main.rs`);
/// `None` keeps it in-memory (tests).
struct LedgerStore {
    path: Option<PathBuf>,
    ledger: Mutex<ApprovalLedger>,
}

impl LedgerStore {
    /// Load from the injected path. Missing file = fresh ledger. A corrupt
    /// file is moved aside (`approvals.json.corrupt`) and a fresh ledger
    /// starts — never silently overwrite a user's standing approvals, and
    /// never refuse to boot over a bad JSON either.
    fn load(path: Option<PathBuf>) -> Self {
        let ledger = match &path {
            Some(p) => match ApprovalLedger::load(p) {
                Ok(ledger) => ledger,
                Err(e) => {
                    let aside = p.with_extension("json.corrupt");
                    eprintln!(
                        "warning: approvals ledger {} is unreadable ({e}); moving it to {}",
                        p.display(),
                        aside.display()
                    );
                    let _ = std::fs::rename(p, &aside);
                    ApprovalLedger::new()
                }
            },
            None => ApprovalLedger::new(),
        };
        Self {
            path,
            ledger: Mutex::new(ledger),
        }
    }

    fn tier_of(&self, identity_key: &str) -> ApprovalTier {
        self.ledger
            .lock()
            .map(|l| l.tier_of(identity_key))
            .unwrap_or(ApprovalTier::Ask) // lock poisoned → fail-closed direction
    }

    /// Three-button record: only `[always allow]` changes the tier, then persists.
    fn record(&self, identity_key: &str, decision: &ApprovalDecision) {
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        if ledger.record_decision(identity_key, decision) {
            self.persist(&ledger);
        }
    }

    /// Direct tier set (`/tools` management surface), persisted.
    fn set_tier(&self, identity_key: &str, tier: ApprovalTier) {
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        ledger.set_tier(identity_key, tier);
        self.persist(&ledger);
    }

    /// Drop one entry (`/tools remove <key>`: no orphan memories), persisted.
    fn remove(&self, identity_key: &str) {
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        if ledger.remove(identity_key) {
            self.persist(&ledger);
        }
    }

    /// Drop a prefix (`plugin remove`: `plugin.{name}.*`), persisted.
    #[allow(dead_code)] // wired by P3 `plugin remove`
    fn remove_prefix(&self, prefix: &str) {
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        if ledger.remove_prefix(prefix) > 0 {
            self.persist(&ledger);
        }
    }

    fn persist(&self, ledger: &ApprovalLedger) {
        let Some(path) = &self.path else { return };
        if let Err(e) = ledger.save(path) {
            eprintln!(
                "warning: cannot save approvals ledger {}: {e}",
                path.display()
            );
        }
    }
}

/// The identity key of an external-tool request, if this is one.
///
/// `action_key` carries the identity key for external tools (see
/// `classify_request`); the title is a fallback. Builtins use scoped keys
/// (`write_file:{path}`, `execute_command:{cmd}`) which can never match a
/// three-prefix key.
fn external_identity_of(request: &ApprovalRequest) -> Option<String> {
    for candidate in request
        .action_key
        .as_deref()
        .into_iter()
        .chain([&*request.title])
    {
        if is_external_identity_key(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// phimint's single approval wiring: one object implementing both layers.
///
/// Both layers consult the same [`ApprovalModeSwitch`], so Shift+Tab changes
/// the next tool call with no rewiring and no queue rebuild. The queue is
/// created once at startup and lives for the whole TUI lifetime — in `auto`
/// mode nothing is ever pushed to it (except external tools, which prompt
/// anyway), in `ask` mode every prompt goes through it. Sub-agents inherit
/// this gate via the parent-policy delegation chain (see `agent.rs`), so they
/// follow the live mode too.
pub struct LiveApprovalGate {
    mode: Arc<ApprovalModeSwitch>,
    queued: QueuedApprovalHandler,
    ledger: LedgerStore,
}

impl LiveApprovalGate {
    pub fn new(
        mode: Arc<ApprovalModeSwitch>,
        queue_tx: mpsc::UnboundedSender<ApprovalItem>,
        ledger_path: Option<PathBuf>,
    ) -> Self {
        Self {
            mode,
            queued: QueuedApprovalHandler::new(queue_tx),
            ledger: LedgerStore::load(ledger_path),
        }
    }

    /// The switch handle, for the UI (Shift+Tab) and the status-bar badge.
    pub fn mode_switch(&self) -> Arc<ApprovalModeSwitch> {
        Arc::clone(&self.mode)
    }

    fn ledger_mode(&self) -> LedgerMode {
        match self.mode.get() {
            RuntimeApprovalMode::Auto => LedgerMode::Auto,
            RuntimeApprovalMode::Ask => LedgerMode::Ask,
            RuntimeApprovalMode::Deny => LedgerMode::Deny,
        }
    }

    /// Tier of an external identity key (for `/tools` and tests).
    pub fn tier_of(&self, identity_key: &str) -> ApprovalTier {
        self.ledger.tier_of(identity_key)
    }

    /// Set a standing tier (`/tools` management: ask each time / always allow / deny).
    pub fn set_tier(&self, identity_key: &str, tier: ApprovalTier) {
        self.ledger.set_tier(identity_key, tier);
    }

    /// `/tools remove <key>`: drop the tool's ledger entry (no orphans).
    pub fn remove_ledger_entry(&self, identity_key: &str) {
        self.ledger.remove(identity_key);
    }

    /// `plugin remove`: drop every `plugin.{name}.*` entry.
    pub fn remove_ledger_prefix(&self, prefix: &str) {
        self.ledger.remove_prefix(prefix);
    }

    /// Human review before install (plugin install / agent-written tools):
    /// **always pops the full picture**, regardless of the approval tier and
    /// the mode switch, and writes no ledger entry (an install is not a tool
    /// call; there is no reusable tier).
    /// It goes through the very same TUI queue as tool approval (the same
    /// popup, y/n).
    ///
    /// Security baseline: any tool/plugin definition written by an agent is
    /// never executed and never "auto-allowed" before a human confirms the
    /// entry's full picture — so the mode is ignored here; `auto` pops too.
    pub async fn confirm_always(&self, request: ApprovalRequest) -> ApprovalDecision {
        let cancel = tokio_util::sync::CancellationToken::new();
        // Queue disconnected (UI exited) → Cancelled → always recorded as a
        // decline (fail-closed).
        self.queued
            .approve(request, cancel)
            .await
            .unwrap_or(ApprovalDecision::Deny)
    }
}

#[async_trait]
impl ToolPolicy for LiveApprovalGate {
    async fn evaluate_approval(&self, tool_name: &str, args: &Value) -> Option<ApprovalRequest> {
        // External tools: the ledger decides *before* the mode switch (SC6 —
        // `ask each time` prompts even in auto). deny is the global brake.
        if is_external_identity_key(tool_name) {
            return match gate_decision(self.ledger.tier_of(tool_name), self.ledger_mode()) {
                ExternalGate::Allow => None,
                ExternalGate::Prompt | ExternalGate::Deny => Some(ApprovalRequest {
                    title: tool_name.to_string(),
                    message: describe_external(tool_name, args),
                    action_key: Some(tool_name.to_string()),
                    risk_level: RiskLevel::Sensitive,
                    raw: Some(args.clone()),
                    source: None,
                }),
            };
        }
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
        // External tools: three-button prompt (or ledger tier), even in auto.
        if let Some(key) = external_identity_of(&request) {
            return match gate_decision(self.ledger.tier_of(&key), self.ledger_mode()) {
                ExternalGate::Deny => Ok(ApprovalDecision::Deny), // tier=deny or brake
                ExternalGate::Allow => Ok(ApprovalDecision::AllowOnce), // tier=always allow
                ExternalGate::Prompt => {
                    let decision = self.queued.approve(request, cancel_token).await?;
                    self.ledger.record(&key, &decision);
                    // Single memory owner: `AllowAlways` is the ledger's; the
                    // engine's action_key cache must not double-write it.
                    Ok(engine_decision_for_external(decision))
                }
            };
        }
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

/// `build_live_approval` output: the gate, the TUI-side queue receiver, and the
/// shared mode switch (for Shift+Tab + the status-bar badge).
///
/// The handler slot is the concrete [`LiveApprovalGate`] (not `dyn`) so callers
/// keep the ledger-management surface (`tier_of` / `set_tier` / `remove_*` for
/// `/tools` and `plugin remove`); it coerces to `Arc<dyn ApprovalHandler>` at
/// `agent::build`.
pub type LiveApprovalBuild = (
    Arc<LiveApprovalGate>,
    Option<Arc<dyn ToolPolicy>>,
    mpsc::UnboundedReceiver<ApprovalItem>,
    Arc<ApprovalModeSwitch>,
);

/// Build the live approval gate for a CLI `--approval` value.
///
/// The gate (policy + handler + queue) is built for **every** mode — `auto`
/// just runs with the switch turned to Auto, where the policy auto-passes
/// everything builtin. Only the switch's initial value differs; Shift+Tab
/// flips it from there. External tools are ledger-gated in every mode.
///
/// `ledger_path` is injected (`~/.phimint/approvals.json` from `main.rs`).
/// `None` keeps the ledger in-memory — tests only.
pub fn build_live_approval(initial: &str, ledger_path: Option<PathBuf>) -> LiveApprovalBuild {
    let mode = Arc::new(ApprovalModeSwitch::new(RuntimeApprovalMode::parse(initial)));
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(LiveApprovalGate::new(
        Arc::clone(&mode),
        queue_tx,
        ledger_path,
    ));
    (
        Arc::clone(&gate),
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
            let (_handler, policy, _rx, switch) = build_live_approval(arg, None);
            assert!(policy.is_some(), "{arg}: the live gate must carry a policy");
            assert_eq!(switch.get(), want, "{arg}: initial mode");
        }
    }

    #[tokio::test]
    async fn auto_mode_policy_passes_everything() {
        let (_handler, policy, _rx, switch) = build_live_approval("auto", None);
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
        let (handler, _policy, mut queue_rx, switch) = build_live_approval("ask", None);
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
        let (handler, _policy, mut queue_rx, _switch) = build_live_approval("deny", None);
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

    // ── External tools: the ledger is above the mode (tool-socket P1) ──────

    fn external_request(key: &str) -> ApprovalRequest {
        ApprovalRequest {
            title: key.to_string(),
            message: describe_external(key, &serde_json::json!({"q": "x"})),
            action_key: Some(key.to_string()),
            risk_level: RiskLevel::Sensitive,
            raw: Some(serde_json::json!({"q": "x"})),
            source: None,
        }
    }

    /// SC6 test criterion (contract §2 hard rule 3): an external tool call at
    /// the `ask each time` tier **still prompts** under Shift+Tab=auto.
    #[tokio::test]
    async fn sc6_ask_tier_external_prompts_even_in_auto_mode() {
        let (_handler, policy, _rx, switch) = build_live_approval("auto", None);
        let policy = policy.expect("policy present");
        for key in [
            "mcp.figma.get_file",
            "script.deploy",
            "plugin.my-tools.search",
        ] {
            let req = policy
                .evaluate_approval(key, &serde_json::json!({"q": "x"}))
                .await;
            assert!(req.is_some(), "{key} must prompt in auto mode");
            let req = req.unwrap();
            assert_eq!(
                req.action_key.as_deref(),
                Some(key),
                "{key}: key is the carrier"
            );
        }
        // Builtin writes still auto-pass in auto mode (unchanged behaviour).
        assert!(
            policy
                .evaluate_approval("write_file", &serde_json::json!({"path": "a"}))
                .await
                .is_none()
        );
        assert_eq!(switch.get(), RuntimeApprovalMode::Auto);
    }

    /// Three-button tier migration + single memory owner: `[allow always]`
    /// only records to the ledger; the engine only ever sees `AllowOnce`
    /// (the action_key cache must not double-write).
    #[tokio::test]
    async fn always_allow_records_ledger_and_demotes_engine_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.json");
        let (handler, _policy, mut queue_rx, _switch) =
            build_live_approval("ask", Some(path.clone()));

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn({
            let handler = handler.clone();
            let cancel = cancel.clone();
            async move {
                handler
                    .approve(external_request("script.deploy"), cancel)
                    .await
            }
        });
        let item = queue_rx.recv().await.expect("external request must prompt");
        item.decision_tx
            .send(ApprovalDecision::AllowAlways)
            .expect("UI answers");
        let decision = handle.await.expect("task").expect("approve");
        assert_eq!(
            decision,
            ApprovalDecision::AllowOnce,
            "AllowAlways must not reach the engine cache"
        );

        // The ledger remembers "always allow" and persists it
        assert_eq!(handler.tier_of("script.deploy"), ApprovalTier::AlwaysAllow);
        let saved = ApprovalLedger::load(&path).expect("ledger persisted");
        assert_eq!(saved.tier_of("script.deploy"), ApprovalTier::AlwaysAllow);

        // Afterwards it also passes in auto mode (the ledger goes before the
        // mode, AlwaysAllow → Allow)
        let (auto_handler, auto_policy, _rx, _switch) =
            build_live_approval("auto", Some(path.clone()));
        assert!(
            auto_policy
                .expect("policy")
                .evaluate_approval("script.deploy", &serde_json::json!({}))
                .await
                .is_none(),
            "tier=always allow must pass in auto"
        );
        assert_eq!(
            auto_handler.tier_of("script.deploy"),
            ApprovalTier::AlwaysAllow,
            "tier survives a reload"
        );
    }

    /// `[allow once]` / `[deny]` do not change the tier; it must ask again
    /// next time.
    #[tokio::test]
    async fn once_and_deny_this_leave_tier_untouched() {
        let (handler, policy, mut queue_rx, _switch) = build_live_approval("auto", None);
        let policy = policy.expect("policy present");

        for decision_out in [ApprovalDecision::AllowOnce, ApprovalDecision::Deny] {
            let cancel = tokio_util::sync::CancellationToken::new();
            let handle = tokio::spawn({
                let handler = handler.clone();
                let cancel = cancel.clone();
                async move {
                    handler
                        .approve(external_request("mcp.notion.search"), cancel)
                        .await
                }
            });
            let item = queue_rx.recv().await.expect("must prompt even in auto");
            item.decision_tx
                .send(decision_out.clone())
                .expect("UI answers");
            let decision = handle.await.expect("task").expect("approve");
            assert_eq!(decision, decision_out, "decision passes through");
            assert_eq!(handler.tier_of("mcp.notion.search"), ApprovalTier::Ask);
            assert!(
                policy
                    .evaluate_approval("mcp.notion.search", &serde_json::json!({}))
                    .await
                    .is_some(),
                "tier unchanged → prompt again"
            );
        }
    }

    /// Shift+Tab=deny is the global brake; it overrides the ledger's
    /// "always allow".
    #[tokio::test]
    async fn deny_brake_overrides_standing_allow() {
        let (handler, policy, mut queue_rx, switch) = build_live_approval("ask", None);
        let policy = policy.expect("policy present");
        handler.set_tier("plugin.my-tools.search", ApprovalTier::AlwaysAllow);

        switch.set(RuntimeApprovalMode::Deny);
        assert!(
            policy
                .evaluate_approval("plugin.my-tools.search", &serde_json::json!({}))
                .await
                .is_some(),
            "brake: still surfaces a request"
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let decision = handler
            .approve(external_request("plugin.my-tools.search"), cancel)
            .await
            .expect("approve");
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(queue_rx.try_recv().is_err(), "brake decides locally");
    }

    /// Ledger tier `deny`: no popup, straight rejection (the /tools
    /// permanent-deny path).
    #[tokio::test]
    async fn ledger_tier_deny_rejects_without_prompt() {
        let (handler, policy, mut queue_rx, _switch) = build_live_approval("auto", None);
        let policy = policy.expect("policy present");
        handler.set_tier("script.deploy", ApprovalTier::Deny);

        assert!(
            policy
                .evaluate_approval("script.deploy", &serde_json::json!({}))
                .await
                .is_some()
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let decision = handler
            .approve(external_request("script.deploy"), cancel)
            .await
            .expect("approve");
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(queue_rx.try_recv().is_err());
    }

    /// `/tools remove` / `plugin remove` ledger cleanup (no orphaned memory).
    #[tokio::test]
    async fn ledger_cleanup_on_remove() {
        let (handler, _policy, _rx, _switch) = build_live_approval("auto", None);
        handler.set_tier("script.deploy", ApprovalTier::AlwaysAllow);
        handler.set_tier("plugin.my-tools.search", ApprovalTier::AlwaysAllow);
        handler.set_tier("plugin.my-tools.deploy", ApprovalTier::AlwaysAllow);
        handler.set_tier("plugin.other.search", ApprovalTier::AlwaysAllow);

        handler.remove_ledger_entry("script.deploy");
        assert_eq!(handler.tier_of("script.deploy"), ApprovalTier::Ask);

        handler.remove_ledger_prefix("plugin.my-tools.");
        assert_eq!(handler.tier_of("plugin.my-tools.search"), ApprovalTier::Ask);
        assert_eq!(handler.tier_of("plugin.my-tools.deploy"), ApprovalTier::Ask);
        assert_eq!(
            handler.tier_of("plugin.other.search"),
            ApprovalTier::AlwaysAllow,
            "prefix cleanup must not touch other plugins"
        );
    }

    /// Classification fallback is fail-closed: the three prefixes never fall
    /// into `_ => None` auto-pass.
    #[test]
    fn classify_request_fallback_is_fail_closed_for_external() {
        for key in [
            "mcp.figma.get_file",
            "script.deploy",
            "plugin.my-tools.search",
            // Malformed long-tail keys also land on the confirmation surface.
            "mcp.foo",
            "plugin.x",
        ] {
            let req = classify_request(key, &serde_json::json!({"a": 1}))
                .unwrap_or_else(|| panic!("{key} must classify as needing confirmation"));
            assert_eq!(req.action_key.as_deref(), Some(key));
            assert!(req.message.contains(key));
        }
    }

    /// Corrupt ledger file: move it to a .corrupt backup, never silently
    /// overwrite it, never refuse to boot.
    #[test]
    fn corrupt_ledger_is_quarantined_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approvals.json");
        std::fs::write(&path, "{broken").unwrap();
        let (handler, _policy, _rx, _switch) = build_live_approval("ask", Some(path.clone()));
        assert_eq!(handler.tier_of("script.deploy"), ApprovalTier::Ask);
        assert!(
            path.with_extension("json.corrupt").exists(),
            "the broken file must be kept aside"
        );
    }
}
