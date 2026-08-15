//! Decompose tool (Phase 4): turn a big task into a parallel plan — or not.
//!
//! The "拆" half of design §7's `decompose`/`merge`. `decompose` makes a *nested*
//! structured LLM call (a tool that itself calls the LLM — a deliberate stress
//! point of the framework) and returns a `Decomposition`: either `serial` ("do it
//! inline, not worth fanning out" — §7.4) or `parallel` with N independent slices.
//!
//! Each slice declares its file boundary (`files`), the minimal context to hand a
//! sub-agent (`context` — problem #2), and the concrete ask (`task`). The
//! decomposition is stashed in the shared [`WorkspaceTracker`] so `merge` can
//! later attribute changes to slices and detect conflicts (problem #3).

use std::path::PathBuf;
use std::sync::Arc;

use agent_base::{ChatMessage, ResponseFormat, StreamClient};
use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use super::repomap::build_repo_map;
use super::workspace::{Slice, WorkspaceTracker};

/// Decomposition strategy (§7.4): fan out only when the work is genuinely parallel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Strategy {
    Serial,
    Parallel,
}

impl<'de> Deserialize<'de> for Strategy {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        match s.trim().to_ascii_lowercase().as_str() {
            "parallel" | "fan_out" | "fan-out" => Ok(Strategy::Parallel),
            // Anything unrecognised → Serial. Fanning out on a mis-parse is the
            // dangerous direction (overlapping edits); serial is always safe.
            _ => Ok(Strategy::Serial),
        }
    }
}

/// A decomposition: either do it inline, or split into independent slices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decomposition {
    pub strategy: Strategy,
    #[serde(default)]
    pub slices: Vec<Slice>,
}

/// The nested-LLM prompt: strict JSON contract, cut on module/file boundaries,
/// and *judge parallelizability* rather than blindly fanning out.
const DECOMPOSE_PROMPT: &str = r#"You decompose a coding task for a multi-agent orchestrator. Given a task and a repository map, decide whether to parallelize and how.

Rules:
1. Judge whether the task is genuinely parallelizable (§parallel vs serial). Prefer SERIAL unless there are 2+ clearly independent sub-tasks that touch DISJOINT files and don't depend on each other's results.
2. Cut on module/file boundaries — never split a single file across two slices.
3. Each slice's `files` is the exact set of files it will create or modify (workspace-relative paths). Slices must NOT overlap on files.
4. `context` is the minimal orientation a sub-agent needs (relevant existing symbols/interfaces), `task` is the concrete instruction.
5. For `serial`, return an empty `slices` array.

Return ONLY a JSON object, no prose, no markdown:
{"strategy": "serial" | "parallel", "slices": [{"name": "...", "files": ["..."], "context": "...", "task": "..."}]}"#;

pub struct DecomposeTool {
    llm: Arc<dyn StreamClient>,
    tracker: Arc<WorkspaceTracker>,
    root: PathBuf,
}

impl DecomposeTool {
    pub fn new(llm: Arc<dyn StreamClient>, tracker: Arc<WorkspaceTracker>, root: PathBuf) -> Self {
        Self { llm, tracker, root }
    }
}

/// Parse an LLM response into a `Decomposition`, tolerating markdown fences.
///
/// Pure and testable. Extracts the first `{...}` JSON object, then deserializes.
pub fn parse_decomposition(raw: &str) -> Result<Decomposition, String> {
    let json = extract_json_object(raw)
        .ok_or_else(|| format!("no JSON object in decompose response: {}", truncate(raw, 200)))?;
    serde_json::from_str::<Decomposition>(&json)
        .map_err(|e| format!("invalid decomposition JSON: {e}"))
}

/// Pull the first balanced-enough `{...}` out of a possibly-fenced response.
fn extract_json_object(s: &str) -> Option<String> {
    let mut s = s.trim();
    // Strip a leading ```json / ``` fence.
    if let Some(rest) = s.strip_prefix("```") {
        s = rest.trim_start();
        if let Some(rest) = s.strip_prefix("json") {
            s = rest.trim_start();
        }
    }
    // Strip a trailing fence.
    if let Some(rest) = s.strip_suffix("```") {
        s = rest.trim_end();
    }
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(s[start..=end].to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect::<String>() + "..."
    } else {
        s.to_string()
    }
}

/// Render a decomposition as a compact plan the orchestrating agent can act on.
pub fn format_plan(decomp: &Decomposition) -> String {
    match decomp.strategy {
        Strategy::Serial => {
            "Strategy: serial — this task isn't worth fanning out. Implement it directly (read, edit, verify).".to_string()
        }
        Strategy::Parallel => {
            let mut out = format!(
                "Strategy: parallel — {} independent slice(s).\n\n",
                decomp.slices.len()
            );
            for slice in &decomp.slices {
                out.push_str(&format!("- slice `{}`\n", slice.name));
                out.push_str(&format!("    files: {}\n", slice.files.join(", ")));
                out.push_str(&format!("    task: {}\n", slice.task.trim()));
            }
            out.push_str("\nSpawn one sub-agent per slice (spawn_agent task_name=<name>, message = context + task, full_permission=true), wait for each, then call `merge`.");
            out
        }
    }
}

#[async_trait]
impl Tool for DecomposeTool {
    fn name(&self) -> &'static str {
        "decompose"
    }

    fn description(&self) -> &'static str {
        "Decompose a task into either a serial plan (do it inline) or parallel independent slices (each with a disjoint file boundary). Call this FIRST for any multi-step task; if it returns `parallel`, spawn one sub-agent per slice, then call `merge` to reconcile."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to decompose (the full user request)."
                }
            },
            "required": ["task"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Decompose a task into parallel slices or a serial plan.".to_string(),
            origin: "phiforge".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");

        if task.is_empty() {
            return Ok(vec![Content::text(
                "[Error]: `task` is required and must be non-empty.",
            )]);
        }

        tracing::info!(task_len = task.len(), "decompose start");

        // Snapshot the workspace *before* any sub-agent touches it, so `merge`
        // can diff against this baseline.
        let repo_map = build_repo_map(&self.root, None).unwrap_or_else(|e| format!("(repo map unavailable: {e})"));

        let user = format!("Task:\n{task}\n\nRepository map:\n{repo_map}");
        let messages = vec![
            ChatMessage::system(DECOMPOSE_PROMPT),
            ChatMessage::user(user),
        ];

        let raw = match self
            .llm
            .chat(&messages, &[], None, Some(&ResponseFormat::JsonObject))
            .await
        {
            Ok(text) => text,
            Err(e) => {
                return Ok(vec![Content::text(format!(
                    "[Error]: decompose LLM call failed: {e}"
                ))]);
            }
        };

        let decomp = match parse_decomposition(&raw) {
            Ok(d) => d,
            Err(e) => {
                return Ok(vec![Content::text(format!(
                    "[Error]: {e}\n(LLM returned: {})",
                    truncate(&raw, 300)
                ))]);
            }
        };

        // Record the snapshot + declared slice boundaries for `merge`.
        self.tracker.record(&self.root, decomp.slices.clone());
        tracing::info!(
            strategy = ?decomp.strategy,
            slices = decomp.slices.len(),
            "decompose done"
        );

        Ok(vec![Content::text(format_plan(&decomp))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_parallel_decomposition() {
        let raw = r#"```json
        {"strategy": "parallel", "slices": [
            {"name": "cache", "files": ["src/cache.rs"], "context": "uses User", "task": "add get_user"},
            {"name": "logging", "files": ["src/logging.rs"], "context": "", "task": "add logger"}
        ]}
        ```"#;
        let d = parse_decomposition(raw).unwrap();
        assert_eq!(d.strategy, Strategy::Parallel);
        assert_eq!(d.slices.len(), 2);
        assert_eq!(d.slices[0].name, "cache");
        assert_eq!(d.slices[0].files, vec!["src/cache.rs"]);
        assert_eq!(d.slices[1].name, "logging");
    }

    #[test]
    fn parse_serial_decomposition_defaults_slices() {
        let raw = r#"{"strategy": "serial"}"#;
        let d = parse_decomposition(raw).unwrap();
        assert_eq!(d.strategy, Strategy::Serial);
        assert!(d.slices.is_empty());
    }

    #[test]
    fn parse_unknown_strategy_falls_back_to_serial() {
        let raw = r#"{"strategy": "banana", "slices": [{"name":"x","files":["a.rs"],"context":"","task":"t"}]}"#;
        let d = parse_decomposition(raw).unwrap();
        assert_eq!(d.strategy, Strategy::Serial);
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_decomposition("sorry, I can't do that").is_err());
        assert!(parse_decomposition("").is_err());
    }

    #[test]
    fn extract_json_object_strips_fences() {
        let s = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_object(s).as_deref(), Some("{\"a\":1}"));
        let s2 = "plain {\"a\":1} trailing";
        assert_eq!(extract_json_object(s2).as_deref(), Some("{\"a\":1}"));
        assert_eq!(extract_json_object("no braces"), None);
    }

    #[test]
    fn format_plan_serial_and_parallel() {
        let serial = format_plan(&Decomposition {
            strategy: Strategy::Serial,
            slices: vec![],
        });
        assert!(serial.contains("serial"), "{serial}");

        let parallel = format_plan(&Decomposition {
            strategy: Strategy::Parallel,
            slices: vec![Slice {
                name: "cache".into(),
                files: vec!["src/cache.rs".into()],
                context: String::new(),
                task: "add get_user".into(),
            }],
        });
        assert!(parallel.contains("parallel"), "{parallel}");
        assert!(parallel.contains("cache"), "{parallel}");
        assert!(parallel.contains("src/cache.rs"), "{parallel}");
    }

    #[test]
    fn metadata_and_schema() {
        // Schema requires `task`.
        let t = DecomposeTool::new(
            std::sync::Arc::new(DummyClient),
            Arc::new(WorkspaceTracker::new()),
            PathBuf::from("."),
        );
        assert_eq!(t.name(), "decompose");
        let schema = t.schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().unwrap().contains(&"task".into()));
    }

    /// Minimal StreamClient stub — the nested LLM call is never exercised in
    /// these unit tests (only the parser + formatting + schema are).
    struct DummyClient;

    #[async_trait::async_trait]
    impl StreamClient for DummyClient {
        async fn stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[Value],
            _reasoning: Option<&agent_base::ReasoningConfig>,
            _response_format: Option<&ResponseFormat>,
        ) -> AgentResult<std::pin::Pin<Box<dyn futures_core::Stream<Item = AgentResult<agent_base::StreamChunk>> + Send>>>
        {
            unimplemented!()
        }

        fn capabilities(&self) -> agent_base::LlmCapabilities {
            agent_base::LlmCapabilities::default()
        }
    }
}
