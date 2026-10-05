//! Startup brand banner: a figlet "phimint" wordmark + tagline + workspace/log
//! metadata. Row spans keep the text pure so copy/selection and the frame
//! capture never see styling.

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
    /// Wordmark face stroke (`█`) at gradient stop `0..=6`, left → right.
    Logo(u8),
    /// Wordmark drop-shadow strokes (`╔═╗║╚╝`).
    LogoShadow,
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
            BannerStyle::LogoShadow => match scheme {
                ColorScheme::Dark => (0x4a, 0x32, 0x26),
                ColorScheme::Light => (0xd9, 0xc3, 0xac),
            },
            BannerStyle::Rule => BannerStyle::LogoShadow.rgb(scheme),
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

/// `ANSI Shadow` figlet glyphs for the letters of "phimint". Each entry is
/// `[row; 6]`, row 0 at the top, and every row of one glyph has the same width
/// (asserted by test). `GLYPHS[i][row]` is the `i`th letter's `row`-th row.
const GLYPHS: [[&str; 6]; 7] = [
    // P
    [
        "██████╗ ",
        "██╔══██╗",
        "██████╔╝",
        "██╔═══╝ ",
        "██║     ",
        "╚═╝     ",
    ],
    // H
    [
        "██╗  ██╗",
        "██║  ██║",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    // I
    ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
    // M
    [
        "███╗   ███╗",
        "████╗ ████║",
        "██╔████╔██║",
        "██║╚██╔╝██║",
        "██║ ╚═╝ ██║",
        "╚═╝     ╚═╝",
    ],
    // I
    ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
    // N
    [
        "███╗   ██╗",
        "████╗  ██║",
        "██╔██╗ ██║",
        "██║╚██╗██║",
        "██║ ╚████║",
        "╚═╝  ╚═══╝",
    ],
    // T
    [
        "████████╗",
        "╚══██╔══╝",
        "   ██║   ",
        "   ██║   ",
        "   ██║   ",
        "   ╚═╝   ",
    ],
];

/// The six wordmark rows: letters joined by single `Default` connector spaces.
/// Each glyph is split per character into face (`█` → `Logo(letter)` gradient
/// stop) and shadow (`╔═╗║╚╝` → `LogoShadow`) runs; consecutive same-style
/// characters merge into one span.
pub fn logo_rows() -> Vec<BannerRow> {
    (0..6)
        .map(|row| {
            let mut spans: Vec<BannerSpan> = Vec::new();
            for (i, glyph) in GLYPHS.iter().enumerate() {
                if i > 0 {
                    push_char(&mut spans, ' ', BannerStyle::Default);
                }
                let stop = i as u8;
                for ch in glyph[row].chars() {
                    let style = match ch {
                        '█' => BannerStyle::Logo(stop),
                        ' ' => BannerStyle::Default,
                        _ => BannerStyle::LogoShadow,
                    };
                    push_char(&mut spans, ch, style);
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
    if let Some(rest) = s.strip_prefix(hs.as_ref()) {
        if rest.starts_with('/') {
            return format!("~{rest}");
        }
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

/// Build the banner: 6 wordmark rows + spacer + slogan + ad + Workspace +
/// Logs + full-width rule = 12 rows. `term_width` is the terminal's column
/// count at startup (resize afterwards may leave the rule slightly stale —
/// clipped when narrower, a stub when wider; accepted by design).
pub fn build(
    workspace: &Path,
    log_path: &Path,
    version: &str,
    term_width: usize,
) -> Vec<BannerRow> {
    let mut rows = logo_rows();
    rows.push(BannerRow { spans: vec![] });
    rows.push(tagline_row(version));
    rows.push(ad_row());
    rows.push(info_row("Workspace", &shorten_home(workspace)));
    rows.push(info_row("Logs", &shorten_home(log_path)));
    rows.push(rule_row(term_width));
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
            BannerStyle::LogoShadow,
            BannerStyle::Rule,
            BannerStyle::Slogan,
            BannerStyle::Version,
            BannerStyle::Label,
            BannerStyle::Value,
            BannerStyle::Ad,
            BannerStyle::Dim,
        ] {
            assert_ne!(s.rgb(ColorScheme::Dark), s.rgb(ColorScheme::Light), "{s:?}");
        }
    }

    /// 全暖约束：文字变体禁止回到中性灰（红通道领先、色散 ≥ 20）。
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
    fn logo_rows_are_six_equal_width_rows() {
        let rows = logo_rows();
        assert_eq!(rows.len(), 6);
        let w = rows[0].text().chars().count();
        for row in &rows {
            assert_eq!(row.text().chars().count(), w, "rows must be equal width");
        }
        for row in &rows {
            for (chunk, style) in &row.spans {
                if *style == BannerStyle::Default {
                    assert!(
                        chunk.chars().all(|c| c == ' '),
                        "non-space Default: {chunk:?}"
                    );
                    continue;
                }
                assert!(
                    chunk.chars().all(|c| c == ' ' || "█╔═╝╗║╚".contains(c)),
                    "bad glyph char: {chunk:?}"
                );
            }
        }
    }

    /// 字面 `█` → `Logo(档)`，阴影 `╔═╗║╚╝` → `LogoShadow`，空格 → `Default`。
    /// 字符集两分零歧义，这是 3D 立体感的机制基础。
    #[test]
    fn wordmark_face_and_shadow_split_by_charset() {
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
                    BannerStyle::LogoShadow => {
                        assert!(
                            !chunk.is_empty() && chunk.chars().all(|c| "╔═╗║╚╝".contains(c)),
                            "{chunk:?}"
                        )
                    }
                    other => panic!("unexpected wordmark style: {other:?}"),
                }
            }
        }
    }

    /// 纯文本不变式：span 合并不得改变可复制文本（copy/frame capture 依赖它）。
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

    /// 渐变语义：stop = 字母序号（PHIMINT 左→右 0..=6）。镜像/错位会让
    /// 分色测试照样通过，这里把定义钉死。一个字母的字面笔画可被阴影/空格拆
    /// 成多段 run（H/M/N/T），故按 GLYPHS 的连续 `█` 段展开期望序列：每段 run
    /// 的 stop 都必须等于其字母序号；基线行（如 `╚═╝`）无 `█`，期望为空。
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
            BannerStyle::LogoShadow,
            BannerStyle::Rule,
            BannerStyle::Version,
            BannerStyle::Label,
            BannerStyle::Value,
            BannerStyle::Dim,
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

    /// 字母数与渐变档数必须一致，否则 `Logo(i)` 在 release 里会被静默夹到 stop 6。
    #[test]
    fn glyph_count_matches_gradient_stops() {
        assert_eq!(GLYPHS.len(), LOGO_STOPS_DARK.len());
        assert_eq!(GLYPHS.len(), LOGO_STOPS_LIGHT.len());
    }

    #[test]
    fn glyphs_are_six_rows_each_with_consistent_width() {
        for (i, glyph) in GLYPHS.iter().enumerate() {
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

    #[test]
    fn build_emits_twelve_rows_with_slogan_ad_and_rule() {
        let rows = build(
            Path::new("/Users/eve/w"),
            Path::new("/Users/eve/.phimint/s/1/session.log"),
            "0.1.0",
            80,
        );
        assert_eq!(
            rows.len(),
            12,
            "6 art + blank + slogan + ad + 2 info + rule"
        );
        assert_eq!(rows[6].text(), "", "spacer row");
        let tagline = rows[7].text();
        assert!(
            tagline.contains("Forged with intent. Shipped with care."),
            "missing slogan: {tagline}"
        );
        assert!(tagline.contains("v0.1.0"), "missing version: {tagline}");
        assert!(
            !tagline.contains("Phimint"),
            "brand is redundant with the wordmark: {tagline}"
        );
        let ad = rows[8].text();
        assert!(ad.starts_with("Built on"), "bad ad row: {ad}");
        assert!(ad.contains("phi-agent"), "missing engine: {ad}");
        assert!(
            ad.contains("https://docs.phiagent.dev/"),
            "missing docs url: {ad}"
        );
        let ws = rows[9].text();
        let logs = rows[10].text();
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
        // Label column is 11 display columns (`{label:<9}  `) on all three
        // label rows — ad/Workspace/Logs values start at the same x.
        for (n, row) in [("ad", &rows[8]), ("ws", &rows[9]), ("logs", &rows[10])] {
            let label = &row.spans[0].0;
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(label.as_str()),
                11,
                "{n} label column must be 11 cols: {label:?}"
            );
        }
    }

    /// Style runs of the content rows, pinned via `to_runs()`: the design's
    /// row/col alignment depends on this exact run order (ad row 8's label
    /// width, slogan emphasis, Ad brightness, Dim separators).
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

        // Row 7 slogan: Slogan body + Version tail.
        let (text, runs) = rows[7].to_runs();
        assert_eq!(
            runs.iter().map(|r| r.style).collect::<Vec<_>>(),
            vec![BannerStyle::Slogan, BannerStyle::Version],
            "row 7 style order: {text}"
        );
        assert_eq!(
            run_text(&text, runs[0].start, runs[0].len),
            "Forged with intent. Shipped with care."
        );
        assert_eq!(run_text(&text, runs[1].start, runs[1].len), "   -   v0.1.0");

        // Row 8 ad: Label + Ad + Dim separator + Dim url.
        let (text, runs) = rows[8].to_runs();
        assert_eq!(
            runs.iter().map(|r| r.style).collect::<Vec<_>>(),
            vec![
                BannerStyle::Label,
                BannerStyle::Ad,
                BannerStyle::Dim,
                BannerStyle::Dim
            ],
            "row 8 style order: {text}"
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
