//! The evidence ledger behind the verify gate.
//!
//! Two things write to it and one thing reads it:
//!
//! - the `verify` tool records what it ran and whether it passed (it is the
//!   only component that knows the exit code);
//! - the gate records edits (it is the only component that sees tool-call
//!   intent);
//! - the gate reads [`EvidenceLedger::unverified`] at hand-back time.
//!
//! The whole point is **freshness**, not coverage. The gate asks one question —
//! "is there a green light *newer than* the last change?" — which is what makes
//! the natural workflow (edit five files, compile once at the end) pass with
//! zero extra cost, while a verify that predates the last edit is correctly
//! treated as stale. This is the check the original gate never made and the
//! reason it could claim "verified" over code that did not compile.
//!
//! Nothing here runs commands or talks to the LLM. It is state plus predicates.

use std::path::PathBuf;

/// What a verification run was checking for. The gate treats the two
/// independently — a green `Check` does not stand in for a green `Test` and
/// vice versa (`notes/verify-gate-design.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyKind {
    /// Compile-level: does the project build at all.
    Check,
    /// Test-level: do the tests pass.
    Test,
}

impl VerifyKind {
    pub fn label(self) -> &'static str {
        match self {
            VerifyKind::Check => "check",
            VerifyKind::Test => "test",
        }
    }
}

/// The outcome of one verification run, as the tool produced it.
#[derive(Debug, Clone)]
pub struct VerifyVerdict {
    pub kind: VerifyKind,
    /// The command that was spawned, for the audit trail and the warning text.
    pub command: String,
    /// `None` when the process was killed or timed out — that is **not** a
    /// pass and **not** a fail, it is the absence of evidence. See
    /// [`EvidenceLedger::unverified`], which reports it as such rather than
    /// folding it into "failed".
    pub exit_code: Option<i32>,
    pub error_count: usize,
    pub warning_count: usize,
    pub summary: String,
}

impl VerifyVerdict {
    /// Whether this run may count as a green light.
    ///
    /// Only the exit code decides. Parsed error counts are for the reader and
    /// may be wrong (an unrecognised toolchain format, a wrapped build); letting
    /// them flip the verdict would make the gate lie in whichever direction the
    /// parser happens to be bad at.
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// One entry in the ledger, with the sequence number that makes it stale or
/// fresh.
#[derive(Debug, Clone)]
pub struct EvidenceRecord {
    pub seq: u64,
    pub verdict: VerifyVerdict,
}

/// Why a hand-back cannot be blessed, in a form the gate can turn into a
/// one-line warning the human will actually read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unverified {
    /// The workspace has no compile layer (vanilla JS, Python). Nothing was
    /// wrong; there is simply nothing to verify against.
    NoCompileLayer,
    /// Nothing was run after the last edit.
    NeverChecked { edited: Vec<PathBuf> },
    /// A check ran and was killed or timed out — no evidence either way.
    TimedOut {
        edited: Vec<PathBuf>,
        command: String,
    },
    /// A check ran and failed.
    CheckFailed {
        edited: Vec<PathBuf>,
        command: String,
        exit_code: Option<i32>,
        error_count: usize,
    },
    /// A check ran and passed, but *before* the last edit — the green light is
    /// about code that no longer exists.
    Stale {
        edited: Vec<PathBuf>,
        command: String,
    },
}

impl Unverified {
    /// What the gate says when it **refuses** a hand-back.
    ///
    /// The wording has to follow the verdict. "Please run `verify`" is wasted
    /// breath — and a wasted turn — when the model already ran it and it
    /// failed, which is exactly what the first smoke run produced (see
    /// `notes/verify-gate-design.md` §8.5). Lazy, lost, stale and killed are
    /// four different problems with four different next steps, and the ledger
    /// is the only thing that knows which one this is.
    ///
    /// Returns the body only; the gate prefixes its self-identifying header.
    pub fn nudge(&self) -> String {
        match self {
            Unverified::NoCompileLayer => {
                "本工作区无编译级验证（该语言无编译层，项目也未提供验证命令），无需验证即可汇报。"
                    .to_string()
            }
            Unverified::NeverChecked { edited } => {
                format!(
                    "代码改动后未见绿灯验证 {} —— 请先执行 `verify`（或项目的编译命令）\
                     确认通过，再重新汇报。",
                    edited_files(edited)
                )
            }
            Unverified::TimedOut { edited, command } => {
                format!(
                    "`{command}` 上次超时被杀，没有留下证据 {} —— 请重新执行 `verify` \
                     拿到退出码，再汇报。",
                    edited_files(edited)
                )
            }
            Unverified::CheckFailed {
                edited,
                command,
                exit_code,
                error_count,
            } => {
                format!(
                    "`{command}` 已经跑过，但未通过（{}，{} 个错误） {} —— 这不是「没验」，\
                     是「验了没过」。请修复后重新验证通过再汇报；若判断本次可以带错交付，\
                     请在汇报里说明理由。",
                    exit_code_note(*exit_code),
                    error_count,
                    edited_files(edited)
                )
            }
            Unverified::Stale { edited, command } => {
                format!(
                    "手上的绿灯是最后一次改动之前的（`{command}` 已失效） {} —— \
                     请重新执行 `verify`，再汇报。",
                    edited_files(edited)
                )
            }
        }
    }

    /// The one line the gate appends to the final reply. Distinguishing
    /// "never checked" from "checked and failed" is free (the ledger knows) and
    /// it is the difference between "it was lazy" and "it tried and lost".
    pub fn warning(&self) -> String {
        match self {
            Unverified::NoCompileLayer => {
                "⚠️ 本工作区无编译级验证（该语言没有编译层），本次无 L2 证据。".to_string()
            }
            Unverified::NeverChecked { edited } => {
                format!("⚠️ 本次改动后未见验证 {}", edited_files(edited))
            }
            Unverified::TimedOut { edited, command } => {
                format!(
                    "⚠️ 验证超时/被杀，无有效证据 `{command}` {}",
                    edited_files(edited)
                )
            }
            Unverified::CheckFailed {
                edited,
                command,
                exit_code,
                error_count,
            } => {
                format!(
                    "⚠️ 验证未通过 `{command}` {} errors={} {}",
                    exit_code_note(*exit_code),
                    error_count,
                    edited_files(edited)
                )
            }
            Unverified::Stale { edited, command } => {
                format!(
                    "⚠️ 验证结果早于最后一次改动（`{command}` 已失效） {}",
                    edited_files(edited)
                )
            }
        }
    }
}

/// The changed-file note both messages end with.
fn edited_files(files: &[PathBuf]) -> String {
    let names: Vec<String> = files
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .take(3)
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("（改动：{}）", names.join(", "))
    }
}

/// The exit code as a reader sees it.
///
/// Not `{:?}`: the first smoke run leaked `exit=Some(101)` into the user's
/// terminal, which is Rust's debug format speaking, not the gate.
fn exit_code_note(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("exit {code}"),
        None => "exit 未知".to_string(),
    }
}

/// Sequence-numbered edit/verify history for one agent run.
///
/// Not thread-safe on its own — callers share it behind a `Mutex` because the
/// tool and the middleware are on different tasks.
#[derive(Debug, Default)]
pub struct EvidenceLedger {
    seq: u64,
    last_edit_seq: u64,
    /// The paths touched by the current unverified batch, for the warning text.
    pending_edits: Vec<PathBuf>,
    /// Every run, newest last. Kept in full so a hand-back can be explained
    /// after the fact; it is bounded by the turn count, not by anything
    /// unbounded.
    records: Vec<EvidenceRecord>,
    /// Whether the workspace can be verified at all. Set once at startup from
    /// `code_intel::lang::resolve_verify_command` (project config → project
    /// convention → language table); `None` means "not decided yet".
    compile_layer: Option<bool>,
}

impl EvidenceLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare whether this workspace has a compile layer. Set at startup;
    /// `false` switches the gate to its honest "nothing to verify against" path
    /// instead of nagging for a verification that cannot exist.
    pub fn set_compile_layer(&mut self, has_compile_layer: bool) {
        self.compile_layer = Some(has_compile_layer);
    }

    /// Record that code changed. Bumps the sequence, which is what makes every
    /// earlier green light stale.
    pub fn record_edit(&mut self, paths: &[PathBuf]) {
        self.seq += 1;
        self.last_edit_seq = self.seq;
        for p in paths {
            if !self.pending_edits.iter().any(|e| e == p) {
                self.pending_edits.push(p.clone());
            }
        }
    }

    /// Record a verification run. The sequence is bumped even for a failed run
    /// — the ledger tracks *when things happened*, not just what passed.
    pub fn record_verify(&mut self, verdict: VerifyVerdict) -> &EvidenceRecord {
        self.seq += 1;
        self.records.push(EvidenceRecord { seq: self.seq, verdict });
        self.records.last().expect("just pushed")
    }

    /// The core predicate: is there a passing run strictly newer than the last
    /// edit?
    ///
    /// `kind` keeps the two legs separate — a green check does not vouch for
    /// tests and a green test run does not vouch for compilation.
    pub fn fresh_green(&self, kind: VerifyKind) -> bool {
        self.records
            .iter()
            .any(|r| r.seq > self.last_edit_seq && r.verdict.kind == kind && r.verdict.passed())
    }

    /// The newest run of `kind`, if any. Used to distinguish "never ran" from
    /// "ran and failed" from "ran and is stale".
    pub fn latest(&self, kind: VerifyKind) -> Option<&EvidenceRecord> {
        self.records.iter().rfind(|r| r.verdict.kind == kind)
    }

    /// Why this hand-back cannot be blessed, or `None` when it can.
    ///
    /// `kind` is the leg the caller cares about (the gate asks for `Check`).
    pub fn unverified(&self, kind: VerifyKind) -> Option<Unverified> {
        if self.compile_layer == Some(false) {
            return Some(Unverified::NoCompileLayer);
        }
        if self.fresh_green(kind) {
            return None;
        }
        let edited = self.pending_edits.clone();
        match self.latest(kind) {
            None => Some(Unverified::NeverChecked { edited }),
            Some(rec) if rec.seq <= self.last_edit_seq => {
                if rec.verdict.exit_code.is_none() {
                    Some(Unverified::TimedOut {
                        edited,
                        command: rec.verdict.command.clone(),
                    })
                } else if rec.verdict.passed() {
                    Some(Unverified::Stale {
                        edited,
                        command: rec.verdict.command.clone(),
                    })
                } else {
                    Some(Unverified::CheckFailed {
                        edited,
                        command: rec.verdict.command.clone(),
                        exit_code: rec.verdict.exit_code,
                        error_count: rec.verdict.error_count,
                    })
                }
            }
            // A run newer than the last edit exists but is not a green light,
            // so it ran and failed (or was killed) after the edits.
            Some(rec) => {
                if rec.verdict.exit_code.is_none() {
                    Some(Unverified::TimedOut {
                        edited,
                        command: rec.verdict.command.clone(),
                    })
                } else {
                    Some(Unverified::CheckFailed {
                        edited,
                        command: rec.verdict.command.clone(),
                        exit_code: rec.verdict.exit_code,
                        error_count: rec.verdict.error_count,
                    })
                }
            }
        }
    }

    /// Clear the pending edit batch. Called when the gate lets a hand-back
    /// through, so the next task's warning does not name this task's files.
    pub fn clear_pending(&mut self) {
        self.pending_edits.clear();
    }

    /// Sequence numbers, for tests and for the audit trail.
    pub fn last_edit_seq(&self) -> u64 {
        self.last_edit_seq
    }
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(kind: VerifyKind, exit: Option<i32>) -> VerifyVerdict {
        VerifyVerdict {
            kind,
            command: "cargo check".to_string(),
            exit_code: exit,
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        }
    }

    fn edit(ledger: &mut EvidenceLedger) {
        ledger.record_edit(&[PathBuf::from("src/a.rs")]);
    }

    #[test]
    fn fresh_green_requires_a_pass_newer_than_the_last_edit() {
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        // Nothing has happened: no green light exists at all.
        assert!(l.unverified(VerifyKind::Check).is_some());

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(0)));
        assert!(l.fresh_green(VerifyKind::Check));
        assert!(l.unverified(VerifyKind::Check).is_none());
    }

    #[test]
    fn an_edit_after_a_pass_makes_the_green_light_stale() {
        // This is the check the original gate never made — the reason it could
        // claim "verified" over code that did not compile.
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(0)));
        edit(&mut l);

        assert!(!l.fresh_green(VerifyKind::Check));
        assert!(matches!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::Stale { .. })
        ));
    }

    #[test]
    fn a_failed_run_is_not_a_green_light() {
        // The original gate cleared its dirty flag on *any* verify call,
        // pass or fail. That was the root cause of its false greens.
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(101)));

        assert!(!l.fresh_green(VerifyKind::Check));
        assert!(matches!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::CheckFailed {
                exit_code: Some(101),
                ..
            })
        ));
    }

    #[test]
    fn a_killed_run_is_absence_of_evidence_not_failure() {
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, None));

        assert!(matches!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::TimedOut { .. })
        ));
    }

    #[test]
    fn never_checked_is_reported_as_such() {
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);
        edit(&mut l);
        assert!(matches!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::NeverChecked { .. })
        ));
    }

    #[test]
    fn a_workspace_without_a_compile_layer_is_never_unverified() {
        // A vanilla JS project: nagging for a `cargo check` would be noise, and
        // claiming a verification would be a lie. Say neither.
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(false);
        edit(&mut l);
        assert_eq!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::NoCompileLayer)
        );
    }

    #[test]
    fn the_two_legs_are_independent() {
        // A green check must not vouch for tests, and vice versa.
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(0)));

        assert!(l.fresh_green(VerifyKind::Check));
        assert!(!l.fresh_green(VerifyKind::Test));
    }

    #[test]
    fn a_pass_before_the_last_edit_does_not_cover_a_later_failure() {
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);

        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(0)));
        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(1)));

        // The newest run is a failure: the old green light is gone and the
        // current state is "checked and failed", not "stale".
        assert!(matches!(
            l.unverified(VerifyKind::Check),
            Some(Unverified::CheckFailed { .. })
        ));
    }

    #[test]
    fn warnings_distinguish_laziness_from_failure() {
        let mut a = EvidenceLedger::new();
        a.set_compile_layer(true);
        edit(&mut a);
        let never = a.unverified(VerifyKind::Check).unwrap().warning();
        assert!(never.contains("未见验证"), "{never}");

        let mut b = EvidenceLedger::new();
        b.set_compile_layer(true);
        edit(&mut b);
        b.record_verify(verdict(VerifyKind::Check, Some(101)));
        let failed = b.unverified(VerifyKind::Check).unwrap().warning();
        assert!(failed.contains("未通过"), "{failed}");
        assert!(!failed.contains("未见验证"), "{failed}");
    }

    #[test]
    fn the_messages_render_the_exit_code_not_the_option() {
        // The first smoke run leaked `exit=Some(101)` into the user's terminal.
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);
        edit(&mut l);
        l.record_verify(verdict(VerifyKind::Check, Some(101)));
        let unv = l.unverified(VerifyKind::Check).unwrap();
        for msg in [unv.warning(), unv.nudge()] {
            assert!(msg.contains("exit 101"), "{msg}");
            assert!(!msg.contains("Some("), "{msg}");
        }
    }

    #[test]
    fn the_nudge_follows_the_verdict() {
        // "You never ran it" and "you ran it and it lost" need different next
        // steps. Asking for a run the model already made costs a whole turn.
        let mut a = EvidenceLedger::new();
        a.set_compile_layer(true);
        edit(&mut a);
        let lazy = a.unverified(VerifyKind::Check).unwrap().nudge();
        assert!(lazy.contains("未见绿灯验证"), "{lazy}");

        let mut b = EvidenceLedger::new();
        b.set_compile_layer(true);
        edit(&mut b);
        b.record_verify(verdict(VerifyKind::Check, Some(101)));
        let lost = b.unverified(VerifyKind::Check).unwrap().nudge();
        assert!(lost.contains("未通过"), "{lost}");
        assert!(!lost.contains("未见绿灯验证"), "{lost}");

        let mut c = EvidenceLedger::new();
        c.set_compile_layer(true);
        edit(&mut c);
        c.record_verify(verdict(VerifyKind::Check, Some(0)));
        edit(&mut c);
        let stale = c.unverified(VerifyKind::Check).unwrap().nudge();
        assert!(stale.contains("之前"), "{stale}");
    }

    #[test]
    fn clearing_pending_stops_naming_old_files() {
        let mut l = EvidenceLedger::new();
        l.set_compile_layer(true);
        edit(&mut l);
        assert!(!l.pending_edits.is_empty());
        l.clear_pending();
        assert!(l.pending_edits.is_empty());
    }
}
