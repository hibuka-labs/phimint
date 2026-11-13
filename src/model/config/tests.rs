//! `ModelConfig` unit tests: tier resolution, pointer semantics, validation
//! and the UI/code_exec parse surface. Split out of `config.rs` to respect the
//! 1000-line file rule (same layout as `src/approval/tests.rs`).

use super::*;

#[test]
fn tier_config_simple() {
    let config = TierConfig::Simple("gpt-4".to_string());
    assert_eq!(config.model(), "gpt-4");
    let (model, url, key, proto) = config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
    assert_eq!(model, "gpt-4");
    assert_eq!(url, "https://api.openai.com/v1");
    assert_eq!(key, "sk-xxx");
    assert_eq!(proto, "openai");
}

#[test]
fn tier_config_full() {
    let config = TierConfig::Full {
        model: "deepseek-chat".to_string(),
        base_url: Some("https://api.deepseek.com/v1".to_string()),
        api_key: Some("sk-deepseek".to_string()),
        protocol: Some("openai".to_string()),
    };
    assert_eq!(config.model(), "deepseek-chat");
    let (model, url, key, proto) = config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
    assert_eq!(model, "deepseek-chat");
    assert_eq!(url, "https://api.deepseek.com/v1");
    assert_eq!(key, "sk-deepseek");
    assert_eq!(proto, "openai");
}

#[test]
fn tier_config_full_inherits() {
    let config = TierConfig::Full {
        model: "gpt-4".to_string(),
        base_url: None,
        api_key: None,
        protocol: None,
    };
    let (model, url, key, proto) = config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
    assert_eq!(model, "gpt-4");
    assert_eq!(url, "https://api.openai.com/v1");
    assert_eq!(key, "sk-xxx");
    assert_eq!(proto, "openai");
}

#[test]
fn model_config_simple_all_same_provider() {
    let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-xxx",
            "protocol": "openai",
            "main": "gpt-5.4-mini",
            "lite": "gpt-4o-mini",
            "advanced": "o1-preview"
        }"#;

    let config: ModelConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.main.model(), "gpt-5.4-mini");
    assert_eq!(config.lite.as_ref().unwrap().model(), "gpt-4o-mini");
    assert_eq!(config.advanced.as_ref().unwrap().model(), "o1-preview");

    let (model, url, _, _) = config.tier_config("main").unwrap();
    assert_eq!(model, "gpt-5.4-mini");
    assert_eq!(url, "https://api.openai.com/v1");

    let (model, url, _, _) = config.tier_config("lite").unwrap();
    assert_eq!(model, "gpt-4o-mini");
    assert_eq!(url, "https://api.openai.com/v1");
}

#[test]
fn model_config_all_different_providers() {
    let json = r#"{
            "main": { "model": "gpt-5.4-mini", "base_url": "https://api.openai.com/v1", "api_key": "sk-openai", "protocol": "openai" },
            "lite": { "model": "deepseek-chat", "base_url": "https://api.deepseek.com/v1", "api_key": "sk-deepseek", "protocol": "openai" },
            "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant", "protocol": "anthropic" }
        }"#;

    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (model, url, _, _) = config.tier_config("main").unwrap();
    assert_eq!(model, "gpt-5.4-mini");
    assert_eq!(url, "https://api.openai.com/v1");

    let (model, url, _, _) = config.tier_config("lite").unwrap();
    assert_eq!(model, "deepseek-chat");
    assert_eq!(url, "https://api.deepseek.com/v1");

    let (model, url, _, _) = config.tier_config("advanced").unwrap();
    assert_eq!(model, "claude-opus-4");
    assert_eq!(url, "https://api.anthropic.com");
}

#[test]
fn model_config_mixed() {
    let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-xxx",
            "protocol": "openai",
            "main": "gpt-5.4-mini",
            "lite": "gpt-4o-mini",
            "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant", "protocol": "anthropic" }
        }"#;

    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (model, url, _, _) = config.tier_config("main").unwrap();
    assert_eq!(model, "gpt-5.4-mini");
    assert_eq!(url, "https://api.openai.com/v1");

    let (model, url, _, _) = config.tier_config("lite").unwrap();
    assert_eq!(model, "gpt-4o-mini");
    assert_eq!(url, "https://api.openai.com/v1");

    let (model, url, _, _) = config.tier_config("advanced").unwrap();
    assert_eq!(model, "claude-opus-4");
    assert_eq!(url, "https://api.anthropic.com");
}

#[test]
fn model_config_fallback_to_main() {
    let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "main": "gpt-4"
        }"#;

    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (model, _, _, _) = config.tier_config("lite").unwrap();
    assert_eq!(model, "gpt-4"); // falls back to main
}

/// protects: THE one resolution rule under a selection pointer — the
/// effective main is the pointer resolved over the top-level defaults,
/// unconfigured lite/advanced follow it, configured ones keep their own
/// definitions, and `configured_main` still reports the configured `main`
/// (what「clear selection」rolls back to). fails_when: the pointer is
/// ignored, a configured tier follows it, or the configured main is
/// lost. no_claim: what a pointer value should be.
#[test]
fn the_pointer_resolves_the_effective_main_and_unconfigured_tiers_follow() {
    let json = r#"{
            "base_url": "https://api.example.com/v1",
            "api_key": "sk-default",
            "protocol": "openai",
            "main": "configured-main",
            "lite": "configured-lite",
            "model": { "model": "pointer-model", "base_url": "https://pointer.example.com/v1" }
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();

    // Effective main = pointer delta over the top-level defaults.
    let (model, url, key, proto) = config.tier_config("main").unwrap();
    assert_eq!(model, "pointer-model");
    assert_eq!(url, "https://pointer.example.com/v1");
    assert_eq!(key, "sk-default", "unset delta fields inherit the defaults");
    assert_eq!(proto, "openai");

    // A configured lite keeps its own definition; the unconfigured
    // advanced follows the effective main.
    assert_eq!(config.tier_config("lite").unwrap().0, "configured-lite");
    assert_eq!(config.tier_config("advanced").unwrap().0, "pointer-model");

    // The configured definition survives for the clear to roll back to.
    assert_eq!(config.configured_main().0, "configured-main");
    assert_eq!(config.tier_def("main").unwrap().model(), "configured-main");
    assert_eq!(
        config.main.model(),
        "configured-main",
        "the field never moves"
    );
}

/// protects: the `model` pointer parses leniently — a `null` is silent
/// and a malformed value warns and reads as no pointer, so a weird
/// config value can never take the whole file down. fails_when: an
/// unparseable pointer value fails the file parse or half-activates.
/// no_claim: what the warning says beyond warning.
#[test]
fn a_malformed_pointer_reads_as_no_pointer_and_the_file_still_lands() {
    for value in ["null", "123", "[]", "{\"nonsense\": true}"] {
        let json = format!(
            r#"{{"base_url": "https://api.example.com/v1", "main": "configured-main", "model": {value}}}"#
        );
        let config: ModelConfig = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("the file parses with model = {value}: {e}"));
        assert!(
            config.model.is_none(),
            "model = {value} reads as no pointer: {:?}",
            config.model
        );
        assert_eq!(
            config.tier_config("main").unwrap().0,
            "configured-main",
            "model = {value}: no pointer, configured main rules"
        );
    }
}

/// protects: the CLI precedence chain — `--model` writes the in-memory
/// `model` slot and outranks a file pointer, which outranks the
/// configured `main`; a bare call leaves the slot alone. fails_when:
/// the CLI is ignored under a pointer or a pointer outranks it.
/// no_claim: persistence (the CLI override is memory-only by contract —
/// nothing in `apply_cli_model` can touch a file).
#[test]
fn cli_model_outranks_the_pointer_in_memory_only() {
    let json = r#"{
            "base_url": "https://api.example.com/v1",
            "main": "configured-main",
            "model": "pointer-model"
        }"#;
    let mut config: ModelConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.tier_config("main").unwrap().0, "pointer-model");

    config.apply_cli_model(Some("cli-model"));
    assert_eq!(config.tier_config("main").unwrap().0, "cli-model");
    assert_eq!(
        config.main.model(),
        "configured-main",
        "the configured definition is preserved (the picker keeps its row)"
    );

    // A bare call is a no-op: the selection stands.
    config.apply_cli_model(None);
    assert_eq!(config.tier_config("main").unwrap().0, "cli-model");
}

/// protects: a name-only selection never drops the configured main's
/// endpoint — `--model x` (and a hand `model` pointer) over a config whose
/// endpoint lives in `main`'s Full fields validates and resolves onto main's
/// endpoint (review findings p2/outside-voice#2 and p2/red-team#1).
/// fails_when: the boot refuses such a config with "base_url must be set at
/// top level or in main tier", or the selection resolves onto an empty
/// endpoint. no_claim: which endpoint a Full pointer delta carries.
#[test]
fn a_name_only_selection_keeps_the_configured_main_endpoint() {
    let json = r#"{
            "main": { "model": "configured-main", "base_url": "https://main.example.com/v1", "api_key": "sk-main" }
        }"#;
    let mut config: ModelConfig = serde_json::from_str(json).unwrap();
    config.apply_cli_model(Some("gpt-5"));
    config
        .validate()
        .expect("boot must not refuse a config the error text blesses");
    let (model, url, key, _) = config.tier_config("main").unwrap();
    assert_eq!(model, "gpt-5", "the selection names the model");
    assert_eq!(
        url, "https://main.example.com/v1",
        "a name-only selection inherits the configured main's endpoint"
    );
    assert_eq!(key, "sk-main");
}

/// protects: a Simple lite/advanced tier over a Full main rides main's
/// endpoint through the same chain — `--lite_model`/`--advanced_model` and
/// a hand `"lite": "name"` are the same Simple shape (review finding
/// p2/red-team#1). fails_when: the lite tier resolves onto an empty
/// endpoint and judge/spawn provider creation falls over.
#[test]
fn a_simple_tier_over_a_full_main_rides_the_main_endpoint() {
    let json = r#"{
            "main": { "model": "m", "base_url": "https://api.x", "api_key": "sk" },
            "lite": "lite-model"
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (model, url, key, _) = config.tier_config("lite").unwrap();
    assert_eq!(model, "lite-model");
    assert_eq!(url, "https://api.x");
    assert_eq!(key, "sk");
}

/// protects: the documented delta semantics where the top level IS set — a
/// Simple pointer still resolves to the top-level endpoint (regression pin
/// for the chain's middle link). fails_when: the configured-main anchor
/// swallows a non-empty top-level default.
#[test]
fn a_simple_pointer_over_a_set_top_level_still_rides_the_top_level() {
    let json = r#"{
            "base_url": "https://top.example.com/v1",
            "api_key": "sk-top",
            "main": { "model": "m", "base_url": "https://main.example.com/v1", "api_key": "sk-main" },
            "model": "pointer-model"
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (_, url, key, _) = config.tier_config("main").unwrap();
    assert_eq!(url, "https://top.example.com/v1", "top-level wins when set");
    assert_eq!(key, "sk-top");
}

/// protects: the anchor's pinned fields belong to the anchor's ENDPOINT — a
/// tier pointing at another vendor's URL never inherits the configured
/// `main`'s api_key or protocol (review finding v11/adversarial-native#4).
/// fails_when: main's `sk-ant-secret` is sent to the deepseek host (the judge
/// rides exactly this tier), or main's anthropic wire format is spoken to it.
/// no_claim: what the provider answers with a blank key (a 401, loud).
#[test]
fn a_foreign_endpoint_never_inherits_the_main_tiers_pinned_fields() {
    let json = r#"{
            "main": { "model": "m", "base_url": "https://api.anthropic.com", "api_key": "sk-ant-secret", "protocol": "anthropic" },
            "lite": { "model": "l", "base_url": "https://api.deepseek.com/v1" }
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (_, url, key, proto) = config.tier_config("lite").unwrap();
    assert_eq!(url, "https://api.deepseek.com/v1");
    assert_ne!(
        key, "sk-ant-secret",
        "the main tier's pinned key must not travel to a foreign endpoint"
    );
    assert_ne!(
        proto, "anthropic",
        "the main tier's wire protocol must not apply to a foreign endpoint"
    );
}

/// protects: the anchor rule is about endpoints, not tiers — a tier on the
/// anchor's own endpoint is the "same provider, different model" shape and
/// still inherits the pinned key.
#[test]
fn a_tier_on_the_anchor_endpoint_still_inherits_the_pinned_key() {
    let json = r#"{
            "main": { "model": "m", "base_url": "https://api.anthropic.com", "api_key": "sk-ant-secret" },
            "lite": { "model": "l", "base_url": "https://api.anthropic.com" }
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (_, url, key, _) = config.tier_config("lite").unwrap();
    assert_eq!(url, "https://api.anthropic.com");
    assert_eq!(key, "sk-ant-secret");
}

/// protects: the same-endpoint check tolerates spelling differences (host
/// case, a trailing slash) — a tier on the anchor's endpoint keeps the
/// pinned key instead of 401-ing where it authenticated before (review
/// finding fix-check#2). fails_when: the exact-string compare drops the key
/// for `…/` spellings. no_claim: path case (paths stay case-sensitive).
#[test]
fn a_spelling_variant_of_the_anchor_endpoint_still_inherits_the_pinned_key() {
    let json = r#"{
            "main": { "model": "m", "base_url": "https://API.Anthropic.com", "api_key": "sk-ant-secret" },
            "lite": { "model": "l", "base_url": "https://api.anthropic.com/" }
        }"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    let (_, url, key, _) = config.tier_config("lite").unwrap();
    assert_eq!(url, "https://api.anthropic.com/");
    assert_eq!(
        key, "sk-ant-secret",
        "a spelling variant is not a foreign endpoint"
    );
}

#[test]
fn validate_empty_model_fails() {
    let config = ModelConfig {
        base_url: Some("http://localhost".to_string()),
        api_key: None,
        protocol: None,
        main: TierConfig::Simple("".to_string()),
        lite: None,
        advanced: None,
        model: None,
        scene_tiers: None,
        update: UpdateConfig::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    };
    assert!(config.validate().is_err());
}

#[test]
fn ui_popup_defaults_to_the_widget_default_style() {
    let cfg: ModelConfig = json5::from_str(r#"{"main": "gpt-4"}"#).unwrap();
    assert_eq!(
        cfg.ui.popup.to_style().unwrap(),
        phi_tui::popup_list::PopupStyle::default()
    );
}

#[test]
fn ui_popup_parses_fill_and_fixed_width() {
    let fill: ModelConfig =
        json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"fill","height":5}}}"#).unwrap();
    let style = fill.ui.popup.to_style().unwrap();
    assert_eq!(style.width, phi_tui::popup_list::WidthSpec::Fill);
    assert_eq!(style.height, 5);

    let fixed: ModelConfig = json5::from_str(
        r#"{"main":"gpt-4","ui":{"popup":{"width":64,"frame":true,"separator":false}}}"#,
    )
    .unwrap();
    let style = fixed.ui.popup.to_style().unwrap();
    assert_eq!(style.width, phi_tui::popup_list::WidthSpec::Fixed(64));
    assert!(style.frame);
    assert!(!style.separator);
}

#[test]
fn ui_tips_defaults() {
    let cfg: ModelConfig = json5::from_str(r#"{"main": "gpt-4"}"#).unwrap();
    assert!(cfg.ui.tips.enabled);
    assert_eq!(cfg.ui.tips.refresh_interval_secs, 86_400);
    assert_eq!(cfg.ui.tips.endpoints.len(), 2);
}

#[test]
fn ui_tips_parses_overrides() {
    let cfg: ModelConfig = json5::from_str(
        r#"{"main":"gpt-4","ui":{"tips":{
                "enabled": false,
                "refresh_interval_secs": 3600,
                "endpoints": ["https://example.com/tips.json"]
            }}}"#,
    )
    .unwrap();
    assert!(!cfg.ui.tips.enabled);
    assert_eq!(cfg.ui.tips.refresh_interval_secs, 3600);
    assert_eq!(
        cfg.ui.tips.endpoints,
        vec!["https://example.com/tips.json".to_string()]
    );
}

#[test]
fn ui_popup_rejects_unknown_width_string() {
    let cfg: ModelConfig =
        json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"wide"}}}"#).unwrap();
    assert!(cfg.ui.popup.to_style().is_err());
}

#[test]
fn ui_popup_rejects_zero_height() {
    let cfg: ModelConfig =
        json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"fill","height":0}}}"#).unwrap();
    let err = cfg.ui.popup.to_style().unwrap_err();
    assert!(
        err.to_string()
            .contains("ui.popup.height must be at least 1"),
        "unhelpful error: {err}"
    );
}

// ── code_exec.mode (M3 T10d runtime switch) ──

/// Absent `code_exec` = `tool` — a config that predates the switch keeps
/// today's shipped behavior exactly.
#[test]
fn code_exec_absent_defaults_to_tool() {
    let json = r#"{"main": "gpt-5.4-mini"}"#;
    let config: ModelConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.code_exec.mode, CodeExecMode::Tool);
}

#[test]
fn code_exec_mode_parses_all_three_roads() {
    for (mode, expected) in [
        ("tool", CodeExecMode::Tool),
        ("act", CodeExecMode::Act),
        ("off", CodeExecMode::Off),
    ] {
        let json = format!(r#"{{"main": "m", "code_exec": {{"mode": "{mode}"}}}}"#);
        let config: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.code_exec.mode, expected, "mode = {mode:?}");
    }
}

#[test]
fn code_exec_mode_rejects_an_unknown_road() {
    let json = r#"{"main": "m", "code_exec": {"mode": "banana"}}"#;
    assert!(
        serde_json::from_str::<ModelConfig>(json).is_err(),
        "an unknown mode must fail loud at parse, not fall through"
    );
}
