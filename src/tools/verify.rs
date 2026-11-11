//! The `verify` tool: run a project's compile-level check and report the result.
//!
//! This is the "must-verify-before-done" half of the verification loop's
//! *mechanism*; the *policy* (when a hand-back may be blessed) lives in
//! `crate::agent::gate`. What this module owns is the run itself, and one
//! design rule dominates everything else here:
//!
//! **spawn the compiler directly, never through a shell.**
//!
//! `phi-kernel-tools`' shell runs `Command::new("sh").arg("-c")` with no
//! `pipefail`, which means `cargo check 2>&1 | tail -40` reports `tail`'s exit
//! code — zero — and a failed build is laundered into a success. That is not a
//! hypothetical: it is the shape of output an agent reaches for when it wants a
//! short summary. The old `verify` tool had the same bug (`run_and_summarize`
//! spawned `sh`), so this file replaces that path rather than reviving it.
//!
//! The other half of the same rule: the **command comes from the project, not
//! from the model** — `code_intel::lang::resolve_verify_command` reads the
//! project's own config, then a slot the project already fills in, then the
//! language table (§5.1b of `notes/verify-gate-design.md`). The caller passes a
//! scope, never an argv. A command the model could pick is a green light it
//! could mint.
//!
//! Parsing lives in `code-intel::verify` and is advisory only — the exit code
//! is the verdict.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use code_intel::lang;
use code_intel::verify as parse;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use crate::verify_ledger::{EvidenceLedger, VerifyKind, VerifyVerdict};

/// Default wall-clock budget for one verification run.
///
/// `cargo check` on a cold, large workspace runs longer than a small one —
/// xwcode measured 427s cold on its own repo. 600s is chosen so a legitimate
/// cold build is not killed and mistaken for "no evidence"; anything slower is
/// a hang, and a hang must not become a green light either.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// A compile-level verification run.
pub struct VerifyTool {
    workspace_root: PathBuf,
    ledger: Arc<Mutex<EvidenceLedger>>,
    timeout: Duration,
}

impl VerifyTool {
    pub fn new(workspace_root: PathBuf, ledger: Arc<Mutex<EvidenceLedger>>) -> Self {
        Self {
            workspace_root,
            ledger,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the run budget (tests, and a project that knows its build is slow).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// What a spawned run produced, before any parsing or verdict.
struct RunOutcome {
    exit_code: Option<i32>,
    combined_output: String,
    timed_out: bool,
}

/// Spawn `prog args` in `cwd` and wait for it, returning the **real** exit code.
///
/// No shell is involved at any point — see the module docs. The child is placed
/// in its own process group so a timeout kills the build's whole tree: killing
/// only the direct child leaves `cargo`'s `rustc` workers holding `target/`'s
/// lock, which is exactly the orphan-xwcode had to fix.
async fn run_command(
    prog: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOutcome, String> {
    let child = tokio::process::Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start `{prog}`: {e}"))?;

    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(RunOutcome {
            exit_code: out.status.code(),
            combined_output: {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                s.push('\n');
                s.push_str(&String::from_utf8_lossy(&out.stderr));
                s
            },
            timed_out: false,
        }),
        Ok(Err(e)) => Err(format!("`{prog}` failed to run: {e}")),
        Err(_) => {
            // Timeout: kill the whole process group, then report absence of
            // evidence (not a failure — see `VerifyVerdict::exit_code`).
            kill_process_group(pid);
            Ok(RunOutcome {
                exit_code: None,
                combined_output: format!(
                    "`{prog}` timed out after {}s and was killed",
                    timeout.as_secs()
                ),
                timed_out: true,
            })
        }
    }
}

/// Kill a process group. `process_group(0)` above makes the child its own group
/// leader, so its pgid is its pid.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    unsafe {
        // SAFETY: `pid` came from `Child::id()`, so the process existed; a
        // negative-pid `killpg` addresses the group it leads. Racing a
        // reaping here is benign — the call just fails with ESRCH.
        libc::killpg(pid as i32, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Split `"cargo check"` / `"npx tsc --noEmit"` into (program, args) for
/// [`tokio::process::Command`].
///
/// Whitespace-splitting is enough for the registry's own command strings and
/// deliberately not a shell parser: a command needing quoting belongs in the
/// project's own verify config, not in a table entry.
fn split_command(command: &str) -> Option<(&str, Vec<&str>)> {
    let mut parts = command.split_whitespace();
    let prog = parts.next()?;
    Some((prog, parts.collect()))
}

#[async_trait]
impl Tool for VerifyTool {
    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Run the project's compile-level check (e.g. `cargo check`, `tsc --noEmit`) and report \
         whether the code still builds. Use after editing code and before reporting a task \
         done. The command comes from the project itself — its own config, then a slot it \
         already fills in, then the language default; you do not pass one. Result is the \
         compiler's real exit code, not a summary of its output. For a project with no \
         compile layer (plain JS, Python, no build script) this reports that there is nothing \
         to verify rather than pretending to have checked."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative file or directory, used to check \
                                    that the scope is code before running. The command itself is \
                                    workspace-level. Omit for a whole-project check."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: self.description().to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        // 1) Is there anything here to verify? A docs-only scope must not spawn
        //    a compiler, and must not record a green light that says nothing
        //    about the change the model just made.
        if let Some(scope) = non_empty_arg(args, "path") {
            let scoped = self.workspace_root.join(&scope);
            if !scope_contains_code(&scoped) {
                return Ok(vec![Content::text(format!(
                    "非代码改动，无需验证（`{scope}` 无源码/构建清单）。\
                     本工作区的编译级验证命令是：{}",
                    describe_workspace_command(&self.workspace_root)
                ))]);
            }
        }

        // 2) Which command? Resolved from the project itself — its own config,
        //    then a slot it already fills in, then the language table. Never
        //    from the model: a command the model could pick is a green light it
        //    could mint.
        let Some(resolved) = lang::resolve_verify_command(&self.workspace_root) else {
            self.ledger.lock().unwrap().set_compile_layer(false);
            return Ok(vec![Content::text(
                "本工作区无编译级验证（该语言无编译层，项目也未提供验证命令），无法给出 L2 证据。\
                 若项目自带构建脚本，可在 `.phimint.toml` 写 `verify_command`，\
                 或用 `execute_command` 显式执行。"
                    .to_string(),
            )]);
        };
        let command = resolved.command;

        let Some((prog, cmd_args)) = split_command(&command) else {
            return Ok(vec![Content::text(format!(
                "[Error]: 验证命令无法拆成程序 + 参数：`{command}`"
            ))]);
        };

        // 3) Run it. Direct spawn, no shell — see the module docs.
        let run = match run_command(prog, &cmd_args, &self.workspace_root, self.timeout).await {
            Ok(r) => r,
            Err(e) => {
                // The run never happened: record nothing. An absence of
                // evidence must not be recorded as evidence.
                return Ok(vec![Content::text(format!("[Error]: {e}"))]);
            }
        };

        // 4) Parse for the reader. The counts are advisory — see
        //    `VerifyVerdict::passed`.
        let parsed = parse::parse_errors(&command, &run.combined_output);
        let verdict = VerifyVerdict {
            kind: VerifyKind::Check,
            command: command.clone(),
            exit_code: run.exit_code,
            error_count: parsed.error_count,
            warning_count: parsed.warning_count,
            summary: parsed.summary.clone(),
        };
        let passed = verdict.passed();

        // 5) Record. This is the only place a green light can come from, which
        //    is why the gate never has to trust a model's claim.
        self.ledger.lock().unwrap().record_verify(verdict);

        Ok(vec![Content::text(render(
            &command,
            resolved.source.label(),
            run.exit_code,
            run.timed_out,
            passed,
            &parsed,
        ))])
    }
}

/// A non-blank string argument, if present.
fn non_empty_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Whether a scope (file or directory) contains anything the compile layer
/// would care about. Directories are checked shallowly-plus-recursively through
/// the language registry's own notion of code.
fn scope_contains_code(path: &Path) -> bool {
    if path.is_file() {
        return lang::is_code_path(&path.to_string_lossy());
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || lang::SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            if scope_contains_code(&child) {
                return true;
            }
        } else if lang::is_code_path(&child.to_string_lossy()) {
            return true;
        }
    }
    false
}

/// Human-readable "what would run here", for the non-code refusal message.
fn describe_workspace_command(root: &Path) -> String {
    match lang::resolve_verify_command(root) {
        Some(v) => format!("`{}`（来源：{}）", v.command, v.source.label()),
        None => "（无）".to_string(),
    }
}

/// The model-facing result. Terse on success, actionable on failure.
///
/// A run that never produced an exit code is reported as *absence of evidence*
/// — "killed", not "failed" — so a timeout cannot be mistaken for a build
/// error and cannot be mistaken for a pass either.
///
/// `source` names where the command came from (project config / project
/// convention / language table). A human reading the transcript needs it: it is
/// the difference between "it ran your build" and "it ran a guess".
fn render(
    command: &str,
    source: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    passed: bool,
    parsed: &parse::ParsedErrors,
) -> String {
    if passed {
        let mut s = format!("✓ `{command}` passed (exit 0) · {source}");
        if parsed.warning_count > 0 {
            s.push_str(&format!(", {} warnings", parsed.warning_count));
        }
        return s;
    }
    let mut s = match exit_code {
        Some(code) => format!("✗ `{command}` failed (exit {code}) · {source}"),
        None if timed_out => {
            format!("✗ `{command}` timed out and was killed — no evidence")
        }
        None => format!("✗ `{command}` was killed before it finished — no evidence"),
    };
    if parsed.error_count > 0 || parsed.warning_count > 0 {
        s.push_str(&format!(
            "\n{} errors, {} warnings:",
            parsed.error_count, parsed.warning_count
        ));
    }
    if !parsed.summary.is_empty() {
        s.push('\n');
        s.push_str(&parsed.summary);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_takes_the_first_word_as_the_program() {
        assert_eq!(split_command("cargo check"), Some(("cargo", vec!["check"])));
        assert_eq!(
            split_command("npx tsc --noEmit"),
            Some(("npx", vec!["tsc", "--noEmit"]))
        );
        assert_eq!(split_command("   "), None);
    }

    #[test]
    fn split_command_is_not_a_shell_parser() {
        // A command needing quoting must not be smuggled through the table.
        // The point is that no `sh` ever sees this string.
        assert_eq!(
            split_command("cargo check 2>&1 | tail -5"),
            Some(("cargo", vec!["check", "2>&1", "|", "tail", "-5"]))
        );
    }

    #[tokio::test]
    async fn run_command_reads_the_real_exit_code() {
        // The whole point of spawning directly: no shell can launder this.
        let out = run_command("false", &[], Path::new("/"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(1));
        assert!(!out.timed_out);

        let out = run_command("true", &[], Path::new("/"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn run_command_does_not_launder_through_a_pipe() {
        // A shell would report `tail`'s exit code here. A direct spawn cannot
        // express a pipe at all — it tries to exec a program literally named
        // `2>&1`, which is the failure mode we want rather than a false pass.
        let out = run_command(
            "sh",
            &["-c", "false 2>&1 | tail -5"],
            Path::new("/"),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        // `sh` itself exits 0 here — which is exactly why the tool must never
        // build its argv through `sh`. Contrast with the check below.
        assert_eq!(out.exit_code, Some(0));

        let out = run_command("false", &[], Path::new("/"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(1), "the direct spawn reports the truth");
    }

    #[tokio::test]
    async fn a_timeout_is_absence_of_evidence_not_a_pass() {
        let out = run_command("sleep", &["5"], Path::new("/"), Duration::from_millis(200))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, None, "a killed run must not report success");
    }

    #[test]
    fn scope_rejects_a_docs_only_directory() {
        let root = std::env::temp_dir().join("phimint_verify_scope_docs");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# hi").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();

        assert!(!scope_contains_code(&root.join("docs")));
        assert!(!scope_contains_code(&root.join("README.md")));
        // The whole root has no code either.
        assert!(!scope_contains_code(&root));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_accepts_source_and_build_manifests() {
        let root = std::env::temp_dir().join("phimint_verify_scope_code");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();

        assert!(scope_contains_code(&root.join("src/a.rs")));
        assert!(scope_contains_code(&root.join("Cargo.toml")));
        assert!(scope_contains_code(&root));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_skips_build_output_directories() {
        let root = std::env::temp_dir().join("phimint_verify_scope_skip");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("target/debug/gen.rs"), "fn x() {}").unwrap();

        // Only code lives under `target/`, which is build output — the scope is
        // code-free.
        assert!(!scope_contains_code(&root));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_is_terse_on_success_and_actionable_on_failure() {
        let ok = parse::ParsedErrors {
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        };
        assert_eq!(
            render("cargo check", "项目约定", Some(0), false, true, &ok),
            "✓ `cargo check` passed (exit 0) · 项目约定"
        );

        let bad = parse::ParsedErrors {
            error_count: 2,
            warning_count: 1,
            summary: "src/a.rs:1:1  error: boom\n".to_string(),
        };
        let s = render("cargo check", "语言命令表", Some(101), false, false, &bad);
        assert!(s.contains("failed (exit 101)"), "{s}");
        assert!(s.contains("2 errors"), "{s}");
        assert!(s.contains("src/a.rs:1:1"), "{s}");
        assert!(s.contains("语言命令表"), "{s}");
    }

    #[test]
    fn render_calls_a_timeout_absence_of_evidence() {
        // A timeout must read as "no evidence", not as a build error — the
        // gate treats the two differently and so should the message.
        let ok = parse::ParsedErrors {
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        };
        let timed_out = render("cargo check", "项目约定", None, true, false, &ok);
        assert!(timed_out.contains("timed out"), "{timed_out}");
        assert!(timed_out.contains("no evidence"), "{timed_out}");
        assert!(!timed_out.contains("failed (exit"), "{timed_out}");

        let killed = render("cargo check", "项目约定", None, false, false, &ok);
        assert!(killed.contains("killed"), "{killed}");
        assert!(killed.contains("no evidence"), "{killed}");
    }
}
