//! Verify tool (Phase 3): run a build/test command and distill compiler errors.
//!
//! The "verification loop" from design §6 (steps 5–6), as a thin consumer-side
//! tool rather than a framework middleware: instead of making the LLM read a
//! wall of `cargo check` output through `execute_command`, `verify` runs the
//! command and returns a terse, actionable summary — `N errors:` with one
//! `file:line:col  code  message` line each. On success it's a single
//! `✓ passed` line.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

/// Default verification command when the caller doesn't pass one.
const DEFAULT_COMMAND: &str = "cargo check";

/// Hard cap on the returned summary, so a pathological command can't flood the
/// LLM (mirrors `RipgrepTool::MAX_RESULT_CHARS`).
const MAX_SUMMARY_CHARS: usize = 4000;

/// One rustc diagnostic: `error[E0308]: mismatched types` → `src/main.rs:5:18`.
#[derive(Debug, PartialEq, Eq)]
struct Diagnostic {
    code: String,
    msg: String,
    /// `file:line:col`, or empty when rustc didn't give a location.
    location: String,
}

/// Parse a rustc/cargo stderr blob into diagnostics.
///
/// rustc emits each error as a `error[EXXXX]: message` line followed (usually)
/// by a `--> file:line:col` location line. We walk the lines, matching the
/// `error[...]:`/`error:` head and looking ahead for the `-->` location.
fn parse_diagnostics(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut lines = stderr.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim();

        // `error[E0308]: msg` or `error: msg`.
        let (code, msg) = if let Some(rest) = line.strip_prefix("error[") {
            match rest.find(']') {
                Some(end) => {
                    let code = rest[..end].to_string();
                    let msg = rest[end + 1..].trim_start_matches(':').trim().to_string();
                    (code, msg)
                }
                None => continue,
            }
        } else if let Some(rest) = line.strip_prefix("error:") {
            let msg = rest.trim();
            // Skip rustc's trailing summary lines (`error: aborting due to N
            // previous errors`, `error: could not compile ...`) — they are not
            // diagnostics and would otherwise show up as a phantom extra error.
            if msg.starts_with("aborting due to") || msg.starts_with("could not compile") {
                continue;
            }
            ("error".to_string(), msg.to_string())
        } else {
            continue;
        };

        // Look ahead for the `--> file:line:col` location line.
        let mut location = String::new();
        while let Some(next) = lines.peek() {
            let next = next.trim();
            if let Some(loc) = next.strip_prefix("--> ") {
                location = loc.to_string();
                lines.next();
                break;
            } else if next.is_empty()
                || next.starts_with('|')
                || next.starts_with('=')
                || next.starts_with("note:")
                || next.starts_with("help:")
            {
                // Skip span/source-fragment/note lines between the head and `-->`.
                lines.next();
            } else {
                // A different line — stop looking for this error's location.
                break;
            }
        }

        diags.push(Diagnostic { code, msg, location });
    }

    diags
}

/// Format diagnostics (or a raw stderr tail when nothing parsed) as a terse summary.
pub fn summarize_errors(stderr: &str) -> String {
    let diags = parse_diagnostics(stderr);
    if diags.is_empty() {
        // Not rustc-style output (a non-cargo command failing) — show a short tail.
        let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).rev().take(20).collect();
        let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return if tail.trim().is_empty() {
            "(no stderr output)".to_string()
        } else {
            format!("(unparsed stderr):\n{tail}")
        };
    }

    let mut out = format!("{} error(s):\n", diags.len());
    for d in diags {
        let loc = if d.location.is_empty() { "?" } else { &d.location };
        out.push_str(&format!("  {loc}  {}  {}\n", d.code, d.msg));
    }
    out
}

/// The verify tool: `sh -c <command>` in the workspace, then summarize.
pub struct VerifyTool {
    timeout_ms: u64,
}

impl VerifyTool {
    pub fn new(timeout_ms: u64) -> Self {
        Self { timeout_ms }
    }
}

/// Run `sh -c <command>` in the workspace and return a terse summary.
///
/// Extracted from [`VerifyTool::call`] so the Phase-4 `merge` tool can reuse the
/// exact same command-running + timeout + error-summarising behaviour (design
/// §7.3 "合并后跑一次验证").
pub async fn run_and_summarize(command: &str, timeout_ms: u64) -> String {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return format!("[Error]: verify spawn failed: {e}");
        }
    };

    let pid = child.id();
    let sleep = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(sleep);

    let summary = tokio::select! {
        result = child.wait_with_output() => match result {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let code = output.status.code().unwrap_or(-1);
                tracing::info!(command = %command, exit_code = code, "verify done");
                if code == 0 {
                    "✓ passed".to_string()
                } else {
                    summarize_errors(&stderr)
                }
            }
            Err(e) => format!("[Error]: verify wait failed: {e}"),
        },
        _ = &mut sleep => {
            if let Some(pid) = pid {
                let _ = tokio::process::Command::new("kill")
                    .arg("-9").arg(pid.to_string())
                    .stdout(Stdio::null()).stderr(Stdio::null())
                    .status().await;
            }
            tracing::warn!(command = %command, timeout_ms = timeout_ms, "verify timed out and killed");
            format!("[verify timed out after {}ms]\ncommand: {}", timeout_ms, command)
        }
    };

    // Defensive cap: a pathological command (e.g. thousands of errors, or a
    // huge unparsed stderr tail) shouldn't flood the LLM.
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        summary.chars().take(MAX_SUMMARY_CHARS).collect::<String>()
    } else {
        summary
    }
}

#[async_trait]
impl Tool for VerifyTool {
    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Run a build/test command (default `cargo check`) and return a terse summary of any compiler errors as `file:line:col  code  message` lines. Prefer this over execute_command for compiling, so failures come back compact and actionable."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to run (default `cargo check`). `cargo build`/`cargo test`/`cargo clippy` all work."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Run a build/test command and summarize compiler errors.".to_string(),
            origin: "phiforge".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_COMMAND)
            .to_string();

        tracing::info!(command = %command, "verify start");

        let summary = run_and_summarize(&command, self.timeout_ms).await;

        Ok(vec![Content::text(summary)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_ERRORS: &str = "\
error[E0382]: borrow of moved value: `s`
 --> src/main.rs:42:13
  |
42 |     let y = s;
  |             ^ value moved here
  |
  = note: move occurs because `s` has type `String`

error[E0308]: mismatched types
  --> src/lib.rs:7:5
   |
 7 |     x
   |     ^ expected `u32`, found `&str`

error: aborting due to 2 previous errors
";

    #[test]
    fn parses_rustc_errors() {
        let diags = parse_diagnostics(CARGO_ERRORS);
        assert_eq!(diags.len(), 2, "{diags:?}");

        assert_eq!(diags[0].code, "E0382");
        assert_eq!(diags[0].location, "src/main.rs:42:13");
        assert!(diags[0].msg.contains("borrow of moved value"));

        assert_eq!(diags[1].code, "E0308");
        assert_eq!(diags[1].location, "src/lib.rs:7:5");
        assert!(diags[1].msg.contains("mismatched types"));
    }

    #[test]
    fn summarize_errors_is_terse_and_actionable() {
        let s = summarize_errors(CARGO_ERRORS);
        assert!(s.starts_with("2 error(s):"), "{s}");
        assert!(s.contains("src/main.rs:42:13  E0382"), "{s}");
        assert!(s.contains("src/lib.rs:7:5  E0308"), "{s}");
        assert!(!s.contains("move occurs because"), "source fragment should be dropped: {s}");
    }

    #[test]
    fn summarize_non_cargo_stderr_returns_tail() {
        let s = summarize_errors("some\nrandom\nfailure\nlines");
        assert!(s.starts_with("(unparsed stderr):"), "{s}");
        assert!(s.contains("failure"), "{s}");
    }

    #[test]
    fn summarize_empty_stderr() {
        assert_eq!(summarize_errors(""), "(no stderr output)");
    }

    #[test]
    fn metadata_and_schema() {
        let tool = VerifyTool::new(30_000);
        assert_eq!(tool.name(), "verify");
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
        // `command` is optional — the tool defaults to `cargo check`.
        assert!(schema["required"].as_array().map(|a| a.is_empty()).unwrap_or(false));
        assert!(tool.description().contains("cargo check"));
    }
}
