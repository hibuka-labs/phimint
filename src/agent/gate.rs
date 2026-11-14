//! The verify gate: the "must-verify-before-done" half of the verification loop.
//!
//! The `verify` tool (in `crate::tools::verify`) supplies **evidence**; this
//! middleware supplies the **policy** that a hand-back without fresh evidence is
//! not allowed to stand unmarked. It is a consumer-side middleware — it uses
//! only the framework's neutral seams (`Middleware::on_post_llm` /
//! `on_user_message`) and pushes no "thou must verify" policy into the engine
//! (design §8.3: other businesses don't need this).
//!
//! Three rules dominate everything here.
//!
//! **1. Freshness, not presence.** The question is never "was `verify` called
//! at some point" — that is the exact bug that made the original gate lie
//! (`src/gate.rs` at `93ae5e6` cleared its dirty flag on *any* verify call, pass
//! or fail). The question is "is there a green light *newer than* the last
//! change", which [`EvidenceLedger::fresh_green`] answers.
//!
//! **2. Intercept once, then degrade to a visible marker.** `max_nudges` is 1
//! (the user's requirement: "verify 拦截一次就够了"). A second nag says the same
//! sentence twice and costs a turn the model could have spent compiling. After
//! that single interception the gate never blocks again — it appends an
//! [`Unverified::warning`] line to the reply so the *human* sees the gap. The
//! marker is a footnote, not a verdict: it distinguishes "lazy" (never checked)
//! from "tried and lost" (checked and failed) for free, because the ledger knows.
//!
//! **3. Never a false green.** Every misjudgement must fall on the side of
//! "keep blocking", never on the side of "passed". That is why edits are
//! recorded from *intent* (a write that failed still counts as an edit — a
//! spurious nag, not a spurious pass) and why an edit arriving in the same turn
//! as a `verify` is deferred to the next LLM turn, so that verify's evidence is
//! never credited with covering an edit that raced it.
//!
//! ### Why this does not fight `DefaultGuard`
//!
//! The seam is structurally in our favour. `handle_post_llm_result` handles
//! `follow_up_message` at `turn_dispatch.rs:242-247` — *before* the four-state
//! dispatch. So the turn on which this middleware sets `follow_up_message`
//! returns `TurnFlow::Continue` and **never reaches `guard.on_turn()`**. The two
//! cannot nudge on the same turn. The residual overlap is only on the degraded
//! turn (where we no longer set `follow_up_message` and the guard is free to do
//! its own completeness work) — and there we only append a marker, which is not
//! a nudge at all. See `notes/verify-gate-design.md` §8.1.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_intel::lang;
use phi_agent::{AgentResult, Middleware, PostLlmCtx, UserMessageCtx};
use serde_json::Value;

use crate::verify_ledger::{EvidenceLedger, Unverified, VerifyKind};

/// Tools that can change source. `code_cell` is here because a CodeAct cell's
/// filesystem side effects are opaque — the misjudgement direction must not be
/// a false pass (design D2). `execute_command` is here for the same reason, but
/// only when the command may actually write: a shell edit (`sed -i`,
/// `cat > file`) is invisible at the tool boundary, so it must count as a
/// possible edit rather than ride on an older verify, while a read-only `ls`
/// or `git status` must not dirty the gate at all (review finding
/// p2/adversarial-native#1). The filter is [`shell_may_mutate`].
const DIRTY_TOOLS: &[&str] = &["write_file", "edit_file", "code_cell", "execute_command"];

/// The multi-agent verbs. A write-capable child edits from its own engine: no
/// parent action carries those writes and the ledger has no child feed
/// (review finding v11/targeted#1), so every delegation verb counts as a
/// possible edit — rule 3's conservative direction, the same treatment
/// unreadable argv gets. Residual (PR notes): a child that keeps writing
/// after the parent's last delegation verb is covered only until the next
/// verify; forwarding child write intents into the ledger is the deep fix.
///
/// Mirrors `phi-kernel-tools`'s `create_all_tools` — `wait_agent` and
/// `followup_task` were dropped from that factory (§8.3, push-based result
/// delivery) and are tested as absent there. `list_agents` is a read, not a
/// delegation verb. Pinned by `the_delegation_verbs_are_the_ones_the_factory_actually_produces`.
const MULTI_AGENT_TOOLS: &[&str] = &["spawn_agent", "send_message", "close_agent"];

/// Shell programs whose every invocation is a read — no flag writes a file
/// without a redirection (which `command::has_mutation_marker` catches
/// anyway).
///
/// Deliberately closed and small. Interpreters (`sh`, `python`, `node`, `awk`,
/// `sed`, `sort -o`, `env`, `xargs`, `find -delete`) are absent from the plain
/// list below: each can write or run something with no marker in the string, so
/// they are decided by their *form* in [`segment_may_mutate`] instead. A reader
/// missing from here just costs a nag; a writer wrongly on it would be a false
/// pass (rule 3).
const SHELL_READ_ONLY: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "pwd",
    "which",
    "stat",
    "du",
    "df",
    "date",
    "diff",
    "cmp",
    "file",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "echo",
    "printf",
    "true",
    "false",
    "whoami",
    "uname",
    "hostname",
    "id",
    "ps",
    "lsof",
    "jq",
    "cut",
    "tr",
    "uniq",
    "cd",
    "test",
    "[",
    "read",
    "export",
    "unset",
    "printenv",
    "sleep",
    "seq",
    "comm",
    "column",
    "strings",
    "xxd",
    "od",
    "hexdump",
    "md5",
    "shasum",
    "sha256sum",
    "nl",
    "tac",
    "rev",
    "paste",
    "join",
    "expand",
    "unexpand",
    "tree",
    "expr",
    "dirname",
];

/// Words that introduce no command of their own: the loop/conditional
/// scaffolding `sh` needs before the real command word turns up, plus the
/// `!` of a negated pipeline. Neither writes, and reading one of them as the
/// command head turned `for f in …; do cat "$f"; done` into a supposed writer.
const SHELL_SCAFFOLDING: &[&str] = &[
    "do", "done", "then", "else", "elif", "fi", "esac", "{", "}", "(", ")", "!", "if", "for",
    "while", "until", "case", "time",
];

/// `git` subcommands that only read. Everything else (`config`, `stash`,
/// `checkout`, `reset`, `push`, …) mutates and must dirty.
const GIT_READ_SUBS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "describe",
    "rev-parse",
    "ls-files",
    "grep",
    "shortlog",
    "rev-list",
    "merge-base",
    "ls-remote",
    "cat-file",
    "ls-tree",
    "show-ref",
    "count-objects",
    "reflog",
    "whatchanged",
    "cherry",
    "name-rev",
    "range-diff",
    "check-ignore",
    "check-attr",
    "summary",
    "help",
    "version",
    "var",
];

/// `git` subcommands that list in one form and write in another — `git branch`
/// reads, `git branch -d` deletes. Read only when the invocation carries a
/// listing flag and none of the writing ones.
const GIT_MIXED_SUBS: &[GitMixed] = &[
    GitMixed {
        sub: "branch",
        reads: &[
            "-l",
            "--list",
            "-a",
            "--all",
            "-v",
            "-vv",
            "--verbose",
            "-n",
            "--show-current",
            "--contains",
            "--merged",
            "--no-merged",
            "--points-at",
        ],
        writes: &[
            "-d",
            "-D",
            "-m",
            "-M",
            "-c",
            "-C",
            "--delete",
            "--move",
            "--copy",
            "--edit-description",
            "--set-upstream-to",
            "--unset-upstream",
            "--track",
        ],
    },
    GitMixed {
        sub: "tag",
        reads: &["-l", "--list", "--contains", "--merged", "--points-at"],
        writes: &[
            "-d",
            "-D",
            "--delete",
            "-a",
            "--annotate",
            "-s",
            "--sign",
            "-f",
            "--force",
            "-m",
            "--message",
            "-u",
            "--local-user",
            "--file",
        ],
    },
    GitMixed {
        sub: "remote",
        reads: &["-v", "-vv", "--verbose", "-n", "show", "get-url"],
        writes: &[
            "add",
            "remove",
            "rename",
            "set-url",
            "prune",
            "update",
            "set-head",
            "--add",
            "--delete",
            "--push",
            "--set-url",
        ],
    },
];

/// A `git` subcommand whose read and write forms are told apart by their flags.
struct GitMixed {
    sub: &'static str,
    reads: &'static [&'static str],
    writes: &'static [&'static str],
}

// The mutation fragments live in `crate::command` (`FLAG_MARKERS`, shared
// with the approval classifier); [`shell_may_mutate`] reads them through
// `command::has_mutation_marker`.

/// Tools that produce compile-level evidence. The `verify` tool records into the
/// ledger itself when it runs; this list is what tells the gate "a verification
/// is in flight", which is what defers same-turn edit recording (rule 3).
///
/// `merge` used to sit here — it was the tool name at `218def4` (2026-08-31,
/// "remove standalone verify tool, inline into merge") — but the tool is
/// `verify` again and nothing else produces compile-level evidence.
/// Pinned by `the_verification_verbs_are_the_ones_the_registry_actually_produces`.
const VERIFY_TOOLS: &[&str] = &["verify"];

/// Gate configuration.
pub struct VerifyGateConfig {
    /// Interceptions allowed per user message. **1** — see rule 2 above and
    /// `notes/verify-gate-design.md` D1.
    pub max_nudges: usize,
    /// Self-identifying header on every interception. User-role (the loop's
    /// only neutral seam for continuing a turn) but it names itself, so both
    /// the completeness judge and a human reading the trace can tell it apart
    /// from real input (design §8.1).
    ///
    /// A header, not a whole message: what is actually wrong depends on the
    /// ledger's verdict ("you never ran it" and "you ran it and it failed" need
    /// different next steps), so the body comes from [`Unverified::nudge`] and
    /// this only says who is speaking.
    pub nudge_header: String,
    /// Whether writing is possible at all right now. In `deny` mode the write
    /// tools are refused outright, so nothing can go dirty and the gate would
    /// only ever be noise — it turns itself off instead.
    ///
    /// A **probe**, not a flag: the approval mode is a live switch (Shift+Tab
    /// cycles `auto` ⇄ `ask` mid-session), so answering this at construction
    /// time would go stale. Called once per LLM turn.
    pub writes_possible: WritesProbe,
}

/// Whether the run can currently write at all. See
/// [`VerifyGateConfig::writes_possible`].
pub type WritesProbe = Arc<dyn Fn() -> bool + Send + Sync>;

impl Default for VerifyGateConfig {
    fn default() -> Self {
        Self {
            max_nudges: 1,
            nudge_header: "[gate] This is not user input - it is a gate reminder.".to_string(),
            writes_possible: Arc::new(|| true),
        }
    }
}

/// Per-user-message gate state. Reset by [`VerifyGateMiddleware::on_user_message`].
#[derive(Debug, Default)]
struct GateState {
    /// Interceptions spent in this user message.
    nudges: usize,
    /// Paths seen in write-tool *intent* this turn but not yet counted as edits.
    ///
    /// Deferred one LLM turn so that a `verify` issued in the same turn as the
    /// write is never credited with covering it (rule 3). Flushed at the top of
    /// the next [`on_post_llm`](Middleware::on_post_llm), and folded into the
    /// ledger at the user boundary rather than dropped.
    deferred_edits: Vec<PathBuf>,
}

/// The verify gate (see the module docs).
pub struct VerifyGateMiddleware {
    config: VerifyGateConfig,
    ledger: Arc<Mutex<EvidenceLedger>>,
    state: Mutex<GateState>,
}

impl VerifyGateMiddleware {
    pub fn new(config: VerifyGateConfig, ledger: Arc<Mutex<EvidenceLedger>>) -> Self {
        Self {
            config,
            ledger,
            state: Mutex::new(GateState::default()),
        }
    }
}

#[async_trait]
impl Middleware for VerifyGateMiddleware {
    /// Each user message is one "edit → verify" cycle and carries exactly one
    /// interception budget (R1 of `notes/verify-gate-design.md` D1′). The R3
    /// per-green reset is gone: re-arming on every passing verify made the nag
    /// unbounded inside one user message (review finding p2/outside-voice#3).
    async fn on_user_message(&self, _ctx: &mut UserMessageCtx) -> AgentResult<()> {
        // Fold any still-deferred edits into the ledger before resetting.
        // Discarding them here would let an earlier verify stay "fresh" over a
        // write it never covered — the false-green window the rule-3 deferral
        // exists to prevent. A spurious nag is acceptable; a spurious pass is not.
        let leftover = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            st.nudges = 0;
            std::mem::take(&mut st.deferred_edits)
        };
        if !leftover.is_empty() {
            let mut ledger = self.ledger.lock().expect("ledger lock poisoned");
            ledger.record_edit(&leftover);
        }
        Ok(())
    }

    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !(self.config.writes_possible)() {
            return Ok(());
        }

        // ── 1. Fold in edits deferred from the previous turn. ──────────────
        // Doing this first is what makes an edit that raced a same-turn `verify`
        // land *after* that verify's evidence in the ledger, so the evidence
        // cannot cover it.
        let (pending_edits, in_flight_verify) = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            let mut edits: Vec<PathBuf> = std::mem::take(&mut st.deferred_edits);

            // ── 2. Classify this turn's actions. ───────────────────────────
            let mut saw_verify = false;
            for action in ctx.actions() {
                let name = action.name.as_str();
                if VERIFY_TOOLS.contains(&name) {
                    saw_verify = true;
                } else if name == "execute_command" {
                    // Shell dirties only when the command may write
                    // ([`shell_may_mutate`]); unreadable argv is the
                    // conservative direction (rule 3).
                    let may_write = match command_arg(action.raw_args().as_ref()) {
                        Some(c) => shell_may_mutate(&c),
                        None => true,
                    };
                    if may_write {
                        edits.push(PathBuf::from("."));
                    }
                } else if MULTI_AGENT_TOOLS.contains(&name) {
                    // Delegated work lands outside this action stream — see
                    // [`MULTI_AGENT_TOOLS`]. The conservative direction: a
                    // delegation verb dirties like an unreadable command.
                    edits.push(PathBuf::from("."));
                } else if DIRTY_TOOLS.contains(&name) {
                    if let Some(p) = edit_path(action.raw_args().as_ref()) {
                        if lang::is_code_path(&p) {
                            edits.push(PathBuf::from(p));
                        }
                    } else {
                        // No resolvable path (a CodeAct cell, or arguments we
                        // could not read). Conservative: count it as an edit.
                        edits.push(PathBuf::from("."));
                    }
                }
            }

            // A verify in *this* turn executes after this hook, so its evidence
            // cannot cover this turn's edits. Defer them to the next turn.
            if saw_verify {
                st.deferred_edits.extend(edits);
                (Vec::new(), true)
            } else {
                (edits, false)
            }
        };

        {
            let mut ledger = self.ledger.lock().expect("ledger lock poisoned");
            if !pending_edits.is_empty() {
                ledger.record_edit(&pending_edits);
            }
        }

        // The nudge budget resets only at the user boundary (R1). A passing
        // verify settles the batch by making `unverified()` return `None`
        // below — it does NOT re-arm the budget: `max_nudges` is per user
        // message, and the old per-green reset made the nag unbounded inside
        // one message (review finding p2/outside-voice#3).

        // A verification in flight: the model is already doing the right thing.
        // Never intercept a turn that ends in a verify call.
        if in_flight_verify {
            return Ok(());
        }

        // ── 3. Is this a hand-back? ───────────────────────────────────────
        // Only a text-only turn with no pending actions is the model saying
        // "I'm done". Anything else is mid-work.
        if ctx.has_action_intent() || ctx.full_text.is_empty() {
            return Ok(());
        }

        // ── 4. Does anything need verifying at all? ───────────────────────
        // No edits → the question never comes up. A workspace with no compile
        // layer → nothing to verify *against*; nagging would be noise and
        // claiming evidence would be a lie. Say neither.
        //
        // Resolved once, here, and carried into both branches below: the
        // interception has to name *why* it is blocking (see
        // [`Unverified::nudge`]), and asking the ledger twice would risk the
        // two halves of one decision disagreeing.
        let unv = {
            let ledger = self.ledger.lock().expect("ledger lock poisoned");
            ledger.unverified(VerifyKind::Check)
        };
        let unv = match unv {
            None => return Ok(()), // fresh green light — nothing to do
            Some(Unverified::NoCompileLayer) => return Ok(()),
            Some(Unverified::NeverChecked { edited, .. }) if edited.is_empty() => {
                return Ok(());
            }
            Some(unv) => unv,
        };

        // ── 5. Intercept once, then degrade to a visible marker. ──────────
        let nudges = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            if st.nudges < self.config.max_nudges {
                st.nudges += 1;
                st.nudges
            } else {
                0
            }
        };

        if nudges > 0 {
            tracing::info!(
                session_id = ctx.session_id.id,
                turn = ctx.turn_count,
                nudge = nudges,
                max_nudges = self.config.max_nudges,
                reason = unv.nudge(),
                "verify gate: suppressing an unverified hand-back"
            );
            // Drop the premature "done" and force one more turn. Because
            // `follow_up_message` short-circuits at `turn_dispatch.rs:242`, the
            // guard is skipped for this turn entirely — no double nudge.
            ctx.skip_push = true;
            ctx.follow_up_message = Some(format!("{} {}", self.config.nudge_header, unv.nudge()));
            return Ok(());
        }

        // Already spent the one interception. Let the reply through and append
        // the warning where the *human* will read it. Not a nudge, so the guard
        // is free to do its own work on this turn.
        {
            let mut ledger = self.ledger.lock().expect("ledger lock poisoned");
            ledger.clear_pending();
        }
        let warning = unv.warning();
        if marker_already_present(&ctx.full_text, &warning) {
            // The model can copy the marker — it is in its own context from the
            // reply we degraded a moment ago, and the first smoke run did
            // exactly that, producing two footnotes for one problem. One is
            // enough.
            tracing::warn!(
                session_id = ctx.session_id.id,
                turn = ctx.turn_count,
                "verify gate: reply already carries the marker, not appending again"
            );
            return Ok(());
        }
        tracing::warn!(
            session_id = ctx.session_id.id,
            turn = ctx.turn_count,
            "verify gate: degraded to a marker after max nudges"
        );
        ctx.full_text.push_str("\n\n");
        ctx.full_text.push_str(&warning);

        Ok(())
    }
}

/// Whether the reply already carries this exact marker.
///
/// Matching is on whitespace-normalised text so a re-wrapped copy still counts.
/// Anything *other* than a faithful copy — a different warning, a warning about
/// something else — is left alone and ours is appended anyway: a missing
/// correct marker is worse than a duplicated one, so this deliberately errs
/// towards appending.
fn marker_already_present(text: &str, warning: &str) -> bool {
    normalize(text).contains(&normalize(warning))
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `path` argument of a write tool call, if it names one.
///
/// Returns `None` for a payload we cannot read or one with no `path` (a CodeAct
/// cell carries `{"code": …}`) — callers treat that as an edit rather than
/// guessing it away.
fn edit_path(raw_args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw_args).ok()?;
    v.get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// The `command` argument of an `execute_command` call, if it names one.
fn command_arg(raw_args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw_args).ok()?;
    v.get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Whether a shell command may change source — the `execute_command` filter
/// behind [`DIRTY_TOOLS`] (review finding p2/adversarial-native#1).
///
/// Closed rule, erroring towards "may mutate": every `;`/`|`/`&`/newline
/// segment must have a head that reads (see [`segment_may_mutate`]), and the
/// raw string must carry no `command::FLAG_MARKERS` fragment and no redirect
/// into a file. The splitter is the shared `command::split_segments` — `sh -c`
/// treats a newline and a lone `&` as separators exactly like `;`, so a private
/// splitter that misses one lets `ls\nrm -rf src/` read as the reader `ls` and
/// leave the ledger green over a shell edit (review findings v11/red-team#1,
/// v11/adversarial-native#2).
///
/// Unknown heads — interpreters (`sh`, `python`, `node`), `tee`, `make` — are
/// writers until proven otherwise, so a writer misread as a reader is
/// impossible and a reader misread as a writer just costs a nag. That nag is
/// not free, though: a survey that gets called "code changed" sends the model
/// off hunting for an edit it never made (dogfood 20261115_e37212d2), which is
/// why the readers that only write under a flag — `sed -i`, `sort -o`,
/// `cargo fmt`, `find -delete` — are judged by their form rather than excluded.
fn shell_may_mutate(command: &str) -> bool {
    if crate::command::has_mutation_marker(command) {
        return true;
    }
    crate::command::split_segments(command)
        .into_iter()
        .any(|seg| segment_may_mutate(&seg))
}

/// One command segment: whether it may change source.
///
/// The head word is the thing that acts, and it is found after the shell's
/// scaffolding and any `VAR=value` prefix — neither of which is a command. What
/// is left is either a plain reader or one of the utilities that reads until
/// you hand it the flag that writes; those are judged by their form here rather
/// than excluded outright, because excluding them is what made a read-only
/// survey dirty.
fn segment_may_mutate(segment: &str) -> bool {
    let words = crate::command::words(segment);
    let mut it = words
        .iter()
        .enumerate()
        .skip_while(|(_, w)| SHELL_SCAFFOLDING.contains(&w.as_str()) || is_assignment(w));
    let Some((idx, head)) = it.next() else {
        return false; // nothing but scaffolding: `done`, `}`, `X=1`
    };
    let after = &words[idx + 1..];

    if head.as_str() == "git" {
        return git_may_mutate(after);
    }
    // `xargs` runs the program named after it; everything before that is flags
    // (`xargs -0 wc -l` reads, `xargs rm` does not).
    let (prog, rest): (&str, &[String]) = if head.as_str() == "xargs" {
        match after.iter().position(|w| !w.starts_with('-')) {
            Some(p) => (
                after[p].rsplit('/').next().unwrap_or(&after[p]),
                &after[p + 1..],
            ),
            None => return false,
        }
    } else {
        (head.rsplit('/').next().unwrap_or(head.as_str()), after)
    };

    match prog {
        // Each of these reads until it is handed the flag that writes, and
        // `has_mutation_marker` has already ruled out redirections and the
        // writing `find` primaries on the way in.
        "sed" => words.iter().any(|w| w.starts_with("-i")),
        "sort" => words.iter().any(|w| w == "-o" || w.starts_with("--output")),
        "awk" | "gawk" | "mawk" | "nawk" => segment.contains("system("),
        "curl" | "wget" => words
            .iter()
            .any(|w| *w == "-o" || *w == "-O" || w.starts_with("-o") || w.starts_with("--output")),
        "find" => false,
        "cargo" => cargo_may_mutate(rest),
        _ => !SHELL_READ_ONLY.contains(&prog),
    }
}

/// A `VAR=value` prefix: it sets up the environment for the command that
/// follows and never writes on its own. Only ever the *head* is checked here,
/// so a `foo=bar` appearing as an argument is left alone.
fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a `git` invocation may change the working tree.
fn git_may_mutate(rest: &[String]) -> bool {
    let Some(sub) = rest.first() else {
        return true; // bare `git`
    };
    let sub = sub.as_str();
    if GIT_READ_SUBS.contains(&sub) {
        return false;
    }
    let Some(mixed) = GIT_MIXED_SUBS.iter().find(|m| m.sub == sub) else {
        return true;
    };
    let has = |flags: &&[&str]| rest.iter().any(|a| flags.contains(&a.as_str()));
    has(&mixed.writes) || !has(&mixed.reads)
}

/// `cargo` writes source through a handful of subcommands (`fmt` without
/// `--check`, `fix`, `add`, `new`); the build loop is the reader. The
/// subcommand is the first word that is neither a flag nor a `+toolchain`.
fn cargo_may_mutate(rest: &[String]) -> bool {
    let Some(sub) = rest
        .iter()
        .find(|w| !w.starts_with('-') && !w.starts_with('+'))
    else {
        return true; // bare `cargo`: no saying what it will do
    };
    if matches!(
        sub.as_str(),
        "check" | "build" | "test" | "clippy" | "doc" | "bench" | "metadata" | "tree"
    ) {
        return false;
    }
    // `cargo fmt` rewrites the files in place unless told to only look.
    if sub == "fmt" {
        return !rest.iter().any(|w| w == "--check");
    }
    true
}

#[cfg(test)]
mod tests;
