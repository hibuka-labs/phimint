use super::*;

/// The single construction point for a sub-agent's multi-agent config (shared by build and tests).
///
/// D0 (fully opened after the 2026-09-20 acceptance run, T11): both modes
/// allow `tools: "write"` children. `has_policy` now only picks the
/// **permission mode**, no longer the write switch:
/// policy present → child writes go through the codex-style approval
/// delegation (agent-works resolves this on every spawn: policy with the
/// parent, or `DenyAllToolPolicy` when the parent carries none); no policy →
/// children write with full permission. With the live approval gate
/// (approval.rs) the policy is always installed, so production children are
/// always in delegation mode and follow the *live* approval mode: auto
/// auto-passes their calls, ask prompts, deny rejects. The `has_policy ==
/// false` branch stays as the guard for no-policy callers — never flip the
/// call-site argument to force it, `None` without a parent policy denies
/// every child tool. Default spawns (no tools argument) stay read-only in
/// both modes, pinned by `critical_default_spawn_children_get_no_write_tools`.
///
/// D3.1: `child_read_only` is explicitly off -- the nudge is left to the
/// framework's per-child rule (write sub-agents are no longer fed read-only
/// discipline; read-only ones are nudged via the resolved exclusion set).
///
/// eng-review finding 1: `notes_write_file` / `notes_append_to_file` are
/// mutating tools in business_tools and must be excluded explicitly (a
/// sub-agent must not pollute parent notes). `history.*` is read-only: kept.
///
/// The read pair rides every child kit (2026-11-01: the M3b face that cut
/// `read_file`/`list_files` when the kit carried `code_cell` is gone — a
/// child's menu mirrors the host's, both vehicles up, the prompt's dispatch
/// rule teaching the split). `code_cell` is never excluded on the tool road:
/// the read-only engine policy is the gate, not an exclusion entry.
pub(super) fn child_multi_agent_config(
    has_policy: bool,
    browser_tools: &[String],
    tool_output_cap: usize,
) -> MultiAgentConfig {
    MultiAgentConfig {
        // Sub-agent permission follows the approval mode (codex-style
        // delegation lives in agent-works): with a policy the child's calls
        // escalate to the parent's live gate; without one, full permission.
        child_permission_mode: if has_policy {
            ChildPermissionMode::None
        } else {
            ChildPermissionMode::Full
        },
        // D0 (T11, opened after the 2026-09-20 acceptance run): write
        // children enabled in both modes — ask delegates approvals
        // (verified on-device), auto writes with full permission
        // (consistent with its parent).
        allow_child_write: true,
        // Sub-agents are read-only investigators by default (read/search/report).
        // The hard gate is this exclusion table; write capability means write_tools
        // members are exempted from it (framework rule), so the table only lists tools no sub-agent should get.
        child_excluded_tools: {
            let mut excluded = vec![
                "write_file".to_string(),
                "edit_file".to_string(),
                "execute_command".to_string(),
                "task_output".to_string(),
                "task_cancel".to_string(),
                "notes_write_file".to_string(),
                "notes_append_to_file".to_string(),
            ];
            // `code_cell` used to stay parent-only (arbitrary code can write
            // the disk). Sub-agents now run their cells in a resident session
            // of their own under a read-only policy — the audit seam refuses
            // every write (PEP 578 mediates `open` into ReadFs/WriteFs) — so
            // cells ride the child kit; the read-only engine policy is the
            // gate, not an exclusion entry. The read pair rides alongside
            // them (both vehicles, host and child alike — 2026-11-01).
            //
            // Every browser tool stays with the parent, read-only child or
            // write: the suite drives one Chrome session behind one lock, so a
            // sub-agent that navigates or clicks would move the view out from
            // under the parent's self-check. None of them are in
            // `write_tools`, so the write exemption does not bring them back.
            excluded.extend(browser_tools.iter().cloned());
            excluded
        },
        // Sub-agents get a narrow slice; reasoning depth is capped to stop them running away (the deepseek-v4-pro lesson).
        child_reasoning_effort: Some(ReasoningEffort::Low),
        // Same §6.5 tool-output valve as root (2026-11-01: without this
        // children run valve-less — a child's 29k-char read sailed past the
        // root's 16k cap).
        child_max_tool_output_chars: Some(tool_output_cap),
        // D3.1: the global nudge is off; the per-child rule owns it.
        child_read_only: false,
        // Hang guard (S9.2): a stuck sub-agent is hard-stopped after 10 minutes and its Error is pushed to the parent.
        control: ControlConfig {
            task_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            ..ControlConfig::default()
        },
        ..MultiAgentConfig::default()
    }
}
