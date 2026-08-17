//! Verify tool (Phase 3): run a build/test command and distill compiler errors.
//!
//! The "verification loop" from design §6 (steps 5–6), as a thin consumer-side
//! tool rather than a framework middleware: instead of making the LLM read a
//! wall of build output through `execute_command`, `verify` runs the command and
//! returns a terse, actionable summary — `N errors:` with one
//! `file:line:col  code  message` line each. On success it's a single
//! `✓ passed` line.
//!
//! Multi-language (Phase 6c): the error parser is selected from the command
//! (`cargo`→rustc, `mvn`/`javac`→javac, `tsc`→tsc, `gcc`/`clang`/`make`→gcc),
//! and the default command is auto-detected from the workspace language. Unknown
//! output falls back to a raw stderr tail, so any language still degrades
//! gracefully.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use crate::lang::default_verify_command;

/// Which error parser `verify` should use for a language's compiler output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorParser {
    Rustc,
    Javac,
    Tsc,
    Gcc,
}

/// Hard cap on the returned summary, so a pathological command can't flood the
/// LLM (mirrors `RipgrepTool::MAX_RESULT_CHARS`).
const MAX_SUMMARY_CHARS: usize = 4000;

/// One compiler diagnostic: `error[E0308]: mismatched types` → `src/main.rs:5:18`.
#[derive(Debug, PartialEq, Eq)]
struct Diagnostic {
    code: String,
    msg: String,
    /// `file:line:col`, or empty when the compiler didn't give a location.
    location: String,
}

/// Pick the error parser for a verify command string.
///
/// The registry's `verify_command` strings and common build tools map cleanly:
/// `cargo`→Rustc, `mvn`/`javac`/`gradle`→Javac, `tsc`→Tsc, `gcc`/`clang`/`make`
/// →Gcc. Unknown commands default to Rustc, whose parser yields nothing on
/// foreign output — so `summarize_errors_with` falls back to the raw tail.
fn parser_for_command(command: &str) -> ErrorParser {
    let c = command.to_ascii_lowercase();
    if c.contains("cargo") {
        return ErrorParser::Rustc;
    }
    if c.contains("mvn") || c.contains("javac") || c.contains("gradle") {
        return ErrorParser::Javac;
    }
    if c.contains("tsc") || c.contains("ts-node") || c.contains("eslint") {
        return ErrorParser::Tsc;
    }
    if c.contains("gcc")
        || c.contains("g++")
        || c.contains("clang")
        || c.contains("make")
        || c.contains("cmake")
        || c.contains("ninja")
    {
        return ErrorParser::Gcc;
    }
    ErrorParser::Rustc
}

/// Parse a rustc/cargo stderr blob into diagnostics.
///
/// rustc emits each error as a `error[EXXXX]: message` line followed (usually)
/// by a `--> file:line:col` location line. We walk the lines, matching the
/// `error[...]:`/`error:` head and looking ahead for the `-->` location.
fn parse_rustc(stderr: &str) -> Vec<Diagnostic> {
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

/// Parse javac / maven-compiler stderr into diagnostics.
///
/// Two flavours:
///   `Foo.java:12: error: incompatible types: ...`  (bare `javac`)
///   `[ERROR] /path/Foo.java:[12,5] error: ...`     (maven wraps the line)
fn parse_javac(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let line = line.strip_prefix("[ERROR]").map(str::trim).unwrap_or(line);
        // Bare form first (`Foo.java:12: error:`): the location ends before the
        // `: error:` colon. Maven's `Foo.java:[12,5] error:` has no colon before
        // `error:`, so it's caught by the second branch.
        if let Some(p) = line.find(": error: ") {
            let location = line[..p].trim().to_string();
            let msg = line[p + ": error: ".len()..].trim().to_string();
            if !location.is_empty() {
                diags.push(Diagnostic { code: String::new(), msg, location });
            }
        } else if let Some(p) = line.find(" error: ") {
            let location = line[..p].trim().to_string();
            let msg = line[p + " error: ".len()..].trim().to_string();
            if !location.is_empty() {
                diags.push(Diagnostic { code: String::new(), msg, location });
            }
        }
    }
    diags
}

/// Parse `tsc` stderr into diagnostics.
///
///   `src/a.ts(12,3): error TS2322: Type 'string' is not assignable ...`
/// Anchored on the stable `error TSxxxx:` head — works with or without a leading
/// `file(line,col)` (project-level errors have none).
fn parse_tsc(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let Some(pos) = line.find("error TS") else { continue };
        let location = line[..pos]
            .trim()
            .trim_end_matches(|c| matches!(c, ':' | '-' | ' '))
            .trim()
            .to_string();
        let (head, msg) = line[pos..].split_once(": ").unwrap_or((&line[pos..], ""));
        let code = head.split_whitespace().nth(1).unwrap_or("").to_string();
        if code.is_empty() {
            continue;
        }
        diags.push(Diagnostic { code, msg: msg.trim().to_string(), location });
    }
    diags
}

/// Parse gcc/clang stderr into diagnostics.
///
///   `foo.c:12:3: error: 'x' undeclared`  (`: fatal error:` for missing headers).
fn parse_gcc(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        for marker in [": fatal error: ", ": error: "] {
            if let Some(p) = line.find(marker) {
                let location = line[..p].trim().to_string();
                let msg = line[p + marker.len()..].trim().to_string();
                if !location.is_empty() {
                    diags.push(Diagnostic { code: String::new(), msg, location });
                }
                break;
            }
        }
    }
    diags
}

/// Format diagnostics (or a raw stderr tail when nothing parsed) as a terse summary.
fn summarize_errors_with(stderr: &str, parser: ErrorParser) -> String {
    let diags = match parser {
        ErrorParser::Rustc => parse_rustc(stderr),
        ErrorParser::Javac => parse_javac(stderr),
        ErrorParser::Tsc => parse_tsc(stderr),
        ErrorParser::Gcc => parse_gcc(stderr),
    };
    if diags.is_empty() {
        // Not compiler-style output (a command we don't parse, or no errors) —
        // show a short tail.
        let tail: Vec<&str> = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(20)
            .collect();
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
    default_command: String,
}

impl VerifyTool {
    pub fn new(workspace_root: &Path, timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            default_command: default_verify_command(workspace_root).to_string(),
        }
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
                    summarize_errors_with(&stderr, parser_for_command(command))
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
        "Run a build/test command (default auto-detected from the workspace language, e.g. `cargo check` / `mvn -q compile` / `npx tsc --noEmit` / `make`) and return a terse summary of any compiler errors as `file:line:col  code  message` lines. Prefer this over execute_command for compiling, so failures come back compact and actionable."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to run (defaults to the workspace language's build command: `cargo check`, `mvn -q compile`, `npx tsc --noEmit`, or `make`)."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Run a build/test command and summarize compiler errors.".to_string(),
            origin: "phimint".to_string(),
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
            .unwrap_or(self.default_command.as_str())
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

    const JAVAC_ERRORS: &str = "\
Foo.java:12: error: incompatible types: int cannot be converted to String
Bar.java:3: error: cannot find symbol
";

    const TSC_ERRORS: &str = "\
src/a.ts(12,3): error TS2322: Type 'string' is not assignable to type 'number'.
src/b.ts(1,5): error TS2304: Cannot find name 'foo'.
error TS18003: No inputs were found in config file 'tsconfig.json'.
";

    const GCC_ERRORS: &str = "\
foo.c:12:3: error: 'x' undeclared (first use in this function)
bar.c:1:10: fatal error: missing.h: No such file or directory
main.cpp:5:1: error: expected ';' before '}' token
";

    #[test]
    fn parses_rustc_errors() {
        let diags = parse_rustc(CARGO_ERRORS);
        assert_eq!(diags.len(), 2, "{diags:?}");

        assert_eq!(diags[0].code, "E0382");
        assert_eq!(diags[0].location, "src/main.rs:42:13");
        assert!(diags[0].msg.contains("borrow of moved value"));

        assert_eq!(diags[1].code, "E0308");
        assert_eq!(diags[1].location, "src/lib.rs:7:5");
        assert!(diags[1].msg.contains("mismatched types"));
    }

    #[test]
    fn parses_javac_errors() {
        let diags = parse_javac(JAVAC_ERRORS);
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].location, "Foo.java:12");
        assert!(diags[0].msg.contains("incompatible types"));
        assert_eq!(diags[1].location, "Bar.java:3");
        assert!(diags[1].msg.contains("cannot find symbol"));
    }

    #[test]
    fn parses_tsc_errors() {
        let diags = parse_tsc(TSC_ERRORS);
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert_eq!(diags[0].code, "TS2322");
        assert_eq!(diags[0].location, "src/a.ts(12,3)");
        assert_eq!(diags[1].code, "TS2304");
        assert_eq!(diags[2].code, "TS18003");
        assert!(diags[2].location.is_empty(), "project-level error has no file location");
    }

    #[test]
    fn parses_gcc_errors() {
        let diags = parse_gcc(GCC_ERRORS);
        assert_eq!(diags.len(), 3, "{diags:?}");
        assert_eq!(diags[0].location, "foo.c:12:3");
        assert!(diags[0].msg.contains("undeclared"));
        assert_eq!(diags[1].location, "bar.c:1:10");
        assert!(diags[1].msg.contains("missing.h"));
        assert_eq!(diags[2].location, "main.cpp:5:1");
    }

    #[test]
    fn parser_for_command_selects_by_tool() {
        assert_eq!(parser_for_command("cargo check"), ErrorParser::Rustc);
        assert_eq!(parser_for_command("mvn -q compile"), ErrorParser::Javac);
        assert_eq!(parser_for_command("javac Foo.java"), ErrorParser::Javac);
        assert_eq!(parser_for_command("npx tsc --noEmit"), ErrorParser::Tsc);
        assert_eq!(parser_for_command("gcc foo.c"), ErrorParser::Gcc);
        assert_eq!(parser_for_command("cmake --build ."), ErrorParser::Gcc);
        // Unknown → Rustc → parser finds nothing → raw tail fallback.
        assert_eq!(parser_for_command("npm run build"), ErrorParser::Rustc);
    }

    #[test]
    fn summarize_errors_is_terse_and_actionable() {
        let s = summarize_errors_with(CARGO_ERRORS, ErrorParser::Rustc);
        assert!(s.starts_with("2 error(s):"), "{s}");
        assert!(s.contains("src/main.rs:42:13  E0382"), "{s}");
        assert!(s.contains("src/lib.rs:7:5  E0308"), "{s}");
        assert!(!s.contains("move occurs because"), "source fragment should be dropped: {s}");
    }

    #[test]
    fn summarize_non_cargo_stderr_returns_tail() {
        let s = summarize_errors_with("some\nrandom\nfailure\nlines", ErrorParser::Rustc);
        assert!(s.starts_with("(unparsed stderr):"), "{s}");
        assert!(s.contains("failure"), "{s}");
    }

    #[test]
    fn summarize_empty_stderr() {
        assert_eq!(summarize_errors_with("", ErrorParser::Rustc), "(no stderr output)");
    }

    #[test]
    fn metadata_and_schema() {
        let tool = VerifyTool::new(std::path::Path::new("."), 30_000);
        assert_eq!(tool.name(), "verify");
        let schema = tool.schema();
        assert_eq!(schema["type"], "object");
        // `command` is optional — the tool auto-detects the workspace language.
        assert!(schema["required"].as_array().map(|a| a.is_empty()).unwrap_or(false));
        assert!(tool.description().contains("cargo check"));
    }
}
