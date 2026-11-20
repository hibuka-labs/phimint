//! Which tools phimint mounts, and which it only names.
//!
//! The init face is fixed (2026-11-20 用户拍板): the mounted set is pinned by
//! `src/agent/tests/tool_face.rs` and grows only on an explicit decision —
//! update that count on purpose. Everything in [`DEFERRED_TOOLS`] is
//! named-only at init and loads on demand via `activate_tool`.
//! Reference point: Claude Code mounts 13; fewer is better.
//!
//! Two lists feed one table: the roster in the system prompt and the
//! candidates `activate_tool` can open. Two lists written separately drift —
//! the roster promises something the opener cannot find. Build both from
//! [`DEFERRED_TOOLS`] and they cannot.
//!
//! The shape is Claude Code's (13 mounted + 14 named-only + a `ToolSearch`
//! that loads schemas on demand). Ours differs only in which tools are
//! deferred: phimint carries browser, canvas, device and multi-agent surfaces
//! that a coding session rarely reaches for.

/// `(name, capability)` for tools named in the roster but not mounted.
///
/// Grouped by the capability the model would actually want — it decides *what
/// it needs*, then opens that group.
pub const DEFERRED_TOOLS: &[(&str, &str)] = &[
    ("browser_navigate", "open and drive a real page"),
    ("browser_click", "open and drive a real page"),
    ("browser_screenshot", "open and drive a real page"),
    ("browser_snapshot", "open and drive a real page"),
    ("browser_close", "open and drive a real page"),
    ("canvas_open", "render design output a person can look at"),
    ("canvas_push", "render design output a person can look at"),
    ("canvas_close", "render design output a person can look at"),
    ("read_image", "read a picture's contents"),
    ("send_message", "delegate independent work to a sub-agent"),
    ("close_agent", "delegate independent work to a sub-agent"),
    ("list_agents", "delegate independent work to a sub-agent"),
    ("current_model", "ask which model is serving this session"),
    ("add_tool", "mount a script as a callable tool"),
    ("verify", "run the project compile check"),
];

/// The roster block appended to the system prompt — names only, no schemas.
///
/// This is Claude Code's split exactly: the prompt says *what exists*, the tool
/// description says *how to open it*. Keeping the two apart is what lets the
/// prompt stay short.
pub fn roster() -> String {
    use std::collections::BTreeMap;

    let mut by_capability: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, capability) in DEFERRED_TOOLS {
        by_capability.entry(capability).or_default().push(name);
    }

    let mut out = String::from(
        "\nSome tools are available but their schemas are not loaded. They are named \
         below; calling one before loading it fails. Use `activate_tool` with the \
         tool's name to load it — it arrives on your next turn and stays for the \
         rest of the session.\n\n",
    );
    for (capability, names) in by_capability {
        out.push_str(&format!("- {capability}: `{}`\n", names.join("`, `")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Value: protects=one table feeding two readers — the roster the model
    /// reads and the names `activate_tool` accepts must be the same list, or
    /// the model opens a tool that is not there. fails_when=a name is in the
    /// table but missing from the rendered roster. why_new=the table is new.
    /// seam=table -> prompt; no_claim=that these are the right tools to defer.
    #[test]
    fn the_roster_names_every_deferred_tool() {
        let text = roster();
        for (name, _) in DEFERRED_TOOLS {
            assert!(
                text.contains(&format!("`{name}`")),
                "the roster must name `{name}` or the model cannot open it"
            );
        }
    }

    /// Value: protects=the model's ability to act on the roster — names with
    /// no way in are names it will route around. fails_when=the roster stops
    /// naming the opener or what opening does. why_new=the roster is new.
    #[test]
    fn the_roster_says_how_to_open_and_that_it_sticks() {
        let text = roster();
        assert!(text.contains("activate_tool"), "name the opener: {text}");
        assert!(
            text.contains("stays for the rest of the session"),
            "say opening is once, or the model re-opens every use: {text}"
        );
        assert!(
            text.contains("calling one before loading it fails"),
            "say what happens if it does not load first, or the failure reads as a bug"
        );
    }
}
