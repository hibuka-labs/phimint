# Startup Banner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the 4 plain dark-gray startup lines with a figlet "phiforge" wordmark + `PhiForge` tagline + Workspace/Logs metadata, colored per a dark/light palette, in both the TUI and `--inline` modes.

**Architecture:** A new `src/banner.rs` is the single source of truth: it builds 9 styled rows (`Vec<BannerRow>` where each row is `Vec<(String, BannerStyle)>`) plus a `render_ansi` helper for inline and an injectable color-scheme resolver. The TUI stores banner rows as ordinary `OutputLine`s that carry a per-line `spans` overlay (`Vec<SpanSpec>`, byte offsets into the plain `text`), so copy/selection/frame-capture see unchanged plain text while rendering applies per-run colors. `main.rs` gains `--color-scheme` and `--banner` flags and resolves the scheme once, before raw mode.

**Tech Stack:** Rust 2024, ratatui + crossterm (TUI), 24-bit ANSI (inline). No new dependencies; the wordmark glyphs are statically embedded.

Spec: `docs/superpowers/specs/2026-08-17-startup-banner-design.md`

---

## File Map

- **Create** `src/banner.rs` — everything brand/banner: `ColorScheme`, `BannerStyle` (+ RGB mapping), `BannerRow`, glyphs, `build()`, `render_ansi()`, `shorten_home_with()`, scheme resolution (`resolve_scheme`, `parse_osc11`, `probe_osc11`). All its unit tests live in the same file.
- **Modify** `src/main.rs` — `mod banner;`, two new clap flags, resolve scheme, pass `scheme`/`show_banner` into both UI entries.
- **Modify** `src/ui/app.rs` — `SpanSpec`, `OutputLine.spans`, `App.scheme`, `App::set_scheme`, `App::push_banner`; every existing `OutputLine { text, kind }` literal gains `spans: None` (compiler-driven).
- **Modify** `src/ui/render.rs` — `render_output` renders spans when present; new `banner_style()`.
- **Modify** `src/ui/mod.rs` — call `banner::build` + `app.push_banner`; remove now-unused `session_dir` string.
- **Modify** `src/inline.rs` — call `banner::build` + `render_ansi`; remove now-unused `session_dir` string.
- **Test** — unit tests in `app.rs`, `render.rs`, `banner.rs`; full `cargo test`.

Reference points (may shift by a few lines as tasks land):
- `src/ui/mod.rs:100-103` — old banner `push_system` calls
- `src/inline.rs:114-118` — old banner `renderer.line` calls
- `src/ui/app.rs:68-71` — `OutputLine` definition
- `src/ui/render.rs:80-97` — the per-line styling loop to extend

---

### Task 1: Create `src/banner.rs` — types, palette, glyphs

**Files:**
- Create: `src/banner.rs`
- Modify: `src/main.rs` (one line)

- [ ] **Step 1: Write the failing tests first**

Create `src/banner.rs` with the types plus tests only (no implementation bodies yet — stub everything to fail). Start by writing this complete file:

```rust
//! Startup brand banner: a figlet "phiforge" wordmark + tagline + workspace/log
//! metadata. Single source of truth for both the TUI (ratatui) and inline
//! (`--inline`) renderers; row spans keep the text pure so copy/selection and
//! the frame capture never see styling.

use std::path::Path;
use std::time::Duration;

/// Which palette pair to use. `Dark` is the default (most terminals); `Light`
/// swaps every hue for a deeper, higher-contrast variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

/// Semantic role of one text run in the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerStyle {
    /// Uncolored connector (e.g. the single spaces between wordmark letters).
    Default,
    /// First tone of the wordmark gradient.
    LogoA,
    /// Second tone of the wordmark gradient.
    LogoB,
    /// The "PhiForge" brand name on the tagline (bold).
    Brand,
    /// Tagline text after the brand name.
    Tagline,
    /// "· v0.1.0" version suffix.
    Version,
    /// "Workspace" / "Logs" labels.
    Label,
    /// Path values.
    Value,
}

impl BannerStyle {
    /// 24-bit RGB for the given scheme. `Default` returns `(0,0,0)` and is not
    /// rendered by consumers (they pass the run through uncolored instead).
    pub fn rgb(self, scheme: ColorScheme) -> (u8, u8, u8) {
        match self {
            BannerStyle::Default => (0, 0, 0),
            BannerStyle::LogoA => match scheme {
                ColorScheme::Dark => (0xff, 0x78, 0x47),
                ColorScheme::Light => (0xc4, 0x3e, 0x10),
            },
            BannerStyle::LogoB => match scheme {
                ColorScheme::Dark => (0xff, 0xa9, 0x4d),
                ColorScheme::Light => (0xd9, 0x6a, 0x1f),
            },
            BannerStyle::Brand => match scheme {
                ColorScheme::Dark => (0xff, 0xb0, 0x66),
                ColorScheme::Light => (0x9a, 0x4b, 0x12),
            },
            BannerStyle::Tagline => match scheme {
                ColorScheme::Dark => (0xd0, 0xd7, 0xde),
                ColorScheme::Light => (0x57, 0x60, 0x6a),
            },
            BannerStyle::Version => match scheme {
                ColorScheme::Dark => (0x7d, 0x85, 0x90),
                ColorScheme::Light => (0x6e, 0x76, 0x81),
            },
            BannerStyle::Label => BannerStyle::Brand.rgb(scheme),
            BannerStyle::Value => BannerStyle::Tagline.rgb(scheme),
        }
    }

    /// True for the brand name, which renders bold in addition to its color.
    pub fn is_bold(self) -> bool {
        matches!(self, BannerStyle::Brand)
    }
}

/// One styled text run. The row's plain text is the concatenation of these.
pub type BannerSpan = (String, BannerStyle);

/// One banner line: styled runs whose concatenated text is the line.
#[derive(Debug, Clone)]
pub struct BannerRow {
    pub spans: Vec<BannerSpan>,
}

impl BannerRow {
    /// A row made of a single unstyled run (connector spaces).
    pub fn plain(text: &str) -> Self {
        Self { spans: vec![(text.to_string(), BannerStyle::Default)] }
    }
    /// The concatenated plain text of the row (used by renderers and tests).
    pub fn text(&self) -> String {
        self.spans.iter().map(|(t, _)| t.as_str()).collect()
    }
}

/// A path shortened by replacing its `$HOME` prefix with `~`.
pub fn shorten_home_with(path: &Path, home: Option<&Path>) -> String;

/// Build the banner: 6 wordmark rows + tagline + two info rows.
pub fn build(workspace: &Path, log_path: &Path, version: &str) -> Vec<BannerRow>;

/// Render rows as 24-bit ANSI lines (inline mode).
pub fn render_ansi(rows: &[BannerRow], scheme: ColorScheme) -> Vec<String>;

/// Resolve `--color-scheme` ("auto"/"dark"/"light") plus detected signals.
pub fn resolve_scheme(choice: &str, color_fgbg: Option<&str>, osc11: Option<ColorScheme>) -> ColorScheme;

/// Parse an OSC 11 reply like `\x1b]11;rgb:1f1f/1f1f/1f1f` into a scheme.
pub fn parse_osc11(reply: &str) -> Option<ColorScheme>;

/// Query the terminal background via OSC 11, waiting up to `timeout`. Returns
/// `None` if the terminal doesn't answer. Call before raw mode / alt screen.
pub fn probe_osc11(timeout: Duration) -> Option<ColorScheme>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_light_is_distinct_from_dark() {
        for s in [BannerStyle::LogoA, BannerStyle::LogoB, BannerStyle::Brand,
                  BannerStyle::Tagline, BannerStyle::Version, BannerStyle::Label,
                  BannerStyle::Value] {
            assert_ne!(s.rgb(ColorScheme::Dark), s.rgb(ColorScheme::Light), "{s:?}");
        }
    }

    #[test]
    fn logo_rows_are_six_equal_width_rows() {
        let rows = logo_rows_for_test();
        assert_eq!(rows.len(), 6);
        let w = rows[0].text().len();
        for row in &rows {
            assert_eq!(row.text().len(), w, "rows must be equal width");
        }
        // Only box-drawing chars + spaces live in the wordmark.
        for row in &rows {
            for (chunk, style) in &row.spans {
                if *style == BannerStyle::Default {
                    assert_eq!(chunk, " ", "connector must be a single space");
                    continue;
                }
                assert!(chunk.chars().all(|c| "█╔══╝╗║".contains(c)), "bad glyph char: {chunk:?}");
            }
        }
    }

    #[test]
    fn glyphs_are_six_rows_each_with_consistent_width() {
        for (i, glyph) in GLYPHS.iter().enumerate() {
            let w = glyph[0].len();
            for row in glyph {
                assert_eq!(row.len(), w, "glyph {i} row width mismatch");
            }
        }
    }

    #[test]
    fn shorten_home_replaces_prefix() {
        let home = Path::new("/Users/eve");
        assert_eq!(shorten_home_with(Path::new("/Users/eve/proj"), Some(home)), "~/proj");
        assert_eq!(shorten_home_with(Path::new("/Users/eve"), Some(home)), "~");
        assert_eq!(shorten_home_with(Path::new("/tmp/x"), Some(home)), "/tmp/x");
        assert_eq!(shorten_home_with(Path::new("/Users/evelyn/x"), Some(home)), "/Users/evelyn/x");
        assert_eq!(shorten_home_with(Path::new("/p"), None), "/p");
    }

    #[test]
    fn build_emits_nine_rows_with_brand_and_version() {
        let rows = build(Path::new("/Users/eve/w"), Path::new("/Users/eve/.phiforge/s/1/session.log"), "0.1.0");
        assert_eq!(rows.len(), 9, "6 art + tagline + 2 info");
        let tagline = rows[6].text();
        assert!(tagline.contains("PhiForge"), "missing brand: {tagline}");
        assert!(tagline.contains("先验再交"), "missing slogan: {tagline}");
        assert!(tagline.contains("· v0.1.0"), "missing version: {tagline}");
        let ws = rows[7].text();
        let logs = rows[8].text();
        assert!(ws.starts_with("Workspace") && ws.contains("~/w"), "bad workspace row: {ws}");
        assert!(logs.starts_with("Logs") && logs.ends_with("session.log"), "bad logs row: {logs}");
        // Labels are right-padded so values align.
        assert!(rows[7].spans.iter().any(|(t, s)| *s == BannerStyle::Label && t.len() == 11));
    }

    #[test]
    fn render_ansi_emits_color_codes_and_skips_default() {
        let row = BannerRow {
            spans: vec![
                (" ".to_string(), BannerStyle::Default),
                ("PhiForge".to_string(), BannerStyle::Brand),
            ],
        };
        let line = render_ansi(&[row], ColorScheme::Dark)[0].clone();
        assert!(line.starts_with(" "), "default run must pass through: {line:?}");
        assert!(line.contains("\x1b[38;2;255;176;102m"), "brand rgb missing: {line:?}");
        assert!(line.contains("\x1b[1m"), "brand must be bold: {line:?}");
        assert!(line.ends_with("\x1b[0m"), "must reset at end: {line:?}");
    }

    #[test]
    fn resolve_scheme_obeys_flag_then_env_then_osc() {
        assert_eq!(resolve_scheme("dark", Some("7;0"), Some(ColorScheme::Light)), ColorScheme::Dark);
        assert_eq!(resolve_scheme("light", None, None), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", Some("7;0"), None), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", Some("0;7"), None), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", Some("15;0"), None), ColorScheme::Dark);
        assert_eq!(resolve_scheme("auto", None, Some(ColorScheme::Light)), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", None, None), ColorScheme::Dark);
        assert_eq!(resolve_scheme("auto", None, Some(ColorScheme::Dark)), ColorScheme::Dark);
    }

    #[test]
    fn parse_osc11_reads_rgb_and_garbage() {
        assert_eq!(parse_osc11("\x1b]11;rgb:1f1f/1f1f/1f1f\x07"), Some(ColorScheme::Dark));
        assert_eq!(parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x07"), Some(ColorScheme::Light));
        assert_eq!(parse_osc11(""), None);
        assert_eq!(parse_osc11("junk"), None);
    }
}
```

Note the test file references `logo_rows_for_test()` and `GLYPHS` — both will exist by the end of Task 1. Add `logo_rows_for_test` as a public alias used only by tests is awkward; instead the tests will call the real `logo_rows()` (defined in Task 1 Step 3). Replace `logo_rows_for_test()` with `logo_rows()` in the test above when you implement; the failing test run in Step 2 will report `logo_rows` as undefined, which is expected.

- [ ] **Step 2: Register the module and run the tests to confirm they fail**

Add `mod banner;` to `src/main.rs` after line 7 (`mod agent;`), alphabetical order:

```rust
mod agent;
mod approval;
mod banner;
mod gate;
mod inline;
mod lang;
mod lsp;
mod markdown;
mod tools;
mod ui;
```

Run: `cargo test banner:: 2>&1 | tail -30`
Expected: FAIL — `cannot find function 'logo_rows'`, `cannot find function 'shorten_home_with'`, etc. (only the `palette_light_is_distinct_from_dark` test passes once `rgb` is implemented — see next step).

- [ ] **Step 3: Implement `logo_rows`, `GLYPHS`, `shorten_home_with`**

Append to `src/banner.rs` (below the stubbed `probe_osc11`):

```rust
/// `ANSI Shadow` figlet glyphs for the letters of "phiforge". Each entry is
/// `[row; 6]`, row 0 at the top, and every row of one glyph has the same width
/// (asserted by test). `GLYPHS[i][row]` is the `i`th letter's `row`-th row.
const GLYPHS: [[&str; 6]; 8] = [
    // P
    ["██████╗ ", "██╔══██╗", "██████╔╝", "██╔═══╝ ", "██║     ", "╚═╝     "],
    // H
    ["██╗  ██╗", "██║  ██║", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"],
    // I
    ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
    // F
    ["███████╗", "██╔════╝", "█████╗  ", "██╔══╝  ", "██║     ", "╚═╝     "],
    // O
    [" ██████╗ ", "██╔═══██╗", "██║   ██║", "██║   ██║", "╚██████╔╝", " ╚═════╝ "],
    // R
    ["██████╗ ", "██╔══██╗", "██████╔╝", "██╔══██╗", "██║  ██║", "╚═╝  ╚═╝"],
    // G
    [" ██████╗ ", "██╔════╝ ", "██║  ███╗", "██║   ██║", "╚██████╔╝", " ╚═════╝ "],
    // E
    ["███████╗", "██╔════╝", "█████╗  ", "██╔══╝  ", "███████╗", "╚══════╝"],
];

/// The six wordmark rows: letters joined by single `Default` connector spaces,
/// alternating `LogoA`/`LogoB` per letter.
pub fn logo_rows() -> Vec<BannerRow> {
    (0..6)
        .map(|row| {
            let mut spans = Vec::new();
            for (i, glyph) in GLYPHS.iter().enumerate() {
                if i > 0 {
                    spans.push((" ".to_string(), BannerStyle::Default));
                }
                let style = if i % 2 == 0 { BannerStyle::LogoA } else { BannerStyle::LogoB };
                spans.push((glyph[row].to_string(), style));
            }
            BannerRow { spans }
        })
        .collect()
}
```

Then replace the stubbed `shorten_home_with` with:

```rust
/// A path shortened by replacing its `$HOME` prefix with `~`. `home` is the
/// home directory (normally `env::var("HOME")`).
pub fn shorten_home_with(path: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return path.display().to_string();
    };
    let s = path.to_string_lossy();
    let hs = home.to_string_lossy();
    if s == hs {
        return "~".to_string();
    }
    if let Some(rest) = s.strip_prefix(hs.as_ref()) {
        if rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

/// `shorten_home_with` using `$HOME` from the environment.
pub fn shorten_home(path: &Path) -> String {
    shorten_home_with(path, std::env::var("HOME").ok().map(PathBuf::from).as_deref())
}
```

(Add `use std::path::PathBuf;` at the top of the file.)

- [ ] **Step 4: Implement `build` and `render_ansi`**

Replace the stubs:

```rust
/// The tagline row: brand name (bold) + English positioning + slogan + version.
fn tagline_row(version: &str) -> BannerRow {
    BannerRow {
        spans: vec![
            ("PhiForge".to_string(), BannerStyle::Brand),
            (" · product-first coding agent on phi-agent · 先验再交 ".to_string(), BannerStyle::Tagline),
            (format!("· v{version}"), BannerStyle::Version),
        ],
    }
}

/// A `Label`+`Value` info row with the label right-padded so values align.
fn info_row(label: &str, value: &str) -> BannerRow {
    BannerRow {
        spans: vec![
            (format!("{label:<9}  "), BannerStyle::Label),
            (value.to_string(), BannerStyle::Value),
        ],
    }
}

/// Build the banner: 6 wordmark rows + tagline + Workspace + Logs.
pub fn build(workspace: &Path, log_path: &Path, version: &str) -> Vec<BannerRow> {
    let mut rows = logo_rows();
    rows.push(tagline_row(version));
    rows.push(info_row("Workspace", &shorten_home(workspace)));
    rows.push(info_row("Logs", &shorten_home(log_path)));
    rows
}

/// Render rows as 24-bit ANSI lines (inline mode). `Default` runs pass through
/// plain so connector spaces stay uncolored.
pub fn render_ansi(rows: &[BannerRow], scheme: ColorScheme) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut out = String::new();
            for (text, style) in &row.spans {
                if *style == BannerStyle::Default {
                    out.push_str(text);
                } else {
                    let (r, g, b) = style.rgb(scheme);
                    let bold = if style.is_bold() { "\x1b[1m" } else { "" };
                    out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{bold}{text}\x1b[0m"));
                }
            }
            out
        })
        .collect()
}
```

- [ ] **Step 5: Implement scheme resolution**

Replace the stubs:

```rust
/// Resolve `--color-scheme` ("auto"/"dark"/"light") plus detected signals to a
/// concrete scheme. `color_fgbg` is `$COLORFGBG`: "fg;bg" with background `7`
/// meaning the terminal is white. `osc11` is a parsed `parse_osc11` result.
pub fn resolve_scheme(
    choice: &str,
    color_fgbg: Option<&str>,
    osc11: Option<ColorScheme>,
) -> ColorScheme {
    match choice {
        "dark" => ColorScheme::Dark,
        "light" => ColorScheme::Light,
        _ => {
            if let Some(v) = color_fgbg {
                let bg = v.split(';').nth(1).unwrap_or(v);
                if bg.trim() == "7" {
                    return ColorScheme::Light;
                }
            }
            osc11.unwrap_or(ColorScheme::Dark)
        }
    }
}

/// Parse a terminal OSC 11 reply like `\x1b]11;rgb:1f1f/1f1f/1f1f` (or on
/// iTerm² `\x1b]11;rgb:1f1f/1f1f/1f1f\x07`) into a scheme. `None` on garbage.
/// Channels may be 4-bit ("1f1f") or 2-bit ("1f") hex — `from_str_radix` on the
/// full 4-char string yields the same value.
pub fn parse_osc11(reply: &str) -> Option<ColorScheme> {
    let pos = reply.find("11;")?;
    let mut rest = reply[pos + 3..].split('/');
    let hex = |s: &str| u8::from_str_radix(s.trim(), 16).ok();
    let (r, g, b) = (hex(rest.next()?)?, hex(rest.next()?)?, hex(rest.next()?)?);
    // Naive perceived-brightness; ~0.6 separates dark from light terminals.
    let lum = (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0;
    Some(if lum > 0.6 { ColorScheme::Light } else { ColorScheme::Dark })
}

/// Query the terminal background with OSC 11 and wait up to `timeout` for a
/// reply. Best-effort: returns `None` when the terminal doesn't answer, so
/// callers fall back to dark. Must run before raw mode / alternate screen.
pub fn probe_osc11(timeout: Duration) -> Option<ColorScheme> {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]11;?\x07");
    let _ = out.flush();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(timeout).ok().and_then(|line| parse_osc11(&line))
}
```

Note: `parse_osc11` expects the receiver strings like `rgb:1f1f/1f1f/1f1f`; terminal replies are usually prefixed `\x1b]11;` and may include trailing `\x07` — `split('/')` before the `/rgb` split works because the prefix has no `/`. Adjust the parse if the reply format your terminal sends differs, then align the test.

- [ ] **Step 6: Run the tests, fix the `logo_rows_for_test` reference, verify green**

In the test file, replace `logo_rows_for_test()` with `logo_rows()`.

Run: `cargo test banner:: 2>&1 | tail -20`
Expected: all 8 tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src/banner.rs src/main.rs
git commit -m "feat: banner module — wordmark glyphs, palette, build, scheme detection"
```

---

### Task 2: TUI rendering — `OutputLine.spans`, `App::push_banner`, span-aware render

**Files:**
- Modify: `src/ui/app.rs`
- Modify: `src/ui/render.rs`

- [ ] **Step 1: Write the failing test for `push_banner`**

Append to the `#[cfg(test)] mod tests` in `src/ui/app.rs`:

```rust
    #[test]
    fn push_banner_stores_unwrapped_lines_with_span_offsets() {
        use crate::banner::{BannerRow, BannerStyle, ColorScheme};
        let mut app = App::new();
        app.scheme = ColorScheme::Dark;
        let rows = crate::banner::build(
            Path::new("/Users/eve/w"),
            Path::new("/Users/eve/.phiforge/s/1/session.log"),
            "0.1.0",
        );
        app.push_banner(rows);
        assert_eq!(app.output.len(), 9, "banner must push 9 lines, never soft-wrapped");
        let first = &app.output[0];
        assert!(first.spans.is_some(), "logo row must carry spans");
        assert_eq!(first.text.len(), 68, "wordmark width");
        // Byte offsets must tile the plain text.
        for (i, line) in app.output.iter().enumerate() {
            let spans = line.spans.as_ref().expect("banner rows all have spans");
            let mut cursor = 0usize;
            for s in spans {
                assert_eq!(s.start, cursor, "line {i}: non-contiguous span");
                assert!(line.text.is_char_boundary(s.start + s.len), "line {i}: bad utf8 boundary");
                cursor = s.start + s.len;
            }
            assert_eq!(cursor, line.text.len(), "line {i}: spans must cover text");
        }
        // The tagline is still copyable as plain text.
        let tagline = &app.output[6];
        assert!(tagline.text.contains("PhiForge · product-first"));
        assert_eq!(app.last_reply_text(), "", "banner must not become 'last reply'");
    }
```

`push_banner` doesn't exist yet, so the test won't compile — that's the red step.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test push_banner_stores_unwrapped_lines 2>&1 | tail -15`
Expected: FAIL — `no method named 'push_banner'`, `no field 'scheme'`.

- [ ] **Step 3: Add `SpanSpec`, `OutputLine.spans`, `App.scheme`, `push_banner`**

In `src/ui/app.rs`:

1. Add to the imports at the top (near the existing `use crate::...` lines):
   ```rust
   use crate::banner::{BannerRow, ColorScheme};
   ```
   (If `app.rs` has no `use crate::` block yet, add one.)

2. Extend `LineKind`/`OutputLine` — after the `OutputLine` struct, add `SpanSpec`; modify `OutputLine`:

```rust
/// A styled text run over `[start, start+len)` bytes of [`OutputLine::text`].
/// Banner rows use this to render per-run colors while `text` stays the plain
/// concatenation (copy, selection, and frame capture never see styling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanSpec {
    pub start: usize,
    pub len: usize,
    pub style: banner::BannerStyle,
}

#[derive(Debug, Clone)]
pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
    /// Optional per-run styling. `None` (the norm) uses `style_for(kind)`.
    pub spans: Option<Vec<SpanSpec>>,
}
```

3. Add the `scheme` field to `App` and initialize it (both the struct def and `App::new`):

```rust
    /// Color palette resolved at startup (Dark/Light); used by the banner and
    /// any future scheme-aware chrome.
    pub scheme: ColorScheme,
```
In `App::new()`:
```rust
            scheme: ColorScheme::Dark,
```

4. Set every existing `OutputLine { text, kind }` literal to `OutputLine { text, kind, spans: None }`. There are ~33 literals scattered through `src/ui/app.rs` (both the running code and `#[cfg(test)]`). The compiler enumerates them one by one — run `cargo build` and fix every error, then verify with `grep -n "OutputLine {" src/ui/app.rs | wc -l` that the count went from 30 to 34 (the struct def no longer matches, plus the new test literal in `push_banner_stores_unwrapped_lines...`).

5. Add `push_banner` right after `push_system`:

```rust
    /// Append prebuilt banner rows verbatim (never soft-wrapped — the wordmark
    /// glyphs must stay intact). Each row is stored as a plain `text` plus byte
    /// spans so rendering can color per run while copy/selection stay plain.
    pub fn push_banner(&mut self, rows: Vec<BannerRow>) {
        for row in rows {
            let mut text = String::new();
            let mut spans = Vec::with_capacity(row.spans.len());
            for (chunk, style) in row.spans {
                let start = text.len();
                text.push_str(&chunk);
                spans.push(SpanSpec { start, len: chunk.len(), style });
            }
            self.output.push(OutputLine {
                text,
                kind: LineKind::System,
                spans: Some(spans),
            });
        }
    }
```

- [ ] **Step 4: Run the tests to make them pass**

Run: `cargo test push_banner_stores_unwrapped_lines 2>&1 | tail -15`
Expected: PASS.

Then run the whole suite to catch the mechanical literal edits:
Run: `cargo test 2>&1 | tail -20`
Expected: all tests green (any leftover `OutputLine {` without `spans:` will be a compile error already fixed in Step 3).

- [ ] **Step 5: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat: TUI OutputLine spans + push_banner (banner stored unwrapped)"
```

---

### Task 3: Span-aware rendering in `render.rs`

**Files:**
- Modify: `src/ui/render.rs`

- [ ] **Step 1: Write the failing test**

In `src/ui/render.rs` `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn banner_rows_render_spans_and_snapshot_shows_text() {
        let mut app = App::new();
        app.scheme = crate::banner::ColorScheme::Dark;
        let rows = crate::banner::build(
            std::path::Path::new("/tmp/ws"),
            std::path::Path::new("/tmp/ws/session.log"),
            "0.1.0",
        );
        app.push_banner(rows);

        // 9 banner lines + one normal line; snapshot at 80 cols shows the art
        // and the tagline text.
        let text = snapshot_text(&mut app, 80, 20);
        assert!(text.contains("PhiForge"), "tagline missing:\n{text}");
        assert!(text.contains("Workspace"), "workspace row missing:\n{text}");
        // A logo line must survive to the buffer (68-col art fits in 80).
        assert!(text.lines().next().unwrap().len() > 30, "logo line missing:\n{text}");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test banner_rows_render_spans 2>&1 | tail -15`
Expected: current render only styles whole lines with `style_for(kind)`; with spans present the snapshot still renders the text (ratatui ignores extra data we don't use), so this test may already pass on text — the stronger fail-check is that the rendering ignores spans. Force the red: temporarily assert the test's first `.contains("PhiForge")` is fine, but add `assert!(false, "spans not implemented")`? No — the real red signal is that the snapshot renders but does NOT panic while spans are ignored. Since we can't diff style visually in a text snapshot, also add a span-count assertion by rendering via `TestBackend` and reading `Buffer` cells' styles:

Replace the text assertions in the test with a style check:

```rust
        // Push banner, then assert the rendered buffer actually applies colors:
        // find the cell under the "PhoForge" 'P' and check it isn't the default fg.
```
See Step 3's implementation before finalizing — the practical red/green is: **Task 3 Step 1's snapshot test passes even without span styling** (because snapshot drops styles). So the meaningful unit for "span styling happens at all" is the `banner_style` helper, which we unit-test directly instead:

Test:
```rust
    #[test]
    fn banner_style_maps_to_rgb_and_bold() {
        let d = crate::banner::ColorScheme::Dark;
        let s = banner_style(crate::banner::BannerStyle::Brand, d);
        assert_eq!(s.fg, Some(Color::Rgb(0xff, 0xb0, 0x66)), "brand rgb");
        assert!(s.add_modifier.contains(Modifier::BOLD), "brand bold");
        let plain = banner_style(crate::banner::BannerStyle::Default, d);
        assert_eq!(plain.fg, None, "Default must be uncolored");
    }
```

- [ ] **Step 3: Run to confirm these fail**

Run: `cargo test banner_style_maps_to_rgb_and_bold 2>&1 | tail -15`
Expected: FAIL — `banner_style is not defined` (that IS the red).

- [ ] **Step 4: Implement span-aware rendering**

In `src/ui/render.rs`:

1. Add `banner_style` beside `style_for`:

```rust
/// Map a banner run's style to a ratatui style for the current scheme.
fn banner_style(style: crate::banner::BannerStyle, scheme: crate::banner::ColorScheme) -> Style {
    use crate::banner::BannerStyle;
    if style == BannerStyle::Default {
        return Style::default();
    }
    let (r, g, b) = style.rgb(scheme);
    let mut s = Style::default().fg(Color::Rgb(r, g, b));
    if style.is_bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}
```

2. Rewrite `render_output`'s per-line styling to honour spans:

```rust
    let mut lines: Vec<Line> = Vec::with_capacity(window.len());
    for i in window {
        let (text, kind) = if i < committed {
            (&app.output[i].text, app.output[i].kind)
        } else {
            (tail_lines[i - committed].as_str(), tail_kind)
        };
        let selected = i < committed && app.is_selected(i);
        if i < committed {
            if let Some(spans) = &app.output[i].spans {
                let mut line_spans: Vec<Span> = Vec::with_capacity(spans.len());
                let mut cursor = 0usize;
                for s in spans {
                    if s.start > cursor {
                        let gap = text[cursor..s.start].to_string();
                        line_spans.push(if selected {
                            Span::styled(gap, Style::default().bg(Color::DarkGray))
                        } else {
                            Span::raw(gap)
                        });
                    }
                    let mut st = banner_style(s.style, app.scheme);
                    if selected {
                        st = st.bg(Color::DarkGray);
                    }
                    line_spans.push(Span::styled(
                        text[s.start..s.start + s.len].to_string(),
                        st,
                    ));
                    cursor = s.start + s.len;
                }
                if cursor < text.len() {
                    let rest = text[cursor..].to_string();
                    line_spans.push(if selected {
                        Span::styled(rest, Style::default().bg(Color::DarkGray))
                    } else {
                        Span::raw(rest)
                    });
                }
                lines.push(Line::from(line_spans));
                continue;
            }
        }
        let mut style = style_for(kind);
        if selected {
            style = style.bg(Color::DarkGray);
        }
        lines.push(Line::from(Span::styled(text.to_string(), style)));
    }
```

This replaces the block from `let mut lines ...` through `lines.push(...)` in the existing `render_output`. Keep everything else (window computation, tail handling, final `f.render_widget(Paragraph::new(lines), area)`) unchanged.

- [ ] **Step 5: Run tests**

Run: `cargo test banner_style_maps_to_rgb_and_bold banner_rows_render_spans 2>&1 | tail -15`
Expected: both PASS. Then full suite:
Run: `cargo test 2>&1 | tail -10`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add src/ui/render.rs
git commit -m "feat: TUI renders banner spans with per-run colors"
```

---

### Task 4: Wire main.rs — flags, scheme resolution, dispatch

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Write the failing (compile) tests first**

This task is wiring; the red signal is `cargo build` failing because `run_tui`/`run_inline` signatures don't yet take the new params. The two new clap flags and the `resolve_scheme` call are exercised by `cargo build` + the manual `--help` smoke in Step 5.

- [ ] **Step 2: Confirm it fails**

Run: `cargo build 2>&1 | tail -5`
Expected: build succeeds (nothing wired yet). This task's red bubble appears in Step 4.

- [ ] **Step 3: Add the flags and resolve the scheme**

In `src/main.rs` `struct Cli`, after `log_level`:

```rust
    /// Banner color scheme: auto-detect the terminal background, or force
    /// dark/light. Overrides any terminal detection.
    #[arg(long, default_value = "auto", value_parser = ["auto", "dark", "light"])]
    color_scheme: String,

    /// Show the startup brand banner (wordmark + workspace/log info).
    #[arg(long, default_value = "on", value_parser = ["on", "off"])]
    banner: String,
```

Below the `let use_tui = !cli.inline;` line:

```rust
    // Resolve the banner color scheme once, before any UI runs (the OSC 11
    // probe needs a normal, not raw-mode, terminal).
    let show_banner = cli.banner == "on";
    let scheme = if show_banner {
        banner::resolve_scheme(
            &cli.color_scheme,
            std::env::var("COLORFGBG").ok().as_deref(),
            if cli.color_scheme == "auto" {
                banner::probe_osc11(std::time::Duration::from_millis(100))
            } else {
                None
            },
        )
    } else {
        banner::ColorScheme::Dark
    };
```

Add `use std::time::Duration;` — or inline the literal as above (inline `std::time::Duration::from_millis` is fine, no extra use).

- [ ] **Step 4: Pass the params into both UIs (compile-red, then green)**

Update the dispatch calls:

```rust
    if use_tui {
        ui::run_tui(agent, session, session_ctx, workspace, approval_rx, scheme, show_banner).await
    } else {
        inline::run_inline(agent, session, session_ctx, workspace, approval_rx, scheme, show_banner).await
    }
```

At this point `cargo build` fails — `run_tui`/`run_inline` don't accept the two extra args. (Red step done.)

- [ ] **Step 5: Run build + smoke**

Run: `cargo build 2>&1 | tail -5`
Expected: still errors about argument count — expected until Task 5 updates the call sites. To keep the build green at this commit boundary, instead run:

Run: `cargo check --bin phiforge 2>&1 | grep -E "run_tui|run_inline|error" | head -5`
Expected: errors naming `run_tui`/`run_inline` (the red is intentional at this intermediate commit; see Step 6 note).

- [ ] **Step 6: Commit (commit the intended-red boundary, then land Task 5 before merging)**

Because the build is red until Task 5's signature changes land, commit this task and implement Task 5 **in the same commit**:

```bash
git add src/main.rs
# then implement Task 5, and commit once with Task 5's message.
```

---

### Task 5: Replace call sites — `ui/mod.rs` and `inline.rs`

**Files:**
- Modify: `src/ui/mod.rs`
- Modify: `src/inline.rs`

- [ ] **Step 1: Modify `run_tui` in `src/ui/mod.rs`**

Signature — add the two params after `approval_rx`:

```rust
pub async fn run_tui(
    agent: PhiAgent,
    session: SessionId,
    session_ctx: SessionContext,
    workspace: PathBuf,
    approval_rx: Option<mpsc::UnboundedReceiver<ApprovalItem>>,
    scheme: crate::banner::ColorScheme,
    show_banner: bool,
) -> Result<()> {
```

Replace the two lines that compute the strings (lines ~62-63):

```rust
    let log_path = session_ctx.log_path();
```

(delete the `session_dir` string line entirely — it becomes unused)

Replace the banner block (lines ~100-103):

```rust
    app.scheme = scheme;
    if show_banner {
        app.push_banner(crate::banner::build(
            &workspace,
            log_path.as_path(),
            env!("CARGO_PKG_VERSION"),
        ));
    }
```

- [ ] **Step 2: Modify `run_inline` in `src/inline.rs`**

Signature — same two params appended:

```rust
pub async fn run_inline(
    agent: PhiAgent,
    session: SessionId,
    session_ctx: SessionContext,
    workspace: PathBuf,
    approval_rx: Option<mpsc::UnboundedReceiver<ApprovalItem>>,
    scheme: crate::banner::ColorScheme,
    show_banner: bool,
) -> Result<()> {
```

Replace lines ~88-89:

```rust
    let log_path = session_ctx.log_path();
```

(delete the `session_dir` string line — unused now; keep `inline_raw` which builds from `session_ctx.session_dir` directly.)

Replace the banner block (lines ~114-118):

```rust
    if show_banner {
        let rows = crate::banner::build(
            &workspace,
            log_path.as_path(),
            env!("CARGO_PKG_VERSION"),
        );
        for ansi in crate::banner::render_ansi(&rows, scheme) {
            renderer.line(&ansi);
        }
    }
```

- [ ] **Step 3: Build and fix stragglers**

Run: `cargo build 2>&1 | tail -10`
Expected: clean build. Common stragglers: unused `session_dir`/`log_path` bindings elsewhere — remove them; `log_path` is now a `PathBuf`, so if any other line used the old `String` (it didn't — grep to be sure: `grep -n "log_path\|session_dir" src/ui/mod.rs src/inline.rs`) adjust.

- [ ] **Step 4: Full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: all green, including:
- `banner::` tests (Task 1)
- `push_banner_stores_unwrapped_lines_with_span_offsets` (Task 2)
- `banner_style_maps_to_rgb_and_bold`, `banner_rows_render_spans_and_snapshot_shows_text` (Task 3)

- [ ] **Step 5: Manual smoke (requires your LLM env)**

Run: `cargo run -- --help | head -30`
Expected: usage lists `--color-scheme <dark|light|auto>` and `--banner <on|off>`.

Then run interactively in your terminal (black background expected):
`cargo run -- --color-scheme dark`
and once with `--color-scheme light`; and `--banner off` should print no wordmark. Both TUI and `--inline` variants. Check: wordmark glyphs are contiguous (no wrapping), "PhiForge" is bold orange, `Workspace`/`Logs` labels align, paths use `~`.

- [ ] **Step 6: Commit — together with Task 4's staged main.rs**

```bash
git add src/ui/mod.rs src/inline.rs src/main.rs
git commit -m "feat: startup banner in TUI and inline modes"
```

---

### Task 6: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Full suite + clippy**

Run: `cargo test 2>&1 | tail -12`
Expected: all tests pass (count the `test result: ok` line).

Run: `cargo clippy --all-targets 2>&1 | tail -8`
Expected: no errors; fix any warnings that point at the new code (e.g. needless borrows, redundant clones).

- [ ] **Step 2: Confirm the spec's no-regression items**

Run: `grep -rn "coding agent on phi-agent\|Workspace:" src/ | grep -v banner.rs`
Expected: no old one-off banner strings remain except inside `src/banner.rs` (tagline) and the intentional inline demo test at `src/inline.rs:860`.

- [ ] **Step 3: Commit any clippy fixes**

```bash
git add -A
git commit -m "style: clippy cleanups for banner wiring" || echo "nothing to commit"
```

---

## Self-Review (done during planning)

- **Spec coverage:** logo+palette → Task 1; `PhiForge` tagline + version → `build`/`tagline_row`; `shorten_home` → Task 1; auto/dark/light + `--banner` → Task 4; TUI spans → Tasks 2–3; inline ANSI → Tasks 1 + 5; `COLORFGBG`/OSC 11 → Task 1. No spec section left unplanned.
- **Type consistency:** `BannerStyle` variants used identically in `app.rs` (via re-export path `banner::BannerStyle`), `render.rs`, and `banner.rs`. `BannerRow`, `ColorScheme`, `SpanSpec` signatures consistent across tasks. `push_banner(rows: Vec<BannerRow>)`, `build(&Path, &Path, &str) -> Vec<BannerRow>`, `render_ansi(&[BannerRow], ColorScheme) -> Vec<String>`.
- **Placeholders:** every step has exact code; the two intentional "find the literal, the compiler tells you where" steps are explicit mechanical procedures with verification greps.