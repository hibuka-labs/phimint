//! `add_tool` — the human-review entry point for agent-written tools
//! (tool-socket P3).
//!
//! "Hook up my deploy.sh as a tool" → this tool writes the entry → **full
//! confirmation** (complete argv + absolute path + timeout + env references)
//! → only after a human nods is it persisted and immediately callable.
//!
//! Two boundaries:
//!
//! - **root-only**: registered post-build (`business_tools` is a build-time
//!   snapshot; a sub-agent's tool surface never sees this tool) — a sub-agent
//!   cannot grow tools for itself;
//! - **the registry port holds a [`Weak`]**: the tool table holds this tool;
//!   a strong reference back would be a cycle.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use phi_agent::script_tool::{ParamSpec, ScriptToolEntry};
use phi_agent::{AgentError, AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::external::state::ExternalState;

/// Agent's self-written tool entry point (see module docs).
pub struct AddTool {
    external: Arc<Mutex<ExternalState>>,
    registry: Weak<RwLock<phi_agent::ToolRegistry>>,
}

impl AddTool {
    /// `external` = the patch panel; `registry` = the registry port (Weak,
    /// cycle-proof).
    pub fn new(
        external: Arc<Mutex<ExternalState>>,
        registry: Weak<RwLock<phi_agent::ToolRegistry>>,
    ) -> Self {
        Self { external, registry }
    }
}

#[async_trait]
impl Tool for AddTool {
    fn name(&self) -> &'static str {
        "add_tool"
    }

    fn description(&self) -> &'static str {
        "Mount a script as a persistent, callable tool (writes tools.json). ALWAYS asks the human to confirm the full entry first — the confirmation shows the complete argv, the absolute program path, the timeout, and every env reference. run[0] must be an absolute path (the only exception: powershell/cmd/pwsh with an absolute script file argument). env values must be whole-value ${ENV:VAR} references — never pass literal secrets. After approval the tool is registered as script.<name> and is callable immediately. Manage it later with /tools."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Tool name, [a-z0-9_-]{1,32}. Registered as script.<name>."
                },
                "description": {
                    "type": "string",
                    "description": "Model-visible summary of what the tool does."
                },
                "run": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "argv template — elements are executed directly, never through a shell. run[0] is the program and must be an absolute path. ${param} elements are substituted per call from `parameters`."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Hard timeout in seconds (default 60). The whole process tree is killed on expiry."
                },
                "env": {
                    "type": "object",
                    "description": "Environment overrides. Every value must be a whole-value ${ENV:VAR} reference (e.g. {\"TOKEN\": \"${ENV:MY_TOKEN}\"}); literal values are refused."
                },
                "parameters": {
                    "type": "object",
                    "description": "Scalar parameter specs for ${param} substitution: {\"env\": {\"type\": \"string\", \"description\": \"target environment\"}}."
                }
            },
            "required": ["name", "description", "run"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Mount a script as a persistent tool (human-confirmed).".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let invalid = |raw: String| AgentError::ToolArgsInvalid {
            name: "add_tool".to_string(),
            raw,
        };

        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let run: Vec<String> = match args.get("run") {
            Some(Value::Array(items)) => {
                let mut run = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => run.push(s.to_string()),
                        None => return Err(invalid("every `run` element must be a string".into())),
                    }
                }
                run
            }
            _ => return Err(invalid("`run` must be an array of strings".into())),
        };
        if name.is_empty() || description.is_empty() {
            return Err(invalid(
                "`name` and `description` are required and must be non-empty".into(),
            ));
        }

        // Name law + path law run at construction (fail-closed; an invalid
        // entry never reaches the confirmation dialog).
        let mut entry =
            ScriptToolEntry::new(name, description, run).map_err(|e| invalid(e.to_string()))?;

        if let Some(secs) = args.get("timeout_secs") {
            match secs.as_u64() {
                Some(s) => entry.timeout_ms = Some(s.saturating_mul(1_000)),
                None => {
                    return Err(invalid(
                        "`timeout_secs` must be a non-negative integer".into(),
                    ));
                }
            }
        }
        if let Some(env) = args.get("env") {
            let Some(map) = env.as_object() else {
                return Err(invalid(
                    "`env` must be an object of ${ENV:VAR} references".into(),
                ));
            };
            for (key, value) in map {
                match value.as_str() {
                    Some(s) => {
                        entry.env.insert(key.clone(), s.to_string());
                    }
                    None => return Err(invalid(format!("env `{key}` must be a string"))),
                }
            }
        }
        if let Some(params) = args.get("parameters") {
            entry.parameters =
                serde_json::from_value::<BTreeMap<String, ParamSpec>>(params.clone())
                    .map_err(|e| invalid(format!("`parameters` invalid: {e}")))?;
        }

        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| invalid("tool registry is gone".into()))?;
        // The mount is human-paced: `add_tool_from_agent` pops the full-entry
        // review and waits for the person. That wait must not spend this call's
        // execution budget — before the clock was pauseable, a slow review ate
        // the engine's 600s and came back as a bogus `[Tool Timeout]` with the
        // mount half-reported (acceptance session 20261018_29e1e182).
        let _human_paced = ctx.pause_execution_clock();
        let mut state = self.external.lock().await;
        let confirm = state.confirm_gate();
        let message = state
            .add_tool_from_agent(&registry, confirm.as_ref(), entry)
            .await;
        Ok(vec![Content::text(message)])
    }
}
