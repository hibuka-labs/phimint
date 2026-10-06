//! Color theme: the product's semantic color slots, two static palettes.
//!
//! Three kinds of color land on screen, and only the last two need a theme:
//!
//! 1. **ANSI semantic hues** (red / green / cyan / yellow) — the terminal's own
//!    palette paints them, so they already mean the same thing on light and
//!    dark backgrounds. Use sites keep `Color::Red` & co. directly.
//! 2. **ANSI grays** — their *role* flips with the background: the gray that
//!    reads "quiet hint" on dark reads "dead" on light. Two semantic slots,
//!    [`muted`] and [`faint`], map to different grays per scheme.
//! 3. **Absolute truecolor** (thought, user, selection) — one RGB value cannot
//!    serve both backgrounds, so both sets live here and the scheme picks.
//!
//! Two static palettes, no dynamic adaptation: `--color-scheme dark|light|auto`
//! resolves once at startup (`banner::resolve_scheme`, keyed off `$COLORFGBG`)
//! and every slot is a pure function of that choice. The exact values are
//! aesthetic calls — when light reads wrong, tune them here and only here.

use ratatui::style::Color;

use crate::banner::ColorScheme;

/// Readable but secondary text: tool arguments, result bodies, popup
/// descriptions. Content — just not where the eye lands first.
pub fn muted(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::Gray,
        ColorScheme::Light => Color::Rgb(0x55, 0x55, 0x55),
    }
}

/// Chrome: borders, timestamps, markers, hint tails. At notice-level on a dark
/// screen; on light it must step *darker*, not lighter, to stay visible.
pub fn faint(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::DarkGray,
        ColorScheme::Light => Color::Rgb(0x8a, 0x8a, 0x8a),
    }
}

/// Process text (thinking). A truecolor mid-grey on dark, deliberately between
/// ANSI Gray and DarkGray: DarkGray sits at notice-level on most dark themes
/// (invisible), Gray reads as content. Thought is process — dimmer than
/// results, still legible. On light the same role wants a mid-dark gray.
pub fn thought(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::Rgb(0x9e, 0x9e, 0x9e),
        ColorScheme::Light => Color::Rgb(0x6b, 0x6b, 0x6b),
    }
}

/// Your own input in the transcript. ANSI Blue is navy on dark themes and
/// near-black on light ones, so both sets are truecolor brights/deeps.
pub fn user(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::Rgb(0x5a, 0xa7, 0xf7),
        ColorScheme::Light => Color::Rgb(0x1a, 0x6f, 0xd4),
    }
}

/// Foregrounds that must read on any background — popup row names on light.
pub fn strong(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::White,
        ColorScheme::Light => Color::Rgb(0x1a, 0x1a, 0x1a),
    }
}

/// Selection highlight background (transcript rows, task panel, menus).
pub fn selection_bg(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::DarkGray,
        ColorScheme::Light => Color::Rgb(0xc8, 0xc8, 0xc8),
    }
}

/// Selection highlight foreground, where a row has no opinion of its own. A
/// `fg` forced over row spans would flatten two-tone rows (the `/` popup's
/// name/description split), so most call sites apply [`selection_bg`] only.
pub fn selection_fg(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::White,
        ColorScheme::Light => Color::Rgb(0x1a, 0x1a, 0x1a),
    }
}

/// A dim foreground stepped up (dark) / down (light) to survive sitting on
/// [`selection_bg`] — a [`faint`] row that would vanish into the highlight.
pub fn lifted(scheme: ColorScheme) -> Color {
    match scheme {
        ColorScheme::Dark => Color::Gray,
        ColorScheme::Light => Color::Rgb(0x3a, 0x3a, 0x3a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = fn(ColorScheme) -> Color;

    const SLOTS: [(&str, Slot); 8] = [
        ("muted", muted),
        ("faint", faint),
        ("thought", thought),
        ("user", user),
        ("strong", strong),
        ("selection_bg", selection_bg),
        ("selection_fg", selection_fg),
        ("lifted", lifted),
    ];

    #[test]
    fn every_slot_differs_between_schemes() {
        // A light terminal wearing the dark palette is the failure this module
        // exists to prevent: gray-on-gray, white-on-white chrome.
        for (name, slot) in SLOTS {
            assert_ne!(
                slot(ColorScheme::Dark),
                slot(ColorScheme::Light),
                "slot {name} is identical in Dark and Light"
            );
        }
    }

    #[test]
    fn gray_slots_stay_on_the_gray_axis() {
        // Hue is the terminal's job; this module owns grays and the four
        // absolute tones only. A stray red here would paint "error" where
        // "hint" is meant.
        for scheme in [ColorScheme::Dark, ColorScheme::Light] {
            for (name, c) in [
                ("muted", muted(scheme)),
                ("faint", faint(scheme)),
                ("thought", thought(scheme)),
                ("lifted", lifted(scheme)),
            ] {
                match c {
                    Color::Gray | Color::DarkGray => {}
                    Color::Rgb(r, g, b) => {
                        assert_eq!(r, g, "{name} is not gray: {c:?}");
                        assert_eq!(g, b, "{name} is not gray: {c:?}");
                    }
                    other => panic!("{name} left the gray axis: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn lifted_stays_ahead_of_faint_on_the_same_axis() {
        // The selection-lift contract: `lifted` is what a `faint` row becomes
        // on the highlight, so it must not collapse into `faint` itself. On
        // Dark both are palette indices with no RGB of their own, so only the
        // Light pair is orderable here.
        assert_ne!(faint(ColorScheme::Light), lifted(ColorScheme::Light));
    }
}
