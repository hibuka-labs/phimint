//! The product's chrome-sources scan (the shell carries its own over its own
//! paths; the product's chrome — the wordmark banner — scans its own here).
//!
//! Kept in its own file, OUTSIDE the scanned set: the banned-glyph table and
//! the marker vocabulary below must never be mistaken for rendering code.

/// Glyphs that render double-width in CJK-ambiguous-width terminals: a
/// chrome source that computes columns must never put them on screen.
const CJK_WIDTH_UNSAFE: &[char] = &[
    '⏺', '⏸', '⏳', '✓', '✔', '✗', '✘', '●', '○', '→', '←', '⇒', '⇐', '⟳', '⚠', '❯', '×', '÷', '±',
    '≤', '≥', '≠', '≈', '≡', '…', '·', '—', '–', '•', '√', '§', '∑', '∏', '∫', '∂', '∇', '∞', '①',
    '▸',
];

#[test]
fn chrome_sources_stay_cjk_width_safe() {
    // Scan the chrome-rendering sources (outside comments) for the symbols.
    const FILES: &[&str] = &["src/banner.rs"];
    for file in FILES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
        let mut exempt = false;
        for (i, line) in source.lines().enumerate() {
            // Region markers live in comments (checked on the raw line): a
            // marked block recolours glyphs the product already wrote and
            // computes no columns, so it can't shift any layout.
            if line.contains("cjk-glyph-exempt: begin") {
                assert!(!exempt, "{file}:{} nested cjk-glyph-exempt begin", i + 1);
                exempt = true;
                continue;
            }
            if line.contains("cjk-glyph-exempt: end") {
                assert!(
                    exempt,
                    "{file}:{} cjk-glyph-exempt end without begin",
                    i + 1
                );
                exempt = false;
                continue;
            }
            if exempt {
                continue;
            }
            let code = line.split_once("//").map_or(line, |(code, _)| code);
            let offenders: Vec<char> = code
                .chars()
                .filter(|c| CJK_WIDTH_UNSAFE.contains(c))
                .collect();
            assert!(
                offenders.is_empty(),
                "{file}:{} renders CJK-double-width glyphs {offenders:?}: {line}",
                i + 1
            );
        }
        assert!(!exempt, "{file}: unclosed cjk-glyph-exempt region");
    }
}
