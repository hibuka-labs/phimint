//! Merge tool (Phase 4): reconcile parallel sub-agent work and verify the whole.
//!
//! The "合" + "验" half of design §7's `decompose`/`merge`. After sub-agents have
//! written their slices into the shared workspace, `merge`:
//!
//! 1. diffs the workspace against the snapshot `decompose` recorded (problem #3's
//!    change attribution, without relying on git);
//! 2. flags conflicts — a changed file declared by 2+ slices (overlap) or changed
//!    outside every declared boundary (out-of-scope edit);
//! 3. runs a build/test command and folds in a terse error summary (problem #4
//!    "合并验证").

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::workspace::{Change, ChangeKind, Slice, WorkspaceTracker, normalize_path};

// ── Verify helpers (migrated from the former `verify` module) ──────────────

/// Which error parser to use for a language's compiler output.
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
fn parse_rustc(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut lines = stderr.lines().peekable();

    while let Some(line) = lines.next() {
        let line = line.trim();

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
            if msg.starts_with("aborting due to") || msg.starts_with("could not compile") {
                continue;
            }
            ("error".to_string(), msg.to_string())
        } else {
            continue;
        };

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
                lines.next();
            } else {
                break;
            }
        }

        diags.push(Diagnostic { code, msg, location });
    }

    diags
}

/// Parse javac / maven-compiler stderr into diagnostics.
fn parse_javac(stderr: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let line = line.strip_prefix("[ERROR]").map(str::trim).unwrap_or(line);
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

/// Run `sh -c <command>` in the workspace and return a terse summary.
async fn run_and_summarize(command: &str, timeout_ms: u64) -> String {
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

    if summary.chars().count() > MAX_SUMMARY_CHARS {
        summary.chars().take(MAX_SUMMARY_CHARS).collect::<String>()
    } else {
        summary
    }
}

// ── Merge tool ─────────────────────────────────────────────────────────────

/// A conflict between declared slice boundaries and what actually changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub reason: String,
}

/// Detect conflicts over *changed* files: a file declared by 2+ slices, or a
/// change landing outside every slice's declared boundary.
///
/// Pure and testable. Overlap is a decomposition bug (two slices claimed the same
/// file); out-of-scope is a sub-agent wandering off its slice.
pub fn detect_conflicts(slices: &[Slice], changes: &[Change]) -> Vec<Conflict> {
    let mut declared: HashMap<String, Vec<String>> = HashMap::new();
    for s in slices {
        for f in &s.files {
            declared
                .entry(normalize_path(f))
                .or_default()
                .push(s.name.clone());
        }
    }

    let mut out: Vec<Conflict> = Vec::new();
    for c in changes {
        let key = normalize_path(&c.path);
        match declared.get(&key) {
            None => out.push(Conflict {
                path: c.path.clone(),
                reason: "changed but not declared by any slice (out-of-scope edit)".to_string(),
            }),
            Some(names) if names.len() > 1 => out.push(Conflict {
                path: c.path.clone(),
                reason: format!("declared by {} slices: {}", names.len(), names.join(", ")),
            }),
            _ => {}
        }
    }
    out
}

pub struct MergeTool {
    tracker: Arc<WorkspaceTracker>,
    root: PathBuf,
    timeout_ms: u64,
}

impl MergeTool {
    pub fn new(tracker: Arc<WorkspaceTracker>, root: PathBuf, timeout_ms: u64) -> Self {
        Self {
            tracker,
            root,
            timeout_ms,
        }
    }
}

/// Render a change list + conflicts + verify result as one compact report.
fn format_report(changes: &[Change], conflicts: &[Conflict], verify: &str) -> String {
    let mut out = String::new();

    if changes.is_empty() {
        out.push_str("No files changed since decompose.\n");
    } else {
        out.push_str(&format!("Changed files ({}):\n", changes.len()));
        for c in changes {
            let kind = match c.kind {
                ChangeKind::Added => "added",
                ChangeKind::Modified => "modified",
                ChangeKind::Removed => "removed",
            };
            out.push_str(&format!("  {kind:9} {}\n", c.path));
        }
    }

    out.push('\n');
    if conflicts.is_empty() {
        out.push_str("Conflicts: none.\n");
    } else {
        out.push_str(&format!("Conflicts ({}):\n", conflicts.len()));
        for c in conflicts {
            out.push_str(&format!("  ⚠️  {} — {}\n", c.path, c.reason));
        }
        out.push_str("  → resolve before proceeding; do not silently overwrite.\n");
    }

    out.push_str(&format!("\nVerification:\n{verify}"));
    out
}

#[async_trait]
impl Tool for MergeTool {
    fn name(&self) -> &'static str {
        "merge"
    }

    fn description(&self) -> &'static str {
        "Reconcile changes after a `decompose`: diff the workspace against the pre-decompose snapshot, report changed files and conflicts (overlapping or out-of-scope edits), and run `cargo check` to verify the whole still compiles. Optional — `verify` alone is enough for the common single-writer flow."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Verification command (default `cargo check`)."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Merge sub-agent work and verify the workspace.".to_string(),
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
            .unwrap_or("cargo check")
            .to_string();

        tracing::info!(command = %command, "merge start");

        let changes = match self.tracker.changed_files(&self.root) {
            Some(c) => c,
            None => {
                return Ok(vec![Content::text(
                    "[Error]: no snapshot recorded — call `decompose` first so `merge` knows the baseline.",
                )]);
            }
        };

        let slices = self.tracker.slices();
        let conflicts = detect_conflicts(&slices, &changes);
        let verify = run_and_summarize(&command, self.timeout_ms).await;

        tracing::info!(
            changes = changes.len(),
            conflicts = conflicts.len(),
            "merge done"
        );

        Ok(vec![Content::text(format_report(
            &changes,
            &conflicts,
            &verify,
        ))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(name: &str, files: &[&str]) -> Slice {
        Slice {
            name: name.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            context: String::new(),
            task: String::new(),
        }
    }

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change {
            path: path.to_string(),
            kind,
        }
    }

    // ── verify helper tests (migrated from the former `verify` module) ────

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

    // ── conflict detection tests ──────────────────────────────────────────

    #[test]
    fn detect_no_conflict_when_disjoint() {
        let slices = vec![slice("cache", &["src/cache.rs"]), slice("logging", &["src/logging.rs"])];
        let changes = vec![
            change("src/cache.rs", ChangeKind::Added),
            change("src/logging.rs", ChangeKind::Added),
        ];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn detect_out_of_scope_edit() {
        let slices = vec![slice("cache", &["src/cache.rs"])];
        let changes = vec![change("src/main.rs", ChangeKind::Modified)];
        let conflicts = detect_conflicts(&slices, &changes);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/main.rs");
        assert!(conflicts[0].reason.contains("out-of-scope"), "{}", conflicts[0].reason);
    }

    #[test]
    fn detect_overlap_when_two_slices_claim_a_file() {
        let slices = vec![
            slice("a", &["src/lib.rs"]),
            slice("b", &["src/lib.rs", "src/b.rs"]),
        ];
        let changes = vec![change("src/lib.rs", ChangeKind::Modified)];
        let conflicts = detect_conflicts(&slices, &changes);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/lib.rs");
        assert!(conflicts[0].reason.contains("2 slices"), "{}", conflicts[0].reason);
    }

    #[test]
    fn overlap_only_matters_on_changed_files() {
        let slices = vec![slice("a", &["src/lib.rs"]), slice("b", &["src/lib.rs", "src/b.rs"])];
        let changes = vec![change("src/b.rs", ChangeKind::Modified)];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn normalize_path_applied_to_slice_declarations() {
        let slices = vec![slice("cache", &["./src/cache.rs"])];
        let changes = vec![change("src/cache.rs", ChangeKind::Added)];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn format_report_lists_changes_conflicts_and_verify() {
        let changes = vec![change("src/cache.rs", ChangeKind::Added)];
        let conflicts = vec![Conflict {
            path: "src/main.rs".into(),
            reason: "out-of-scope".into(),
        }];
        let report = format_report(&changes, &conflicts, "✓ passed");
        assert!(report.contains("src/cache.rs"), "{report}");
        assert!(report.contains("added"), "{report}");
        assert!(report.contains("src/main.rs"), "{report}");
        assert!(report.contains("✓ passed"), "{report}");
    }

    #[test]
    fn format_report_empty_changes() {
        let report = format_report(&[], &[], "✓ passed");
        assert!(report.contains("No files changed"), "{report}");
        assert!(report.contains("Conflicts: none"), "{report}");
    }

    #[test]
    fn metadata_and_schema() {
        let t = MergeTool::new(Arc::new(WorkspaceTracker::new()), PathBuf::from("."), 30_000);
        assert_eq!(t.name(), "merge");
        let schema = t.schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().map(|a| a.is_empty()).unwrap_or(false));
    }
}
