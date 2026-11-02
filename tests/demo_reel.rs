//! README demo reel generator (product asset, not part of the test suite).
//!
//! Writes ANSI frames (real colours, real rendering) for a scripted session
//! so the README's GIF/screenshots are produced from the actual UI rather
//! than a mockup. Ignored by default so `cargo test` never writes files;
//! run it explicitly with
//!
//!     PHIMINT_REEL_OUT=docs/assets/frames cargo test --test demo_reel frame_reel -- --ignored
//!
//! The emitted `*.ans` files are then painted to PNG and assembled by the
//! companion script (see the project notes).

use std::io::Write as _;

use phi_agent::{PlanItem, PlanStepStatus, RuntimeEvent, SessionId};
use phi_agent_tui::ColorScheme;
use phi_agent_tui::app::{App, TuiEvent};
use phi_agent_tui::render::draw;
use phimint::banner::{brand_rows, build};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Cell;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

/// SGR sequence for one cell's style, or an empty string when it matches
/// default terminal styling.
fn cell_sgr(cell: &Cell) -> String {
    fn color(c: Color, fg: bool) -> String {
        let chan = if fg { 38 } else { 48 };
        match c {
            Color::Reset => String::new(),
            Color::Black => format!("{}", if fg { 30 } else { 40 }),
            Color::Red => format!("{}", if fg { 31 } else { 41 }),
            Color::Green => format!("{}", if fg { 32 } else { 42 }),
            Color::Yellow => format!("{}", if fg { 33 } else { 43 }),
            Color::Blue => format!("{}", if fg { 34 } else { 44 }),
            Color::Magenta => format!("{}", if fg { 35 } else { 45 }),
            Color::Cyan => format!("{}", if fg { 36 } else { 46 }),
            Color::Gray => format!("{}", if fg { 37 } else { 47 }),
            Color::DarkGray => format!("{}", if fg { 90 } else { 100 }),
            Color::LightRed => format!("{}", if fg { 91 } else { 101 }),
            Color::LightGreen => format!("{}", if fg { 92 } else { 102 }),
            Color::LightYellow => format!("{}", if fg { 93 } else { 103 }),
            Color::LightBlue => format!("{}", if fg { 94 } else { 104 }),
            Color::LightMagenta => format!("{}", if fg { 95 } else { 105 }),
            Color::LightCyan => format!("{}", if fg { 96 } else { 106 }),
            Color::White => format!("{}", if fg { 97 } else { 107 }),
            Color::Rgb(r, g, b) => format!("{chan};2;{r};{g};{b}"),
            Color::Indexed(i) => format!("{chan};5;{i}"),
        }
    }

    let mut parts: Vec<String> = Vec::new();
    let m = cell.modifier;
    if m.contains(Modifier::BOLD) {
        parts.push("1".into());
    }
    if m.contains(Modifier::DIM) {
        parts.push("2".into());
    }
    if m.contains(Modifier::ITALIC) {
        parts.push("3".into());
    }
    if m.contains(Modifier::UNDERLINED) {
        parts.push("4".into());
    }
    if m.contains(Modifier::REVERSED) {
        parts.push("7".into());
    }
    let fg = color(cell.fg, true);
    if !fg.is_empty() {
        parts.push(fg);
    }
    let bg = color(cell.bg, false);
    if !bg.is_empty() {
        parts.push(bg);
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parts.join(";"))
    }
}

/// Render the app to a grid of styled text — same wide-glyph handling as the
/// shell's text snapshot, but keeping each run's colours.
fn snapshot_ansi(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("offscreen terminal");
    terminal.draw(|f| draw(f, app)).expect("offscreen draw");
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let cells = buf.content();
    let w = area.width as usize;

    let mut out = String::from("\x1b[0m");
    let mut cur = String::new();
    for y in 0..area.height {
        if y > 0 {
            out.push_str("\x1b[0m\n");
            cur.clear();
        }
        let mut x = 0usize;
        while x < w {
            let cell = &cells[y as usize * w + x];
            let sgr = cell_sgr(cell);
            if sgr != cur {
                out.push_str("\x1b[0m");
                out.push_str(&sgr);
                cur = sgr;
            }
            let sym = cell.symbol();
            out.push_str(sym);
            x += sym.width().max(1);
        }
    }
    out.push_str("\x1b[0m");
    out
}

fn reel_text(text: &str) -> RuntimeEvent {
    RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_thought(text: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_tool_started(name: &str, args: &str) -> RuntimeEvent {
    RuntimeEvent::ActionStarted {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_tool_finished(name: &str, summary: &str) -> RuntimeEvent {
    RuntimeEvent::ActionFinished {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        summary: summary.to_string(),
        agent_id: None,
        trace_id: None,
        denied: false,
        details: None,
    }
}

/// The README demo reel: a short, believable coding session.
#[test]
#[ignore = "writes README demo assets; run explicitly with PHIMINT_REEL_OUT"]
fn frame_reel() {
    const W: u16 = 100;
    const H: u16 = 32;

    let out_dir = std::path::PathBuf::from(
        std::env::var("PHIMINT_REEL_OUT").unwrap_or_else(|_| "target/reel".to_string()),
    );
    std::fs::create_dir_all(&out_dir).expect("create reel dir");

    let mut app = App::new();
    app.push_banner(brand_rows(
        build(
            std::path::Path::new("/Users/you/src/your-project"),
            std::path::Path::new("/home/you/.phimint/sessions/20260930_demo01"),
            // No leading `v`: the banner's tagline row prepends it.
            "0.1.0",
            W as usize,
        ),
        ColorScheme::Dark,
    ));
    app.set_notice(
        "Tip: trackpad scrolling needs View > Allow Mouse Reporting; fn+Up/fn+Down always work",
    );

    let mut frames: Vec<String> = Vec::new();
    let mut snap = |app: &mut App| frames.push(snapshot_ansi(app, W, H));
    snap(&mut app);

    // --- turn 1: orient in an unfamiliar repo -----------------------------
    app.push_user("Map this repo and tell me where the background-task wake policy lives.");
    app.running = true;
    snap(&mut app);

    for chunk in [
        "Let me get the lay of the land first, ",
        "then search for the wake policy directly.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_thought(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::Runtime(reel_tool_started("repo_map", "{}")));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "repo_map",
        "Repository layout (6 modules, 48 files)",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "search_content",
        r#"{"query":"bg_wake|BG_WAKE_QUIET"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "search_content",
        "12 matches in 3 files",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(RuntimeEvent::PlanUpdated {
        session_id: SessionId::new(1),
        objective: "Locate and summarize the background-task wake policy".to_string(),
        explanation: None,
        plan: vec![
            PlanItem {
                step: "Map the repo and locate the wake module".into(),
                status: PlanStepStatus::Completed,
            },
            PlanItem {
                step: "Trace who calls the wake and when".into(),
                status: PlanStepStatus::InProgress,
            },
            PlanItem {
                step: "Summarize the delivery policy for the user".into(),
                status: PlanStepStatus::Pending,
            },
        ],
        agent_id: None,
        trace_id: None,
    }));
    snap(&mut app);

    for chunk in [
        "The policy lives in ",
        "`src/ui/bg_wake.rs`, and `run.rs` only executes the side effects. ",
        "It batches completions behind a 15s quiet window so a burst of ",
        "finished tasks costs one notification turn, not ten.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_text(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::TurnDone);
    snap(&mut app);

    // --- turn 2: a tool-heavy change --------------------------------------
    app.push_user("Add a regression test for the quiet-window batching.");
    app.running = true;
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "read_file",
        r#"{"path":"src/ui/bg_wake.rs"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "read_file",
        "Read 292 lines",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "edit_file",
        r#"{"path":"src/ui/bg_wake_tests.rs"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "edit_file",
        "Applied edit",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "execute_command",
        r#"{"command":"cargo test bg_wake","background":false}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "execute_command",
        "test result: ok. 14 passed; 0 failed",
    )));
    snap(&mut app);

    for chunk in [
        "Added `wake_batches_completions_inside_the_quiet_window`. ",
        "It pins the 15s window with an injected clock so the test never sleeps, ",
        "and asserts that two tasks finishing 3s apart produce one wake.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_text(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::TurnDone);
    snap(&mut app);

    // --- write the frames --------------------------------------------------
    for (i, frame) in frames.iter().enumerate() {
        let path = out_dir.join(format!("frame_{i:04}.ans"));
        let mut f = std::fs::File::create(&path).expect("write frame");
        f.write_all(frame.as_bytes()).expect("write frame bytes");
    }
    println!(
        "wrote {} frames ({}x{}) to {}",
        frames.len(),
        W,
        H,
        out_dir.display()
    );
}
