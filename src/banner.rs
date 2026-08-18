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

/// Semantic role of one text run in the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerStyle {
    /// Uncolored connector (e.g. the single spaces between wordmark letters).
    Default,
    /// First tone of the wordmark gradient.
    LogoA,
    /// Second tone of the wordmark gradient.
    LogoB,
    /// The "Phimint" brand name on the tagline (bold).
    Brand,
    /// Tagline text after the brand name.
    Tagline,
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

/// A styled byte-range within an output line's plain text (TUI rendering).
/// `start`/`len` align with the concatenated run strings in `BannerRow::text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanSpec {
    pub start: usize,
    pub len: usize,
    pub style: BannerStyle,
}

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
                spans.push(SpanSpec { start: off, len: s.len(), style: *style });
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
    ["██████╗ ", "██╔══██╗", "██████╔╝", "██╔═══╝ ", "██║     ", "╚═╝     "],
    // H
    ["██╗  ██╗", "██║  ██║", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"],
    // I
    ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
    // M
    ["███╗   ███╗", "████╗ ████║", "██╔████╔██║", "██║╚██╔╝██║", "██║ ╚═╝ ██║", "╚═╝     ╚═╝"],
    // I
    ["██╗", "██║", "██║", "██║", "██║", "╚═╝"],
    // N
    ["███╗   ██╗", "████╗  ██║", "██╔██╗ ██║", "██║╚██╗██║", "██║ ╚████║", "╚═╝  ╚═══╝"],
    // T
    ["████████╗", "╚══██╔══╝", "   ██║   ", "   ██║   ", "   ██║   ", "   ╚═╝   "],
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

/// The tagline row: brand name (bold) + English positioning + slogan + version.
fn tagline_row(version: &str) -> BannerRow {
    BannerRow {
        spans: vec![
            ("Phimint".to_string(), BannerStyle::Brand),
            (format!(" v{version} · Forged with intent. Shipped with care. · Built on phi-agent"), BannerStyle::Tagline),
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
        for s in [BannerStyle::LogoA, BannerStyle::LogoB, BannerStyle::Brand,
                  BannerStyle::Tagline, BannerStyle::Label,
                  BannerStyle::Value] {
            assert_ne!(s.rgb(ColorScheme::Dark), s.rgb(ColorScheme::Light), "{s:?}");
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
                    assert_eq!(chunk, " ", "connector must be a single space");
                    continue;
                }
                assert!(chunk.chars().all(|c| c == ' ' || "█╔═╝╗║╚".contains(c)), "bad glyph char: {chunk:?}");
            }
        }
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
        assert_eq!(shorten_home_with(Path::new("/Users/eve/proj"), Some(home)), "~/proj");
        assert_eq!(shorten_home_with(Path::new("/Users/eve"), Some(home)), "~");
        assert_eq!(shorten_home_with(Path::new("/tmp/x"), Some(home)), "/tmp/x");
        assert_eq!(shorten_home_with(Path::new("/Users/evelyn/x"), Some(home)), "/Users/evelyn/x");
        assert_eq!(shorten_home_with(Path::new("/p"), None), "/p");
    }

    #[test]
    fn build_emits_nine_rows_with_brand_and_version() {
        let rows = build(Path::new("/Users/eve/w"), Path::new("/Users/eve/.phimint/s/1/session.log"), "0.1.0");
        assert_eq!(rows.len(), 9, "6 art + tagline + 2 info");
        let tagline = rows[6].text();
        assert!(tagline.contains("Phimint"), "missing brand: {tagline}");
        assert!(tagline.contains("Forged with intent"), "missing slogan: {tagline}");
        assert!(tagline.contains("v0.1.0"), "missing version: {tagline}");
        let ws = rows[7].text();
        let logs = rows[8].text();
        let want_ws = shorten_home(Path::new("/Users/eve/w"));
        let want_logs = shorten_home(Path::new("/Users/eve/.phimint/s/1/session.log"));
        assert!(ws.starts_with("Workspace") && ws.ends_with(&want_ws), "bad workspace row: {ws}");
        assert!(logs.starts_with("Logs") && logs.ends_with(&want_logs), "bad logs row: {logs}");
        assert!(rows[7].spans.iter().any(|(t, s)| *s == BannerStyle::Label && t.len() == 11));
    }

    #[test]
    fn resolve_scheme_obeys_flag_then_env_then_osc() {
        assert_eq!(resolve_scheme("dark", Some("7;0"), Some(ColorScheme::Light)), ColorScheme::Dark);
        assert_eq!(resolve_scheme("light", None, None), ColorScheme::Light);
        // $COLORFGBG is "fg;bg" — background 7 means a light terminal.
        assert_eq!(resolve_scheme("auto", Some("7;0"), None), ColorScheme::Dark);
        assert_eq!(resolve_scheme("auto", Some("0;7"), None), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", Some("15;0"), None), ColorScheme::Dark);
        assert_eq!(resolve_scheme("auto", None, Some(ColorScheme::Light)), ColorScheme::Light);
        assert_eq!(resolve_scheme("auto", None, None), ColorScheme::Dark);
        assert_eq!(resolve_scheme("auto", None, Some(ColorScheme::Dark)), ColorScheme::Dark);
    }

    #[test]
    fn to_runs_produces_byte_ranges_skipping_default() {
        use BannerStyle as S;
        let row = BannerRow {
            spans: vec![
                ("██".to_string(), S::LogoA),
                (" ".to_string(), S::Default),
                ("abc".to_string(), S::Brand),
            ],
        };
        let (text, runs) = row.to_runs();
        assert_eq!(text, "██ abc");
        assert_eq!(
            runs,
            vec![
                SpanSpec { start: 0, len: 6, style: S::LogoA },
                SpanSpec { start: 7, len: 3, style: S::Brand },
            ]
        );
    }
}