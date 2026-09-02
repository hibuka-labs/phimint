//! phimint-specific prompt fragments.
//!
//! Replaces the monolithic `SYSTEM_PROMPT` constant with composable fragments
//! that auto-sync tool descriptions with registered tools.

use phi_agent::{FragmentContext, PromptFragment};

// ── Personality (priority: 10) ─────────────────────────────────────────────

/// Concise, direct, friendly personality definition.
#[derive(Clone)]
pub struct PhimintPersonalityFragment;

impl PromptFragment for PhimintPersonalityFragment {
    fn name(&self) -> &str {
        "phimint_personality"
    }

    fn priority(&self) -> i32 {
        10
    }

    fn render(&self, _ctx: &FragmentContext) -> Option<String> {
        Some(PERSONALITY.to_string())
    }
}

const PERSONALITY: &str = r#"## Personality

Your default personality and tone is concise, direct, and friendly. You communicate efficiently, always keeping the user clearly informed about ongoing actions without unnecessary detail. You always prioritize actionable guidance, clearly stating assumptions, environment prerequisites, and next steps. Unless explicitly asked, you avoid excessively verbose explanations about your work."#;

// ── Safety + Progress + Final (priority: 50) ───────────────────────────────

/// Safety rules, progress updates, and final message format.
#[derive(Clone)]
pub struct PhimintSafetyFragment;

impl PromptFragment for PhimintSafetyFragment {
    fn name(&self) -> &str {
        "phimint_safety"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn render(&self, _ctx: &FragmentContext) -> Option<String> {
        Some(SAFETY.to_string())
    }
}

const SAFETY: &str = r#"### Safety
- For destructive operations (rm -rf, git push --force, dropping tables), confirm with the user first.
- When a tool call is denied, adjust your approach — don't retry the same call verbatim.
- Do not attempt to fix unrelated bugs or broken tests. It is not your responsibility to fix them. (You may mention them to the user in your final message though.)

### Progress updates
For longer tasks requiring many tool calls, provide concise progress updates (1-2 sentences) recapping progress so far in plain language.

### Final message
Your final message should read naturally, like an update from a concise teammate. Be concise and factual — no filler or conversational commentary. Use present tense and active voice. When referencing files, include the path so the user can click to open."#;

// ── Workflow (priority: 60) ────────────────────────────────────────────────

/// Orient → Plan → Edit → Validate → Keep going workflow.
#[derive(Clone)]
pub struct PhimintWorkflowFragment;

impl PromptFragment for PhimintWorkflowFragment {
    fn name(&self) -> &str {
        "phimint_workflow"
    }

    fn priority(&self) -> i32 {
        60
    }

    fn render(&self, _ctx: &FragmentContext) -> Option<String> {
        Some(WORKFLOW.to_string())
    }
}

const WORKFLOW: &str = r#"## How to work

### Orient first
Use `repo_map` for layout, `search_content` to locate symbols, then `read_file` on the relevant files. Don't read every source file — scope to the task. If the user's request is ambiguous, investigate with `search_content` and `repo_map` before asking. Only ask when genuinely unclear.

### Plan when needed
For complex tasks (3+ steps), call `update_plan` first and update statuses as you go. Keep steps concise (5–7 words each). Don't use plans for simple or single-step queries.

### Edit precisely
- Use `edit_file` for targeted changes. `old_text` must match exactly and appear exactly once.
- Use `write_file` for new files or full rewrites.
- Use `append_to_file` for adding to the end of existing files.
- Do not waste tokens by re-reading files after editing them — the tool call will fail if it didn't work.
- Match the surrounding code's style, naming, and comment density. Don't introduce a different idiom.

### Validate your work
After editing, call `diagnostics` for a quick check. Before reporting done, ensure code compiles: run `cargo check` (or equivalent). For big changes, also run tests. Both must pass.

When testing, start as specific as possible to the code you changed, then make your way to broader tests as you build confidence.

When compilation fails, read the error, fix, and re-check. If a fix doesn't work after 3 attempts, explain what you tried and ask the user.

### Keep going
You are a coding agent. Please keep going until the query is completely resolved, before ending your turn and yielding back to the user. Only terminate your turn when you are sure that the problem is solved. Do NOT guess or make up an answer.

### Be efficient
- Prefer dedicated tools over shell: `read_file` instead of `cat`, `search_content` instead of `grep`. Shell bypasses output limits and tool controls.
- For bug fixes or logic changes, reproduce the issue first (failing test, small script, or direct command) before editing. Simple fixes (typos, configs, one-liners) can skip this.
- Do not re-derive facts already established in the conversation. Once the user confirms a decision, move on."#;

// ── Tools (priority: 70, dynamic) ─────────────────────────────────────────

/// Dynamically generate tool descriptions from registered tools.
///
/// Replaces the hardcoded "## Tools available" section. Tool descriptions
/// auto-sync with the actual registered tools.
#[derive(Clone)]
pub struct PhimintToolsFragment;

impl PromptFragment for PhimintToolsFragment {
    fn name(&self) -> &str {
        "phimint_tools"
    }

    fn priority(&self) -> i32 {
        70
    }

    fn render(&self, ctx: &FragmentContext) -> Option<String> {
        if ctx.tool_definitions.is_empty() {
            return None;
        }
        let mut lines = vec!["## Tools available".to_string()];
        lines.push(String::new()); // blank line

        for def in ctx.tool_definitions {
            let name = def
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            let desc = def
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or("");
            if desc.is_empty() {
                lines.push(format!("- `{}`", name));
            } else {
                // Truncate long descriptions for the prompt
                let short_desc = if desc.len() > 120 {
                    format!("{}…", &desc[..120])
                } else {
                    desc.to_string()
                };
                lines.push(format!("- `{}` — {}", name, short_desc));
            }
        }
        Some(lines.join("\n"))
    }
}

// ── Multi-Agent (priority: 80, conditional) ───────────────────────────────

/// Multi-agent instructions — only rendered when sub-agent tools are available.
#[derive(Clone)]
pub struct PhimintMultiAgentFragment;

impl PromptFragment for PhimintMultiAgentFragment {
    fn name(&self) -> &str {
        "phimint_multi_agent"
    }

    fn priority(&self) -> i32 {
        80
    }

    fn render(&self, ctx: &FragmentContext) -> Option<String> {
        // Only inject if spawn_agent tool is available
        let has_spawn_agent = ctx.tool_definitions.iter().any(|def| {
            def.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                == Some("spawn_agent")
        });
        if !has_spawn_agent {
            return None;
        }
        Some(MULTI_AGENT.to_string())
    }
}

const MULTI_AGENT: &str = r#"## Multi-agent (tasks with clearly independent parts)
Sub-agents are READ-ONLY (read/search/report, no writes or mutating commands). You perform all edits.
- Prefer multiple sub-agents to parallelize your work. Time is a constraint so parallelism resolves the task faster.
- If sub-agents are running, **wait for them before yielding**, unless the user asks an explicit question.
  - If the user asks a question, answer it first, then continue coordinating sub-agents.
- When you ask a sub-agent to do the work for you, your only role becomes to coordinate them. Do not perform the actual work while they are working.
- When you have a plan with multiple steps, process them in parallel by spawning one agent per step when possible.

When done, briefly report what you changed."#;

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::compose_fragments;

    #[test]
    fn test_all_fragments_compose() {
        let tool_def = serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from disk",
                "parameters": {}
            }
        });
        let spawn_agent_def = serde_json::json!({
            "type": "function",
            "function": {
                "name": "spawn_agent",
                "description": "Spawn a sub-agent",
                "parameters": {}
            }
        });
        let fragments: Vec<Box<dyn PromptFragment>> = vec![
            Box::new(PhimintPersonalityFragment),
            Box::new(PhimintSafetyFragment),
            Box::new(PhimintWorkflowFragment),
            Box::new(PhimintToolsFragment),
            Box::new(PhimintMultiAgentFragment),
        ];
        let ctx = FragmentContext {
            tool_definitions: &[tool_def, spawn_agent_def],
            session_id: "test",
        };
        let result = compose_fragments(&fragments, &ctx);

        // Verify ordering: Personality (10) < Safety (50) < Workflow (60) < Tools (70) < MultiAgent (80)
        let personality_pos = result.find("Personality").unwrap();
        let safety_pos = result.find("Safety").unwrap();
        let workflow_pos = result.find("Orient first").unwrap();
        let tools_pos = result.find("Tools available").unwrap();
        let multi_pos = result.find("Multi-agent").unwrap();

        assert!(personality_pos < safety_pos);
        assert!(safety_pos < workflow_pos);
        assert!(workflow_pos < tools_pos);
        assert!(tools_pos < multi_pos);
    }

    #[test]
    fn test_multi_agent_fragment_skips_without_spawn_agent() {
        let tool_def = serde_json::json!({
            "type": "function",
            "function": { "name": "read_file", "description": "Read", "parameters": {} }
        });
        let frag = PhimintMultiAgentFragment;
        let ctx = FragmentContext {
            tool_definitions: &[tool_def],
            session_id: "test",
        };
        assert!(frag.render(&ctx).is_none());
    }

    #[test]
    fn test_tools_fragment_dynamic() {
        let tool_def = serde_json::json!({
            "type": "function",
            "function": {
                "name": "search_content",
                "description": "Search file contents with ripgrep",
                "parameters": {}
            }
        });
        let frag = PhimintToolsFragment;
        let ctx = FragmentContext {
            tool_definitions: &[tool_def],
            session_id: "test",
        };
        let output = frag.render(&ctx).unwrap();
        assert!(output.contains("search_content"));
        assert!(output.contains("ripgrep"));
    }

    #[test]
    fn test_fragment_names() {
        assert_eq!(PhimintPersonalityFragment.name(), "phimint_personality");
        assert_eq!(PhimintSafetyFragment.name(), "phimint_safety");
        assert_eq!(PhimintWorkflowFragment.name(), "phimint_workflow");
        assert_eq!(PhimintToolsFragment.name(), "phimint_tools");
        assert_eq!(PhimintMultiAgentFragment.name(), "phimint_multi_agent");
    }
}
