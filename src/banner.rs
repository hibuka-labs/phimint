//! Startup brand banner: a flat block "phimint" wordmark + tagline +
//! workspace/log metadata, laid out as a logo lockup (wordmark left, text
//! right, bottoms aligned) when the terminal is wide enough and as a stack
//! when it is not. Row spans keep the text pure so copy/selection and the
//! frame capture never see styling.

use std::path::Path;
use std::path::PathBuf;

/// Which palette pair to use. `Dark` is the default (most terminals); `Light`
/// swaps every hue for a deeper, higher-contrast variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

/// Semantic role of one text run in the banner. The palette is all-warm:
/// hierarchy comes from lightness steps (bright gold → orange → burnt →
/// ember-brown), never from neutral gray.
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
    /// Turn-final `- error` status marker (dark red).
    Error,
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
            BannerStyle::Error => match scheme {
                ColorScheme::Dark => (0xc0, 0x40, 0x30),
                ColorScheme::Light => (0x8a, 0x22, 0x18),
            },
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
mod tests {
    use super::*;

    #[test]
    fn palette_light_is_distinct_from_dark() {
        for s in [
            BannerStyle::Logo(0),
            BannerStyle::Logo(3),
            BannerStyle::Logo(6),
            BannerStyle::Rule,
            BannerStyle::Slogan,
            BannerStyle::Version,
            BannerStyle::Label,
            BannerStyle::Value,
            BannerStyle::Ad,
            BannerStyle::Dim,
            BannerStyle::Error,
        ] {
            assert_ne!(s.rgb(ColorScheme::Dark), s.rgb(ColorScheme::Light), "{s:?}");
        }
    }

    /// All-warm constraint: text variants must not fall back to neutral gray
    /// (red channel leading, spread ≥ 20).
    #[test]
    fn text_styles_stay_warm() {
        for scheme in [ColorScheme::Dark, ColorScheme::Light] {
            for s in [
                BannerStyle::Slogan,
                BannerStyle::Version,
                BannerStyle::Label,
                BannerStyle::Value,
                BannerStyle::Ad,
                BannerStyle::Dim,
                BannerStyle::Error,
            ] {
                let (r, g, b) = s.rgb(scheme);
                assert!(
                    r > g && g >= b,
                    "{s:?}/{scheme:?} not warm-ordered: {r},{g},{b}"
                );
                let spread = i32::from(r.max(g).max(b)) - i32::from(r.min(g).min(b));
                assert!(spread >= 20, "{s:?}/{scheme:?} too neutral: {r},{g},{b}");
            }
        }
    }

    #[test]
    fn logo_rows_are_five_equal_width_rows() {
        let rows = logo_rows();
        assert_eq!(rows.len(), 5);
        let w = rows[0].text().chars().count();
        for row in &rows {
            assert_eq!(row.text().chars().count(), w, "rows must be equal width");
        }
        // Flat wordmark: the glyph table holds face fill and padding, nothing
        // else. The old `╔═╗║╚╝` drop-shadow strokes are gone for good.
        for glyph in GLYPHS {
            for row in glyph {
                assert!(
                    row.chars().all(|c| c == '█' || c == ' '),
                    "non-flat glyph char: {row:?}"
                );
            }
        }
    }

    /// Literal `█` → `Logo(stop)`, space → `Default`. The charset splits
    /// cleanly into the two, zero ambiguity — that is the mechanical basis
    /// for gradient color splitting.
    #[test]
    fn wordmark_face_fill_split_by_charset() {
        for row in logo_rows() {
            for (chunk, style) in &row.spans {
                match style {
                    BannerStyle::Default => {
                        assert!(chunk.chars().all(|c| c == ' '), "{chunk:?}")
                    }
                    BannerStyle::Logo(_) => {
                        assert!(
                            !chunk.is_empty() && chunk.chars().all(|c| c == '█'),
                            "{chunk:?}"
                        )
                    }
                    other => panic!("unexpected wordmark style: {other:?}"),
                }
            }
        }
    }

    /// Plain-text invariant: span merging must not change the copyable text
    /// (copy/frame capture depend on it).
    #[test]
    fn wordmark_row_text_is_single_space_joined_glyphs() {
        for (r, row) in logo_rows().iter().enumerate() {
            let expected: String = GLYPHS
                .iter()
                .map(|glyph| glyph[r])
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(row.text(), expected, "row {r} text drifted");
        }
    }

    /// Gradient semantics: stop = letter index (PHIMINT left→right 0..=6).
    /// Mirroring/misalignment would still let the color-split test pass, so
    /// the definition is pinned here. A letter's literal can be split by
    /// spaces into several runs (M/N/T), so the expected sequence is expanded
    /// over GLYPHS' contiguous `█` runs: every run's stop must equal its
    /// letter index.
    #[test]
    fn gradient_stop_equals_letter_index_left_to_right() {
        for (r, row) in logo_rows().iter().enumerate() {
            let mut expected: Vec<u8> = Vec::new();
            for (i, glyph) in GLYPHS.iter().enumerate() {
                let mut in_face = false;
                for ch in glyph[r].chars() {
                    if ch == '█' {
                        if !in_face {
                            expected.push(i as u8);
                        }
                        in_face = true;
                    } else {
                        in_face = false;
                    }
                }
            }
            let got: Vec<u8> = row
                .spans
                .iter()
                .filter_map(|(_, s)| match s {
                    BannerStyle::Logo(i) => Some(*i),
                    _ => None,
                })
                .collect();
            assert_eq!(got, expected, "row {r}: stops must equal letter indices");
        }
    }

    #[test]
    fn logo_gradient_stops_are_seven_distinct_steps() {
        for scheme in [ColorScheme::Dark, ColorScheme::Light] {
            let stops: std::collections::HashSet<(u8, u8, u8)> =
                (0..7u8).map(|i| BannerStyle::Logo(i).rgb(scheme)).collect();
            assert_eq!(
                stops.len(),
                7,
                "{scheme:?}: stops must be pairwise distinct"
            );
            assert_ne!(
                BannerStyle::Logo(0).rgb(scheme),
                BannerStyle::Logo(6).rgb(scheme),
                "gradient must move across the wordmark"
            );
        }
    }

    #[test]
    fn bold_set_is_logo_slogan_ad() {
        for i in 0..7u8 {
            assert!(BannerStyle::Logo(i).is_bold(), "Logo({i})");
        }
        assert!(BannerStyle::Slogan.is_bold());
        assert!(BannerStyle::Ad.is_bold());
        for s in [
            BannerStyle::Default,
            BannerStyle::Rule,
            BannerStyle::Version,
            BannerStyle::Label,
            BannerStyle::Value,
            BannerStyle::Dim,
            BannerStyle::Error,
        ] {
            assert!(!s.is_bold(), "{s:?} must not be bold");
        }
    }

    /// Golden values from the approved design mockup — a channel typo in the
    /// tables must not sail through the distinctness checks above.
    #[test]
    fn logo_stop_tables_match_design_golden_values() {
        assert_eq!(
            LOGO_STOPS_DARK,
            [
                (255, 106, 61),
                (255, 120, 66),
                (255, 135, 72),
                (255, 150, 78),
                (255, 164, 83),
                (255, 178, 88),
                (255, 193, 94),
            ]
        );
        assert_eq!(
            LOGO_STOPS_LIGHT,
            [
                (196, 62, 16),
                (201, 75, 21),
                (205, 87, 26),
                (210, 100, 31),
                (215, 113, 36),
                (219, 125, 41),
                (224, 138, 46),
            ]
        );
    }

    /// Letter count and gradient stop count must match, or `Logo(i)` gets
    /// silently clamped to stop 6 in release builds.
    #[test]
    fn glyph_count_matches_gradient_stops() {
        assert_eq!(GLYPHS.len(), LOGO_STOPS_DARK.len());
        assert_eq!(GLYPHS.len(), LOGO_STOPS_LIGHT.len());
    }

    #[test]
    fn glyphs_are_five_rows_each_with_consistent_width() {
        for (i, glyph) in GLYPHS.iter().enumerate() {
            assert_eq!(glyph.len(), 5, "glyph {i} row count");
            let w = glyph[0].chars().count();
            for row in glyph {
                assert_eq!(row.chars().count(), w, "glyph {i} row width mismatch");
            }
        }
    }

    #[test]
    fn shorten_home_replaces_prefix() {
        let home = Path::new("/Users/eve");
        assert_eq!(
            shorten_home_with(Path::new("/Users/eve/proj"), Some(home)),
            "~/proj"
        );
        assert_eq!(shorten_home_with(Path::new("/Users/eve"), Some(home)), "~");
        assert_eq!(shorten_home_with(Path::new("/tmp/x"), Some(home)), "/tmp/x");
        assert_eq!(
            shorten_home_with(Path::new("/Users/evelyn/x"), Some(home)),
            "/Users/evelyn/x"
        );
        assert_eq!(shorten_home_with(Path::new("/p"), None), "/p");
    }

    /// The four content rows' plain text: slogan, ad, Workspace, Logs. Same
    /// words in both layouts — only their position differs.
    fn assert_content_text(tagline: &str, ad: &str, ws: &str, logs: &str) {
        assert!(
            tagline.contains("Forged with intent. Shipped with care."),
            "missing slogan: {tagline}"
        );
        assert!(tagline.contains("v0.1.0"), "missing version: {tagline}");
        assert!(
            !tagline.contains("Phimint"),
            "brand is redundant with the wordmark: {tagline}"
        );
        assert!(ad.starts_with("Built on"), "bad ad row: {ad}");
        assert!(ad.contains("phi-agent"), "missing engine: {ad}");
        assert!(
            ad.contains("https://docs.phiagent.dev/"),
            "missing docs url: {ad}"
        );
        let want_ws = shorten_home(Path::new("/Users/eve/w"));
        let want_logs = shorten_home(Path::new("/Users/eve/.phimint/s/1/session.log"));
        assert!(
            ws.starts_with("Workspace") && ws.ends_with(&want_ws),
            "bad ws row: {ws}"
        );
        assert!(
            logs.starts_with("Logs") && logs.ends_with(&want_logs),
            "bad logs: {logs}"
        );
    }

    /// Label column is 11 display columns (`{label:<9}  `) on all three label
    /// rows — ad/Workspace/Logs values start at the same x. Pinned on the row
    /// builders so neither layout can reflow it.
    #[test]
    fn label_column_is_eleven_display_columns() {
        for (n, row) in [
            ("ad", ad_row()),
            ("ws", info_row("Workspace", "/w")),
            ("logs", info_row("Logs", "/l")),
        ] {
            let label = &row.spans[0].0;
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(label.as_str()),
                11,
                "{n} label column must be 11 cols: {label:?}"
            );
        }
    }

    #[test]
    fn build_stacks_wordmark_and_text_when_the_lockup_does_not_fit() {
        let rows = build(
            Path::new("/Users/eve/w"),
            Path::new("/Users/eve/.phimint/s/1/session.log"),
            "0.1.0",
            80,
        );
        assert_eq!(
            rows.len(),
            11,
            "5 art + spacer + slogan + ad + 2 info + rule"
        );
        // The wordmark owns its five rows outright.
        for (i, art) in logo_rows().iter().enumerate() {
            assert_eq!(rows[i].text(), art.text(), "row {i} must be bare wordmark");
        }
        assert_eq!(rows[5].text(), "", "spacer row");
        assert_content_text(
            &rows[6].text(),
            &rows[7].text(),
            &rows[8].text(),
            &rows[9].text(),
        );
    }

    #[test]
    fn build_locks_up_wordmark_and_text_when_it_fits() {
        let rows = build(
            Path::new("/Users/eve/w"),
            Path::new("/Users/eve/.phimint/s/1/session.log"),
            "0.1.0",
            200,
        );
        assert_eq!(rows.len(), 6, "5 lockup rows + rule");
        let logo = logo_rows();
        // Char offset of the right-hand block: wordmark + gap. Slicing by char
        // (not display width) so a CJK path in the text block cannot skew it.
        let x = logo[0].text().chars().count() + LOCKUP_GAP;

        // Row 0 is wordmark only — the text block hangs at the bottom.
        assert_eq!(rows[0].text(), logo[0].text(), "row 0 carries no text");
        for (i, art) in logo.iter().enumerate().skip(1) {
            assert!(
                rows[i].text().starts_with(&art.text()),
                "row {i} must lead with the wordmark: {}",
                rows[i].text()
            );
        }
        // Rows 1..=4 carry slogan/ad/Workspace/Logs, bottoms aligned onto the
        // wordmark's last row.
        let right = |i: usize| -> String { rows[i].text().chars().skip(x).collect() };
        assert_content_text(&right(1), &right(2), &right(3), &right(4));

        // The right-hand text keeps its own runs — the slogan still rides
        // Slogan, just shifted past the wordmark.
        let (_, runs) = rows[1].to_runs();
        assert!(
            runs.iter().any(|r| r.style == BannerStyle::Slogan),
            "slogan run lost in the lockup"
        );
    }

    /// The lockup is a width decision, not a taste one: taken only when
    /// wordmark + gap + widest text row fit, so nothing clips. Paths are data —
    /// a long one must push the banner back to the stack.
    #[test]
    fn build_picks_the_lockup_only_when_the_block_fits() {
        let ws = Path::new("/Users/eve/w");
        let log = Path::new("/Users/eve/.phimint/s/1/session.log");
        let stacked = build(ws, log, "0.1.0", 80);
        let lockup = build(ws, log, "0.1.0", 400);
        let text_width = (6..10).map(|i| row_width(&stacked[i])).max().unwrap();
        let need = row_width(&logo_rows()[0]) + LOCKUP_GAP + text_width;

        assert_eq!(
            build(ws, log, "0.1.0", need - 1).len(),
            stacked.len(),
            "stacked one column short of the need"
        );
        assert_eq!(
            build(ws, log, "0.1.0", need).len(),
            lockup.len(),
            "lockup exactly at the need"
        );

        let long = Path::new("/Users/eve/a/deeply/nested/workspace/that/keeps/going/on/and/on");
        assert_eq!(
            build(long, log, "0.1.0", need).len(),
            stacked.len(),
            "long path must fall back to the stack"
        );
    }

    /// Style runs of the content rows, pinned via `to_runs()`: the design's
    /// row/col alignment depends on this exact run order (label width, slogan
    /// emphasis, Ad brightness, Dim separators).
    #[test]
    fn content_row_runs_match_design_styles() {
        let rows = build(
            Path::new("/Users/eve/w"),
            Path::new("/Users/eve/.phimint/s/1/session.log"),
            "0.1.0",
            80,
        );
        fn run_text(text: &str, start: usize, len: usize) -> &str {
            &text[start..start + len]
        }

        // Row 6 slogan: Slogan body + Version tail.
        let (text, runs) = rows[6].to_runs();
        assert_eq!(
            runs.iter().map(|r| r.style).collect::<Vec<_>>(),
            vec![BannerStyle::Slogan, BannerStyle::Version],
            "row 6 style order: {text}"
        );
        assert_eq!(
            run_text(&text, runs[0].start, runs[0].len),
            "Forged with intent. Shipped with care."
        );
        assert_eq!(run_text(&text, runs[1].start, runs[1].len), "   -   v0.1.0");

        // Row 7 ad: Label + Ad + Dim separator + Dim url.
        let (text, runs) = rows[7].to_runs();
        assert_eq!(
            runs.iter().map(|r| r.style).collect::<Vec<_>>(),
            vec![
                BannerStyle::Label,
                BannerStyle::Ad,
                BannerStyle::Dim,
                BannerStyle::Dim
            ],
            "row 7 style order: {text}"
        );
        assert_eq!(run_text(&text, runs[0].start, runs[0].len), "Built on   ");
        assert_eq!(run_text(&text, runs[1].start, runs[1].len), "phi-agent");
        assert_eq!(run_text(&text, runs[2].start, runs[2].len), " - ");
        assert_eq!(
            run_text(&text, runs[3].start, runs[3].len),
            "https://docs.phiagent.dev/"
        );
    }

    #[test]
    fn rule_row_is_full_width_dash_line() {
        for width in [1usize, 60, 100] {
            let rows = build(Path::new("/w"), Path::new("/l"), "0.1.0", width);
            let rule = rows.last().unwrap();
            let (text, spans) = rule.to_runs();
            assert_eq!(text.chars().count(), width, "rule width {width}");
            assert!(
                text.chars().all(|c| c == '─'),
                "rule must be dashes: {text:?}"
            );
            assert_eq!(spans.len(), 1, "rule is one run");
            assert_eq!(spans[0].style, BannerStyle::Rule);
        }
    }

    #[test]
    fn resolve_scheme_obeys_flag_then_env_then_osc() {
        assert_eq!(
            resolve_scheme("dark", Some("7;0"), Some(ColorScheme::Light)),
            ColorScheme::Dark
        );
        assert_eq!(resolve_scheme("light", None, None), ColorScheme::Light);
        // $COLORFGBG is "fg;bg" — background 7 means a light terminal.
        assert_eq!(resolve_scheme("auto", Some("7;0"), None), ColorScheme::Dark);
        assert_eq!(
            resolve_scheme("auto", Some("0;7"), None),
            ColorScheme::Light
        );
        assert_eq!(
            resolve_scheme("auto", Some("15;0"), None),
            ColorScheme::Dark
        );
        assert_eq!(
            resolve_scheme("auto", None, Some(ColorScheme::Light)),
            ColorScheme::Light
        );
        assert_eq!(resolve_scheme("auto", None, None), ColorScheme::Dark);
        assert_eq!(
            resolve_scheme("auto", None, Some(ColorScheme::Dark)),
            ColorScheme::Dark
        );
    }

    #[test]
    fn to_runs_produces_byte_ranges_skipping_default() {
        use BannerStyle as S;
        let row = BannerRow {
            spans: vec![
                ("██".to_string(), S::Logo(0)),
                (" ".to_string(), S::Default),
                ("abc".to_string(), S::Slogan),
            ],
        };
        let (text, runs) = row.to_runs();
        assert_eq!(text, "██ abc");
        assert_eq!(
            runs,
            vec![
                SpanSpec {
                    start: 0,
                    len: 6,
                    style: S::Logo(0)
                },
                SpanSpec {
                    start: 7,
                    len: 3,
                    style: S::Slogan
                },
            ]
        );
    }
}
