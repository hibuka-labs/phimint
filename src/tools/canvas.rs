//! Canvas tools (M3): thin `phi_agent::Tool` shells over `phi_canvas::CanvasTools`.
//!
//! Mechanism lives in phi-canvas — server, session token, envelope validation,
//! screen persistence, and the locked M3 return texts. This file is only the
//! adaptation into the tool registry. Product semantics stay in the host: the
//! click mailbox (M2, `ui/click_mailbox`), the `y` consent gate and browser
//! launch (1C, `ui/canvas_host`), and the status bar (1C).
//!
//! The launcher handed in here is the consent gate itself: `canvas_open` may
//! only *ask*, never open a browser on its own, and `canvas_push`'s implicit
//! open never even asks (the run loop raises the prompt — see `ui/canvas_host`).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use phi_canvas::{BrowserLauncher, CanvasTools, ToolDescriptor, tool_descriptors};
use serde_json::Value;

/// Session-scoped canvas handle: `~/.phimint/canvas/<session>/` (M3 store).
///
/// `launcher` is the host's consent gate (1C) — pass [`ConsentLauncher`]
/// (`ui/canvas_host`) so a model open raises the `y` prompt and nothing else.
pub fn new_canvas_tools(
    base_dir: &Path,
    session_id: &str,
    launcher: Arc<dyn BrowserLauncher>,
) -> Arc<CanvasTools> {
    Arc::new(
        CanvasTools::new(base_dir.join("canvas").join(session_id), session_id)
            .with_launcher(launcher),
    )
}

/// One adapter per canvas verb. Name / description / schema come locked from
/// phi-canvas's [`tool_descriptors`] so the tool face cannot drift from the
/// M3 contract; only the dispatch is ours.
pub(crate) struct CanvasTool {
    descriptor: ToolDescriptor,
    tools: Arc<CanvasTools>,
}

/// The three canvas tools, ready for `register_arc` on the root registry.
pub(crate) fn canvas_tool_adapters(tools: Arc<CanvasTools>) -> Vec<CanvasTool> {
    tool_descriptors()
        .into_iter()
        .map(|descriptor| CanvasTool {
            descriptor,
            tools: tools.clone(),
        })
        .collect()
}

#[async_trait]
impl Tool for CanvasTool {
    fn name(&self) -> &'static str {
        self.descriptor.name
    }

    fn description(&self) -> &'static str {
        self.descriptor.description
    }

    fn schema(&self) -> Value {
        self.descriptor.schema.clone()
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.descriptor.name.to_string(),
            description: "Design canvas (visual design companion).".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        use phi_canvas::tools::{TOOL_CLOSE, TOOL_OPEN, TOOL_PUSH};

        let result = match self.descriptor.name {
            TOOL_OPEN => self.tools.open().await,
            TOOL_PUSH => self.tools.push(args).await,
            TOOL_CLOSE => self.tools.close().await,
            other => unreachable!("descriptor set is locked in phi-canvas: {other}"),
        };
        Ok(vec![Content::text(match result {
            Ok(text) => text,
            Err(err) => format!("[Error]: {err}"),
        })])
    }
}
