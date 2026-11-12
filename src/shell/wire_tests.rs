//! Tests for the product wire's seams (T7c red-list relocations).
//!
//! Two seams, both driven through the REAL closures the shell calls — the
//! old copies in `src/ui/` re-implemented the dispatch inline, which is
//! exactly the drift the rewrite kills:
//!
//! - the skill bridge's scope dispatch (the five skill-turn-log contract
//!   tests: raw-command submission, ephemeral bodies, activation
//!   persistence, idempotent re-trigger, catalog-refresh coexistence);
//! - the resume seam's enumerate closure (the picker's listing rules).

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;
use crate::approval::build_live_approval;
use crate::shell::skills::{refresh_catalog, render_catalog};
use phi_agent::{resolve_session, save_turn_log};
use phi_agent_tui::InteractSlot;

/// A wire with a throwaway interaction gate (the skill/resume seams never
/// touch it, but [`Wire::new`] takes the live handle).
fn test_wire(resolver: SkillResolver) -> Arc<Wire> {
    let telemetry = Arc::new(SkillTelemetry::new());
    let activity = Arc::new(crate::tools::canvas::CanvasActivity::new());
    let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
    let interact_slot = Arc::new(InteractSlot::new(gate));
    Wire::new(Arc::new(resolver), telemetry, activity, interact_slot)
}

// ── Skill routing (skill-lifetime v3) ───────────────────────────────────

/// Fixture with one session-scope skill (frontmatter default) and one
/// turn-scope skill (`scope: turn`).
fn fixture_skill_dir(tmp: &Path) -> PathBuf {
    let session_dir = tmp.join("skills").join("catalog-skill");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("SKILL.md"),
        "---\nname: catalog-skill\ndescription: guard fixture skill\n\
         user-invocable: true\n---\n\nEPHEMERAL-FIXTURE-BODY skill instructions for: $ARGUMENTS",
    )
    .unwrap();
    let turn_dir = tmp.join("skills").join("oneshot-skill");
    std::fs::create_dir_all(&turn_dir).unwrap();
    std::fs::write(
        turn_dir.join("SKILL.md"),
        "---\nname: oneshot-skill\ndescription: one-shot fixture skill\n\
         user-invocable: true\nscope: turn\n---\n\nONESHOT-FIXTURE-BODY instructions",
    )
    .unwrap();
    tmp.join("skills")
}

/// Spec §Verification (skill-lifetime v3): a session-scope skill turn
/// (frontmatter default) submits the RAW command — `$ARGUMENTS` reach the
/// model through it — while the active list gains the entry and the
/// composed prompt gains the Active Skills section. `turn_NNN.jsonl`'s
/// `user_input` stays the raw command, never the body.
#[test]
fn session_scope_turn_keeps_raw_command_and_activates_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = fixture_skill_dir(tmp.path());
    let wire = test_wire(SkillResolver::from_dirs(&[skill_dir]));
    let session_dir = tmp.path().join("sessions").join("skill-session-activate");
    std::fs::create_dir_all(&session_dir).unwrap();
    wire.skill.lock().unwrap().session_dir = Some(session_dir.clone());
    let bridge = wire.skill_bridge();

    let raw = "/catalog-skill fix the parser";
    let outcome = (bridge.route)(raw);
    let RouteOutcome::SessionBaked {
        input,
        prompt,
        skill,
        warnings,
    } = outcome
    else {
        panic!("frontmatter default must route as SessionBaked");
    };
    assert_eq!(
        input, raw,
        "session-scope turn must submit the raw command, not the body"
    );
    assert!(
        !input.contains("EPHEMERAL-FIXTURE-BODY"),
        "raw command must not leak the body"
    );
    assert_eq!(
        skill, "catalog-skill",
        "canonical name straight from SKILL.md"
    );
    assert!(
        warnings.is_empty(),
        "a healthy route warns nothing: {warnings:?}"
    );

    // Activation side effects: the section rides on the pristine base.
    let base = agent::compose_system_prompt(&wire.resolver);
    assert!(
        prompt.starts_with(&base),
        "base prompt must be preserved verbatim"
    );
    assert!(
        prompt.contains("## Active Skills") && prompt.contains("EPHEMERAL-FIXTURE-BODY"),
        "composed prompt must contain the baked section"
    );

    // Persisted for resume, round-trip through the store.
    let expected = wire
        .resolver
        .resolve_with_meta(raw)
        .expect("fixture resolves");
    assert_eq!(
        load_active_skills(&session_dir),
        vec![ActiveSkillEntry {
            name: expected.name.clone(),
            body: expected.body.clone(),
        }],
        "persisted list must round-trip"
    );

    // The turn log's `user_input` is the raw command the route handed back
    // (the shell logs that verbatim) — never the body.
    let session_ctx = resolve_session(Some("skill-session-activate"), tmp.path()).unwrap();
    save_turn_log(&session_ctx, 1, &[], &input).unwrap();
    let log = std::fs::read_to_string(session_ctx.turn_path(1)).unwrap();
    let meta: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(
        meta["user_input"], "/catalog-skill fix the parser",
        "turn JSONL user_input must be the raw command"
    );
    assert!(
        !log.contains("EPHEMERAL-FIXTURE-BODY"),
        "turn JSONL must never contain the skill body: {log}"
    );
}

/// Turn-scope skills (`scope: turn`) keep the v2 ephemeral contract: the
/// resolved body is what gets submitted (as ephemeral input), and the
/// active list is untouched.
#[test]
fn turn_scope_turn_submits_body_and_skips_activation() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = fixture_skill_dir(tmp.path());
    let wire = test_wire(SkillResolver::from_dirs(&[skill_dir]));
    let bridge = wire.skill_bridge();

    let outcome = (bridge.route)("/oneshot-skill");
    let RouteOutcome::Ephemeral { input: body } = outcome else {
        panic!("scope: turn must route as Ephemeral");
    };
    let resolved = wire
        .resolver
        .resolve_with_meta("/oneshot-skill")
        .expect("fixture must resolve (fixture mismatch)");
    assert_eq!(resolved.scope, SkillScope::Turn, "scope: turn must parse");
    assert_eq!(
        body, resolved.body,
        "turn-scope turn must submit the resolved body (ephemeral path)"
    );
    assert!(
        !body.is_empty() && body != "/oneshot-skill",
        "body must differ from the raw command (fixture mismatch)"
    );
    // No activation: the dispatcher's Session branch never runs for Turn.
    assert!(
        wire.skill.lock().unwrap().active.is_empty(),
        "a turn-scope skill must not enter the active list"
    );
}

/// Non-skill turns behave exactly as before: no resolve transform, no
/// ephemeral dispatch, no activation, log records the verbatim input.
#[test]
fn turn_log_records_plain_input_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let wire = test_wire(SkillResolver::from_dirs(&[tmp.path().join("nonexistent")]));
    let bridge = wire.skill_bridge();

    let input = "fix the flaky test in app.rs";
    let outcome = (bridge.route)(input);
    let RouteOutcome::Turn { input: submitted } = outcome else {
        panic!("plain input must route as Turn");
    };
    assert_eq!(submitted, input, "plain input must pass through untouched");

    let session_ctx = resolve_session(Some("skill-ephemeral-plain"), tmp.path()).unwrap();
    save_turn_log(&session_ctx, 1, &[], &submitted).unwrap();
    let log = std::fs::read_to_string(session_ctx.turn_path(1)).unwrap();
    let meta: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(meta["user_input"], "fix the flaky test in app.rs");
}

/// Verification #3: re-triggering a session-scope skill (with different
/// `$ARGUMENTS`, hence a different resolved body) must replace the entry
/// in place — exactly one entry per name, exactly one section copy.
#[test]
fn repeated_session_activation_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = fixture_skill_dir(tmp.path());
    let wire = test_wire(SkillResolver::from_dirs(&[skill_dir]));
    let session_dir = tmp.path().join("sessions").join("skill-session-repeat");
    std::fs::create_dir_all(&session_dir).unwrap();
    wire.skill.lock().unwrap().session_dir = Some(session_dir.clone());
    let bridge = wire.skill_bridge();

    // Two triggers, different args → different bodies ($ARGUMENTS baked in).
    let first_body = wire
        .resolver
        .resolve_with_meta("/catalog-skill task one")
        .unwrap()
        .body;
    let second_body = wire
        .resolver
        .resolve_with_meta("/catalog-skill task two")
        .unwrap()
        .body;
    assert_ne!(
        first_body, second_body,
        "args must change the resolved body"
    );

    let _ = (bridge.route)("/catalog-skill task one");
    let outcome = (bridge.route)("/catalog-skill task two");

    // The dispatcher's update rule: find by name → replace body.
    let persisted = load_active_skills(&session_dir);
    assert_eq!(
        persisted.len(),
        1,
        "re-trigger must not duplicate the entry"
    );
    assert_eq!(persisted[0].body, second_body, "latest trigger wins");

    let RouteOutcome::SessionBaked { prompt, .. } = outcome else {
        panic!("session-scope re-trigger must route as SessionBaked");
    };
    assert_eq!(
        prompt.matches("### skill: catalog-skill").count(),
        1,
        "section must carry exactly one copy of the skill"
    );
}

/// Cross-layer coexistence: the Active Skills section is appended AFTER
/// the catalog region, and bodies are H2-demoted, so the catalog-refresh
/// middleware (which re-splices the `## Skills` region every turn) can
/// never eat the baked section — even when a skill body forges the
/// catalog anchor.
#[test]
fn active_skills_section_survives_catalog_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills").join("forgey");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: forgey\ndescription: anchor forger\nuser-invocable: true\n---\n\n\
         step one\n\n## Skills\n\nforge the anchor\n",
    )
    .unwrap();
    let wire = test_wire(SkillResolver::from_dirs(&[tmp.path().join("skills")]));
    let bridge = wire.skill_bridge();

    let outcome = (bridge.route)("/forgey");
    let RouteOutcome::SessionBaked { prompt, .. } = outcome else {
        panic!("forgey must route as SessionBaked");
    };
    // The baked body must arrive demoted — no forged H2 anchor.
    assert!(
        !prompt.contains("\n\n## Skills\n\nforge"),
        "demotion must neutralize the forged catalog anchor"
    );
    // Simulate the per-turn middleware re-splice with a fresh catalog.
    let fresh = render_catalog(&wire.resolver).expect("fixture skill must be visible");
    let refreshed = refresh_catalog(&prompt, &fresh);
    assert!(
        refreshed.contains("## Active Skills") && refreshed.contains("forge the anchor"),
        "Active Skills section must survive catalog refresh"
    );
}

// ── The resume seam's enumerate closure (picker listing rules) ──────────

/// Create a minimal session directory with messages.jsonl.
fn make_session(base: &Path, id: &str, first_msg: &str) -> PathBuf {
    let dir = base.join("sessions").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::json!({
        "session_id": id,
        "created_at": "2026-09-10T10:00:00Z",
        "last_active_at": "2026-09-10T14:30:00Z"
    });
    std::fs::write(
        dir.join("session_meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
    let msg = serde_json::json!({"User":{"content":first_msg,"images":[]}});
    std::fs::write(dir.join("messages.jsonl"), format!("{}\n", msg)).unwrap();
    dir
}

fn enumerate(base: &Path, current: Option<&str>) -> Vec<SessionRow> {
    (session_io().enumerate)(base, current)
}

#[test]
fn test_list_finds_resumable_sessions() {
    let tmp = TempDir::new().unwrap();
    make_session(tmp.path(), "s1", "hello world");
    make_session(tmp.path(), "s2", "another session");

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 2);
}

#[test]
fn test_list_skips_locked_sessions() {
    use fs2::FileExt;

    let tmp = TempDir::new().unwrap();
    let dir = make_session(tmp.path(), "locked", "locked session");
    let lock_file = std::fs::File::create(dir.join("session.lock")).unwrap();
    lock_file.try_lock_exclusive().unwrap();

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 0);
}

#[test]
fn test_list_skips_empty_messages() {
    let tmp = TempDir::new().unwrap();
    let dir = make_session(tmp.path(), "empty", "content");
    let sys = serde_json::json!({"System":{"content":"system prompt","ephemeral":false}});
    std::fs::write(dir.join("messages.jsonl"), format!("{}\n", sys)).unwrap();

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 0);
}

#[test]
fn test_list_skips_no_messages_jsonl() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("sessions").join("legacy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session_meta.json"),
        r#"{"session_id":"legacy","created_at":"2026-09-10T10:00:00Z","last_active_at":"2026-09-10T10:00:00Z"}"#,
    )
    .unwrap();

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 0);
}

#[test]
fn test_list_skips_current_session() {
    let tmp = TempDir::new().unwrap();
    make_session(tmp.path(), "current", "skip me");

    let rows = enumerate(tmp.path(), Some("current"));
    assert_eq!(rows.len(), 0);
}

#[test]
fn test_sort_order_newest_first() {
    let tmp = TempDir::new().unwrap();
    make_session(tmp.path(), "old", "old session");
    std::thread::sleep(std::time::Duration::from_millis(50));
    make_session(tmp.path(), "new", "new session");

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].title, "new session");
    assert_eq!(rows[1].title, "old session");
}

/// End-to-end over real session directories: one recorded in this folder
/// shows up in the default view, one recorded elsewhere does not.
#[test]
fn workspace_scope_filters_real_sessions() {
    let tmp = TempDir::new().unwrap();
    let here = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

    for (id, cwd) in [
        ("mine", here.to_string_lossy().into_owned()),
        ("theirs", "/somewhere/else".into()),
    ] {
        let dir = make_session(tmp.path(), id, id);
        let meta = serde_json::json!({
            "session_id": id,
            "last_active_at": "2026-09-10T14:30:00Z",
            "cwd": cwd,
        });
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
    }

    let rows = enumerate(tmp.path(), None);
    assert_eq!(rows.len(), 2);

    let shown: Vec<&SessionRow> = rows.iter().filter(|r| r.in_workspace).collect();
    assert_eq!(shown.len(), 1);
    assert!(
        shown[0].session_dir.ends_with("mine"),
        "the default view must show the session born in this folder: {:?}",
        shown[0].session_dir
    );
}

// ── Vision capability probe (the paste pipeline's gate) ─────────────────

/// A store whose live main tier is `model` on `base_url` — the facts the
/// vision probe reads.
fn vision_store(model: &str, base_url: &str) -> Arc<AsyncMutex<ModelStore>> {
    let provider = phi_agent::create_provider(&phi_agent::llm_trait::LlmConfig {
        model: model.to_string(),
        base_url: base_url.to_string(),
        api_key: "sk-test".to_string(),
        protocol: Some(phi_agent::llm_trait::Protocol::OpenAi),
        ..Default::default()
    })
    .expect("the boot provider builds");
    let config = crate::model::config::ModelConfig {
        base_url: Some(base_url.to_string()),
        api_key: Some("sk-test".to_string()),
        protocol: Some("openai".to_string()),
        main: crate::model::config::TierConfig::Simple(model.to_string()),
        lite: None,
        advanced: None,
        model: None,
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    };
    Arc::new(AsyncMutex::new(ModelStore::new(config, provider)))
}

/// protects: the paste gate's vision answer is a REGISTRY FACT of the live
/// main model — a vision-capable model answers `Some(true)`, a known
/// non-vision model `Some(false)`, and a model the registry has never heard
/// of answers `None` (never the fallback profile's guess, and never a
/// refusal the paste pipeline would have to guess around). fails_when: an
/// unknown model borrows the default profile's capabilities, or the probe
/// answers from anything but the live main tier. no_claim: what the shell
/// does with the answer (phi-agent-tui owns the gate), or the provider's
/// real capabilities — the registry's recorded facts stand in for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_vision_probe_answers_the_registry_fact_of_the_live_model() {
    let capable = vision_store("mimo-v2.5-pro", "https://api.example-mimo.com/v1");
    assert_eq!(
        vision_probe(Arc::clone(&capable))(),
        Some(true),
        "a vision-capable model answers Some(true)"
    );

    let text_only = vision_store("deepseek-chat", "https://api.deepseek.com/v1");
    assert_eq!(
        vision_probe(Arc::clone(&text_only))(),
        Some(false),
        "a known text-only model answers Some(false)"
    );

    let unknown = vision_store("some-unknown-model", "https://api.example.com/v1");
    assert_eq!(
        vision_probe(Arc::clone(&unknown))(),
        None,
        "an unknown model answers None — never the fallback profile's guess"
    );
}

/// protects: the probe never blocks on the store lock — a `/model` swap
/// holding the lock answers `None` (the paste rides on and the engine's
/// image budget judges it) instead of deadlocking the paste pipeline.
/// fails_when: the probe blocks the composer until the swap lands, or it
/// answers a stale model while the lock is held. no_claim: which model the
/// swap was heading for — the store is locked by the test itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_locked_store_answers_the_probe_without_blocking() {
    let store = vision_store("mimo-v2.5-pro", "https://api.example-mimo.com/v1");
    let probe = vision_probe(Arc::clone(&store));
    let held = store.lock().await;
    assert_eq!(
        probe(),
        None,
        "the lock is held: the probe answers None instead of waiting"
    );
    drop(held);
    assert_eq!(
        probe(),
        Some(true),
        "the lock is free again: the probe answers the live fact"
    );
}

/// protects: mid-swap — the engine serving a different model than the tier
/// table names — the probe steps aside (`None`) instead of answering the
/// registry fact of a model that is not serving; once the two agree again,
/// the fact returns. fails_when: the probe answers for the tier-table model
/// while the serving resolver names another. no_claim: which model the swap
/// is heading for, or when it lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_serving_model_that_differs_from_the_tier_table_answers_none() {
    let store = vision_store("mimo-v2.5-pro", "https://api.example-mimo.com/v1");
    let probe = vision_probe(Arc::clone(&store));
    assert_eq!(probe(), Some(true), "in agreement: the live fact");

    store
        .lock()
        .await
        .bind_serving_name(Arc::new(|| Some("the-old-model".to_string())));
    assert_eq!(
        probe(),
        None,
        "the engine serves another model: no fact belongs to the pick yet"
    );

    store
        .lock()
        .await
        .bind_serving_name(Arc::new(|| Some("mimo-v2.5-pro".to_string())));
    assert_eq!(probe(), Some(true), "in agreement again: the fact returns");
}

// ── Runtime notices ─────────────────────────────────────────────────────

/// protects: a rejected request surfaces as ONE system line in the
/// transcript — the probe hands the notice to the shell, which pushes it as
/// a system row (never a popup over the composer). fails_when: the probe
/// stays silent while the endpoint rejects, or it repeats the same line on
/// every tick. no_claim: the transcript rendering (phi-tui pushes it).
#[test]
fn the_notice_probe_surfaces_an_auth_rejection_once() {
    let _guard = crate::model::auth_watch::watch_test_lock().blocking_lock();
    crate::model::auth_watch::reset();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    crate::model::auth_watch::set_context(crate::model::auth_watch::AuthContext {
        config_path: path.clone(),
        masked_key: "sk-a…9x2".to_string(),
        config_base_url: "https://api.openai.com/v1".to_string(),
        key_source: None,
    });
    crate::model::auth_watch::record_failure(
        &phi_agent::llm_trait::LlmError::api(401, "invalid api key"),
        "gpt-5.4-mini",
        "https://api.openai.com/v1",
        // The key the request travelled under (masks `sk-a…9x2`, the
        // context's own mask).
        "sk-a0000009x2",
    );

    let probe = notice_probe();
    let lines = probe();
    assert!(
        lines.iter().any(|l| l.contains("rejected this request")),
        "the rejection reaches the transcript: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("sk-a…9x2")),
        "the notice carries the masked key: {lines:?}"
    );
    assert!(
        probe().is_empty(),
        "the notice is not repeated on the next tick"
    );
}
