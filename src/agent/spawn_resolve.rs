//! Spawn model resolver — the wire-invariant gatekeeper for sub-agent models.
//!
//! Sits between the LLM's `spawn_agent` `model` argument and the child's
//! model slot. Tier tokens (`main`/`lite`/`advanced`) die at this seam:
//! `body["model"]` only ever carries a real model name (design decision 1).
//! Resolution runs per spawn — never frozen at build — so `/model`, `/lite`
//! and `/advanced` changes take effect on the very next spawn.
//!
//! Same-endpoint rule (design decision 5): a resolved tier's endpoint must
//! match the boot tuple (the endpoint the child's client was built from).
//! A cross-endpoint tier writes NO override — the child keeps its own
//! client's model — and a notice says so. The notice is never silent
//! (review A-3): user-event channel first, tool-result text as fallback.
//!
//! Every spawn appends one audit line under `~/.phimint/model-route/`
//! (review A-2: fail-open — a write failure is logged, never blocking).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

// Names come through the phi-agent facade (Cargo.toml's "no direct deps"
// rule). `agent-base` stays out of the dependency list here — the guard-pin
// seam (T4) is the one place that must name `ReactLoopGuard` itself.
use phi_agent::{AgentResult, Content, NoticeKind, Tool, ToolContext, UserEvent};

use crate::model::config::EndpointTuple;
use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;

/// The router scene key this resolver serves. The router's scene table is
/// the single vocabulary source (review A-1) — a scene value that stops
/// routing must fail the tests below, not silently start a second table.
pub const SCENE_SUB_AGENT: &str = "sub_agent";

/// Replacement description for the `model` argument in the tool schema.
///
/// The schema the framework generates from `SpawnAgentArgs::model` still
/// carries a stale "accepted and stored but ignored at LLM-call time" note
/// (that note is wrong — the override does reach `body["model"]`). This
/// wrapper owns the contract now (SC-4); the recognized token set is the
/// tier table's keys, the names listed here are the shipped tiers.
pub const MODEL_ARG_CONTRACT: &str = "Optional model tier for the sub-agent: \
`lite` (the default when omitted), `advanced`, or `main`. \
A tier name is resolved to its real model name at spawn time. \
A real model name passes through unchanged. \
If the resolved tier lives on a different endpoint than this session's \
provider, no model override is written and a notice is emitted.";

/// What the resolver decided to write into the child's model slot.
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnModel {
    /// Write this REAL model name into the child's model slot.
    Override(String),
    /// Cross-endpoint fallback (design decision 5): write no override — the
    /// child keeps its own client's model and a notice says so.
    Inherit,
}

/// One resolution — also the audit line body (plus ts/session/scene).
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The `model` argument as requested, `None` when omitted.
    pub requested: Option<String>,
    /// The tier the request resolved through, `None` for a passthrough.
    pub resolved_tier: Option<String>,
    /// The real model name the tier points at (recorded even when the
    /// endpoint rule suppresses the override — the audit must never say
    /// "advanced" while the run actually sits on `main`).
    pub resolved_name: Option<String>,
    /// `false` only when the same-endpoint check ran and failed.
    pub endpoint_ok: bool,
    pub action: SpawnModel,
}

/// A snapshot of everything resolution needs — taken fresh per spawn, so a
/// tier change made mid-session applies to the very next spawn.
pub struct RouteSnapshot {
    /// Tier for this scene when no `model` argument is given
    /// (`PhimintRouter::route(SCENE_SUB_AGENT)`).
    pub default_tier: String,
    /// Tier token → full endpoint tuple: the vocabulary (review A-1) and the
    /// resolution target in one table.
    pub tiers: Vec<(String, EndpointTuple)>,
    /// Boot endpoint tuple — the same-endpoint anchor (design decision 5).
    pub boot: EndpointTuple,
}

/// Resolve a spawn `model` argument against a snapshot. Pure — the live
/// store/ruler access lives behind [`RouteSource`].
pub fn resolve(requested: Option<&str>, snap: &RouteSnapshot) -> Resolution {
    let tier = match requested {
        None => Some(snap.default_tier.clone()),
        Some(s) if snap.tiers.iter().any(|(t, _)| t == s) => Some(s.to_string()),
        Some(_) => None,
    };

    let Some(tier) = tier else {
        // Escape hatch (review C-2): a real name passes through verbatim;
        // a typo surfaces on the provider's error surface — visible, and
        // the audit keeps the requested name greppable.
        let name = requested
            .map(str::to_string)
            .expect("tier is None only when requested is Some");
        return Resolution {
            requested: Some(name.clone()),
            resolved_tier: None,
            resolved_name: Some(name.clone()),
            endpoint_ok: true, // the check does not apply to a passthrough
            action: SpawnModel::Override(name),
        };
    };

    let (name, tuple) = snap
        .tiers
        .iter()
        .find(|(t, _)| *t == tier)
        .map(|(_, tuple)| (tuple.0.clone(), tuple.clone()))
        .unwrap_or_else(|| (snap.boot.0.clone(), snap.boot.clone()));
    let endpoint_ok = endpoint_of(&tuple) == endpoint_of(&snap.boot);
    let action = if endpoint_ok {
        SpawnModel::Override(name.clone())
    } else {
        SpawnModel::Inherit
    };

    Resolution {
        requested: requested.map(str::to_string),
        resolved_tier: Some(tier),
        resolved_name: Some(name),
        endpoint_ok,
        action,
    }
}

/// The endpoint part of a tuple (`base_url`, `api_key`, `protocol`). The
/// model NAME is deliberately excluded — same endpoint with a different
/// model is exactly what the override is for.
fn endpoint_of(tuple: &EndpointTuple) -> (&str, &str, &str) {
    (tuple.1.as_str(), tuple.2.as_str(), tuple.3.as_str())
}

/// Per-call routing data (the live store + router behind an async seam so
/// the resolver core stays pure and testable).
#[async_trait::async_trait]
pub trait RouteSource: Send + Sync {
    async fn snapshot(&self) -> RouteSnapshot;
}

/// The production [`RouteSource`]: reads the live tier table per spawn.
pub struct StoreRouteSource {
    pub store: Arc<Mutex<ModelStore>>,
    pub router: Arc<PhimintRouter>,
}

#[async_trait::async_trait]
impl RouteSource for StoreRouteSource {
    async fn snapshot(&self) -> RouteSnapshot {
        let store = self.store.lock().await;
        let tiers = store
            .list_tiers()
            .into_iter()
            .map(|(tier, _)| (tier.to_string(), store.tier_tuple(tier)))
            .collect();
        RouteSnapshot {
            default_tier: self.router.route(SCENE_SUB_AGENT).to_string(),
            tiers,
            boot: store.default_tuple(),
        }
    }
}

/// One route-audit line per spawn, appended under `dir`
/// (`~/.phimint/model-route/model-route-YYYYMM.jsonl`).
///
/// Fail-open by design (review A-2): a write failure is logged and dropped —
/// observability must never block the spawn control path. The session id
/// arrives per call (the tool context carries it), so one writer serves any
/// number of sessions.
#[derive(Clone)]
pub struct AuditWriter {
    dir: PathBuf,
}

impl AuditWriter {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn write(&self, session: &str, resolution: &Resolution) {
        let now = chrono::Local::now();
        let path = self
            .dir
            .join(format!("model-route-{}.jsonl", now.format("%Y-%m")));
        let line = json!({
            "ts": now.to_rfc3339(),
            "session": session,
            "scene": SCENE_SUB_AGENT,
            "requested": resolution.requested,
            "resolved_tier": resolution.resolved_tier,
            "resolved_name": resolution.resolved_name,
            "endpoint_ok": resolution.endpoint_ok,
        });
        let payload = format!("{line}\n");
        let result = std::fs::create_dir_all(&self.dir).and_then(|_| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| f.write_all(payload.as_bytes()))
        });
        if let Err(e) = result {
            tracing::warn!("route audit write failed ({}): {}", path.display(), e);
        }
    }
}

/// Review A-3: the cross-endpoint notice must never be silent. Primary
/// channel first — [`ToolContext::emit_user_event`], the framework's
/// dual-write seam (the event bus copy is what feeds a TUI; a raw
/// `user_event_tx` send reaches the run's persistence callback only and
/// renders nowhere). An `Info` notice stays a persistent transcript system
/// line (a `Progress` event would flash the status bar and vanish unseen).
/// When the channel is gone (nested/headless contexts), the caller gets the
/// text back to append to the tool result.
fn deliver_notice(ctx: &ToolContext, text: &str) -> Option<String> {
    if ctx.user_event_tx.is_closed() {
        return Some(text.to_string());
    }
    ctx.emit_user_event(UserEvent::Notice {
        kind: NoticeKind::Info,
        source: "spawn-model-route".to_string(),
        text: text.to_string(),
    });
    None
}

/// Wraps the framework's `spawn_agent` tool: resolves the `model` argument
/// at call time, rewrites it so the tier token dies here, audits the spawn
/// and notices cross-endpoint fallbacks.
pub struct SpawnResolverTool {
    inner: Arc<dyn Tool>,
    source: Arc<dyn RouteSource>,
    audit: AuditWriter,
}

impl SpawnResolverTool {
    pub fn wrap(
        inner: Arc<dyn Tool>,
        source: Arc<dyn RouteSource>,
        audit: AuditWriter,
    ) -> Arc<dyn Tool> {
        Arc::new(Self {
            inner,
            source,
            audit,
        })
    }
}

#[async_trait::async_trait]
impl Tool for SpawnResolverTool {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn description(&self) -> &'static str {
        self.inner.description()
    }

    /// Only the `model` property's description is rewritten — that is where
    /// the stale `SpawnAgentArgs` note reaches the LLM (SC-4). Everything
    /// else passes through untouched.
    fn schema(&self) -> Value {
        let mut schema = self.inner.schema();
        if let Some(model) = schema.pointer_mut("/properties/model")
            && let Some(obj) = model.as_object_mut()
        {
            obj.insert("description".to_string(), json!(MODEL_ARG_CONTRACT));
        }
        schema
    }

    async fn call(&self, args: &Value, ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let requested = args.get("model").and_then(Value::as_str);
        let resolution = resolve(requested, &self.source.snapshot().await);
        self.audit.write(&ctx.session_id.to_string(), &resolution);

        // Rewrite the argument so the token dies at this seam: `Inherit`
        // removes the key entirely — the child's model slot stays empty and
        // the child runs its own client's model.
        let mut new_args = match args {
            Value::Object(map) => Value::Object(map.clone()),
            other => other.clone(),
        };
        let mut notice = None;
        match &resolution.action {
            SpawnModel::Override(name) => {
                if let Value::Object(map) = &mut new_args {
                    map.insert("model".to_string(), Value::String(name.clone()));
                }
            }
            SpawnModel::Inherit => {
                if let Value::Object(map) = &mut new_args {
                    map.remove("model");
                }
                notice = Some(format!(
                    "spawn model route: tier `{}` resolves to `{}` on another \
endpoint than this session's provider; no override written — the sub-agent \
runs its own provider's model",
                    resolution.resolved_tier.as_deref().unwrap_or("?"),
                    resolution.resolved_name.as_deref().unwrap_or("?"),
                ));
            }
        }

        let fallback = notice.as_deref().and_then(|text| deliver_notice(ctx, text));

        let mut out = self.inner.call(&new_args, ctx).await?;
        if let Some(text) = fallback {
            out.push(Content::Text { text });
        }
        Ok(out)
    }
}

/// Mounting helper for the multi-agent tool factory: wrap every tool that
/// exposes a `model` argument with [`SpawnResolverTool`] — that property is
/// the tier-token leak channel, so the invariant "a tier token never reaches
/// the wire" is enforced on each of them (design decision 1). Tools without
/// a `model` property pass through untouched.
pub fn wrap_multi_agent_tools(
    tools: Vec<Arc<dyn Tool>>,
    source: Arc<dyn RouteSource>,
    audit: AuditWriter,
) -> Vec<Arc<dyn Tool>> {
    tools
        .into_iter()
        .map(|tool| {
            if tool.schema().pointer("/properties/model").is_some() {
                SpawnResolverTool::wrap(tool, source.clone(), audit.clone())
            } else {
                tool
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use phi_agent::llm_trait::{
        Capabilities, ChatRequest, ChatResponse, ChatStream, LlmError, LlmProvider, ProviderInfo,
    };

    fn tuple(model: &str, base_url: &str, key: &str, protocol: &str) -> EndpointTuple {
        (
            model.to_string(),
            base_url.to_string(),
            key.to_string(),
            protocol.to_string(),
        )
    }

    fn snapshot() -> RouteSnapshot {
        RouteSnapshot {
            default_tier: "lite".to_string(),
            tiers: vec![
                (
                    "main".to_string(),
                    tuple("main-real", "https://a", "k", "openai"),
                ),
                (
                    "lite".to_string(),
                    tuple("lite-real", "https://a", "k", "openai"),
                ),
                (
                    "advanced".to_string(),
                    tuple("adv-real", "https://a", "k", "openai"),
                ),
            ],
            boot: tuple("boot-main", "https://a", "k", "openai"),
        }
    }

    // ── resolve(): default / tokens / passthrough ─────────────────────────

    #[test]
    fn omitted_model_uses_default_scene_tier() {
        let r = resolve(None, &snapshot());
        assert_eq!(r.resolved_tier.as_deref(), Some("lite"));
        assert_eq!(r.action, SpawnModel::Override("lite-real".to_string()));
        assert!(r.requested.is_none());
    }

    #[test]
    fn tier_token_resolves_to_real_name() {
        let r = resolve(Some("advanced"), &snapshot());
        assert_eq!(r.resolved_tier.as_deref(), Some("advanced"));
        assert_eq!(r.resolved_name.as_deref(), Some("adv-real"));
        assert_eq!(r.action, SpawnModel::Override("adv-real".to_string()));
    }

    #[test]
    fn resolution_follows_tier_change_per_call() {
        // `/lite` mid-session moves the tier's model name — the next spawn
        // must see it (nothing is frozen at build).
        let mut snap = snapshot();
        snap.tiers[1].1 = tuple("lite-new", "https://a", "k", "openai");
        let r = resolve(Some("lite"), &snap);
        assert_eq!(r.action, SpawnModel::Override("lite-new".to_string()));
    }

    #[test]
    fn real_name_passes_through_verbatim() {
        let r = resolve(Some("claude-opus-5-5"), &snapshot());
        assert_eq!(r.resolved_tier, None);
        assert_eq!(r.resolved_name.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            r.action,
            SpawnModel::Override("claude-opus-5-5".to_string())
        );
    }

    #[test]
    fn unknown_tier_name_is_a_passthrough_not_a_token() {
        // Vocabulary comes from the tier table (review A-1): a string that
        // is not a table key is a real-name guess, never a rewritten token.
        let r = resolve(Some("Lite"), &snapshot());
        assert_eq!(r.resolved_tier, None);
        assert_eq!(r.action, SpawnModel::Override("Lite".to_string()));
        assert!(r.endpoint_ok, "a passthrough fails no endpoint check");
    }

    // ── resolve(): same-endpoint rule (design decision 5) ────────────────

    #[test]
    fn same_endpoint_writes_override() {
        let mut snap = snapshot();
        snap.boot = tuple("boot-main", "https://a", "k", "openai");
        snap.tiers[1].1 = tuple("lite-real", "https://a", "k", "openai");
        let r = resolve(Some("lite"), &snap);
        assert!(r.endpoint_ok);
        assert_eq!(r.action, SpawnModel::Override("lite-real".to_string()));
    }

    #[test]
    fn cross_endpoint_falls_back_to_inherit() {
        let mut snap = snapshot();
        snap.tiers[2].1 = tuple("adv-real", "https://other", "k2", "openai");
        let r = resolve(Some("advanced"), &snap);
        assert!(!r.endpoint_ok);
        assert_eq!(r.action, SpawnModel::Inherit);
        // The audit still records the real name the tier points at.
        assert_eq!(r.resolved_name.as_deref(), Some("adv-real"));
    }

    #[test]
    fn endpoint_check_compares_url_key_protocol_not_model_name() {
        let mut snap = snapshot();
        snap.boot = tuple("boot-main", "https://a", "k", "openai");
        // Different model name, same endpoint → override is the point.
        snap.tiers[1].1 = tuple("lite-real", "https://a", "k", "openai");
        assert!(resolve(Some("lite"), &snap).endpoint_ok);
        // Same model name, different key → different endpoint, no override.
        snap.tiers[1].1 = tuple("boot-main", "https://a", "k-OTHER", "openai");
        assert_eq!(resolve(Some("lite"), &snap).action, SpawnModel::Inherit);
    }

    // ── notice delivery (review A-3) ─────────────────────────────────────

    #[test]
    fn notice_goes_to_the_user_event_channel_first() {
        let mut ctx = ToolContext::for_test();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.user_event_tx = tx;
        let fallback = deliver_notice(&ctx, "cross-endpoint");
        assert_eq!(fallback, None);
        match rx.try_recv() {
            // Info, not Progress: the TUI keeps an Info notice as a
            // persistent transcript line — a Progress flash would vanish
            // unseen (review A-3 "never silent").
            Ok(UserEvent::Notice {
                kind: NoticeKind::Info,
                source,
                text,
            }) => {
                assert_eq!(text, "cross-endpoint");
                assert_eq!(source, "spawn-model-route");
            }
            other => panic!("expected an Info notice on the channel, got {other:?}"),
        }
    }

    #[test]
    fn notice_falls_back_to_text_when_the_channel_is_dead() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<UserEvent>();
        drop(rx);
        let mut ctx = ToolContext::for_test();
        ctx.user_event_tx = tx;
        let fallback = deliver_notice(&ctx, "cross-endpoint");
        assert_eq!(fallback.as_deref(), Some("cross-endpoint"));
    }

    // ── audit (review A-2) ───────────────────────────────────────────────

    #[test]
    fn audit_writes_one_json_line_per_spawn() {
        let dir = std::env::temp_dir().join(format!(
            "phimint-route-audit-{}-{}",
            std::process::id(),
            "lines"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let writer = AuditWriter::new(&dir);
        let snap = snapshot();
        writer.write("sess-1", &resolve(Some("lite"), &snap));
        writer.write("sess-1", &resolve(None, &snap));

        let month = chrono::Local::now().format("%Y-%m");
        let path = dir.join(format!("model-route-{month}.jsonl"));
        let body = std::fs::read_to_string(&path).expect("audit file written");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one line per spawn");
        let first: Value = serde_json::from_str(lines[0]).expect("valid JSON line");
        assert_eq!(first["session"], "sess-1");
        assert_eq!(first["scene"], "sub_agent");
        assert_eq!(first["requested"], "lite");
        assert_eq!(first["resolved_tier"], "lite");
        assert_eq!(first["resolved_name"], "lite-real");
        assert_eq!(first["endpoint_ok"], true);
        let second: Value = serde_json::from_str(lines[1]).expect("valid JSON line");
        assert_eq!(second["requested"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_failure_is_fail_open() {
        // `dir` is a plain file: every write must fail — and must not panic
        // or block the spawn.
        let path = std::env::temp_dir().join(format!(
            "phimint-route-audit-blocked-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"not a dir").expect("plant blocker file");
        let writer = AuditWriter::new(&path);
        writer.write("sess-1", &resolve(None, &snapshot()));
        let _ = std::fs::remove_file(&path);
    }

    // ── decorator: the wire invariant (design decision 1) ────────────────

    struct CaptureTool {
        seen: Arc<StdMutex<Option<Value>>>,
        out: Vec<Content>,
    }

    #[async_trait::async_trait]
    impl Tool for CaptureTool {
        fn name(&self) -> &'static str {
            "spawn_agent"
        }
        fn description(&self) -> &'static str {
            "inner description"
        }
        fn schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "TODO(layer-3): ignored at LLM-call time"}
                }
            })
        }
        async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
            *self.seen.lock().unwrap() = Some(args.clone());
            Ok(self.out.clone())
        }
    }

    struct FakeSource(RouteSnapshot);
    #[async_trait::async_trait]
    impl RouteSource for FakeSource {
        async fn snapshot(&self) -> RouteSnapshot {
            RouteSnapshot {
                default_tier: self.0.default_tier.clone(),
                tiers: self.0.tiers.clone(),
                boot: self.0.boot.clone(),
            }
        }
    }

    fn wrapped(snap: RouteSnapshot) -> (Arc<StdMutex<Option<Value>>>, Arc<dyn Tool>) {
        let seen = Arc::new(StdMutex::new(None));
        let inner = Arc::new(CaptureTool {
            seen: seen.clone(),
            out: vec![Content::Text {
                text: "child report: ok".to_string(),
            }],
        });
        let dir =
            std::env::temp_dir().join(format!("phimint-route-audit-wrap-{}", std::process::id()));
        let tool =
            SpawnResolverTool::wrap(inner, Arc::new(FakeSource(snap)), AuditWriter::new(dir));
        (seen, tool)
    }

    fn seen_args(seen: &Arc<StdMutex<Option<Value>>>) -> Value {
        seen.lock().unwrap().clone().expect("inner tool was called")
    }

    #[tokio::test]
    async fn tier_token_never_reaches_the_wire() {
        let (seen, tool) = wrapped(snapshot());
        let args = json!({"task_name": "t", "task": "do it", "model": "lite"});
        tool.call(&args, &ToolContext::for_test())
            .await
            .expect("call");
        let got = seen_args(&seen);
        assert_eq!(got["model"], "lite-real", "token must be resolved here");
        assert_ne!(got["model"], "lite");
    }

    #[tokio::test]
    async fn omitted_model_gets_the_default_tier_name() {
        let (seen, tool) = wrapped(snapshot());
        let args = json!({"task_name": "t", "task": "do it"});
        tool.call(&args, &ToolContext::for_test())
            .await
            .expect("call");
        assert_eq!(seen_args(&seen)["model"], "lite-real");
    }

    #[tokio::test]
    async fn passthrough_keeps_the_real_name_verbatim() {
        let (seen, tool) = wrapped(snapshot());
        let args = json!({"task_name": "t", "task": "do it", "model": "my-model-v2"});
        tool.call(&args, &ToolContext::for_test())
            .await
            .expect("call");
        assert_eq!(seen_args(&seen)["model"], "my-model-v2");
    }

    #[tokio::test]
    async fn cross_endpoint_removes_the_model_key_and_notices() {
        let mut snap = snapshot();
        snap.tiers[2].1 = tuple("adv-real", "https://other", "k2", "openai");
        let (seen, tool) = wrapped(snap);
        let args = json!({"task_name": "t", "task": "do it", "model": "advanced"});
        let out = tool
            .call(&args, &ToolContext::for_test())
            .await
            .expect("call");
        let got = seen_args(&seen);
        assert!(got.get("model").is_none(), "no override may be written");
        // `for_test()` drops the receiver, so the notice must arrive as
        // appended tool text — never silent (review A-3).
        assert!(
            out.iter().any(|c| matches!(c, Content::Text { text }
                if text.contains("no override written"))),
            "fallback notice text missing from tool result: {out:?}"
        );
    }

    #[tokio::test]
    async fn schema_model_contract_replaces_the_stale_note() {
        let (_seen, tool) = wrapped(snapshot());
        let schema = tool.schema();
        let desc = schema
            .pointer("/properties/model/description")
            .and_then(Value::as_str)
            .expect("model description present");
        assert!(!desc.contains("TODO"), "stale note must be gone: {desc}");
        assert!(desc.contains("lite"), "contract names the tiers: {desc}");
        // Contract pin (SC-4 / R2-4): the description is what the model
        // reads, so it must teach the resolver's rule — omission resolves to
        // the configured `sub_agent` tier — and never the inner tool's stale
        // "inherit the parent's model" rule. A rewording that drops either
        // half fails here.
        assert!(
            desc.contains("the default when omitted"),
            "contract must teach the omission rule: {desc}"
        );
        assert!(
            !desc.to_lowercase().contains("inherit"),
            "the inner tool's stale inherit rule must not survive: {desc}"
        );
    }

    #[tokio::test]
    async fn inner_name_and_description_pass_through() {
        let (_seen, tool) = wrapped(snapshot());
        assert_eq!(tool.name(), "spawn_agent");
        assert_eq!(tool.description(), "inner description");
    }

    // ── factory mounting helper ──────────────────────────────────────────

    // ── the production route source (the live seam) ──────────────────────

    /// Value: protects=the live snapshot wiring (scene→default tier, the
    /// tier table, the boot endpoint anchor) the resolver reads per spawn;
    /// fails_when=the production source hands the resolver a stale or wrong
    /// tier table / a boot anchor from another tuple, so a tier lands an
    /// override on a foreign endpoint or the boot model is mis-anchored;
    /// why_new=only the FakeSource seam is exercised today, so the wiring
    /// between the store, the scene table and the resolver is unproven;
    /// seam=none; no_claim=the resolver core (proven above) and the booted
    /// TUI (pty scenarios 13/15).
    #[tokio::test]
    async fn the_production_source_serves_the_live_table_and_the_boot_anchor() {
        use crate::model::config::{ModelConfig, TierConfig};

        struct StubProvider;
        #[async_trait::async_trait]
        impl LlmProvider for StubProvider {
            async fn stream(&self, _r: ChatRequest) -> Result<ChatStream, LlmError> {
                Err(LlmError::api(500, "stub"))
            }
            async fn chat(&self, _r: ChatRequest) -> Result<ChatResponse, LlmError> {
                Err(LlmError::api(500, "stub"))
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::default()
            }
            fn info(&self) -> ProviderInfo {
                ProviderInfo {
                    name: "stub".to_string(),
                    model: "stub-model".to_string(),
                    version: None,
                }
            }
        }

        let config = ModelConfig {
            base_url: Some("https://a".to_string()),
            api_key: Some("k".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("main-real".to_string()),
            lite: Some(TierConfig::Simple("lite-real".to_string())),
            advanced: Some(TierConfig::Simple("adv-real".to_string())),
            model: None,
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
            code_exec: Default::default(),
        };
        let store = Arc::new(Mutex::new(ModelStore::new(
            config,
            Arc::new(StubProvider) as Arc<dyn LlmProvider>,
        )));
        let source = StoreRouteSource {
            store: Arc::clone(&store),
            router: Arc::new(PhimintRouter::new()),
        };

        let snap = source.snapshot().await;
        assert_eq!(
            snap.default_tier, "lite",
            "the sub_agent scene decides the default tier"
        );
        let tier = snap
            .tiers
            .iter()
            .find(|(t, _)| t == "lite")
            .map(|(_, tuple)| tuple.clone())
            .expect("the lite tier is in the table");
        assert_eq!(tier.0, "lite-real", "the tier carries its real model name");
        assert_eq!(
            snap.boot,
            store.lock().await.default_tuple(),
            "the boot anchor is the store's own boot tuple"
        );

        // Freshness: the source is read per spawn, so a mid-session tier
        // change must be visible on the very next snapshot.
        store
            .lock()
            .await
            .set_tier_model("lite", "lite-new".to_string());
        let snap2 = source.snapshot().await;
        let tier2 = snap2
            .tiers
            .iter()
            .find(|(t, _)| t == "lite")
            .map(|(_, tuple)| tuple.clone())
            .expect("the lite tier is still in the table");
        assert_eq!(tier2.0, "lite-new", "no snapshot may go stale");
    }

    /// Value: protects=the tier-token wire invariant when the scene table
    /// names a tier the configuration never defines; fails_when=the
    /// fallback writes the tier token (or an empty model) into the child's
    /// model slot instead of the boot model; why_new=the `unwrap_or_else`
    /// branch of the tier lookup is uncovered, so a tier-token leak on that
    /// path would ship green; seam=none; no_claim=the token kill on a
    /// configured tier (proven above) and the booted TUI (pty 15).
    #[test]
    fn an_unconfigured_scene_tier_falls_back_to_the_boot_model_never_the_token() {
        let mut snap = snapshot();
        // A scene table may name a tier the config never defines.
        snap.default_tier = "fast".to_string();
        snap.tiers.retain(|(t, _)| t != "fast");
        let r = resolve(None, &snap);
        assert_eq!(r.resolved_tier.as_deref(), Some("fast"));
        assert!(
            r.endpoint_ok,
            "the boot tuple is trivially the same endpoint as itself"
        );
        assert_eq!(
            r.action,
            SpawnModel::Override("boot-main".to_string()),
            "the boot model is written, never the tier token"
        );
        assert_ne!(
            r.action,
            SpawnModel::Override("fast".to_string()),
            "a tier token must never reach the wire"
        );
    }

    /// Value: protects=the cross-endpoint notice is never silent and never
    /// doubled (review A-3); fails_when=the notice is appended to the tool
    /// result even though the user-event channel carried it (a duplicate
    /// transcript line) or the channel delivery is dropped; why_new=the
    /// tool-level test only covers the dead-channel fallback, so the live
    /// channel path has no assertion on either half; seam=none; no_claim=the
    /// channel-dead fallback (proven above) and the booted TUI (pty 15).
    #[tokio::test]
    async fn a_live_channel_carries_the_notice_once_and_the_result_stays_clean() {
        let mut snap = snapshot();
        snap.tiers[2].1 = tuple("adv-real", "https://other", "k2", "openai");
        let (seen, tool) = wrapped(snap);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = ToolContext::for_test();
        ctx.user_event_tx = tx;

        let out = tool
            .call(
                &json!({"task_name": "t", "task": "do it", "model": "advanced"}),
                &ctx,
            )
            .await
            .expect("call");

        assert!(
            seen_args(&seen).get("model").is_none(),
            "no override may be written"
        );
        assert!(
            !out.iter().any(|c| matches!(c, Content::Text { text }
                if text.contains("no override written"))),
            "the notice was delivered on the channel — the result must not repeat it: {out:?}"
        );
        match rx.try_recv() {
            Ok(UserEvent::Notice { kind, source, text }) => {
                assert_eq!(kind, NoticeKind::Info);
                assert_eq!(source, "spawn-model-route");
                assert!(
                    text.contains("no override written"),
                    "the notice says what happened: {text}"
                );
            }
            other => panic!("expected the notice on the channel, got {other:?}"),
        }
    }

    #[test]
    fn wrap_multi_agent_tools_gates_only_model_bearing_tools() {
        struct PlainTool;
        #[async_trait::async_trait]
        impl Tool for PlainTool {
            fn name(&self) -> &'static str {
                "list_agents"
            }
            fn description(&self) -> &'static str {
                "plain"
            }
            fn schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            async fn call(&self, _args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let seen = Arc::new(StdMutex::new(None));
        let model_bearer: Arc<dyn Tool> = Arc::new(CaptureTool { seen, out: vec![] });
        let plain: Arc<dyn Tool> = Arc::new(PlainTool);
        let dir = std::env::temp_dir().join(format!(
            "phimint-route-audit-factory-{}",
            std::process::id()
        ));
        let tools = wrap_multi_agent_tools(
            vec![model_bearer, plain],
            Arc::new(FakeSource(snapshot())),
            AuditWriter::new(dir),
        );
        assert_eq!(tools.len(), 2);
        // The model bearer is wrapped: its schema now teaches the resolver's
        // contract. The plain tool passes through untouched.
        let desc = tools[0]
            .schema()
            .pointer("/properties/model/description")
            .and_then(Value::as_str)
            .expect("rewritten description")
            .to_string();
        assert_eq!(desc, MODEL_ARG_CONTRACT);
        assert_eq!(tools[0].name(), "spawn_agent", "name passes through");
        assert_eq!(tools[1].name(), "list_agents");
        assert!(
            tools[1].schema().pointer("/properties/model").is_none(),
            "a tool without a model arg is never rewritten"
        );
    }
}
