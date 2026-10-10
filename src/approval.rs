//! Approval policy + interactive CLI handler (Phase 3).
//!
//! Approval in agent-base is **two layers, both required**:
//! - [`ToolPolicy`] (the *gate*): decides whether a call needs approval at all
//!   and assigns its `risk_level`. Returning `None` auto-approves; returning
//!   `Some(ApprovalRequest)` defers to the handler.
//! - [`ApprovalHandler`] (the *decision*): `AllowOnce` / `AllowAlways` / `Deny`.
//!
//! phimint previously wired *only* a handler (`Auto`/`DenyAll`), so
//! `--approval deny` was a silent no-op — with no policy, `process_approval`
//! short-circuits and never consults the handler. `ask` mode needs both layers.

use std::io::{self, Write};
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{
    AgentError, AgentResult, ApprovalDecision, ApprovalHandler, ApprovalMode, ApprovalRequest,
    AutoApprovalHandler, QueuedApprovalHandler, RiskLevel, ToolPolicy,
};
use serde_json::Value;
use tokio::sync::mpsc;

// The pending-request type now lives in the framework (phi_agent::cli::approval,
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

// ── The gate: ApprovalPolicy ────────────────────────────────────────────────

/// phimint's tool policy: auto-approve reads, prompt on writes and risky shell.
#[derive(Debug, Clone, Default)]
pub struct ApprovalPolicy;

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

#[async_trait]
impl ToolPolicy for ApprovalPolicy {
    async fn evaluate_approval(&self, tool_name: &str, args: &Value) -> Option<ApprovalRequest> {
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
}

// ── The decision: CliApprovalHandler ────────────────────────────────────────

/// Interactive terminal approval: `y` allow-once, `a` allow-always, `n` deny.
///
/// Mirrors `phi-agent/src/bin/phi/approval.rs` — reads stdin while racing the
/// caller's cancellation token so Ctrl+C still interrupts a pending prompt.
#[derive(Debug, Clone, Default)]
pub struct CliApprovalHandler;

impl CliApprovalHandler {
    pub fn new() -> Self {
        Self
    }

    fn risk_badge(level: &RiskLevel) -> &'static str {
        match level {
            RiskLevel::Safe => "\u{1F7E2} Safe",
            RiskLevel::Sensitive => "\u{1F7E1} Sensitive",
            RiskLevel::Destructive => "\u{1F534} Destructive",
        }
    }

    async fn prompt(
        &self,
        request: &ApprovalRequest,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        eprintln!();
        eprintln!("  !! {}", request.title);
        eprintln!("     Risk: {}", Self::risk_badge(&request.risk_level));
        eprintln!("     {}", request.message);
        eprintln!();

        loop {
            if cancel_token.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            eprint!("     Confirm? [y=allow / a=allow always / n=deny]: ");
            io::stderr()
                .flush()
                .map_err(|e| AgentError::internal(format!("flush stderr failed: {e}")))?;

            let line = read_stdin_line_cancellable(cancel_token).await?;
            match map_input(&line) {
                Some(decision) => return Ok(decision),
                None => eprintln!("     Invalid input - enter y / a / n"),
            }
        }
    }
}

/// Map a user's approval keystroke to a decision. `None` = unrecognised input.
///
/// Extracted as a pure function so the interactive prompt stays thin and the
/// mapping is unit-testable without driving stdin.
fn map_input(s: &str) -> Option<ApprovalDecision> {
    match s.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalDecision::AllowOnce),
        "a" | "always" => Some(ApprovalDecision::AllowAlways),
        "n" | "no" => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

#[async_trait]
impl ApprovalHandler for CliApprovalHandler {
    async fn approve(
        &self,
        request: ApprovalRequest,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        self.prompt(&request, &cancel_token).await
    }
}

// ── The decision: QueuedApprovalHandler (Phase 5b) ─────────────────────────────
//
// The queued handler itself (`QueuedApprovalHandler` + `ApprovalItem`) is
// framework code now — re-exported from phi-agent (`cli::approval`), since any
// phi-agent UI runtime needs the same enqueue-and-answer pattern. What stays
// here is only the wiring: `build_queued_approval` pairs the handler with
// phimint's policy.

/// Read a stdin line, racing against `cancel_token` so the prompt doesn't block
/// the runtime or ignore Ctrl+C.
async fn read_stdin_line_cancellable(
    cancel_token: &tokio_util::sync::CancellationToken,
) -> AgentResult<String> {
    use tokio::io::AsyncBufReadExt;

    tokio::select! {
        _ = cancel_token.cancelled() => Err(AgentError::Cancelled),
        result = async {
            let stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut lines = stdin.lines();
            match lines.next_line().await {
                Ok(Some(line)) => Ok(line),
                Ok(None) => Err(AgentError::Cancelled),
                Err(e) => Err(AgentError::internal(format!("read stdin failed: {e}"))),
            }
        } => result,
    }
}

// ── Wiring ──────────────────────────────────────────────────────────────────

/// Build the approval handler + policy for a CLI mode.
///
/// `auto` → handler only (policy stays `None`, every call auto-approved);
/// `deny` → deny-all handler **plus** policy (policy is what makes deny real);
/// `ask` → interactive handler **plus** policy (prompt on writes / risky shell).
pub fn build_approval(mode: &str) -> (Arc<dyn ApprovalHandler>, Option<Arc<dyn ToolPolicy>>) {
    match mode {
        "deny" => (
            Arc::new(AutoApprovalHandler::new(ApprovalMode::DenyAll)),
            Some(Arc::new(ApprovalPolicy)),
        ),
        "ask" => (
            Arc::new(CliApprovalHandler::new()),
            Some(Arc::new(ApprovalPolicy)),
        ),
        _ => (Arc::new(AutoApprovalHandler::new(ApprovalMode::Auto)), None),
    }
}

/// `build_queued_approval` output: handler, policy, and the TUI-side receiver.
pub type QueuedApprovalBuild = (
    Arc<dyn ApprovalHandler>,
    Option<Arc<dyn ToolPolicy>>,
    mpsc::UnboundedReceiver<ApprovalItem>,
);

/// Build the queued approval handler + policy for `ask` mode (Phase 5b).
///
/// The handler enqueues requests instead of reading stdin; the caller keeps the
/// returned receiver and feeds it to the TUI popup, which drains it and renders
/// one prompt at a time.
pub fn build_queued_approval() -> QueuedApprovalBuild {
    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    (
        Arc::new(QueuedApprovalHandler::new(queue_tx)),
        Some(Arc::new(ApprovalPolicy)),
        queue_rx,
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

    #[tokio::test]
    async fn policy_auto_approves_read_tools() {
        let p = ApprovalPolicy;
        assert!(
            p.evaluate_approval("read_file", &serde_json::json!({}))
                .await
                .is_none()
        );
        assert!(
            p.evaluate_approval("search_content", &serde_json::json!({}))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn policy_prompts_on_writes() {
        let p = ApprovalPolicy;
        let req = p
            .evaluate_approval("write_file", &serde_json::json!({"path": "src/lib.rs"}))
            .await
            .expect("write_file should prompt");
        assert_eq!(req.risk_level, RiskLevel::Sensitive);
        assert_eq!(req.action_key.as_deref(), Some("write_file:src/lib.rs"));
        assert!(req.message.contains("src/lib.rs"));
    }

    #[tokio::test]
    async fn write_approval_key_is_scoped_to_path() {
        // `AllowAlways` must grant a narrow standing approval (this file), not
        // every write_file — two different paths must produce different keys.
        let p = ApprovalPolicy;
        let a = p
            .evaluate_approval("write_file", &serde_json::json!({"path": "src/cache.rs"}))
            .await
            .unwrap();
        let b = p
            .evaluate_approval("write_file", &serde_json::json!({"path": "src/logging.rs"}))
            .await
            .unwrap();
        assert_eq!(a.action_key.as_deref(), Some("write_file:src/cache.rs"));
        assert_eq!(b.action_key.as_deref(), Some("write_file:src/logging.rs"));
        assert_ne!(a.action_key, b.action_key);
    }

    #[tokio::test]
    async fn command_approval_key_is_scoped_to_command() {
        let p = ApprovalPolicy;
        let req = p
            .evaluate_approval(
                "execute_command",
                &serde_json::json!({"command": "rm -rf /tmp/x"}),
            )
            .await
            .expect("rm should prompt");
        assert_eq!(req.risk_level, RiskLevel::Destructive);
        assert_eq!(
            req.action_key.as_deref(),
            Some("execute_command:rm -rf /tmp/x")
        );
        // Whitespace is normalised so `touch X` and `touch   X` share a key.
        let a = p
            .evaluate_approval(
                "execute_command",
                &serde_json::json!({"command": "touch  X"}),
            )
            .await
            .unwrap();
        let b = p
            .evaluate_approval(
                "execute_command",
                &serde_json::json!({"command": "touch X"}),
            )
            .await
            .unwrap();
        assert_eq!(a.action_key, b.action_key);
    }

    #[tokio::test]
    async fn policy_classifies_shell() {
        let p = ApprovalPolicy;
        // Safe command → no prompt.
        assert!(
            p.evaluate_approval(
                "execute_command",
                &serde_json::json!({"command": "cargo check"})
            )
            .await
            .is_none()
        );
        // Destructive command → prompt with Destructive risk.
        let req = p
            .evaluate_approval(
                "execute_command",
                &serde_json::json!({"command": "rm -rf /tmp/x"}),
            )
            .await
            .expect("rm should prompt");
        assert_eq!(req.risk_level, RiskLevel::Destructive);
        // Unknown command → Sensitive.
        let req = p
            .evaluate_approval("execute_command", &serde_json::json!({"command": "make"}))
            .await
            .expect("make should prompt");
        assert_eq!(req.risk_level, RiskLevel::Sensitive);
    }

    #[test]
    fn map_input_parses_decisions() {
        assert_eq!(map_input("y"), Some(ApprovalDecision::AllowOnce));
        assert_eq!(map_input("yes"), Some(ApprovalDecision::AllowOnce));
        assert_eq!(map_input("  Y  "), Some(ApprovalDecision::AllowOnce));
        assert_eq!(map_input("a"), Some(ApprovalDecision::AllowAlways));
        assert_eq!(map_input("always"), Some(ApprovalDecision::AllowAlways));
        assert_eq!(map_input("n"), Some(ApprovalDecision::Deny));
        assert_eq!(map_input("no"), Some(ApprovalDecision::Deny));
        assert_eq!(map_input("maybe"), None);
        assert_eq!(map_input(""), None);
    }

    #[test]
    fn build_approval_modes() {
        let (_, policy) = build_approval("auto");
        assert!(policy.is_none(), "auto has no policy");
        let (_, policy) = build_approval("deny");
        assert!(policy.is_some(), "deny must have a policy");
        let (_, policy) = build_approval("ask");
        assert!(policy.is_some(), "ask must have a policy");
        let (_, policy) = build_approval("bogus");
        assert!(policy.is_none(), "unknown mode falls back to auto");
    }

    #[tokio::test]
    async fn build_queued_approval_wires_handler_to_queue() {
        // The sunk-down handler (now from phi-agent) must still be reachable
        // through phimint's wiring: requests enqueued, policy attached.
        let (handler, policy, mut queue_rx) = build_queued_approval();
        assert!(policy.is_some(), "queued mode must carry the policy");

        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = tokio::spawn({
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
}
