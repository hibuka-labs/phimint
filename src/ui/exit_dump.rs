//! Exit transcript replay: what the restored terminal keeps when phimint quits.
//!
//! phimint draws on the alternate screen (`EnterAlternateScreen` in `run.rs`),
//! which the terminal discards on `LeaveAlternateScreen` — and no escape
//! sequence moves alt-screen content back to the primary screen. Keeping the
//! record the way Claude Code does (its UI never leaves the primary screen) is
//! therefore a re-print: after the terminal is restored, the committed
//! transcript is written out as plain rows and lands in the shell's scrollback.
//!
//! What the replay contains, and why:
//!
//! - **The committed transcript, not the chrome.** Status bar, composer and
//!   spinners live only on the alt-screen and are gone for good; the rows
//!   re-printed here are the same settled text the viewport showed. Banner
//!   art (styled runs, the wordmark) is skipped — the replay has its own
//!   header rule and 58 columns of `█` are noise in a scrollback.
//! - **Folded payloads are expanded.** A tool result commits as a one-line
//!   meta row with the full answer in `LineDetail::Folded.raw`; the replay
//!   prints the payload under it so the output stays copyable after exit.
//!   Thinking folds print the meta row only — reasoning is not part of the
//!   conversation record (and is on disk regardless).
//! - **Sub-agents get their own sections.** Child transcripts are kept out of
//!   the main one (fan-in puts only their result in), so the replay re-attaches
//!   them under a `sub-agent <id>` rule.
//!
//! Best-effort by construction: `phimint | head` closes the pipe mid-replay,
//! and the replay stops writing rather than fighting the EPIPE.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use phi_tui::lines::{DiffLineKind, LineDetail, LineKind, MetaHead, OutputLine};

use crate::banner::BannerStyle;
use crate::config::ExitDumpMode;
use crate::ui::app::fmt_k;

/// Session facts for the replay's header and footer. Assembled at the quit
/// site, while the `App` that holds the transcript is still alive.
pub struct SessionFacts<'a> {
    pub version: &'a str,
    pub session_id: &'a str,
    pub session_dir: &'a Path,
    /// User turns seen in this session (the title heuristic's counter).
    pub turns: usize,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Render the exit replay as plain rows (no trailing newlines).
///
/// Pure: takes the committed lines and returns what the restored terminal
/// should show. [`replay_to_stdout`] is the IO wrapper.
pub fn render(
    mode: ExitDumpMode,
    facts: &SessionFacts<'_>,
    main: &[OutputLine<BannerStyle>],
    sub_agents: &BTreeMap<String, Vec<OutputLine<BannerStyle>>>,
) -> Vec<String> {
    if mode == ExitDumpMode::None {
        return Vec::new();
    }
    let mut rows = Vec::new();
    rows.push(header_row(facts));
    rows.push(String::new());
    match mode {
        ExitDumpMode::None => unreachable!("handled above"),
        ExitDumpMode::Full => {
            for line in main {
                line_rows(&mut rows, line);
            }
            for (agent_id, lines) in sub_agents {
                rows.push(String::new());
                rows.push(format!("── sub-agent {agent_id} ──"));
                for line in lines {
                    line_rows(&mut rows, line);
                }
            }
        }
        ExitDumpMode::Summary => {
            for line in final_answer(main) {
                rows.push(line.text.clone());
            }
        }
    }
    rows.push(String::new());
    rows.push(footer_row(facts));
    rows
}

/// Write `rows` to `w`, stopping at the first error (a closed pipe ends the
/// replay early instead of erroring once per remaining row).
pub fn write_rows(w: &mut impl Write, rows: &[String]) {
    for row in rows {
        if writeln!(w, "{row}").is_err() {
            return;
        }
    }
    let _ = w.flush();
}

/// Render + write to stdout. Called on every clean exit path, after
/// `TerminalGuard` has restored the terminal — the rows must land on the
/// primary screen or they disappear with the alt-screen.
pub fn replay_to_stdout(
    mode: ExitDumpMode,
    facts: &SessionFacts<'_>,
    main: &[OutputLine<BannerStyle>],
    sub_agents: &BTreeMap<String, Vec<OutputLine<BannerStyle>>>,
) {
    let rows = render(mode, facts, main, sub_agents);
    let mut out = io::stdout().lock();
    write_rows(&mut out, &rows);
}

/// `── phimint 0.4.1 · session X · 3 turns · 12.4k tok ──`
fn header_row(facts: &SessionFacts<'_>) -> String {
    let turns = facts.turns;
    format!(
        "── phimint {} · session {} · {} {} · {} tok ──",
        facts.version,
        facts.session_id,
        turns,
        if turns == 1 { "turn" } else { "turns" },
        fmt_k(facts.tokens_in.saturating_add(facts.tokens_out)),
    )
}

/// `── full log <dir>/messages.jsonl · phimint --resume to continue ──`
fn footer_row(facts: &SessionFacts<'_>) -> String {
    format!(
        "── full log {}/messages.jsonl · phimint --resume to continue ──",
        facts.session_dir.display()
    )
}

/// Append one committed line and, for folds, the payload it summarizes.
fn line_rows(out: &mut Vec<String>, line: &OutputLine<BannerStyle>) {
    // Banner rows are the only styled runs in the transcript; the wordmark is
    // alt-screen chrome and does not belong in the scrollback.
    if line.spans.is_some() {
        return;
    }
    out.push(line.text.clone());
    match &line.detail {
        Some(LineDetail::Folded { raw, meta_head, .. }) => {
            // Thinking is summarized, never expanded — see the module doc.
            if line.kind == LineKind::Thought || raw.is_empty() {
                return;
            }
            // The meta row already displays the payload's first line when it
            // is the answer's head; printing it again would double it.
            let skip_head = *meta_head == MetaHead::Whole;
            for (i, raw_line) in raw.lines().enumerate() {
                if skip_head && i == 0 {
                    continue;
                }
                out.push(format!("    {raw_line}"));
            }
        }
        Some(LineDetail::Diff { path, hunks }) => {
            out.push(format!("  --- {path}"));
            for hunk in hunks {
                out.push(format!("  {}", hunk.header));
                for dl in &hunk.lines {
                    let mark = match dl.kind {
                        DiffLineKind::Add => '+',
                        DiffLineKind::Del => '-',
                        DiffLineKind::Context => ' ',
                    };
                    out.push(format!("  {mark} {}", dl.text));
                }
            }
        }
        None => {}
    }
}

/// The trailing run of assistant prose — what `summary` keeps of the session.
///
/// Assistant text commits as [`LineKind::Normal`]; everything else (user echo,
/// tools, turn summaries) sits around it. Sessions that never produced prose
/// (quit mid-tool) reduce to the header and footer.
fn final_answer(main: &[OutputLine<BannerStyle>]) -> &[OutputLine<BannerStyle>] {
    let Some(last) = main
        .iter()
        .rposition(|l| l.kind == LineKind::Normal && !l.text.is_empty())
    else {
        return &[];
    };
    let mut begin = last;
    while begin > 0 && main[begin - 1].kind == LineKind::Normal && !main[begin - 1].text.is_empty()
    {
        begin -= 1;
    }
    &main[begin..=last]
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_tui::lines::{DiffHunk, DiffLine, MetaHead};

    fn facts() -> SessionFacts<'static> {
        SessionFacts {
            version: "0.4.1",
            session_id: "20261014_test",
            session_dir: Path::new("/tmp/s/20261014_test"),
            turns: 3,
            tokens_in: 10_000,
            tokens_out: 2_400,
        }
    }

    fn line(kind: LineKind, text: &str) -> OutputLine<BannerStyle> {
        OutputLine {
            text: text.to_string(),
            kind,
            ..Default::default()
        }
    }

    fn folded(
        kind: LineKind,
        text: &str,
        raw: &str,
        meta_head: MetaHead,
    ) -> OutputLine<BannerStyle> {
        OutputLine {
            text: text.to_string(),
            kind,
            detail: Some(LineDetail::Folded {
                raw: raw.to_string(),
                line_count: raw.lines().count(),
                char_count: raw.chars().count(),
                duration_secs: None,
                meta_head,
            }),
            ..Default::default()
        }
    }

    fn render_one(mode: ExitDumpMode, main: Vec<OutputLine<BannerStyle>>) -> Vec<String> {
        render(mode, &facts(), &main, &BTreeMap::new())
    }

    #[test]
    fn none_mode_renders_nothing() {
        let rows = render_one(ExitDumpMode::None, vec![line(LineKind::Normal, "hi")]);
        assert!(rows.is_empty());
    }

    #[test]
    fn full_mode_frames_the_session_with_header_and_footer() {
        let rows = render_one(
            ExitDumpMode::Full,
            vec![
                line(LineKind::User, "> ping"),
                line(LineKind::Normal, "pong"),
            ],
        );
        assert_eq!(
            rows[0],
            "── phimint 0.4.1 · session 20261014_test · 3 turns · 12.4k tok ──"
        );
        assert_eq!(rows[1], "");
        assert!(rows.contains(&"> ping".to_string()));
        assert!(rows.contains(&"pong".to_string()));
        assert_eq!(
            rows.last().unwrap(),
            "── full log /tmp/s/20261014_test/messages.jsonl · phimint --resume to continue ──"
        );
    }

    #[test]
    fn banner_rows_are_skipped() {
        let mut art = line(LineKind::Normal, "██████  ██");
        art.spans = Some(Vec::new()); // styled run == banner art
        let rows = render_one(
            ExitDumpMode::Full,
            vec![art, line(LineKind::Normal, "after")],
        );
        assert!(
            !rows.iter().any(|r| r.contains('█')),
            "banner art leaked into the replay: {rows:?}"
        );
        assert!(rows.contains(&"after".to_string()));
    }

    #[test]
    fn tool_result_folds_expand_under_the_meta_row() {
        let rows = render_one(
            ExitDumpMode::Full,
            vec![folded(
                LineKind::ToolResult,
                "  ├ < head line…",
                "head line full\nsecond line",
                MetaHead::Abbreviated,
            )],
        );
        let body: Vec<&str> = rows
            .iter()
            .filter(|r| r.contains("head") || r.contains("second"))
            .map(String::as_str)
            .collect();
        // Meta row keeps its glyph; the abbreviated head is not re-printed,
        // the full head and the rest of the payload are.
        assert_eq!(
            body,
            vec!["  ├ < head line…", "    head line full", "    second line"]
        );
    }

    #[test]
    fn whole_head_is_not_printed_twice() {
        let rows = render_one(
            ExitDumpMode::Full,
            vec![folded(
                LineKind::ToolResult,
                "  < only line",
                "only line\nmore",
                MetaHead::Whole,
            )],
        );
        let payload: Vec<&str> = rows
            .iter()
            .filter(|r| r.starts_with("    "))
            .map(String::as_str)
            .collect();
        assert_eq!(payload, vec!["    more"], "head duplicated: {rows:?}");
    }

    #[test]
    fn thinking_folds_stay_summarized() {
        let rows = render_one(
            ExitDumpMode::Full,
            vec![folded(
                LineKind::Thought,
                "… thinking 2 lines · 3s (Ctrl+O)",
                "internal reasoning\nmore reasoning",
                MetaHead::None,
            )],
        );
        assert!(
            !rows.iter().any(|r| r.contains("internal reasoning")),
            "thinking payload leaked into the replay: {rows:?}"
        );
    }

    #[test]
    fn diffs_render_path_hunks_and_marks() {
        let mut edit = line(LineKind::Tool, "  ├ [tool] edit_file");
        edit.detail = Some(LineDetail::Diff {
            path: "src/main.rs".to_string(),
            hunks: vec![DiffHunk {
                header: "@@ -1,2 +1,2 @@".to_string(),
                edit_index: 0,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Del,
                        text: "old".to_string(),
                        old_line: Some(1),
                        new_line: None,
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "new".to_string(),
                        old_line: None,
                        new_line: Some(1),
                    },
                ],
            }],
        });
        let rows = render_one(ExitDumpMode::Full, vec![edit]);
        assert!(rows.contains(&"  --- src/main.rs".to_string()));
        assert!(rows.contains(&"  @@ -1,2 +1,2 @@".to_string()));
        assert!(rows.contains(&"  - old".to_string()));
        assert!(rows.contains(&"  + new".to_string()));
    }

    #[test]
    fn sub_agents_get_their_own_section() {
        let mut subs = BTreeMap::new();
        subs.insert(
            "root/review".to_string(),
            vec![line(LineKind::Normal, "child says")],
        );
        let rows = render(ExitDumpMode::Full, &facts(), &[], &subs);
        assert!(rows.contains(&"── sub-agent root/review ──".to_string()));
        assert!(rows.contains(&"child says".to_string()));
    }

    #[test]
    fn summary_keeps_the_final_answer_only() {
        let rows = render_one(
            ExitDumpMode::Summary,
            vec![
                line(LineKind::User, "> what is 2+2"),
                line(LineKind::Normal, "thinking about it"),
                folded(LineKind::ToolResult, "  < 4", "4", MetaHead::Whole),
                line(LineKind::Normal, "It is 4."),
                line(LineKind::Normal, "Done."),
            ],
        );
        assert_eq!(
            rows,
            vec![
                "── phimint 0.4.1 · session 20261014_test · 3 turns · 12.4k tok ──".to_string(),
                String::new(),
                "It is 4.".to_string(),
                "Done.".to_string(),
                String::new(),
                "── full log /tmp/s/20261014_test/messages.jsonl · phimint --resume to continue ──"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn write_rows_stops_on_the_first_error() {
        struct OneShot {
            writes: usize,
        }
        impl Write for OneShot {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                if self.writes > 2 {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = OneShot { writes: 0 };
        let rows: Vec<String> = (0..10).map(|i| format!("row {i}")).collect();
        write_rows(&mut sink, &rows);
        // Two rows land, the third errors, and the remaining seven are not
        // attempted (an error per row is what a dead pipe would otherwise cost).
        assert_eq!(sink.writes, 3);
    }
}
