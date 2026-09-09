//! `skill` tool (skill-injection D2): model-initiated skill body loader.
//!
//! When the model sees a skill in the system-prompt catalog, it calls this tool
//! with the skill's `name` to load the full instruction body. The resolver does
//! an exact name match (the model has the exact name from the catalog) and
//! returns the resolved body. The slash path (`run.rs:480`) is untouched.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};
use tokio::task;

use crate::skills::SkillResolver;
use crate::telemetry::SkillTelemetry;

/// Output budget for a single skill body call. When the body exceeds this
/// limit, the tool returns a truncated excerpt plus the `SKILL.md` absolute
/// path so the model can call `read_file` to finish (Codex fallback).
const MAX_SKILL_BODY_CHARS: usize = 16_000;

/// Model-facing skill body loader (read-only, no approval needed).
pub struct SkillTool {
    resolver: Arc<SkillResolver>,
    telemetry: Arc<SkillTelemetry>,
}

impl SkillTool {
    pub fn new(resolver: Arc<SkillResolver>, telemetry: Arc<SkillTelemetry>) -> Self {
        Self { resolver, telemetry }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill's full instruction body by name. Returns the skill body, or an error with available skill names if not found."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill's exact name (as listed in the system prompt catalog)."
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments passed to the skill ($ARGUMENTS placeholder in the body)."
                }
            },
            "required": ["name"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Load a skill body by exact name.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let name = match args.get("name").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None => {
                return Ok(vec![Content::text(
                    "Error: missing required field `name` (string).".to_string(),
                )]);
            }
        };
        let raw_args = args
            .get("args")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let resolver = Arc::clone(&self.resolver);
        let telemetry = self.telemetry.clone();
        let max_chars = ctx.max_output_chars.unwrap_or(MAX_SKILL_BODY_CHARS);

        // All work is in-memory; spawn_blocking keeps the async runtime free.
        let result = task::spawn_blocking(move || {
            match resolver.resolve_by_name(&name, &raw_args) {
                Some((body, matched_name)) => {
                    // Record model-triggered skill load for telemetry (M3b).
                    telemetry.record_model(matched_name);
                    tracing::info!(
                        name = %name,
                        matched = %matched_name,
                        body_len = body.len(),
                        "resolved skill via tool"
                    );
                    if body.chars().count() <= max_chars {
                        Ok(body)
                    } else {
                        // Overflow: truncated excerpt + path so the model can
                        // use read_file to finish the read.
                        let truncated: String = body.chars().take(max_chars).collect();
                        let path_hint = resolver
                            .source_path_for(matched_name)
                            .map(|p| format!("\n\n[truncated - full body at: {}]", p.display()))
                            .unwrap_or_default();
                        Ok(format!("{truncated}{path_hint}"))
                    }
                }
                None => {
                    let available = resolver.all_skill_names();
                    let listing: Vec<&str> = available.iter().take(20).copied().collect();
                    let omitted = available.len().saturating_sub(20);
                    let mut msg = format!(
                        "Error: no skill named \"{name}\". Available skills: {listing:?}"
                    );
                    if omitted > 0 {
                        msg.push_str(&format!(" (+{omitted} more)"));
                    }
                    tracing::info!(name = %name, "skill tool: no match");
                    Err(msg)
                }
            }
        })
        .await;

        match result {
            Ok(Ok(body)) => Ok(vec![Content::text(body)]),
            Ok(Err(msg)) => Ok(vec![Content::text(msg)]),
            Err(e) => Ok(vec![Content::text(format!("[Error]: skill tool task failed: {e}"))]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a temporary skill directory with one or more skills.
    ///
    /// Returns `(Arc<SkillResolver>, TempDir)` — the guard must live as long as
    /// the resolver, because `source_path_for` points into the temp directory
    /// and the overflow test relies on that path being readable.
    fn make_resolver(skills: &[(&str, &str, bool)]) -> (Arc<SkillResolver>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body, invocable) in skills {
            let dir = tmp.path().join("skills").join(name);
            fs::create_dir_all(&dir).unwrap();
            let inv = if *invocable { "true" } else { "false" };
            fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test skill\nuser-invocable: {inv}\n---\n\n{body}"),
            )
            .unwrap();
        }
        (Arc::new(SkillResolver::from_dirs(&[tmp.path().join("skills")])), tmp)
    }

    fn ctx() -> ToolContext {
        ToolContext::for_test()
    }

    fn text(out: Vec<Content>) -> String {
        out.into_iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn happy_path_returns_body() {
        let (resolver, _guard) = make_resolver(&[("code-review", "Review this PR carefully.", true)]);
        let tool = SkillTool::new(resolver, std::sync::Arc::new(SkillTelemetry::new()));
        let out = tool.call(&json!({"name": "code-review"}), &ctx()).await.unwrap();
        assert_eq!(text(out), "Review this PR carefully.");
    }

    #[tokio::test]
    async fn happy_path_with_args_substitution() {
        let (resolver, _guard) = make_resolver(&[("greet", "Hello $ARGUMENTS!", true)]);
        let tool = SkillTool::new(resolver, std::sync::Arc::new(SkillTelemetry::new()));
        let out = tool.call(&json!({"name": "greet", "args": "world"}), &ctx()).await.unwrap();
        assert_eq!(text(out), "Hello world!");
    }

    #[tokio::test]
    async fn not_found_returns_available_names() {
        let (resolver, _guard) = make_resolver(&[("alpha", "a", true), ("beta", "b", true)]);
        let tool = SkillTool::new(resolver, std::sync::Arc::new(SkillTelemetry::new()));
        let out = text(tool.call(&json!({"name": "gamma"}), &ctx()).await.unwrap());
        assert!(out.contains("no skill named"), "{out}");
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("beta"), "{out}");
    }

    #[tokio::test]
    async fn non_user_invocable_is_accessible_by_tool() {
        // user_invocable:false only blocks the user slash path, not the model tool.
        let (resolver, _guard) = make_resolver(&[("internal", "internal body", false)]);
        let tool = SkillTool::new(resolver, std::sync::Arc::new(SkillTelemetry::new()));
        let out = text(tool.call(&json!({"name": "internal"}), &ctx()).await.unwrap());
        assert_eq!(out, "internal body");
    }

    #[tokio::test]
    async fn long_body_truncates_with_path_hint() {
        // Default max_output_chars from build() is 16_000; exceed it.
        // We can't construct a ToolContext with a custom max_output_chars
        // from outside agent-base (event_bus is pub(crate)), so we test
        // against the tool's internal default (MAX_SKILL_BODY_CHARS = 16_000).
        let long_body = "x".repeat(17_000);
        let (resolver, _guard) = make_resolver(&[("long", &long_body, true)]);
        let tool = SkillTool::new(resolver, std::sync::Arc::new(SkillTelemetry::new()));
        let out = text(tool.call(&json!({"name": "long"}), &ctx()).await.unwrap());
        assert!(out.contains("[truncated"), "{out}");
        assert!(out.contains("SKILL.md"), "must include path for read_file continuation: {out}");
    }
}
