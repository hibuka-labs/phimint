//! App-side skills wiring.
//!
//! The skills capability (filesystem discovery, catalog injection + prompt
//! surgery, `skill` tool, per-turn refresh middleware, telemetry) lives in
//! agent-works' `skill` module and arrives through phi-agent's re-exports.
//! This module keeps only phimint's own policy: which directories phimint
//! scans by default, and the bundled `design-canvas` routing skill (1C task 9)
//! that is materialized next to them at startup. ops-agent/db-agent just
//! supply their own directory lists.

use std::path::PathBuf;

pub use phi_agent::{
    SkillCatalogRefreshMiddleware, SkillResolver, SkillScope, SkillTelemetry, SkillTool,
    demote_h2_headings, refresh_catalog, render_catalog,
};

use std::io;
use std::path::Path;

/// A session-scope skill activated in this session (skill-lifetime v3).
///
/// Stores the **body**, not just the name: re-triggering with new `$ARGUMENTS`
/// refreshes the entry, and resume re-baking works even if the SKILL.md was
/// deleted mid-session (spec edge case).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct ActiveSkillEntry {
    pub name: String,
    pub body: String,
}

/// Header of the session-scope section appended to the system prompt.
///
/// Lives OUTSIDE the catalog region (`SkillCatalogRefreshMiddleware` anchors
/// on `## Skills` and bounds regions at `\n\n## `), so catalog refreshes and
/// active-skill rewrites never touch each other's bytes.
const ACTIVE_SKILLS_HEADER: &str = "## Active Skills";

/// Append the Active Skills section to a composed system prompt.
///
/// Rebuilt from `active` on every call — idempotent by construction. Bodies
/// get `demote_h2_headings` so a `## Skills` heading inside a skill body can
/// never forge a catalog anchor. Empty list → input unchanged
/// (byte-identical, same contract as the catalog's no-skills baseline).
pub(crate) fn append_active_skills(prompt: &str, active: &[ActiveSkillEntry]) -> String {
    if active.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::with_capacity(prompt.len() + 256);
    out.push_str(prompt);
    out.push_str("\n\n");
    out.push_str(ACTIVE_SKILLS_HEADER);
    out.push_str(
        "\n\nThe skills below were activated by the user and remain in effect for this \
         entire session. Follow them in every turn.\n",
    );
    for entry in active {
        out.push_str(&format!(
            "\n### skill: {}\n\n{}\n",
            entry.name,
            demote_h2_headings(&entry.body)
        ));
    }
    out
}

/// Path of the per-session active-skills list (resume support).
pub(crate) fn active_skills_path(session_dir: &Path) -> PathBuf {
    session_dir.join("active_skills.json")
}

/// Load the active-skills list; missing/corrupt file → empty (fresh state).
pub(crate) fn load_active_skills(session_dir: &Path) -> Vec<ActiveSkillEntry> {
    let path = active_skills_path(session_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str(&text) {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "corrupt active_skills.json, ignoring");
            Vec::new()
        }
    }
}

/// Persist the active-skills list (sync I/O, matching `save_turn_log`).
///
/// Atomic via tmp+rename (same pattern as `persist_window_messages`): a
/// crash mid-write must never leave a corrupt file — that would silently
/// drop the session's active skills on resume.
pub(crate) fn save_active_skills(
    session_dir: &Path,
    active: &[ActiveSkillEntry],
) -> io::Result<()> {
    let path = active_skills_path(session_dir);
    let json = serde_json::to_string_pretty(active)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// The bundled design-canvas skill (1C task 9): canvas routing judgment, when
/// to push a screen, and what goes on it. Compiled into the binary and
/// materialized into [`builtin_skills_dir`] at startup — install scripts know
/// nothing about skills.
pub const DESIGN_CANVAS_SKILL: &str = include_str!("assets/design-canvas/SKILL.md");

/// Skill directories phimint scans by default (policy: the family `.claude/` convention).
///
/// Order is scan priority (later wins on name collision): the bundled
/// `~/.phimint/skills` copy is the floor, then user-level, then project-level.
/// Future configuration (`.claude/phimint.toml` `[skills] dirs`) swaps in here;
/// callers unchanged.
pub fn default_skill_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    vec![
        builtin_skills_dir(),                // bundled (lowest priority)
        home.join(".claude").join("skills"), // user level
        PathBuf::from(".claude/skills"),     // project level (highest)
    ]
}

/// Where phimint's bundled skills live on disk — the lowest-priority scan
/// dir. Overridable by the same skill name in `~/.claude/skills` or the
/// project's `.claude/skills`.
pub fn builtin_skills_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".phimint")
        .join("skills")
}

/// Materialize the bundled skills under `root` (normally
/// [`builtin_skills_dir`]). Each skill is written when missing or stale —
/// the bundled copy is an owned asset and is refreshed to match the binary,
/// so customize by placing a same-named skill in a higher-priority scan dir
/// rather than editing files here. Returns the paths written.
pub fn materialize_builtin_skills(root: &Path) -> io::Result<Vec<PathBuf>> {
    let skill_dir = root.join("design-canvas");
    let path = skill_dir.join("SKILL.md");
    if std::fs::read_to_string(&path).is_ok_and(|s| s == DESIGN_CANVAS_SKILL) {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(&skill_dir)?;
    // tmp+rename (same pattern as `save_active_skills`): a crash mid-write
    // must never leave a half-written SKILL.md behind.
    let tmp = skill_dir.join("SKILL.md.tmp");
    std::fs::write(&tmp, DESIGN_CANVAS_SKILL)?;
    std::fs::rename(&tmp, &path)?;
    Ok(vec![path])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dirs_layer_builtin_lowest_project_highest() {
        let dirs = default_skill_dirs();
        assert_eq!(dirs.len(), 3);
        assert_eq!(
            dirs[0],
            builtin_skills_dir(),
            "bundled skills scan first (lowest priority)"
        );
        assert!(
            dirs[1].ends_with(".claude/skills"),
            "user level second: {:?}",
            dirs[1]
        );
        assert_eq!(
            dirs[2],
            PathBuf::from(".claude/skills"),
            "project level last (highest priority)"
        );
    }

    #[test]
    fn builtin_skills_dir_sits_beside_sessions() {
        assert!(builtin_skills_dir().ends_with(".phimint/skills"));
    }

    mod materialize {
        use super::*;

        #[test]
        fn writes_design_canvas_skill() {
            let tmp = tempfile::tempdir().unwrap();
            let written = materialize_builtin_skills(tmp.path()).unwrap();
            assert_eq!(written, vec![tmp.path().join("design-canvas/SKILL.md")]);
            let on_disk = std::fs::read_to_string(&written[0]).unwrap();
            assert_eq!(on_disk, DESIGN_CANVAS_SKILL);
        }

        #[test]
        fn second_call_is_a_noop() {
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtin_skills(tmp.path()).unwrap();
            assert!(
                materialize_builtin_skills(tmp.path()).unwrap().is_empty(),
                "unchanged bundled content must not be rewritten"
            );
        }

        /// The bundled copy is an owned asset: a stale on-disk version is
        /// refreshed to the binary's content on the next startup.
        #[test]
        fn stale_copy_is_refreshed() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("design-canvas");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "stale user edit").unwrap();
            let written = materialize_builtin_skills(tmp.path()).unwrap();
            assert_eq!(written.len(), 1);
            assert_eq!(
                std::fs::read_to_string(&written[0]).unwrap(),
                DESIGN_CANVAS_SKILL
            );
        }

        /// Directory name must match the frontmatter `name` — the loader
        /// rejects a mismatch, which would silently cost the model the skill.
        #[test]
        fn materialized_layout_scans_as_a_skill() {
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtin_skills(tmp.path()).unwrap();
            let resolver = SkillResolver::from_dirs(&[tmp.path().to_path_buf()]);
            assert!(resolver.skill_names().contains(&"design-canvas"));
        }
    }

    /// Guard: the routing doctrine is the product (1C task 9) — if these
    /// lines drift out, the model loses the "when to use the canvas" rules.
    mod design_canvas_content {
        use super::*;

        /// Load the bundled skill through the real loader. Doubles as the
        /// YAML quoting guard: a colon in `description` unquoted would make
        /// `serde_yaml` reject the whole skill and silently cost the model
        /// the routing rules.
        fn load_bundled() -> SkillResolver {
            let tmp = tempfile::tempdir().unwrap();
            materialize_builtin_skills(tmp.path()).unwrap();
            SkillResolver::from_dirs(&[tmp.path().to_path_buf()])
        }

        /// Content match that ignores wrapping: the SKILL.md is hard-wrapped,
        /// so a sentinel phrase must be free to straddle a line break.
        fn contains_rule(needle: &str) -> bool {
            let flatten = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            flatten(DESIGN_CANVAS_SKILL).contains(&flatten(needle))
        }

        fn assert_rules(context: &str, needles: &[&str]) {
            for needle in needles {
                assert!(contains_rule(needle), "{context} missing: {needle}");
            }
        }

        #[test]
        fn bundled_skill_loads_and_is_model_visible() {
            let resolver = load_bundled();
            assert!(
                resolver.skill_names().contains(&"design-canvas"),
                "bundled skill must parse as design-canvas"
            );
            let catalog = render_catalog(&resolver).expect("catalog for a model-visible skill");
            assert!(
                catalog.contains("- design-canvas: "),
                "catalog entry missing: {catalog}"
            );
        }

        #[test]
        fn routing_rules_are_present() {
            assert_rules(
                "routing rule",
                &[
                    // 纯文字选择不起画布
                    "answer inline in the terminal",
                    // 「长什么样」开画布
                    "Appearance questions",
                    // 判断困难宁开不省 + 理由是「不开错更贵」
                    "When unsure, open the canvas",
                    "Not opening is the more expensive mistake",
                    // 升降维不重新描述需求
                    "用图给我看",
                    "never make them restate the question",
                ],
            );
        }

        #[test]
        fn host_owns_waiting_screen_and_model_never_forces_a_browser() {
            assert_rules(
                "lifecycle rule",
                &[
                    "继续在终端讨论…", // waiting 屏由 host 自动推
                    "You never craft that screen",
                    "never retry `canvas_open` to force a browser open",
                    "(画布选择)", // M2 answer prefix the model must recognize
                ],
            );
        }

        #[test]
        fn screen_composition_names_the_real_component_classes() {
            assert_rules(
                "component class",
                &[".cards", ".mockup", ".split", ".placeholder", "body_html"],
            );
        }
    }

    mod active_skills {
        use super::*;

        fn resolver_with(tmp: &Path, name: &str, body: &str) -> SkillResolver {
            let dir = tmp.join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: fixture\nuser-invocable: true\n---\n\n{body}"
                ),
            )
            .unwrap();
            SkillResolver::from_dirs(&[tmp.join("skills")])
        }

        #[test]
        fn append_is_noop_when_list_empty() {
            let out = append_active_skills("base prompt", &[]);
            assert_eq!(out, "base prompt", "empty list must be byte-identical");
        }

        #[test]
        fn appended_section_demotes_h2_and_lists_every_entry() {
            let active = vec![
                ActiveSkillEntry {
                    name: "a".into(),
                    body: "## Inner\n\nbody a\n".into(),
                },
                ActiveSkillEntry {
                    name: "b".into(),
                    body: "plain body b\n".into(),
                },
            ];
            let out = append_active_skills("base", &active);
            assert!(out.starts_with("base"));
            assert!(out.contains(ACTIVE_SKILLS_HEADER));
            assert!(out.contains("### skill: a") && out.contains("### skill: b"));
            // Demoted: the baked body must not carry an H2 heading.
            assert!(out.contains("### Inner") && !out.contains("\n## Inner"));
        }

        #[test]
        fn append_active_skills_preserves_base_and_appends_section() {
            let tmp = tempfile::tempdir().unwrap();
            let resolver = resolver_with(tmp.path(), "s1", "body one");
            // The base is whatever the host composed at build time — here the
            // one composition point, in production the FULL build prompt
            // (catalog + tb suffix + CLAUDE.md + memory) captured from
            // `runtime().config().system_prompt`.
            let base = crate::agent::compose_system_prompt(&resolver);
            let out = append_active_skills(
                &base,
                &[ActiveSkillEntry {
                    name: "s1".into(),
                    body: "body one".into(),
                }],
            );
            assert!(out.starts_with(&base), "base must be preserved verbatim");
            assert!(out.contains("## Active Skills"));
            assert!(out.contains("body one"));
        }

        #[test]
        fn active_skills_json_round_trips() {
            let tmp = tempfile::tempdir().unwrap();
            let active = vec![ActiveSkillEntry {
                name: "s1".into(),
                body: "body with \"quotes\" and\nnewlines".into(),
            }];
            save_active_skills(tmp.path(), &active).unwrap();
            assert_eq!(load_active_skills(tmp.path()), active);
        }

        /// Spec edge case: resume must re-bake from the STORED body — the
        /// SKILL.md may have been deleted (or edited) mid-session.
        #[test]
        fn resume_rebakes_from_stored_body_after_skill_file_deleted() {
            let tmp = tempfile::tempdir().unwrap();
            let _resolver = resolver_with(tmp.path(), "gone", "stored body survives");
            let active = vec![ActiveSkillEntry {
                name: "gone".into(),
                body: "stored body survives".into(),
            }];
            save_active_skills(tmp.path(), &active).unwrap();

            // The skill file disappears mid-session; a fresh resolver no
            // longer knows it.
            std::fs::remove_dir_all(tmp.path().join("skills").join("gone")).unwrap();
            let fresh_resolver = SkillResolver::from_dirs(&[tmp.path().join("skills")]);

            // agent_loop startup path: load + append to the pristine base.
            let loaded = load_active_skills(tmp.path());
            let base = crate::agent::compose_system_prompt(&fresh_resolver);
            let prompt = append_active_skills(&base, &loaded);
            assert!(
                prompt.contains("## Active Skills") && prompt.contains("stored body survives"),
                "resume must re-bake from the stored body"
            );
        }

        /// Spec edge case: a corrupt active_skills.json falls back to an
        /// empty list — a fresh prompt, never a crash.
        #[test]
        fn corrupt_active_skills_json_loads_empty() {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("active_skills.json"), "{not json").unwrap();
            assert!(load_active_skills(tmp.path()).is_empty());
        }
    }
}
