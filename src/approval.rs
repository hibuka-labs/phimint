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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

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

/// What the browser action is about to touch, for the prompt and the grant key.
fn browser_target(args: &Value) -> String {
    args.get("selector")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            args.get("index")
                .and_then(Value::as_u64)
                .map(|i| format!("element #{i}"))
        })
        .unwrap_or_else(|| "(page)".to_string())
}

/// A one-line description of a browser action for the approval prompt.
fn describe_browser(tool_name: &str, args: &Value) -> String {
    let target = browser_target(args);
    match tool_name {
        "browser_click" => format!("Browser: click {target}"),
        "browser_input_fill" => {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            format!("Browser: type into {target}: {text:?}")
        }
        "browser_select" => {
            let value = args.get("value").and_then(Value::as_str).unwrap_or("?");
            format!("Browser: select {value:?} in {target}")
        }
        "browser_press_key" => {
            let key = args.get("key").and_then(Value::as_str).unwrap_or("?");
            format!("Browser: press key {key}")
        }
        "browser_evaluate" => {
            let script = args.get("script").and_then(Value::as_str).unwrap_or("");
            format!("Browser: run JavaScript:\n{}", short_title(script))
        }
        "browser_navigate" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("?");
            format!("Browser: open {url}")
        }
        other => other.to_string(),
    }
}

/// What the approval prompt shows for a code step: every cell's intent and a
/// head of its code — enough for a human to see what they are approving.
/// Only reachable when the `code-exec` feature ships the tool (M3 T10d ②).
#[cfg(feature = "code-exec")]
fn describe_code_cell(args: &Value) -> String {
    let Some(cells) = args.get("cells").and_then(Value::as_array) else {
        return "code_cell: (no cells)".to_string();
    };
    let mut out = format!("Run {} code cell(s):", cells.len());
    for (i, cell) in cells.iter().enumerate() {
        let intent = cell.get("intent").and_then(Value::as_str).unwrap_or("");
        let code = cell.get("code").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("\n{}. {intent}: {}", i + 1, short_title(code)));
    }
    out
}

/// Whether a URL points back at this machine.
///
/// The design canvas is served on `127.0.0.1`, so the agent looking at what it
/// just showed the user is not network access and must not cost an approval.
/// Everywhere else is — same rule `curl` already follows.
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url
        .trim()
        .strip_prefix("http://")
        .or_else(|| url.trim().strip_prefix("https://"))
    else {
        return false;
    };
    // Authority, then host: drop path/query, any userinfo, then the port.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host_port.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080` → `::1`
        Some(after) => after.split(']').next().unwrap_or(""),
        None => host_port.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Scope a browser action to the control it touches, the way `write_file` is
/// scoped to a path: "allow always" on click `#save` must not also allow
/// clicking everything else on the page.
fn browser_action_key(tool_name: &str, args: &Value) -> String {
    let target = args
        .get("selector")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            args.get("index")
                .and_then(Value::as_u64)
                .map(|i| format!("element #{i}"))
        })
        .or_else(|| {
            args.get("key")
                .and_then(Value::as_str)
                .map(|k| format!("key {k}"))
        })
        .unwrap_or_else(|| "(page)".to_string());
    format!("{tool_name}:{target}")
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
    format!("execute_command:{}", normalize_command(command))
}

/// The command half of a scoped standing-approval key: whitespace-normalised,
/// so `cargo  test` and `cargo test` cache the same grant.
fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `verify` tool's ask-gate arm.
///
/// The tool never takes its command from the model —
/// `code_intel::lang::resolve_verify_command` reads the workspace — and a pass
/// mints the green light the hand-back is held on. A command the project
/// supplies in its own `.phimint.toml` is repository-controlled text: it
/// prompts like the same command pasted into `execute_command`, with the
/// source surfaced and any standing grant scoped to that exact command. The
/// convention slot and the language table compose phimint's OWN fixed strings
/// (the repository selects which of phimint's checks runs, never the command
/// text), so those run unprompted like reads.
fn verify_approval_request(args: &Value, workspace_root: &Path) -> Option<ApprovalRequest> {
    let resolved = code_intel::lang::resolve_verify_command(workspace_root)?;
    if resolved.source != code_intel::lang::VerifyCommandSource::ProjectConfig {
        return None;
    }
    let command = resolved.command;
    Some(ApprovalRequest {
        title: format!("verify: {}", short_title(&command)),
        message: format!(
            "{command}\n\nThe project's own config ({}) supplied this verify \
             command; its pass marks the code verified. Approve it the way you \
             would the same command pasted into execute_command.",
            resolved.source.label()
        ),
        action_key: Some(format!("verify:{}", normalize_command(&command))),
        risk_level: RiskLevel::Sensitive,
        raw: Some(args.clone()),
        source: None,
    })
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

        // Browser automation (phi-tools' 22-tool suite). The read half only
        // looks at a page the agent is already driving — no prompt. The arms
        // below it prompt. The `browser_*` tail is fail-closed on purpose: a
        // tool added to the suite later asks until someone classifies it,
        // instead of landing in the auto-approve fallback.
        "browser_screenshot"
        | "browser_snapshot"
        | "browser_get_markdown"
        | "browser_extract_content"
        | "browser_read_links"
        | "browser_tab_list"
        | "browser_wait"
        | "browser_scroll"
        | "browser_hover"
        | "browser_go_back"
        | "browser_go_forward"
        | "browser_new_tab"
        | "browser_switch_tab"
        | "browser_close_tab"
        | "browser_close"
        | "browser_restart" => None,

        // Opening a page is network access. The canvas lives on loopback, so
        // looking at your own work is free; anything else is the same ask
        // `curl` already triggers.
        "browser_navigate" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("");
            if is_loopback_url(url) {
                None
            } else {
                Some(ApprovalRequest {
                    title: tool_name.to_string(),
                    message: describe_browser(tool_name, args),
                    action_key: Some(format!("browser_navigate:{url}")),
                    risk_level: RiskLevel::Sensitive,
                    raw: Some(args.clone()),
                    source: None,
                })
            }
        }

        // These change page state: clicking, typing, picking, keystrokes.
        "browser_click" | "browser_input_fill" | "browser_select" | "browser_press_key" => {
            Some(ApprovalRequest {
                title: tool_name.to_string(),
                message: describe_browser(tool_name, args),
                action_key: Some(browser_action_key(tool_name, args)),
                risk_level: RiskLevel::Sensitive,
                raw: Some(args.clone()),
                source: None,
            })
        }

        // Arbitrary JavaScript in a page. The payload is a fresh script every
        // call, so no standing grant exists that is narrower than "all JS" —
        // leave `action_key` unset and the engine re-asks each time instead of
        // caching "allow always" as a permanent license.
        "browser_evaluate" => Some(ApprovalRequest {
            title: tool_name.to_string(),
            message: describe_browser(tool_name, args),
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: Some(args.clone()),
            source: None,
        }),

        // Code cells (CodeAct main road): arbitrary code in a resident
        // interpreter — one call is one step of side effects. The code is
        // fresh every call, so no standing grant is narrower than "all code"
        // (same reasoning as `browser_evaluate`): `action_key` unset, re-ask
        // each time. Sensitive, not Destructive — what a step does is the
        // cells' business; the engine's policy/journal seam audits every
        // side effect underneath.
        #[cfg(feature = "code-exec")]
        "code_cell" => Some(ApprovalRequest {
            title: tool_name.to_string(),
            message: describe_code_cell(args),
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: Some(args.clone()),
            source: None,
        }),

        name if name.starts_with("browser_") => Some(ApprovalRequest {
            title: name.to_string(),
            message: describe_browser(name, args),
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: Some(args.clone()),
            source: None,
        }),

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

/// Runtime approval mode and its shared live switch: the mechanism face is
/// the shell's ([`phi_agent_tui::RuntimeApprovalMode`] /
/// [`phi_agent_tui::ApprovalModeSwitch`]); the grading tables, risk copy and
/// the ledger below are product policy.
pub use phi_agent_tui::{ApprovalModeSwitch, RuntimeApprovalMode};

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
            // Not `eprintln!`: this runs while the TUI is up, and a write to
            // stderr lands on the alternate screen (the renderer never
            // repaints those cells). The tracing sink owns the session log.
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "cannot save approvals ledger"
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
    /// The workspace root `verify` resolves its command against. The policy
    /// decides before the tool runs, so the gate needs the tool's root to tell
    /// a project-supplied command from phimint's own (see
    /// [`verify_approval_request`]).
    workspace_root: PathBuf,
}

impl LiveApprovalGate {
    pub fn new(
        mode: Arc<ApprovalModeSwitch>,
        queue_tx: mpsc::UnboundedSender<ApprovalItem>,
        ledger_path: Option<PathBuf>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            mode,
            queued: QueuedApprovalHandler::new(queue_tx),
            ledger: LedgerStore::load(ledger_path),
            workspace_root,
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
            RuntimeApprovalMode::Ask | RuntimeApprovalMode::Deny => match tool_name {
                // `verify` resolves its command from the workspace, not the
                // model; a project-supplied one prompts (its own arm).
                "verify" => verify_approval_request(args, &self.workspace_root),
                _ => classify_request(tool_name, args),
            },
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
/// `None` keeps the ledger in-memory — tests only. `workspace_root` is the
/// workspace `verify` resolves its command against: the gate decides before
/// the tool runs, so it must know the tool's root (see
/// [`verify_approval_request`]).
pub fn build_live_approval_with_workspace(
    initial: &str,
    ledger_path: Option<PathBuf>,
    workspace_root: &Path,
) -> LiveApprovalBuild {
    let mode = Arc::new(ApprovalModeSwitch::new(RuntimeApprovalMode::parse(initial)));
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(LiveApprovalGate::new(
        Arc::clone(&mode),
        queue_tx,
        ledger_path,
        workspace_root.to_path_buf(),
    ));
    (
        Arc::clone(&gate),
        Some(gate as Arc<dyn ToolPolicy>),
        queue_rx,
        mode,
    )
}

/// The builder for callers with no verify surface to scope (the unit and
/// integration tests): the workspace-scoped builder pointed at the process's
/// cwd, which stays inert for every other tool. Production passes the
/// session's workspace to [`build_live_approval_with_workspace`] so `verify`'s
/// arm resolves the project's own command against the right root.
pub fn build_live_approval(initial: &str, ledger_path: Option<PathBuf>) -> LiveApprovalBuild {
    build_live_approval_with_workspace(initial, ledger_path, Path::new("."))
}

/// The `verify` ask-gate arm: only a project-supplied command prompts.
#[cfg(test)]
mod verify_arm_tests {
    use super::*;
    use serde_json::json;

    /// protects: a verify command the project supplies in its own config is
    /// approved like a shell command - the ask names the exact command and
    /// where it came from, and a standing grant is scoped to that command.
    /// fails_when: `.phimint.toml`'s verify_command runs unprompted in ask
    /// mode, laundering a repository string into a green light.
    /// no_claim: the popup's rendering (phi-tui's).
    #[test]
    fn a_project_supplied_verify_command_prompts_with_its_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".phimint.toml"),
            "verify_command = \"cargo test --all-features\"\n",
        )
        .unwrap();
        let request =
            verify_approval_request(&json!({}), dir.path()).expect("a project command prompts");
        assert!(
            request.message.contains("cargo test --all-features"),
            "the ask names the exact command: {}",
            request.message
        );
        assert!(
            request.message.contains(".phimint.toml"),
            "the ask names where it came from: {}",
            request.message
        );
        assert_eq!(
            request.action_key.as_deref(),
            Some("verify:cargo test --all-features"),
            "a standing grant is scoped to this command"
        );
    }

    /// protects: phimint's own resolution strings (the convention slot, the
    /// language table) run unprompted - the repository selects which check
    /// runs, never the command text. fails_when: every cargo project's verify
    /// nags in ask mode. no_claim: which command the table picks.
    #[test]
    fn a_phimint_resolved_verify_command_runs_unprompted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert!(
            verify_approval_request(&json!({}), dir.path()).is_none(),
            "a convention command is phimint's own string - no ask"
        );
    }

    /// protects: a workspace with no compile layer produces no ask (the tool
    /// says nothing to verify; nothing runs). fails_when: verify prompts for
    /// a command that does not exist.
    #[test]
    fn a_workspace_without_a_verify_command_never_asks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_approval_request(&json!({}), dir.path()).is_none());
    }

    /// protects: the dispatch routes only `verify` to the new arm - every
    /// other tool keeps the old classification. fails_when: the arm swallows
    /// write_file's prompt, or the gate asks on reads.
    #[tokio::test]
    async fn the_verify_arm_routes_only_verify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".phimint.toml"),
            "verify_command = \"make check\"\n",
        )
        .unwrap();
        let (_gate, policy, _queue_rx, _switch) =
            build_live_approval_with_workspace("ask", None, dir.path());
        let policy = policy.expect("the gate is the policy");
        assert!(
            policy
                .evaluate_approval("verify", &json!({}))
                .await
                .is_some(),
            "a project-supplied verify command asks"
        );
        assert!(
            policy
                .evaluate_approval("write_file", &json!({"path": "a.txt"}))
                .await
                .is_some(),
            "write_file still prompts"
        );
        assert!(
            policy
                .evaluate_approval("read_file", &json!({"path": "a.txt"}))
                .await
                .is_none(),
            "reads still pass"
        );
    }
}

#[cfg(test)]
mod tests;
