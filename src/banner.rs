//! Startup brand banner: a flat block "phimint" wordmark + tagline +
//! workspace/log metadata, laid out as a logo lockup (wordmark left, text
//! right, bottoms aligned) when the terminal is wide enough and as a stack
//! when it is not. Row spans keep the text pure so copy/selection and the
//! frame capture never see styling.

use std::path::Path;
use std::path::PathBuf;

use phi_agent_tui::{BrandRow, ColorScheme, ShellToken};

/// Semantic role of one text run in the banner — the wordmark generator's
/// internal vocabulary (design §4.3: `BannerStyle` retreats to this file; it
/// never crosses the shell seam again — see [`brand_rows`]). The palette is
/// all-warm: hierarchy comes from lightness steps (bright gold → orange →
/// burnt → ember-brown), never from neutral gray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerStyle {
    /// Uncolored spaces (letter connectors and intra-glyph padding).
    Default,
    /// Wordmark face fill (`█`) at gradient stop `0..=6`, left → right.
    Logo(u8),
    /// Full-width divider rule under the banner block.
    Rule,
    /// The slogan (bold).
    Slogan,
    /// `   -   v{version}` tail.
    Version,
    /// "Workspace" / "Logs" / "Built on" labels.
    Label,
    /// Path values.
    Value,
    /// `phi-agent` in the ad row — the brightest run of the card (bold).
    Ad,
    /// `-` separators and the docs URL.
    Dim,
}

/// 7-stop face gradient (stop 0 = leftmost letter). Precomputed so render
/// stays a table lookup; values match the approved design mockup exactly.
const LOGO_STOPS_DARK: [(u8, u8, u8); 7] = [
    (255, 106, 61),
    (255, 120, 66),
    (255, 135, 72),
    (255, 150, 78),
    (255, 164, 83),
    (255, 178, 88),
    (255, 193, 94),
];
const LOGO_STOPS_LIGHT: [(u8, u8, u8); 7] = [
    (196, 62, 16),
    (201, 75, 21),
    (205, 87, 26),
    (210, 100, 31),
    (215, 113, 36),
    (219, 125, 41),
    (224, 138, 46),
];

impl BannerStyle {
    /// 24-bit RGB for the given scheme. `Default` returns `(0,0,0)` and is not
    /// rendered by consumers (they pass the run through uncolored instead).
    pub fn rgb(self, scheme: ColorScheme) -> (u8, u8, u8) {
        match self {
            BannerStyle::Default => (0, 0, 0),
            BannerStyle::Logo(i) => {
                let stops = match scheme {
                    ColorScheme::Dark => &LOGO_STOPS_DARK,
                    ColorScheme::Light => &LOGO_STOPS_LIGHT,
                };
                debug_assert!(
                    usize::from(i) < stops.len(),
                    "gradient stop out of range: {i}"
                );
                stops[usize::from(i).min(stops.len() - 1)]
            }
            BannerStyle::Rule => match scheme {
                // Warm ember-brown, not neutral gray: the divider closes the
                // banner block without breaking the all-warm palette.
                ColorScheme::Dark => (0x4a, 0x32, 0x26),
                ColorScheme::Light => (0xd9, 0xc3, 0xac),
            },
            BannerStyle::Slogan => match scheme {
                ColorScheme::Dark => (0xff, 0xb0, 0x66),
                ColorScheme::Light => (0x9a, 0x4b, 0x12),
            },
            BannerStyle::Version => match scheme {
                ColorScheme::Dark => (0xb0, 0x7a, 0x52),
                ColorScheme::Light => (0x9c, 0x72, 0x50),
            },
            BannerStyle::Label => match scheme {
                ColorScheme::Dark => (0xf2, 0xa4, 0x62),
                ColorScheme::Light => (0xb5, 0x62, 0x1c),
            },
            BannerStyle::Value => match scheme {
                ColorScheme::Dark => (0xd1, 0x8f, 0x5d),
                ColorScheme::Light => (0x8f, 0x5a, 0x36),
            },
            BannerStyle::Ad => match scheme {
                ColorScheme::Dark => (0xff, 0xc6, 0x6e),
                ColorScheme::Light => (0xc4, 0x3e, 0x10),
            },
            BannerStyle::Dim => BannerStyle::Version.rgb(scheme),
        }
    }

    /// True for runs that render bold in addition to their color.
    pub fn is_bold(self) -> bool {
        matches!(
            self,
            BannerStyle::Logo(_) | BannerStyle::Slogan | BannerStyle::Ad
        )
    }
}

/// One styled text run. The row's plain text is the concatenation of these.
pub type BannerSpan = (String, BannerStyle);

/// One banner line: styled runs whose concatenated text is the line.
#[derive(Debug, Clone)]
pub struct BannerRow {
    pub spans: Vec<BannerSpan>,
}

/// A styled byte-range within an output line's plain text (TUI rendering).
/// `start`/`len` align with the concatenated run strings in `BannerRow::text`.
/// The generic span model lives in `ui::lines` (style-token agnostic); this
/// alias pins the banner's token so `SpanSpec { .. }` literals keep working.
pub type SpanSpec = phi_tui::lines::SpanSpec<BannerStyle>;

impl BannerRow {
    /// The concatenated plain text of the row (used by renderers and tests).
    pub fn text(&self) -> String {
        self.spans.iter().map(|(t, _)| t.as_str()).collect()
    }

    /// The plain text plus its non-`Default` styled ranges (byte offsets into
    /// the text). `Default` runs (e.g. wordmark connector spaces) are dropped —
    /// renderers fall back to the line's own style for them.
    pub fn to_runs(&self) -> (String, Vec<SpanSpec>) {
        let text = self.text();
        let mut spans = Vec::with_capacity(self.spans.len());
        let mut off = 0usize;
        for (s, style) in &self.spans {
            if *style != BannerStyle::Default {
                spans.push(SpanSpec {
                    start: off,
                    len: s.len(),
                    style: *style,
                });
            }
            off += s.len();
        }
        (text, spans)
    }
}

/// Compile one brand role for `scheme` into the shell's inline-style channel.
fn compile_style(role: BannerStyle, scheme: ColorScheme) -> ShellToken {
    ShellToken::Literal {
        rgb: role.rgb(scheme),
        bold: role.is_bold(),
    }
}

/// Compile generator rows into the shell's brand channel (design §4.3, Issue
/// 2.2): `Default` runs drop out (renderers fall back to the line's own style),
/// and each brand role resolves to a literal color for `scheme` — the gradient
/// ramp lands as per-letter literals, because terminals have no gradient
/// primitive. This is where the brand vocabulary stops: past this seam the
/// rows carry literal colors, never [`BannerStyle`].
pub fn brand_rows(rows: Vec<BannerRow>, scheme: ColorScheme) -> Vec<BrandRow> {
    rows.into_iter()
        .map(|row| {
            let (text, spans) = row.to_runs();
            let spans = spans
                .into_iter()
                .map(|s| phi_tui::lines::SpanSpec {
                    start: s.start,
                    len: s.len,
                    style: compile_style(s.style, scheme),
                })
                .collect();
            BrandRow { text, spans }
        })
        .collect()
}

/// Flat block glyphs for the letters of "phimint": the `ANSI Shadow` figlet
/// faces with their `╔═╗║╚╝` drop-shadow strokes removed, leaving only the
/// `█` letterforms and the spaces that hold each glyph's width — so the
/// wordmark keeps its 58-column footprint and its letter gaps, just without
/// the 3D edge. Each entry is `[row; 5]`, row 0 at the top, and every row of
/// one glyph has the same width (asserted by test). `GLYPHS[i][row]` is the
/// `i`th letter's `row`-th row.
const GLYPHS: [[&str; 5]; 7] = [
    // P
    ["██████  ", "██   ██ ", "██████  ", "██      ", "██      "],
    // H
    ["██   ██ ", "██   ██ ", "███████ ", "██   ██ ", "██   ██ "],
    // I
    ["██ ", "██ ", "██ ", "██ ", "██ "],
    // M
    [
        "███    ███ ",
        "████  ████ ",
        "██ ████ ██ ",
        "██  ██  ██ ",
        "██      ██ ",
    ],
    // I
    ["██ ", "██ ", "██ ", "██ ", "██ "],
    // N
    [
        "███    ██ ",
        "████   ██ ",
        "██ ██  ██ ",
        "██  ██ ██ ",
        "██   ████ ",
    ],
    // T
    [
        "████████ ",
        "   ██    ",
        "   ██    ",
        "   ██    ",
        "   ██    ",
    ],
];

/// The five wordmark rows: letters joined by single `Default` connector
/// spaces. Each glyph is split per character into face fill (`█` →
/// `Logo(letter)` gradient stop) and padding (` ` → `Default`) runs;
/// consecutive same-style characters merge into one span. Anything that is
/// not face fill is padding — the flat wordmark has no shadow strokes.
pub fn logo_rows() -> Vec<BannerRow> {
    (0..5)
        .map(|row| {
            let mut spans: Vec<BannerSpan> = Vec::new();
            for (i, glyph) in GLYPHS.iter().enumerate() {
                if i > 0 {
                    push_char(&mut spans, ' ', BannerStyle::Default);
                }
                let stop = i as u8;
                for ch in glyph[row].chars() {
                    if ch == '█' {
                        push_char(&mut spans, ch, BannerStyle::Logo(stop));
                    } else {
                        push_char(&mut spans, ' ', BannerStyle::Default);
                    }
                }
            }
            BannerRow { spans }
        })
        .collect()
}

/// Append `ch` to `spans`, merging into the previous run when styles match.
fn push_char(spans: &mut Vec<BannerSpan>, ch: char, style: BannerStyle) {
    match spans.last_mut() {
        Some((text, prev)) if *prev == style => text.push(ch),
        _ => spans.push((ch.to_string(), style)),
    }
}

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
    if let Some(rest) = s.strip_prefix(hs.as_ref())
        && rest.starts_with('/')
    {
        return format!("~{rest}");
    }
    s.into_owned()
}

/// `shorten_home_with` using `$HOME` from the environment.
pub fn shorten_home(path: &Path) -> String {
    shorten_home_with(
        path,
        std::env::var("HOME").ok().map(PathBuf::from).as_deref(),
    )
}

/// The slogan row: brand voice (emphasis) + `   -   v{version}` tail.
/// The wordmark already carries the name — no "Phimint" here.
/// Separator is ASCII `-`, not `·`: per the CJK chrome guard
/// (`chrome_sources_stay_cjk_width_safe`), which was introduced with the
/// `·` → `-` retrofit in 3ae1926.
fn tagline_row(version: &str) -> BannerRow {
    BannerRow {
        spans: vec![
            (
                "Forged with intent. Shipped with care.".to_string(),
                BannerStyle::Slogan,
            ),
            (format!("   -   v{version}"), BannerStyle::Version),
        ],
    }
}

/// The phi-agent endorsement row (label column width matches `info_row`).
/// Separator is ASCII `-`, not `·` — same CJK chrome guard as `tagline_row`.
fn ad_row() -> BannerRow {
    BannerRow {
        spans: vec![
            ("Built on   ".to_string(), BannerStyle::Label),
            ("phi-agent".to_string(), BannerStyle::Ad),
            (" - ".to_string(), BannerStyle::Dim),
            ("https://docs.phiagent.dev/".to_string(), BannerStyle::Dim),
        ],
    }
}

/// The full-width divider: `term_width` dashes as one `Rule` run. It marks
/// the end of the banner block ("page-header" close), not a content underline.
/// Callers pass `term_width ≥ 1` (run.rs clamps); the `.max(1)` below is a
/// belt-and-suspenders floor, so width 0 still renders 1 dash.
fn rule_row(term_width: usize) -> BannerRow {
    BannerRow {
        spans: vec![("─".repeat(term_width.max(1)), BannerStyle::Rule)],
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

/// Columns of breathing room between the wordmark and the right-hand text
/// block when the two sit side by side (the logo lockup).
const LOCKUP_GAP: usize = 2;

/// Display columns of a banner row's plain text (CJK path segments count 2).
fn row_width(row: &BannerRow) -> usize {
    unicode_width::UnicodeWidthStr::width(row.text().as_str())
}

/// Build the banner. Two layouts, picked by whether the lockup fits:
///
/// * **lockup** — the wordmark on the left, the slogan/ad/info block on the
///   right, bottoms aligned so the block's last row shares the wordmark's
///   last row. Taken when `term_width ≥ wordmark + gap + widest text row`.
/// * **stacked** — the classic stack: wordmark, spacer, then the text block
///   underneath. Narrow terminals land here so nothing clips.
///
/// Either way the block ends with the full-width `─` rule. `term_width` is the
/// terminal's column count at startup (resize afterwards may leave the rule
/// slightly stale — clipped when narrower, a stub when wider; accepted by
/// design).
pub fn build(
    workspace: &Path,
    log_path: &Path,
    version: &str,
    term_width: usize,
) -> Vec<BannerRow> {
    let text = [
        tagline_row(version),
        ad_row(),
        info_row("Workspace", &shorten_home(workspace)),
        info_row("Logs", &shorten_home(log_path)),
    ];
    let logo = logo_rows();
    let logo_width = logo.iter().map(row_width).max().unwrap_or(0);
    let text_width = text.iter().map(row_width).max().unwrap_or(0);

    let mut rows = if term_width >= logo_width + LOCKUP_GAP + text_width {
        lockup_rows(&logo, &text, LOCKUP_GAP)
    } else {
        stacked_rows(&logo, &text)
    };
    rows.push(rule_row(term_width));
    rows
}

/// The side-by-side lockup: one row per wordmark row, with the text block hung
/// off the right and bottom-aligned — text row `j` lands on wordmark row
/// `j + offset`, where `offset` is whatever keeps the block's last row on the
/// wordmark's last row. The caller only picks this layout when the combined
/// width fits, so nothing here has to deal with overflow.
fn lockup_rows(logo: &[BannerRow], text: &[BannerRow], gap: usize) -> Vec<BannerRow> {
    let offset = logo.len().saturating_sub(text.len());
    let height = logo.len().max(offset + text.len());
    (0..height)
        .map(|i| {
            let mut spans = logo.get(i).map(|r| r.spans.clone()).unwrap_or_default();
            if let Some(t) = i.checked_sub(offset).and_then(|j| text.get(j)) {
                spans.push((" ".repeat(gap), BannerStyle::Default));
                spans.extend(t.spans.iter().cloned());
            }
            BannerRow { spans }
        })
        .collect()
}

/// The classic stack: wordmark, spacer, then the text block underneath.
fn stacked_rows(logo: &[BannerRow], text: &[BannerRow]) -> Vec<BannerRow> {
    let mut rows = logo.to_vec();
    rows.push(BannerRow { spans: vec![] });
    rows.extend(text.iter().cloned());
    rows
}

/// Resolve `--color-scheme` ("auto"/"dark"/"light") plus detected signals to a
/// concrete scheme. `color_fgbg` is `$COLORFGBG`: "fg;bg" with background `7`
/// meaning the terminal is white. `osc11` is an optional override from terminal
/// probing (currently unused, always `None`).
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

#[cfg(test)]
mod tests;

// The product's chrome-sources scan (the shell carries its own over its own
// paths; the product's chrome — the wordmark banner — scans its own here).
// Its own file, outside the scanned set: the banned-glyph table and the
// marker vocabulary must never be mistaken for rendering code.
#[cfg(test)]
#[path = "banner/chrome_scan_tests.rs"]
mod chrome_scan;
