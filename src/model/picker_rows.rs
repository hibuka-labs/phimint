//! The `/model` picker's candidate rows and confirm copy
//! (notes/spec-model-picker.md, revised by notes/model-pointer-design.md):
//! enumerate the configured models, compose the row labels, diff capability
//! bits, and word the confirmation.
//!
//! Pure data — no channels, no store mutation. The command handler owns the
//! pick flow (push a row list, apply the answer); everything about WHAT the
//! rows say lives here. A pick persists as the `model` selection pointer in
//! `config.json` (see [`crate::model::persist`]); the tier definitions
//! `main`/`lite`/`advanced` are never rewritten, so the shelf is stable
//! across switches. Two label axes stay orthogonal on purpose: a tier tag
//! follows the DECLARATION (`[main]` marks the configured `main` name even
//! when a pointer displaced it) while the current mark and the float-to-top
//! follow the EFFECTIVE main. The endpoint travels with the name (a Full
//! tier's base_url/key/protocol win over a Simple tier's inherited ones) —
//! except on the effective main's own name, where the live tuple wins.

use phi_agent::llm_trait::{Capabilities, Protocol};
use phi_agent::{ModelProfile, ModelRegistry};
use phi_agent_tui::PickRow;

use crate::model::config::{EndpointTuple, TierConfig};
use crate::model::store::ModelStore;

/// The answer key of the「clear selection」row: an internal sentinel, never a
/// model name. The command handler intercepts it on BOTH roads — the picker
/// answer and a typed `/model __clear_selection__` — before
/// [`resolve_selection`] can ever read it.
pub const CLEAR_SELECTION_ID: &str = "__clear_selection__";

/// Label of the「clear selection」row.
const CLEAR_SELECTION_LABEL: &str = "Clear selection (back to configured default)";

/// One picker candidate: the display facts plus the endpoint tuple the
/// selection installs.
#[derive(Debug)]
pub struct Candidate {
    /// The answer key (model name) and the row's identity for dedup.
    pub model: String,
    /// The endpoint tuple this row installs (Full config wins on same name).
    pub base_url: String,
    pub api_key: String,
    pub protocol: String,
    /// Which declared tiers name this model (declaration view — `[main]`
    /// marks the configured `main` definition even when a pointer displaced
    /// it), in declaration order — `["main", "lite"]` reads as `[main, lite]`.
    pub tiers: Vec<String>,
    /// The current main model (floats to the top of the picker).
    pub current: bool,
    /// Whether this row's tuple came from a Full config (dedup bookkeeping —
    /// a Full endpoint wins over a Simple one on the same name).
    from_full: bool,
}

/// Enumerate the picker's candidates: one row per configured model name —
/// tier configs dedup on the name (tags merging), a Full tier's endpoint wins
/// over a Simple tier's inherited one, and the current main model floats to
/// the top. A row whose name is the effective main carries the EFFECTIVE
/// tuple (the live one wins over the declaration — re-picking the current row
/// must be a tuple-equivalence skip, never a silent endpoint swap back to the
/// declaration); when that name is not in the config at all (e.g. after a
/// literal `/model some-name`) it gets a row of its own on the main endpoint.
///
/// gstack-shortcut(dec-b9ec9639): picker shelf ≤4 rows (3 tier defs deduped
/// by name + a displaced current row) plus the synthetic clear-selection row;
/// upgrade when a fourth standing candidate model is needed in the list.
pub fn build_candidates(store: &ModelStore) -> Vec<Candidate> {
    let config = store.config();
    let defaults = (
        config.base_url.as_deref().unwrap_or(""),
        config.api_key.as_deref().unwrap_or(""),
        config.protocol.as_deref().unwrap_or(""),
    );
    // The declared tier definitions, in declaration order — one declaration
    // source (`tier_names` + `tier_def`), never a hand-written list.
    let declared: Vec<(&str, &TierConfig)> = config
        .tier_names()
        .into_iter()
        .filter_map(|tier| config.tier_def(tier).map(|cfg| (tier, cfg)))
        .collect();

    // Rows from the declared configs, deduped by model name. A Full config
    // carries its own endpoint and wins over a Simple one on the same name;
    // two Fulls of one name keep the first (declaration order).
    let mut rows: Vec<Candidate> = Vec::new();
    for (_, cfg) in &declared {
        let (model, base_url, api_key, protocol) = cfg.resolve(defaults.0, defaults.1, defaults.2);
        let from_full = matches!(cfg, TierConfig::Full { .. });
        match rows.iter_mut().find(|row| row.model == model) {
            Some(row) => {
                if from_full && !row.from_full {
                    row.base_url = base_url;
                    row.api_key = api_key;
                    row.protocol = protocol;
                    row.from_full = true;
                }
            }
            None => rows.push(Candidate {
                model,
                base_url,
                api_key,
                protocol,
                tiers: Vec::new(),
                current: false,
                from_full,
            }),
        }
    }

    // The effective main owns its name's row: same name, the live tuple wins
    // over the declaration (see the module docs); a literal-set name absent
    // from the config gets a row of its own on the main endpoint (its tuple
    // IS the main tier's — the name, on the main endpoint).
    let current_tuple = store.tier_tuple("main");
    let current_main = current_tuple.0.clone();
    match rows.iter_mut().find(|row| row.model == current_main) {
        Some(row) => {
            row.base_url = current_tuple.1.clone();
            row.api_key = current_tuple.2.clone();
            row.protocol = current_tuple.3.clone();
            row.from_full = true;
        }
        None => rows.insert(
            0,
            Candidate {
                model: current_main.clone(),
                base_url: current_tuple.1,
                api_key: current_tuple.2,
                protocol: current_tuple.3,
                tiers: Vec::new(),
                current: false,
                from_full: true,
            },
        ),
    }

    // Tags from the declared tiers' DECLARED models — a tag follows the
    // declaration, never the runtime: the configured `main` name keeps its
    // `[main]` tag even when a pointer displaced it, and an unconfigured tier
    // is not in `declared` and never claims a tag.
    for (tier, cfg) in &declared {
        let (model, ..) = cfg.resolve(defaults.0, defaults.1, defaults.2);
        if let Some(row) = rows.iter_mut().find(|row| row.model == model) {
            row.tiers.push((*tier).to_string());
        }
    }

    // The current mark, and the float-to-top.
    for row in &mut rows {
        row.current = row.model == current_main;
    }
    if let Some(pos) = rows.iter().position(|row| row.current) {
        let row = rows.remove(pos);
        rows.insert(0, row);
    }
    rows
}

/// The picker's display rows: `label = model [tier, tags]`, the current row
/// bolded (the `current` flag) with a note saying so — plus the trailing
///「clear selection」row exactly when a selection rides on the configured
/// `main` (it is an action, never a shelf candidate).
pub fn pick_rows(store: &ModelStore, candidates: &[Candidate]) -> Vec<PickRow> {
    let mut rows: Vec<PickRow> = candidates
        .iter()
        .map(|row| PickRow {
            id: row.model.clone(),
            label: if row.tiers.is_empty() {
                row.model.clone()
            } else {
                format!("{} [{}]", row.model, row.tiers.join(", "))
            },
            note: row.current.then(|| "current".to_string()),
            current: row.current,
        })
        .collect();
    if selection_active(store) {
        rows.push(PickRow {
            id: CLEAR_SELECTION_ID.to_string(),
            label: CLEAR_SELECTION_LABEL.to_string(),
            note: None,
            current: false,
        });
    }
    rows
}

/// Whether a selection is riding on top of the configured `main`: the `model`
/// pointer is set in the loaded config, or the runtime main tier has moved
/// off the configured definition. The second half exists because a pick
/// writes the tier table and the file, not the in-memory slot (R6) — right
/// after an in-session pick the slot alone would read "no pointer" while the
/// selection is live.
fn selection_active(store: &ModelStore) -> bool {
    store.config().model.is_some() || store.tier_tuple("main") != store.configured_main()
}

/// Resolve a confirmed pick (or a typed name) to the tuple to install: a
/// candidate's own endpoint, or — the literal fallback for a name the config
/// does not list — the current main endpoint with the typed name.
/// [`CLEAR_SELECTION_ID`] never reaches here: the command handler
/// intercepts it (picker answer and typed input alike) and runs the clear
/// flow instead.
pub fn resolve_selection(store: &ModelStore, id: &str) -> EndpointTuple {
    for row in build_candidates(store) {
        if row.model == id {
            return (row.model, row.base_url, row.api_key, row.protocol);
        }
    }
    let (_, base_url, api_key, protocol) = store.tier_tuple("main");
    (id.to_string(), base_url, api_key, protocol)
}

/// Whether a confirmed pick or typed name is a literal nothing vouches for:
/// absent from the candidate rows AND unknown to the model registry (looked
/// up on the endpoint a literal rides — the current main one). The literal
/// still applies (the fallback is by design); the caller warns on the
/// confirm line so a typo cannot silently become every later boot's model.
pub fn is_unvalidated_literal(store: &ModelStore, id: &str) -> bool {
    if build_candidates(store).iter().any(|row| row.model == id) {
        return false;
    }
    let (_model, base_url, _api_key, protocol) = store.tier_tuple("main");
    ModelRegistry::builtin()
        .lookup_known(
            id,
            Some(base_url.as_str()),
            protocol.parse::<Protocol>().ok(),
        )
        .is_none()
}

/// One capability bit: its display name and the getter over a profile's
/// [`Capabilities`].
type CapabilityBit = (&'static str, fn(&Capabilities) -> bool);

/// The capability bits that differ between two endpoints, from the model
/// registry. An endpoint the registry does not know has no facts — its bits
/// read "unknown" and are never guessed from the fallback profile. Identical
/// bits are omitted. Args are `(model, base_url, protocol)` triples; the
/// endpoint matters because protocol (explicit or URL-inferred) picks the
/// profile.
pub fn capability_diffs(from: (&str, &str, &str), to: (&str, &str, &str)) -> Vec<String> {
    let registry = ModelRegistry::builtin();
    let a = known(&registry, from);
    let b = known(&registry, to);
    let bits: [CapabilityBit; 4] = [
        ("streaming", |c| c.supports_streaming),
        ("tools", |c| c.supports_tools),
        ("vision", |c| c.supports_vision),
        ("thinking", |c| c.supports_thinking),
    ];
    bits.iter()
        .filter_map(|(name, get)| {
            let left = a.as_ref().map(|profile| get(&profile.capabilities));
            let right = b.as_ref().map(|profile| get(&profile.capabilities));
            (left != right).then(|| format!("{name}: {} -> {}", flag(left), flag(right)))
        })
        .collect()
}

/// The registry's knowledge of one endpoint (`None` = never heard of it —
/// the caller renders "unknown", never the fallback profile's guesses).
fn known(
    registry: &ModelRegistry,
    (model, base_url, protocol): (&str, &str, &str),
) -> Option<ModelProfile> {
    registry.lookup_known(model, Some(base_url), protocol.parse::<Protocol>().ok())
}

fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

/// The confirmation copy for a main-model swap: `X (was Y)` plus only the
/// capability bits that differ — or the "unchanged" line when the pick
/// installed the endpoint that was already live (`unchanged` compares the
/// full tuple: a same-name pick can still change the endpoint, and then it
/// IS a change).
pub fn confirm_line(from_model: &str, to_model: &str, unchanged: bool, diffs: &[String]) -> String {
    if unchanged {
        return format!("Main model unchanged: {to_model}");
    }
    let mut out = format!("Main model set to: {to_model} (was {from_model})");
    for diff in diffs {
        out.push_str("\n  ");
        out.push_str(diff);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::ModelConfig;
    use std::sync::Arc;

    fn base_config() -> ModelConfig {
        ModelConfig {
            base_url: Some("https://api.example.com/v1".to_string()),
            api_key: Some("sk-test".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("gpt-4".to_string()),
            lite: Some(TierConfig::Simple("gpt-4".to_string())),
            advanced: Some(TierConfig::Simple("o1-preview".to_string())),
            model: None,
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
            code_exec: Default::default(),
        }
    }

    struct MockProvider;

    #[async_trait::async_trait]
    impl phi_agent::llm_trait::LlmProvider for MockProvider {
        async fn stream(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
            Err(phi_agent::llm_trait::LlmError::llm("mock"))
        }
        async fn chat(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatResponse, phi_agent::llm_trait::LlmError> {
            Err(phi_agent::llm_trait::LlmError::llm("mock"))
        }
        fn capabilities(&self) -> phi_agent::llm_trait::Capabilities {
            Default::default()
        }
        fn info(&self) -> phi_agent::llm_trait::ProviderInfo {
            phi_agent::llm_trait::ProviderInfo {
                name: "mock".to_string(),
                model: "mock-model".to_string(),
                version: None,
            }
        }
    }

    fn mock_store(config: ModelConfig) -> ModelStore {
        ModelStore::new(config, Arc::new(MockProvider))
    }

    /// protects: tier configs dedup on the model name and the tier tags merge
    /// in declaration order. fails_when: one name yields two rows or a tag is
    /// dropped/duplicated. no_claim: which models a product configures.
    #[test]
    fn rows_dedup_on_the_name_and_merge_tier_tags() {
        let store = mock_store(base_config());
        let rows = build_candidates(&store);
        assert_eq!(rows.len(), 2, "gpt-4 twice, o1-preview once: {rows:?}");
        assert_eq!(rows[0].model, "gpt-4");
        assert_eq!(rows[0].tiers, vec!["main".to_string(), "lite".to_string()]);
        assert_eq!(rows[1].model, "o1-preview");
        assert_eq!(rows[1].tiers, vec!["advanced".to_string()]);
        let labels: Vec<String> = pick_rows(&store, &rows)
            .into_iter()
            .map(|r| r.label)
            .collect();
        assert_eq!(labels[0], "gpt-4 [main, lite]");
        assert_eq!(labels[1], "o1-preview [advanced]");
    }

    /// protects: the endpoint travels with the name — a Full tier's own
    /// base_url/key/protocol win over a Simple tier's inherited ones on the
    /// same name — EXCEPT on the effective main's name, where the live tuple
    /// wins (re-picking the current row must be a tuple-equivalence skip,
    /// never a silent endpoint swap back to the declaration). fails_when:
    /// the Full endpoint is silently dropped on a shelf name, or a re-pick
    /// of the current row swaps the endpoint. no_claim: which endpoint a
    /// product chooses.
    #[test]
    fn a_full_endpoint_wins_on_a_shelf_name_but_the_live_tuple_wins_on_its_own() {
        // Full wins on a name that is not the effective main.
        let mut config = base_config();
        config.lite = Some(TierConfig::Simple("shared".to_string()));
        config.advanced = Some(TierConfig::Full {
            model: "shared".to_string(),
            base_url: Some("https://other.example.com/v2".to_string()),
            api_key: Some("sk-other".to_string()),
            protocol: Some("anthropic".to_string()),
        });
        let rows = build_candidates(&mock_store(config));
        let shared = rows.iter().find(|row| row.model == "shared").expect("row");
        assert_eq!(shared.base_url, "https://other.example.com/v2");
        assert_eq!(shared.api_key, "sk-other");
        assert_eq!(shared.protocol, "anthropic");

        // The effective main's name: the live tuple wins over a same-named
        // tier's Full endpoint.
        let mut config = base_config();
        config.lite = Some(TierConfig::Full {
            model: "gpt-4".to_string(),
            base_url: Some("https://other.example.com/v2".to_string()),
            api_key: Some("sk-other".to_string()),
            protocol: Some("anthropic".to_string()),
        });
        let rows = build_candidates(&mock_store(config));
        assert_eq!(rows[0].model, "gpt-4");
        assert_eq!(
            rows[0].base_url, "https://api.example.com/v1",
            "the live tuple wins on the current name: {:?}",
            rows[0]
        );
        assert_eq!(rows[0].api_key, "sk-test");
    }

    /// protects: the current main model floats to the top and wears the
    /// current mark — while a tier tag follows the DECLARATION, so a
    /// displaced current keeps its row (on the main endpoint) but the `[main]`
    /// tag stays on the configured `main` name. fails_when: the current row
    /// is missing or buried, or the tag drifts onto the pointer row.
    /// no_claim: how the current model got current.
    #[test]
    fn the_current_model_floats_top_and_the_tag_stays_on_the_declaration() {
        let mut config = base_config();
        config.advanced = Some(TierConfig::Simple("gpt-4-turbo".to_string()));
        let mut store = mock_store(config);
        store.set_tier_model("main", "totally-new".to_string());
        let rows = build_candidates(&store);
        assert_eq!(rows[0].model, "totally-new", "the current row is on top");
        assert!(rows[0].current);
        let picker = pick_rows(&store, &rows);
        assert_eq!(picker[0].note.as_deref(), Some("current"));
        assert_eq!(
            picker[0].label, "totally-new",
            "a displaced current claims no tier tag: {:?}",
            picker[0]
        );
        assert!(
            picker.iter().any(|row| row.label == "gpt-4 [main, lite]"),
            "the configured main keeps its tags: {:?}",
            picker
        );
        assert!(!picker[1].current);
    }

    /// protects: the「clear selection」row shows exactly when a selection
    /// rides the configured `main` — a pointer in the config or an in-session
    /// pick off the configured definition — and never over a pristine config;
    /// under a pointer the tags stay on the declarations. fails_when: the row
    /// vanishes while a selection is live, appears without one, or the
    /// pointer row steals the `[main]` tag. no_claim: what clearing does (the
    /// command handler owns the clear flow).
    #[test]
    fn the_clear_selection_row_tracks_the_selection() {
        // A pointer in the config: the row shows, and the configured main
        // keeps its tags on its own name.
        let mut config = base_config();
        config.model = Some(TierConfig::Simple("pointer-model".to_string()));
        let store = mock_store(config);
        let rows = pick_rows(&store, &build_candidates(&store));
        assert_eq!(rows[0].id, "pointer-model", "the effective main floats top");
        assert_eq!(
            rows.last().map(|row| (row.id.as_str(), row.label.as_str())),
            Some((
                CLEAR_SELECTION_ID,
                "Clear selection (back to configured default)"
            )),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.label == "gpt-4 [main, lite]"),
            "the configured main keeps its tags: {rows:?}"
        );

        // A pointer naming the configured main is still a selection.
        let mut config = base_config();
        config.model = Some(TierConfig::Simple("gpt-4".to_string()));
        let store = mock_store(config);
        let rows = pick_rows(&store, &build_candidates(&store));
        assert_eq!(
            rows.last().map(|row| row.id.as_str()),
            Some(CLEAR_SELECTION_ID),
            "{rows:?}"
        );

        // An in-session pick (tier table moved, in-memory slot not written):
        // still a selection.
        let mut store = mock_store(base_config());
        store
            .apply_picked(
                "picked-model".to_string(),
                "https://api.example.com/v1".to_string(),
                "sk-test".to_string(),
                "openai".to_string(),
            )
            .expect("provider creation succeeds");
        let rows = pick_rows(&store, &build_candidates(&store));
        assert_eq!(
            rows.last().map(|row| row.id.as_str()),
            Some(CLEAR_SELECTION_ID),
            "{rows:?}"
        );

        // Pristine config: hidden.
        let store = mock_store(base_config());
        let rows = pick_rows(&store, &build_candidates(&store));
        assert!(
            rows.iter().all(|row| row.id != CLEAR_SELECTION_ID),
            "{rows:?}"
        );
    }

    /// protects: capability bits are registry facts — only differing bits
    /// are listed, and a model the registry does not know reads "unknown"
    /// instead of borrowing the fallback profile's guesses. fails_when: a
    /// fabricated bit (the default profile's answer posing as knowledge) or
    /// an omitted known difference. no_claim: what a product does with the
    /// lines.
    #[test]
    fn capability_diffs_list_only_real_differences() {
        // deepseek-chat is known (vision off); "some-unknown" is not.
        let unknown = capability_diffs(
            ("deepseek-chat", "https://api.deepseek.com/v1", "openai"),
            ("some-unknown", "https://api.example.com/v1", "openai"),
        );
        assert_eq!(
            unknown,
            vec![
                "streaming: on -> unknown".to_string(),
                "tools: on -> unknown".to_string(),
                "vision: off -> unknown".to_string(),
                "thinking: on -> unknown".to_string(),
            ]
        );
        // Known vs known: deepseek-chat (vision off) vs mimo-v2.5-pro
        // (vision on) differs on exactly one bit.
        let known = capability_diffs(
            ("deepseek-chat", "https://api.deepseek.com/v1", "openai"),
            ("mimo-v2.5-pro", "https://api.example-mimo.com/v1", "openai"),
        );
        assert_eq!(known, vec!["vision: off -> on".to_string()]);
        // Same endpoint on both sides, and two unknowns, are empty.
        assert!(
            capability_diffs(
                ("deepseek-chat", "https://api.deepseek.com/v1", "openai"),
                ("deepseek-chat", "https://api.deepseek.com/v1", "openai"),
            )
            .is_empty()
        );
        assert!(
            capability_diffs(
                ("who-1", "https://a.example.com/v1", "openai"),
                ("who-2", "https://b.example.com/v1", "openai"),
            )
            .is_empty()
        );
    }

    /// protects: the confirmation line carries the new name and the old one;
    /// a pick that installed the live endpoint says "unchanged" instead of
    /// pretending, while a same-name endpoint swap reports as a change.
    /// fails_when: the old name disappears from the copy or an endpoint swap
    /// reads as "unchanged". no_claim: the product's voice beyond this shape.
    #[test]
    fn confirm_line_says_what_changed_and_what_did_not() {
        assert_eq!(
            confirm_line(
                "gpt-4",
                "gpt-4-turbo",
                false,
                &["vision: off -> on".to_string()]
            ),
            "Main model set to: gpt-4-turbo (was gpt-4)\n  vision: off -> on"
        );
        assert_eq!(
            confirm_line("gpt-4", "gpt-4", true, &[]),
            "Main model unchanged: gpt-4"
        );
        // Same name, different endpoint (the full tuple moved): a real
        // change, reported as one.
        assert_eq!(
            confirm_line("gpt-4", "gpt-4", false, &[]),
            "Main model set to: gpt-4 (was gpt-4)"
        );
    }

    /// protects: the literal fallback keeps the main endpoint and swaps only
    /// the name; a known candidate keeps its own tuple. fails_when: a literal
    /// pick silently changes the endpoint. no_claim: whether the typed name
    /// exists at the provider.
    #[test]
    fn resolve_selection_keeps_the_endpoint_on_a_literal_name() {
        let store = mock_store(base_config());
        let literal = resolve_selection(&store, "brand-new");
        assert_eq!(
            literal,
            (
                "brand-new".to_string(),
                "https://api.example.com/v1".to_string(),
                "sk-test".to_string(),
                "openai".to_string(),
            )
        );
    }
}
