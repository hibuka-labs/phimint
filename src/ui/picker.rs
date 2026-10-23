//! Interactive session picker for `phimint --resume`.
//!
//! Uses `phi_agent::list_sessions` for scanning and presents a ratatui list
//! for the user to choose from.  Returns the selected session's directory
//! path, or `None` if the user pressed Esc.
//!
//! The list is scoped by [`crate::session_scope`]: by default only sessions
//! from the folder phimint was launched in, `Tab` widens to everything.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use phi_agent::SessionInfo;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::banner::ColorScheme;
use crate::session_scope::{self, Scope};
use crate::tools::ask_user::AskOption;
use crate::ui::theme;
use unicode_width::UnicodeWidthStr;

/// Show the picker from within an active TUI session.
///
/// Leaves the caller's alternate screen, runs the picker in its own, and
/// returns to the caller's screen.  Raw mode stays on (idempotent).
pub fn show_picker_from_tui(
    base_dir: &Path,
    current_session_id: Option<&str>,
    scheme: ColorScheme,
) -> io::Result<Option<PathBuf>> {
    let entries = phi_agent::list_sessions(base_dir, current_session_id);
    if entries.is_empty() {
        return Ok(None);
    }
    let workspace = session_scope::current_workspace();

    // Leave the caller's alternate screen.
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen)?;

    let result = (|| {
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut state = ListState::default();
        state.select(Some(0));
        let r = picker_loop(
            &mut terminal,
            &entries,
            &mut state,
            scheme,
            workspace.as_deref(),
        );
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        r
    })();

    // Re-enter the caller's alternate screen.
    enable_raw_mode()?;
    let mut stdout2 = io::stdout();
    execute!(stdout2, EnterAlternateScreen)?;

    result
}

/// Show an interactive picker and return the selected session directory.
///
/// Returns `Ok(None)` if the user pressed Esc or the list is empty.
pub fn show_picker(
    base_dir: &Path,
    current_session_id: Option<&str>,
    scheme: ColorScheme,
) -> io::Result<Option<PathBuf>> {
    let entries = phi_agent::list_sessions(base_dir, current_session_id);

    if entries.is_empty() {
        eprintln!("No resumable sessions found.");
        return Ok(None);
    }
    let workspace = session_scope::current_workspace();

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ListState::default();
    state.select(Some(0));

    let result = picker_loop(
        &mut terminal,
        &entries,
        &mut state,
        scheme,
        workspace.as_deref(),
    );

    // Clean up terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

/// The picker event loop — renders the list and handles keyboard input.
///
/// `entries` is every resumable session; the loop narrows it to the current
/// [`Scope`] and lets `Tab` flip between the two.
fn picker_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    entries: &[SessionInfo],
    state: &mut ListState,
    scheme: ColorScheme,
    workspace: Option<&Path>,
) -> io::Result<Option<PathBuf>> {
    let mut scope = Scope::Workspace;
    let mut shown = session_scope::visible(entries, scope, workspace);
    clamp_selection(state, shown.len());

    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            let chunks = Layout::vertical([
                Constraint::Min(1),    // list
                Constraint::Length(1), // footer hint
            ])
            .split(area);

            // Empty body is a first-class state, not a blank screen: the
            // sessions still exist, they just live in other folders.
            if shown.is_empty() {
                frame.render_widget(
                    Paragraph::new(empty_state_lines(entries.len(), scheme)),
                    chunks[0],
                );
            } else {
                let items: Vec<ListItem> = shown
                    .iter()
                    .map(|e| {
                        // Format: "  2026-09-10T14:30  title text"
                        let date = format_date(e.last_active_at);
                        let line = Line::from(vec![
                            Span::styled(
                                format!("  {}  ", date),
                                Style::default().fg(theme::faint(scheme)),
                            ),
                            Span::raw(&e.title),
                        ]);
                        ListItem::new(line)
                    })
                    .collect();

                let list = List::new(items)
                    .block(
                        Block::default()
                            .title(title(scope))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme::faint(scheme))),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(theme::selection_bg(scheme))
                            .fg(theme::selection_fg(scheme))
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("> ");

                frame.render_stateful_widget(list, chunks[0], state);
            }

            // Footer hint.
            let hint = Span::styled(
                footer(scope, shown.len(), entries.len()),
                Style::default().fg(theme::faint(scheme)),
            );
            frame.render_widget(Paragraph::new(hint), chunks[1]);
        })?;

        // Handle key events.
        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Up => {
                    let selected = state.selected().unwrap_or(0);
                    if selected > 0 {
                        state.select(Some(selected - 1));
                    }
                }
                KeyCode::Down => {
                    let selected = state.selected().unwrap_or(0);
                    if selected + 1 < shown.len() {
                        state.select(Some(selected + 1));
                    }
                }
                KeyCode::Tab => {
                    scope = scope.toggled();
                    shown = session_scope::visible(entries, scope, workspace);
                    clamp_selection(state, shown.len());
                }
                KeyCode::Enter => {
                    if let Some(idx) = state.selected()
                        && let Some(entry) = shown.get(idx)
                    {
                        return Ok(Some(entry.session_dir.clone()));
                    }
                }
                KeyCode::Esc => return Ok(None),
                _ => {}
            }
        }
    }
}

/// Keep the selection inside the visible list after a scope switch.
fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        state.select(Some(state.selected().unwrap_or(0).min(len - 1)));
    }
}

fn title(scope: Scope) -> String {
    // ASCII only: the chrome-width guard rejects em dashes and middots,
    // which render double-width in CJK terminals and shift the box borders.
    match scope {
        Scope::Workspace => " Resume Session  [this folder] ".to_string(),
        Scope::All => " Resume Session  [all folders] ".to_string(),
    }
}

fn footer(scope: Scope, shown: usize, total: usize) -> String {
    match scope {
        // Say what Tab does next, not where you are: the empty state is the
        // moment this line earns its keep.
        Scope::Workspace => format!(
            " ↑↓ navigate  Enter select  Tab all folders  Esc cancel   |  {} in this folder, {} total ",
            shown, total
        ),
        Scope::All => {
            format!(
                " ↑↓ navigate  Enter select  Tab this folder  Esc cancel   |  {} total ",
                total
            )
        }
    }
}

/// What to render when the current scope shows nothing.
///
/// "Nothing here" and "nothing anywhere" are different situations and must
/// not look the same — the first is a filter, the second is a fresh install.
fn empty_state_lines(total: usize, scheme: ColorScheme) -> Vec<Line<'static>> {
    let faint = Style::default().fg(theme::faint(scheme));
    if total == 0 {
        return vec![
            Line::from(""),
            Line::styled("  No sessions yet.", faint),
            Line::styled("  Start one with `phimint`; it will show up here.", faint),
        ];
    }
    vec![
        Line::from(""),
        Line::styled("  No sessions from this folder yet.", faint),
        Line::styled(
            format!(
                "  {} live in other folders, or predate folder tracking.",
                total
            ),
            faint,
        ),
        Line::styled("  Press Tab to see them all.", faint),
    ]
}

/// Format a `SystemTime` for display with relative time.
///
/// Recent times use a relative form (see the arms below for the exact wording); older ones fall back to `YYYY-MM-DD HH:MM`.
fn format_date(time: SystemTime) -> String {
    let duration = match SystemTime::now().duration_since(time) {
        Ok(d) => d,
        Err(_) => return "just now".to_string(), // clock skew
    };

    let secs = duration.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 30 * 86400 {
        format!("{}d ago", secs / 86400)
    } else {
        // Absolute date for old sessions.
        let dt: chrono::DateTime<chrono::Local> = time.into();
        dt.format("%Y-%m-%d %H:%M").to_string()
    }
}

// ── Concept-choice rows (ask_user) ─────────────────────────────────────────

/// One row of a question asked in the terminal: the label the user reads, plus
/// an optional note under it. The caller maps its own answer key to the index
/// this comes back as — the picker never sees it.
#[derive(Debug, Clone)]
pub struct ChoiceRow {
    pub label: String,
    pub note: Option<String>,
}

/// The rows a pending question shows — labels and notes only; the answer ids
/// and the answer channel stay with the prompt itself.
pub fn choice_rows(options: &[AskOption]) -> Vec<ChoiceRow> {
    options
        .iter()
        .map(|option| ChoiceRow {
            label: option.label.clone(),
            note: option.note.clone(),
        })
        .collect()
}

/// One option row: the cursor column, the label, and its note indented
/// underneath. The cursor reserves the same two columns on every row, so
/// labels line up whether or not the row is the one under it.
pub fn choice_lines(row: &ChoiceRow, selected: bool, scheme: ColorScheme) -> Vec<Line<'static>> {
    let cursor = if selected { "> " } else { "  " };
    let label_style = if selected {
        Style::default()
            .bg(theme::selection_bg(scheme))
            .fg(theme::selection_fg(scheme))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("{cursor}{}", row.label),
        label_style,
    ))];
    if let Some(note) = &row.note {
        lines.push(Line::styled(
            format!("    {note}"),
            Style::default().fg(theme::faint(scheme)),
        ));
    }
    lines
}

/// How many rows the option list takes: every label plus every note. The
/// window's own border is the popup's to budget, not the list's.
pub fn choice_list_height(rows: &[ChoiceRow]) -> u16 {
    rows.iter()
        .map(|row| if row.note.is_some() { 2 } else { 1 })
        .sum()
}

/// The key legend. The way out must never scroll off the window, so the full
/// legend gives way to shorter ones as the terminal narrows (and to a bare
/// key pair as a last resort).
///
/// ASCII only in the chrome: em dashes and middots render double-width in CJK
/// terminals and shift the box borders.
pub fn choice_footer(width: u16) -> &'static str {
    const LEGENDS: [&str; 3] = [
        " ↑↓ navigate  Enter select  Esc skip ",
        " ↑↓ move  Enter select  Esc skip ",
        " Enter pick  Esc skip ",
    ];
    LEGENDS
        .iter()
        .find(|legend| legend.width() <= width as usize)
        .copied()
        .unwrap_or(" Enter/Esc ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Create a minimal session directory with messages.jsonl.
    fn make_session(base: &Path, id: &str, first_msg: &str) -> PathBuf {
        let dir = base.join("sessions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "session_id": id,
            "created_at": "2026-09-10T10:00:00Z",
            "last_active_at": "2026-09-10T14:30:00Z"
        });
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        let msg = serde_json::json!({"User":{"content":first_msg,"images":[]}});
        std::fs::write(dir.join("messages.jsonl"), format!("{}\n", msg)).unwrap();
        dir
    }

    #[test]
    fn test_list_finds_resumable_sessions() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "s1", "hello world");
        make_session(tmp.path(), "s2", "another session");

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_list_skips_locked_sessions() {
        use fs2::FileExt;

        let tmp = TempDir::new().unwrap();
        let dir = make_session(tmp.path(), "locked", "locked session");
        let lock_file = std::fs::File::create(dir.join("session.lock")).unwrap();
        lock_file.try_lock_exclusive().unwrap();

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_list_skips_empty_messages() {
        let tmp = TempDir::new().unwrap();
        let dir = make_session(tmp.path(), "empty", "content");
        let sys = serde_json::json!({"System":{"content":"system prompt","ephemeral":false}});
        std::fs::write(dir.join("messages.jsonl"), format!("{}\n", sys)).unwrap();

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_list_skips_no_messages_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sessions").join("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session_meta.json"),
            r#"{"session_id":"legacy","created_at":"2026-09-10T10:00:00Z","last_active_at":"2026-09-10T10:00:00Z"}"#,
        )
        .unwrap();

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_list_skips_current_session() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "current", "skip me");

        let entries = phi_agent::list_sessions(tmp.path(), Some("current"));
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_sort_order_newest_first() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "old", "old session");
        std::thread::sleep(std::time::Duration::from_millis(50));
        make_session(tmp.path(), "new", "new session");

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "new session");
        assert_eq!(entries[1].title, "old session");
    }

    #[test]
    fn test_format_date_relative() {
        let now = SystemTime::now();
        assert_eq!(format_date(now - Duration::from_secs(30)), "just now");
        assert!(format_date(now - Duration::from_secs(300)).contains("m ago"));
        assert!(format_date(now - Duration::from_secs(10800)).contains("h ago"));
        assert!(format_date(now - Duration::from_secs(10 * 86400)).contains("d ago"));
        let formatted = format_date(now - Duration::from_secs(60 * 86400));
        assert!(
            formatted.contains("-"),
            "expected absolute date for old timestamp, got: {}",
            formatted
        );
    }

    #[test]
    fn test_format_date_future_clock_skew() {
        let future = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(format_date(future), "just now");
    }

    // ── scope wiring: the picker's two views ──

    #[test]
    fn clamp_selection_keeps_the_cursor_inside_the_list() {
        let mut state = ListState::default();
        state.select(Some(5));
        clamp_selection(&mut state, 2);
        assert_eq!(state.selected(), Some(1));

        clamp_selection(&mut state, 0);
        assert_eq!(
            state.selected(),
            None,
            "an empty list has nothing to select"
        );

        let mut fresh = ListState::default();
        clamp_selection(&mut fresh, 3);
        assert_eq!(fresh.selected(), Some(0));
    }

    #[test]
    fn footer_points_at_the_other_scope() {
        assert!(footer(Scope::Workspace, 2, 9).contains("Tab all folders"));
        assert!(footer(Scope::All, 9, 9).contains("Tab this folder"));
        assert!(footer(Scope::Workspace, 2, 9).contains("2 in this folder"));
        assert!(footer(Scope::All, 9, 9).contains("9 total"));
    }

    #[test]
    fn empty_state_distinguishes_a_filter_from_a_fresh_install() {
        let joined =
            |lines: Vec<Line<'static>>| lines.iter().map(|l| l.to_string()).collect::<String>();

        let fresh = joined(empty_state_lines(0, ColorScheme::Dark));
        assert!(
            fresh.contains("No sessions yet"),
            "a fresh install must say so: {fresh}"
        );

        let filtered = joined(empty_state_lines(12, ColorScheme::Dark));
        assert!(
            filtered.contains("Press Tab"),
            "the way out must be on screen: {filtered}"
        );
        assert!(
            filtered.contains("12"),
            "the hidden count must be on screen: {filtered}"
        );
    }

    /// End-to-end over real session directories: one recorded in this folder
    /// shows up in the default view, one recorded elsewhere does not.
    #[test]
    fn workspace_scope_filters_real_sessions() {
        let tmp = TempDir::new().unwrap();
        let here = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

        for (id, cwd) in [
            ("mine", here.to_string_lossy().into_owned()),
            ("theirs", "/somewhere/else".into()),
        ] {
            let dir = make_session(tmp.path(), id, id);
            let meta = serde_json::json!({
                "session_id": id,
                "last_active_at": "2026-09-10T14:30:00Z",
                "cwd": cwd,
            });
            std::fs::write(
                dir.join("session_meta.json"),
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();
        }

        let entries = phi_agent::list_sessions(tmp.path(), None);
        assert_eq!(entries.len(), 2);

        let shown = session_scope::visible(&entries, Scope::Workspace, Some(&here));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].session_id, "mine");
    }

    // ── concept-choice rows (ask_user) ──

    fn rows(spec: &[(&str, Option<&str>)]) -> Vec<ChoiceRow> {
        spec.iter()
            .map(|(label, note)| ChoiceRow {
                label: (*label).to_string(),
                note: note.map(|n| n.to_string()),
            })
            .collect()
    }

    #[test]
    fn choice_list_height_counts_every_label_and_note() {
        // Two plain rows take two rows; a note costs a second line each.
        assert_eq!(choice_list_height(&rows(&[("a", None), ("b", None)])), 2);
        assert_eq!(
            choice_list_height(&rows(&[("a", Some("n")), ("b", None)])),
            3
        );
        // No options is no rows — the popup draws nothing rather than an
        // empty box to skip.
        assert_eq!(choice_list_height(&[]), 0);
    }

    #[test]
    fn a_choice_note_sits_under_its_label() {
        let lines = choice_lines(
            &rows(&[("side-by-side", Some("dense"))])[0],
            false,
            ColorScheme::Dark,
        );
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(text[0].trim(), "side-by-side");
        assert!(
            text[1].contains("dense"),
            "the note must render under the label: {text:?}"
        );
    }

    #[test]
    fn the_cursor_column_is_reserved_on_every_row() {
        // The cursor is the line prefix, so an unselected row must still
        // carry the two columns — otherwise labels jump when the cursor
        // moves past them.
        let unselected = choice_lines(&rows(&[("搜索", None)])[0], false, ColorScheme::Dark);
        let selected = choice_lines(&rows(&[("搜索", None)])[0], true, ColorScheme::Dark);
        assert!(
            unselected[0].to_string().starts_with("  "),
            "{unselected:?}"
        );
        assert!(selected[0].to_string().starts_with("> "), "{selected:?}");
    }

    #[test]
    fn choice_chrome_says_how_to_skip_at_every_width() {
        // The way out is load-bearing (a skip is an answer), so it is the one
        // thing every width of the legend keeps — and the widest legend is
        // what a comfortable terminal gets.
        for width in [80u16, 40, 30, 20, 12] {
            let footer = choice_footer(width);
            assert!(
                footer.contains("Esc"),
                "width {width} lost the way out: {footer}"
            );
            assert!(
                footer.width() <= width as usize,
                "width {width} overflows its legend: {footer}"
            );
        }
        assert!(
            choice_footer(80).contains("navigate"),
            "{}",
            choice_footer(80)
        );
    }

    #[test]
    fn choice_rows_carry_labels_and_notes_but_never_the_ids() {
        // The rows are the view's projection of the question; the answer ids
        // and channel stay with the prompt (see App::answer_ask_front).
        let options = vec![
            AskOption {
                id: "a".into(),
                label: "侧边栏".into(),
                note: Some("筛选常驻".into()),
            },
            AskOption {
                id: "b".into(),
                label: "顶部".into(),
                note: None,
            },
        ];
        let rows = choice_rows(&options);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "侧边栏");
        assert_eq!(rows[0].note.as_deref(), Some("筛选常驻"));
        assert_eq!(rows[1].note, None);
    }
}
