//! Emission-replay regression test: does the ratatui-crossterm diff path place
//! every changed cell — glyph AND style — exactly where the buffer says it is?
//!
//! Renders phimint-shaped frames (CJK transcript, composer with a REVERSED fake
//! cursor, right-aligned status whose scroll counter changes length) through the
//! real widget pipeline into buffers, replays `CrosstermBackend::draw`'s real
//! byte stream into an xterm-semantics grid (CJK double-width, wrap-pending
//! autowrap, SGR reverse/background), and compares the grid against the expected
//! buffer after every frame.
//!
//! A mismatch is a physical-terminal desync that a later Ctrl+L would heal —
//! the exact "input displays misaligned" symptom. `frames.txt` cannot catch
//! these on its own: it drops styles.

use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

const W: u16 = 211;
const H: u16 = 58;

/// SGR state that leaves a visible artifact when it goes stale.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Style2 {
    reverse: bool,
    /// True when a non-default background is active (colour identity collapsed —
    /// "some background" is what leaves a smudge).
    bg: bool,
}

impl Style2 {
    const PLAIN: Self = Self {
        reverse: false,
        bg: false,
    };
}

/// Minimal xterm-ish grid: CJK double-width with blank continuation cells,
/// wrap-pending autowrap, and the reverse/background SGR state per cell.
struct Grid {
    cells: Vec<Vec<char>>,
    styles: Vec<Vec<Style2>>,
    width: usize,
    height: usize,
    r: usize,
    c: usize,
    wrap_pending: bool,
    pen: Style2,
}

impl Grid {
    fn new(width: u16, height: u16) -> Self {
        let (width, height) = (width as usize, height as usize);
        Self {
            cells: vec![vec![' '; width]; height],
            styles: vec![vec![Style2::PLAIN; width]; height],
            width,
            height,
            r: 0,
            c: 0,
            wrap_pending: false,
            pen: Style2::PLAIN,
        }
    }

    fn put(&mut self, ch: char, w: usize) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.c = 0;
            self.r += 1;
            self.scroll_if_needed();
        }
        self.cells[self.r][self.c] = ch;
        self.styles[self.r][self.c] = self.pen;
        for k in 1..w {
            if self.c + k < self.width {
                self.cells[self.r][self.c + k] = '\u{0}';
                self.styles[self.r][self.c + k] = self.pen;
            }
        }
        self.c += w;
        if self.c >= self.width {
            self.c = self.width - 1;
            self.wrap_pending = true;
        }
    }

    fn scroll_if_needed(&mut self) {
        if self.r >= self.height {
            self.r = self.height - 1;
            self.cells.remove(0);
            self.styles.remove(0);
            self.cells.push(vec![' '; self.width]);
            self.styles.push(vec![Style2::PLAIN; self.width]);
        }
    }

    fn feed(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                self.control(ch);
                continue;
            }
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = None;
                    for n in chars.by_ref() {
                        let b = n as u32;
                        // CSI grammar: params 0x30-0x3F, intermediates 0x20-0x2F,
                        // final 0x40-0x7E (so 'm'/'H'/'J' terminate).
                        if (0x30..=0x3F).contains(&b) || (0x20..=0x2F).contains(&b) {
                            params.push(n);
                        } else if (0x40..=0x7E).contains(&b) {
                            final_byte = Some(n);
                            break;
                        } else {
                            break;
                        }
                    }
                    match final_byte {
                        Some('H') | Some('f') => {
                            let mut it = params.split(';');
                            let row: usize =
                                it.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
                            let col: usize =
                                it.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
                            self.r = (row - 1).min(self.height - 1);
                            self.c = (col - 1).min(self.width - 1);
                            self.wrap_pending = false;
                        }
                        Some('J') => {
                            self.cells = vec![vec![' '; self.width]; self.height];
                            self.styles = vec![vec![Style2::PLAIN; self.width]; self.height];
                            self.wrap_pending = false;
                        }
                        Some('K') => {
                            for x in self.c..self.width {
                                self.cells[self.r][x] = ' ';
                                self.styles[self.r][x] = Style2::PLAIN;
                            }
                        }
                        Some('m') => self.sgr(&params),
                        _ => {}
                    }
                }
                Some(']') => {
                    // OSC: ESC ] ... BEL | ESC \
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        }
    }

    fn sgr(&mut self, params: &str) {
        let nums: Vec<u32> = if params.is_empty() {
            vec![0]
        } else {
            params
                .split(';')
                .map(|s| s.parse::<u32>().unwrap_or(0))
                .collect()
        };
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => self.pen = Style2::PLAIN,
                7 => self.pen.reverse = true,
                27 => self.pen.reverse = false,
                // Colour families that paint a background. `48` (extended
                // colour) sits inside 40..=49 and is collapsed to "some bg".
                40..=49 | 100..=109 => {
                    self.pen.bg = nums[i] != 49;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn control(&mut self, ch: char) {
        match ch {
            '\r' => {
                self.c = 0;
                self.wrap_pending = false;
            }
            '\n' => {
                self.r += 1;
                self.wrap_pending = false;
                self.scroll_if_needed();
            }
            c if (c as u32) < 0x20 => {}
            c => {
                let w = UnicodeWidthChar::width(c).unwrap_or(1).max(1);
                self.put(c, w);
            }
        }
    }

    /// Text grid; wide-char continuation cells (`\0`) collapsed.
    fn text(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                let mut out = String::new();
                for c in row {
                    if *c != '\u{0}' {
                        out.push(*c);
                    }
                }
                out.trim_end().to_string()
            })
            .collect()
    }

    /// Glyph occupying one cell (`\u{0}` = wide-char continuation).
    fn cell_at(&self, row: usize, col: usize) -> char {
        self.cells[row][col]
    }
}

/// The expected text of a rendered buffer (mirrors `render::buffer_to_text`).
fn buffer_text(buf: &Buffer) -> Vec<String> {
    let area = buf.area;
    let cells = buf.content();
    let width = area.width as usize;
    (0..area.height as usize)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0;
            while x < width {
                let sym = cells[y * width + x].symbol();
                line.push_str(sym);
                x += unicode_width::UnicodeWidthStr::width(sym).max(1);
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// One phimint-shaped frame: transcript row, composer row with a REVERSED fake
/// cursor, status row with a right-aligned scroll counter.
fn frame_lines(
    transcript: &str,
    composer: &str,
    cursor_byte: usize,
    scroll: Option<u32>,
) -> Vec<Line<'static>> {
    let full = format!("> {composer}");
    let mut comp_spans: Vec<Span<'static>> = Vec::new();
    let mut byte = 0usize;
    let mut placed = false;
    for ch in full.chars() {
        if byte == cursor_byte {
            comp_spans.push(Span::styled(
                ch.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            placed = true;
        } else {
            comp_spans.push(Span::raw(ch.to_string()));
        }
        byte += ch.len_utf8();
    }
    if !placed {
        // Cursor past the end: REVERSED blank (phimint's render_composer shape).
        comp_spans.push(Span::styled(
            " ".to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }

    let status = match scroll {
        Some(n) => {
            let left = "auto mode on (shift+tab to cycle)";
            let right = format!("scroll {n}/288 (End to bottom)");
            let pad = (W as usize).saturating_sub(left.len() + right.len());
            format!("{left}{}{right}", " ".repeat(pad))
        }
        None => "auto mode on (shift+tab to cycle)".to_string(),
    };

    vec![
        Line::from(transcript.to_string()),
        Line::from(String::new()),
        Line::from("─".repeat(W as usize)),
        Line::from(comp_spans),
        Line::from("─".repeat(W as usize)),
        Line::from(status),
    ]
}

/// Feed every frame through the real backend and compare glyph + style against
/// the buffer after each one. Returns human-readable mismatch lines.
fn drive(frames: Vec<Vec<Line<'static>>>) -> Vec<String> {
    let backend = TestBackend::new(W, H);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut grid = Grid::new(W, H);
    let mut sink: Vec<u8> = Vec::new();
    let mut prev: Option<Buffer> = None;
    let mut failures = Vec::new();

    for (i, lines) in frames.into_iter().enumerate() {
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(lines), f.area());
            })
            .expect("draw");
        let current = terminal.backend().buffer().clone();

        sink.clear();
        {
            let mut writer = CrosstermBackend::new(&mut sink);
            match &prev {
                None => {
                    // First frame: emit every cell, like a post-clear repaint.
                    let blank = Buffer::empty(current.area);
                    ratatui::backend::Backend::draw(&mut writer, blank.diff(&current).into_iter())
                        .expect("emit");
                }
                Some(p) => {
                    ratatui::backend::Backend::draw(&mut writer, p.diff(&current).into_iter())
                        .expect("emit");
                }
            }
        }
        grid.feed(&sink);

        let expected = buffer_text(&current);
        let got = grid.text();
        if got != expected {
            for (row, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
                if g != e {
                    failures.push(format!(
                        "frame {i} row {row} glyph:\n   grid: {g:?}\n   want: {e:?}"
                    ));
                }
            }
        }

        // Style check: reverse video and background must match cell-for-cell.
        // Wide-glyph continuation columns are skipped — the buffer stores them
        // as reset blanks while the terminal paints them with the glyph's style.
        let cells = current.content();
        let width = current.area.width as usize;
        for y in 0..current.area.height as usize {
            let mut x = 0usize;
            while x < width {
                let cell = &cells[y * width + x];
                let sym_w = unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1);
                let want = Style2 {
                    reverse: cell.modifier.contains(Modifier::REVERSED),
                    bg: cell.bg != Color::Reset,
                };
                let got = grid.styles[y][x];
                if got != want {
                    failures.push(format!(
                        "frame {i} cell ({x},{y}) style: grid={got:?} want={want:?} sym={:?}",
                        cell.symbol()
                    ));
                }
                x += sym_w;
            }
        }
        prev = Some(current);
    }
    failures
}

#[test]
fn cjk_composer_typing_emission_is_faithful() {
    let typed = [
        "",
        "在",
        "在办",
        "在办完",
        "在办完给",
        "在办完",
        "在办",
        "在",
        "",
    ];
    let frames = typed
        .into_iter()
        .map(|text| {
            frame_lines(
                "PR #9 已合并（squash efe3039），issue #6 自动关闭。",
                text,
                format!("> {text}").len(),
                None,
            )
        })
        .collect();
    let failures = drive(frames);
    assert!(
        failures.is_empty(),
        "emission desync:\n{}",
        failures.join("\n")
    );
}

#[test]
fn scroll_counter_length_change_emission_is_faithful() {
    let frames = [236u32, 219, 210, 203, 199, 216, 236, 214, 201, 192, 36, 1]
        .into_iter()
        .map(|n| frame_lines("transcript row with CJK 内容 and ascii", "", 2, Some(n)))
        .collect();
    let failures = drive(frames);
    assert!(
        failures.is_empty(),
        "emission desync:\n{}",
        failures.join("\n")
    );
}

#[test]
fn scrolling_transcript_with_cjk_emission_is_faithful() {
    let frames = (0..12)
        .map(|i| {
            frame_lines(
                &format!("row {i} 混合内容 mixed content 行尾"),
                "在",
                3,
                Some(200 + i),
            )
        })
        .collect();
    let failures = drive(frames);
    assert!(
        failures.is_empty(),
        "emission desync:\n{}",
        failures.join("\n")
    );
}

#[test]
fn cjk_wide_char_shrink_emission_is_faithful() {
    // Cursor ON the last char (REVERSED on a wide glyph, VISIBLE_ON_BLANK) so
    // backspace takes the shrink-trailing path.
    let frames = ["在办", "在", ""]
        .into_iter()
        .map(|text| {
            let cursor = format!("> {text}")
                .char_indices()
                .nth(format!("> {text}").chars().count().saturating_sub(1))
                .map(|(i, _)| i)
                .unwrap_or(0);
            frame_lines("固定 transcript 行 fixed", text, cursor, None)
        })
        .collect();
    let failures = drive(frames);
    assert!(
        failures.is_empty(),
        "emission desync:\n{}",
        failures.join("\n")
    );
}

/// Cursor walks back and forth across CJK with REVERSED on wide glyphs — the
/// visible "block cursor" path, including every style flip.
#[test]
fn reversed_cursor_walking_cjk_emission_is_faithful() {
    let text = "在办完给";
    let full = format!("> {text}");
    let cursors: Vec<usize> = full
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(full.len()))
        .collect();
    let mut frames = Vec::new();
    for &c in cursors.iter() {
        frames.push(frame_lines("transcript 安静一行", text, c, None));
    }
    for &c in cursors.iter().rev() {
        frames.push(frame_lines("transcript 安静一行", text, c, None));
    }
    let failures = drive(frames);
    assert!(
        failures.is_empty(),
        "emission desync:\n{}",
        failures.join("\n")
    );
}

/// Sensitivity check for the instrument itself: a wide glyph must advance the
/// pen by two columns, so a width-blind "print at x+1" (what ratatui-crossterm
/// does after a wide cell) lands at x+2 and shows up as a mismatch. Without
/// this, the faithful-emission tests above could pass vacuously.
#[test]
fn grid_sees_wide_char_pen_advance() {
    let mut g = Grid::new(10, 2);
    // CUP(1,1) + "在" (two columns) + "X" printed with NO further MoveTo.
    g.feed(b"\x1b[1;1H\xE5\x9C\xA8X");
    // The 'X' lands in column 2 because the wide glyph consumed 0..=1.
    assert_eq!(g.cell_at(0, 0), '在');
    assert_eq!(g.cell_at(0, 1), '\u{0}', "continuation cell");
    assert_eq!(g.cell_at(0, 2), 'X', "wide glyph must advance 2 cols");
    assert_eq!(g.cell_at(0, 3), ' ');
}

/// `frames.txt` is captured by a SECOND `draw()` of the same `App` that the
/// screen draw just mutated. If `draw` were not idempotent the log would show a
/// different frame than the terminal — so pin that property.
#[test]
fn draw_is_idempotent_for_the_composer_shape() {
    use phimint::ui::app::App;
    use phimint::ui::render::draw as app_draw;
    let mut app = App::new();
    // A composer buffer with CJK and a fake cursor is pure render state; the
    // real risk is the viewport anchor. Feed the app some transcript rows so
    // `render_output` runs its anchor path, then draw twice.
    for i in 0..40 {
        app.transcript
            .push_system(&format!("row {i} 一些中文内容 with ascii tail"));
    }
    app.composer.insert_str("在办完给");

    let mut t1 = Terminal::new(TestBackend::new(W, H)).unwrap();
    t1.draw(|f| app_draw(f, &mut app)).unwrap();
    let first = t1.backend().buffer().clone();

    let mut t2 = Terminal::new(TestBackend::new(W, H)).unwrap();
    t2.draw(|f| app_draw(f, &mut app)).unwrap();
    let second = t2.backend().buffer().clone();

    let (a, b) = (buffer_text(&first), buffer_text(&second));
    for (row, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x, y, "draw() not idempotent at row {row}");
    }
}
