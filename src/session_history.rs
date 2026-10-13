//! Session history restoration — the display/recovery side of resume.
//!
//! `messages.jsonl` and the history archive have different jobs:
//!
//! - `messages.jsonl` is the **live window** — what the model resumes with.
//!   It is intentionally bounded: a rotation archives the outgoing window
//!   and starts a fresh one.
//! - `~/.phimint/history/<session>/windows/NNN.jsonl` is the **past** — the
//!   archived windows, reachable through the `history.*` tools.
//!
//! Resume has to serve both, or a rotated session opens on its recent tail
//! with no visible past (and, when a process dies mid-turn before the
//! turn-end persist, with no live window either). This module bridges the
//! two stores; the message schema lives in `phi-kernel-tools`, the
//! `messages.jsonl` format in `phi-agent`, and phimint owns the wiring.

use std::path::Path;

use phi_agent::ChatMessage;
use phi_kernel_tools::context_rotation::HistoryStore;

/// Every archived window for a session, oldest first — for transcript replay.
///
/// Display only. The model still resumes from the live window and recovers
/// the rest through the `history.*` tools; replaying the archive into the
/// model would defeat the whole point of rotation.
pub fn load_archived_messages(base_dir: &Path, session_id: &str) -> Vec<ChatMessage> {
    HistoryStore::new(base_dir, session_id).load_all_messages()
}

/// Drop the leading `live` messages the archived replay already showed.
///
/// The same message can legitimately sit in both slices:
///
/// - [`restore_messages_jsonl`] copies an archived window into
///   `messages.jsonl`, so that whole window is archived *and* live;
/// - a rotation seeds the fresh window with a trail of real user messages
///   taken from the outgoing one.
///
/// Rendering both shows them twice. Only a leading run is dropped — the
/// first message that is not already in the archive is where genuinely new
/// content starts, and everything from there is shown. Matched on
/// role + content; messages without one (empty Assistant text, System,
/// Custom) are never treated as repeats.
pub fn drop_already_shown<'a>(
    archived: &[ChatMessage],
    live: &'a [ChatMessage],
) -> &'a [ChatMessage] {
    let seen: std::collections::HashSet<_> = archived.iter().filter_map(replay_key).collect();
    let mut start = 0;
    for msg in live {
        match replay_key(msg) {
            Some(key) if seen.contains(&key) => start += 1,
            _ => break,
        }
    }
    &live[start..]
}

/// Identity of a message for replay de-duplication.
fn replay_key(msg: &ChatMessage) -> Option<(u8, String, String)> {
    match msg {
        ChatMessage::User { content, .. } => Some((0, String::new(), content.clone())),
        ChatMessage::Assistant {
            content: Some(c), ..
        } if !c.is_empty() => Some((1, String::new(), c.clone())),
        ChatMessage::Tool { name, content, .. } => {
            Some((2, name.clone().unwrap_or_default(), content.clone()))
        }
        _ => None,
    }
}

/// Rebuild a missing `messages.jsonl` from the history archive.
///
/// A context rotation used to delete `messages.jsonl` after archiving, so a
/// session that rotated and then died has real history on disk but nothing
/// to resume from — it was unlisted by the picker, and (after the listing
/// fix) listed but amnesiac. Rebuilding from the **newest** archived window
/// restores the conversation that was live when the process died; the older
/// windows stay in the archive where they belong. Rebuilding from every
/// window would re-inject the already-rotated past into the live window and
/// have it archived a second time on the next reset.
///
/// Idempotent: does nothing when `messages.jsonl` is present and non-blank.
/// Returns whether a restore happened.
pub fn restore_messages_jsonl(base_dir: &Path, session_id: &str, session_dir: &Path) -> bool {
    let path = session_dir.join("messages.jsonl");
    let missing = match std::fs::read_to_string(&path) {
        Ok(s) => s.lines().all(|l| l.trim().is_empty()),
        Err(_) => true,
    };
    if !missing {
        return false;
    }

    let store = HistoryStore::new(base_dir, session_id);
    let mut windows: Vec<usize> = store
        .list_windows()
        .iter()
        .filter_map(|id| id.parse::<usize>().ok())
        .collect();
    windows.sort_unstable();
    let Some(newest) = windows.last() else {
        return false;
    };

    let messages = store.load_window_messages(*newest);
    if messages.is_empty() {
        return false;
    }

    match phi_agent::persist_window_messages(session_dir, &messages) {
        Ok(()) => {
            tracing::info!(
                session_id,
                window = newest,
                message_count = messages.len(),
                "restored messages.jsonl from history archive"
            );
            true
        }
        Err(e) => {
            tracing::warn!(session_id, error = %e, "failed to restore messages.jsonl from archive");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_kernel_tools::context_rotation::HistoryStore;
    use tempfile::TempDir;

    #[test]
    fn load_archived_messages_spans_every_window() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "s1");
        store
            .archive_window(1, &[ChatMessage::user("first")])
            .unwrap();
        store
            .archive_window(2, &[ChatMessage::user("second")])
            .unwrap();

        let loaded = load_archived_messages(tmp.path(), "s1");
        assert_eq!(loaded.len(), 2);
        assert!(matches!(&loaded[0], ChatMessage::User { content, .. } if content == "first"));
        assert!(matches!(&loaded[1], ChatMessage::User { content, .. } if content == "second"));
    }

    /// The damage this exists for: `messages.jsonl` deleted by a rotation,
    /// process died before the turn-end persist. The conversation is in the
    /// archive and nowhere else.
    #[test]
    fn restore_rebuilds_missing_messages_jsonl_from_newest_window() {
        let tmp = TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions").join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();

        let store = HistoryStore::new(tmp.path(), "s1");
        store
            .archive_window(1, &[ChatMessage::user("old window")])
            .unwrap();
        store
            .archive_window(2, &[ChatMessage::user("live window")])
            .unwrap();

        assert!(
            restore_messages_jsonl(tmp.path(), "s1", &session_dir),
            "must restore"
        );

        let stored = std::fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        assert!(
            stored.contains("live window"),
            "newest window is the live one: {stored}"
        );
        assert!(
            !stored.contains("old window"),
            "older windows stay in the archive: {stored}"
        );
    }

    #[test]
    fn restore_is_idempotent_when_messages_jsonl_present() {
        let tmp = TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions").join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("messages.jsonl"),
            "{\"User\":{\"content\":\"keep me\"}}\n",
        )
        .unwrap();

        let store = HistoryStore::new(tmp.path(), "s1");
        store
            .archive_window(1, &[ChatMessage::user("from archive")])
            .unwrap();

        assert!(
            !restore_messages_jsonl(tmp.path(), "s1", &session_dir),
            "must not touch a live file"
        );
        let stored = std::fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        assert!(
            stored.contains("keep me"),
            "existing file must be untouched: {stored}"
        );
    }

    #[test]
    fn restore_noop_without_any_history() {
        let tmp = TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions").join("s1");
        std::fs::create_dir_all(&session_dir).unwrap();
        assert!(!restore_messages_jsonl(tmp.path(), "s1", &session_dir));
        assert!(!session_dir.join("messages.jsonl").exists());
    }

    // ── drop_already_shown: a restored window must not render twice ──

    fn tool(name: &str, content: &str) -> ChatMessage {
        ChatMessage::Tool {
            tool_call_id: "tc".into(),
            name: Some(name.into()),
            content: content.into(),
        }
    }

    #[test]
    fn drop_already_shown_removes_a_restored_window_from_the_live_slice() {
        // What a heal leaves behind: the archive has the full window
        // (tool outputs included), the live copy lost the orphaned tools.
        let archived = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("a"),
            tool("shell", "output"),
        ];
        let live = vec![ChatMessage::user("q"), ChatMessage::assistant("a")];
        assert!(
            drop_already_shown(&archived, &live).is_empty(),
            "restored copy must not render twice"
        );
    }

    #[test]
    fn drop_already_shown_keeps_genuinely_new_tail() {
        let archived = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("a"),
            tool("shell", "output"),
        ];
        let live = vec![
            ChatMessage::user("q"),
            ChatMessage::assistant("a"),
            ChatMessage::user("and then?"),
            ChatMessage::assistant("more"),
        ];
        let fresh = drop_already_shown(&archived, &live);
        assert_eq!(fresh.len(), 2);
        assert!(matches!(&fresh[0], ChatMessage::User { content, .. } if content == "and then?"));
    }

    #[test]
    fn drop_already_shown_leaves_a_live_window_with_no_overlap_alone() {
        let archived = vec![ChatMessage::user("old question")];
        let live = vec![ChatMessage::user("new question")];
        assert_eq!(drop_already_shown(&archived, &live).len(), 1);
    }

    #[test]
    fn drop_already_shown_without_archive_shows_everything() {
        let live = vec![ChatMessage::user("q"), ChatMessage::assistant("a")];
        assert_eq!(drop_already_shown(&[], &live).len(), 2);
    }

    /// A rotation seeds the fresh window with a trail of real user messages
    /// from the outgoing one — those are repeats too.
    #[test]
    fn drop_already_shown_covers_the_rotation_user_trail() {
        let archived = vec![
            ChatMessage::user("keep this question"),
            ChatMessage::assistant("outgoing answer"),
        ];
        let live = vec![
            ChatMessage::user("keep this question"),
            ChatMessage::user("reconstruct state."),
        ];
        let fresh = drop_already_shown(&archived, &live);
        assert_eq!(fresh.len(), 1);
        assert!(
            matches!(&fresh[0], ChatMessage::User { content, .. } if content == "reconstruct state.")
        );
    }
}
