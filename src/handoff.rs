//! Mechanical handoff — the activity ledger (token-budget v4).
//!
//! Session 20260909_e7053736 proved the design rule "don't bet the handoff
//! on the LLM": 9/9 fallback nudges were ignored and the model re-read the
//! same files in all 10 windows. This module extracts the handoff
//! deterministically from the outgoing window's message list — zero model
//! participation:
//!
//! - **Covered actions**: every work tool call, mapped through a
//!   phimint-specific argument table (`read_file` → path+lines,
//!   `execute_command` → command head, ...). Context-management churn
//!   (`history.*`, `notes.read_*`) is excluded — it is not work.
//! - **Last narration**: the model's final non-empty assistant text, so the
//!   fresh window knows where the train stopped.
//!
//! The compactor injects the ledger into the fresh window via the existing
//! thread-hint slot and mirrors it to `notes/activity_ledger.md`, so even
//! at the smallest viable budget `notes.list_files` is never empty.

use phi_agent::ChatMessage;

/// Keep at most this many distinct actions (the most recent ones).
const MAX_ENTRIES: usize = 30;
/// Per-entry char cap (paths/commands can be long; CJK-safe truncation).
const MAX_ENTRY_CHARS: usize = 80;
/// Last-narration char cap.
const MAX_NARRATION_CHARS: usize = 200;
/// Whole-ledger char cap (injected into every fresh window — must stay
/// small against a minimum viable budget of ~4k work-room tokens).
const MAX_LEDGER_CHARS: usize = 1200;

/// Extract the activity ledger from an outgoing window's messages.
///
/// Returns `None` when the window contains no work worth handing off (no
/// tool calls and no assistant narration — e.g. a window that died to its
/// seed).
pub fn extract_ledger(messages: &[ChatMessage]) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut narration: Option<String> = None;

    for msg in messages {
        if let ChatMessage::Assistant {
            content,
            tool_calls,
            ..
        } = msg
        {
            if let Some(text) = content {
                let flat = one_line(text);
                if !flat.is_empty() {
                    // Keep the LAST narration — where the train stopped.
                    narration = Some(truncate_chars(&flat, MAX_NARRATION_CHARS));
                }
            }
            if let Some(calls) = tool_calls {
                for call in calls {
                    if let Some(entry) = ledger_entry(&call.name, &call.arguments) {
                        if !entries.contains(&entry) {
                            entries.push(entry);
                        }
                    }
                }
            }
        }
    }

    if entries.is_empty() && narration.is_none() {
        return None;
    }

    let mut out = String::from("<activity_ledger>\n");
    out.push_str("Covered in the previous window (do NOT redo this work):\n");
    let skipped = entries.len().saturating_sub(MAX_ENTRIES);
    if skipped > 0 {
        out.push_str(&format!("- (+{skipped} earlier actions omitted)\n"));
    }
    for entry in &entries[skipped..] {
        out.push_str("- ");
        out.push_str(entry);
        out.push('\n');
    }
    if let Some(text) = narration {
        out.push_str("Where you left off: ");
        out.push_str(&text);
        out.push('\n');
    }
    out.push_str(
        "Continue from here; re-read a file only for details this ledger lacks.\n",
    );
    out.push_str("</activity_ledger>");

    // Pathological growth guard (a 1200-char ledger is ~400 tokens).
    if out.chars().count() > MAX_LEDGER_CHARS {
        let head: String = out.chars().take(MAX_LEDGER_CHARS - 3).collect();
        return Some(format!("{head}..."));
    }
    Some(out)
}

/// Map one tool call to a ledger line. `None` = not worth recording
/// (context-management churn) or nothing extractable.
fn ledger_entry(tool: &str, args_json: &str) -> Option<String> {
    let line = match tool {
        // Work tools with a known argument shape.
        "read_file" => {
            let args = parse(args_json)?;
            str_field(&args, "path").map(|p| format!("read {p}{}", line_range(&args)))?
        }
        "write_file" => {
            let args = parse(args_json)?;
            str_field(&args, "path").map(|p| format!("wrote {p}"))?
        }
        "edit_file" => {
            let args = parse(args_json)?;
            str_field(&args, "path").map(|p| format!("edited {p}"))?
        }
        "execute_command" => {
            let args = parse(args_json)?;
            str_field(&args, "command")
                .map(|c| format!("ran `{}`", truncate_chars(&one_line(&c), 60)))?
        }
        "search_content" => {
            let args = parse(args_json)?;
            let pattern = str_field(&args, "pattern")?;
            match str_field(&args, "path") {
                Some(p) => format!("searched \"{pattern}\" in {p}"),
                None => format!("searched \"{pattern}\""),
            }
        }
        "repo_map" => {
            let args = parse(args_json)?;
            match str_field(&args, "path") {
                Some(p) => format!("mapped structure of {p}"),
                None => "mapped repo structure".to_string(),
            }
        }
        "spawn_agent" => {
            let args = parse(args_json)?;
            str_field(&args, "task_name")
                .or_else(|| str_field(&args, "name"))
                .map(|n| format!("spawned sub-agent {n}"))?
        }
        "notes.write_file" | "notes.append_to_file" => {
            let args = parse(args_json)?;
            str_field(&args, "path").map(|p| format!("note {p}"))?
        }
        // Private context-management churn — not work.
        "history.list_windows" | "history.list_items" | "history.read_item"
        | "history.search_contents"
        | "notes.list_files" | "notes.read_file" | "notes.search_contents" => {
            return None;
        }
        // Unknown work tool: record the name plus its first string argument
        // (good enough to identify the call; keeps the ledger future-proof).
        _ => {
            let args = parse(args_json)?;
            let first = args.as_object().and_then(|o| {
                o.values().find_map(|v| v.as_str())
            });
            match first {
                Some(v) => format!("{tool} {}", truncate_chars(&one_line(v), 40)),
                None => tool.to_string(),
            }
        }
    };
    Some(truncate_chars(&line, MAX_ENTRY_CHARS))
}

fn parse(args_json: &str) -> Option<serde_json::Value> {
    serde_json::from_str(args_json).ok()
}

fn str_field(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// `(L10-60)` suffix when the read was paginated.
fn line_range(args: &serde_json::Value) -> String {
    let offset = args.get("offset").and_then(|v| v.as_u64());
    let limit = args.get("limit").and_then(|v| v.as_u64());
    match (offset, limit) {
        (Some(o), Some(l)) => format!("(L{}-{})", o, o + l),
        (Some(o), None) => format!("(L{o}-)"),
        _ => String::new(),
    }
}

/// Collapse whitespace to single spaces (CJK-safe: char-based).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Char-based truncation with an ASCII ellipsis (CJK fonts render `…`
/// double-width — never use it).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::llm_trait::ToolCallMessage;

    fn assistant(content: &str, calls: Vec<ToolCallMessage>) -> ChatMessage {
        ChatMessage::Assistant {
            content: if content.is_empty() {
                None
            } else {
                Some(content.to_string())
            },
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
        }
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCallMessage {
        ToolCallMessage {
            id: "t1".into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn extracts_work_actions_in_order() {
        let msgs = vec![
            assistant("我先看结构。", vec![
                call("repo_map", serde_json::json!({})),
                call("read_file", serde_json::json!({"path": "README.md"})),
                call("read_file", serde_json::json!({"path": "src/main.rs", "offset": 10, "limit": 50})),
                call("execute_command", serde_json::json!({"command": "cargo test --lib"})),
                call("edit_file", serde_json::json!({"path": "src/lib.rs"})),
            ]),
        ];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("<activity_ledger>"));
        assert!(ledger.contains("- mapped repo structure"));
        assert!(ledger.contains("- read README.md"));
        assert!(ledger.contains("- read src/main.rs(L10-60)"));
        assert!(ledger.contains("- ran `cargo test --lib`"));
        assert!(ledger.contains("- edited src/lib.rs"));
        assert!(ledger.contains("Where you left off: 我先看结构。"));
    }

    #[test]
    fn excludes_context_management_churn() {
        let msgs = vec![assistant("", vec![
            call("history.list_windows", serde_json::json!({})),
            call("notes.list_files", serde_json::json!({})),
            call("notes.read_file", serde_json::json!({"path": "thread_hint.md"})),
        ])];
        assert!(extract_ledger(&msgs).is_none());
    }

    #[test]
    fn notes_writes_are_recorded() {
        let msgs = vec![assistant("", vec![
            call("notes.write_file", serde_json::json!({"path": "thread_hint.md", "content": "..."})),
        ])];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("- note thread_hint.md"));
    }

    #[test]
    fn dedups_repeated_reads() {
        let msgs = vec![assistant("", vec![
            call("read_file", serde_json::json!({"path": "README.md"})),
            call("read_file", serde_json::json!({"path": "README.md"})),
            call("read_file", serde_json::json!({"path": "src/lib.rs"})),
        ])];
        let ledger = extract_ledger(&msgs).unwrap();
        assert_eq!(ledger.matches("- read README.md").count(), 1);
        assert!(ledger.contains("- read src/lib.rs"));
    }

    #[test]
    fn keeps_most_recent_entries_when_overflowing() {
        let mut calls = Vec::new();
        for i in 0..40 {
            calls.push(call(
                "read_file",
                serde_json::json!({"path": format!("file_{i:02}.rs")}),
            ));
        }
        let msgs = vec![assistant("", calls)];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("earlier actions omitted"));
        // The tail (most recent) survives; the head does not.
        assert!(!ledger.contains("file_00.rs"));
        assert!(ledger.contains("file_39.rs"));
    }

    #[test]
    fn long_command_and_narration_are_truncated() {
        let long_cmd = "x".repeat(300);
        let long_text = "思".repeat(300);
        let msgs = vec![
            assistant(&long_text, vec![
                call("execute_command", serde_json::json!({"command": long_cmd})),
            ]),
        ];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("..."));
        assert!(ledger.chars().count() < 1000);
    }

    #[test]
    fn unknown_tool_falls_back_to_name_and_first_arg() {
        let msgs = vec![assistant("", vec![
            call("future_tool", serde_json::json!({"target": "something", "n": 3})),
        ])];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("- future_tool something"));
    }

    #[test]
    fn spawn_agent_uses_task_name() {
        let msgs = vec![assistant("", vec![
            call(
                "spawn_agent",
                serde_json::json!({"task_name": "analyzer", "task": "read the code"}),
            ),
        ])];
        let ledger = extract_ledger(&msgs).unwrap();
        assert!(ledger.contains("- spawned sub-agent analyzer"));
    }

    #[test]
    fn empty_window_yields_none() {
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            assistant("", vec![]),
        ];
        assert!(extract_ledger(&msgs).is_none());
    }
}
