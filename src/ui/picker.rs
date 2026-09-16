//! Interactive session picker for `phimint --resume`.
//!
//! Uses `phi_agent::list_sessions` for scanning and presents a ratatui list
//! for the user to choose from.  Returns the selected session's directory
//! path, or `None` if the user pressed Esc.

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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

/// Show the picker from within an active TUI session.
///
/// Leaves the caller's alternate screen, runs the picker in its own, and
/// returns to the caller's screen.  Raw mode stays on (idempotent).
pub fn show_picker_from_tui(
    base_dir: &Path,
    current_session_id: Option<&str>,
) -> io::Result<Option<PathBuf>> {
    let entries = phi_agent::list_sessions(base_dir, current_session_id);
    if entries.is_empty() {
        return Ok(None);
    }

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
        let r = picker_loop(&mut terminal, &entries, &mut state);
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
pub fn show_picker(base_dir: &Path, current_session_id: Option<&str>) -> io::Result<Option<PathBuf>> {
    let entries = phi_agent::list_sessions(base_dir, current_session_id);

    if entries.is_empty() {
        eprintln!("No resumable sessions found.");
        return Ok(None);
    }

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ListState::default();
    state.select(Some(0));

    let result = picker_loop(&mut terminal, &entries, &mut state);

    // Clean up terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

/// The picker event loop — renders the list and handles keyboard input.
fn picker_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    entries: &[SessionInfo],
    state: &mut ListState,
) -> io::Result<Option<PathBuf>> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            let chunks = Layout::vertical([
                Constraint::Min(1),    // list
                Constraint::Length(1), // footer hint
            ])
            .split(area);

            // Build list items.
            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| {
                    // Format: "  2026-09-10T14:30  title text"
                    let date = format_date(e.last_active_at);
                    let line = Line::from(vec![
                        Span::styled(format!("  {}  ", date), Style::default().fg(Color::DarkGray)),
                        Span::raw(&e.title),
                    ]);
                    ListItem::new(line)
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .title(" Resume Session ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ");

            frame.render_stateful_widget(list, chunks[0], state);

            // Footer hint.
            let hint = Span::styled(
                " ↑↓ navigate  Enter select  Esc cancel ",
                Style::default().fg(Color::DarkGray),
            );
            frame.render_widget(ratatui::widgets::Paragraph::new(hint), chunks[1]);
        })?;

        // Handle key events.
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()?
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
                        if selected + 1 < entries.len() {
                            state.select(Some(selected + 1));
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(idx) = state.selected() {
                            return Ok(Some(entries[idx].session_dir.clone()));
                        }
                    }
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }
}

/// Format a `SystemTime` for display with relative time.
///
/// Shows "刚刚" / "N分钟前" / "N小时前" / "N天前" / "YYYY-MM-DD HH:MM".
fn format_date(time: SystemTime) -> String {
    let duration = match SystemTime::now().duration_since(time) {
        Ok(d) => d,
        Err(_) => return "刚刚".to_string(), // clock skew
    };

    let secs = duration.as_secs();
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{}分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{}小时前", secs / 3600)
    } else if secs < 30 * 86400 {
        format!("{}天前", secs / 86400)
    } else {
        // Absolute date for old sessions.
        let dt: chrono::DateTime<chrono::Local> = time.into();
        dt.format("%Y-%m-%d %H:%M").to_string()
    }
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
        std::fs::write(dir.join("session_meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
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
        assert_eq!(format_date(now - Duration::from_secs(30)), "刚刚");
        assert!(format_date(now - Duration::from_secs(300)).contains("分钟前"));
        assert!(format_date(now - Duration::from_secs(10800)).contains("小时前"));
        assert!(format_date(now - Duration::from_secs(10 * 86400)).contains("天前"));
        let formatted = format_date(now - Duration::from_secs(60 * 86400));
        assert!(formatted.contains("-"), "expected absolute date for old timestamp, got: {}", formatted);
    }

    #[test]
    fn test_format_date_future_clock_skew() {
        let future = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(format_date(future), "刚刚");
    }
}
