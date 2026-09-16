//! App-side skills wiring.
//!
//! The skills capability (filesystem discovery, catalog injection + prompt
//! surgery, `skill` tool, per-turn refresh middleware, telemetry) lives in
//! agent-works' `skill` module and arrives through phi-agent's re-exports.
//! This module keeps only phimint's own policy: which directories phimint
//! scans by default. ops-agent/db-agent 各自提供自己的目录列表即可。

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

/// phimint 默认扫描的 skill 目录（应用策略：家族 `.claude/` 目录约定）。
///
/// 用户级低优先级、项目级高优先级——同名覆盖由 `SkillResolver::from_dirs`
/// 的扫描顺序保证（后扫描者优先）。将来的配置化（`.claude/phimint.toml`
/// `[skills] dirs`）在这里替换实现即可，调用点不变。
pub fn default_skill_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    vec![
        home.join(".claude").join("skills"), // 用户级（低优先级）
        PathBuf::from(".claude/skills"),     // 项目级（高优先级）
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dirs_layer_project_over_user() {
        let dirs = default_skill_dirs();
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].ends_with(".claude/skills"), "user level first: {:?}", dirs[0]);
        assert_eq!(dirs[1], PathBuf::from(".claude/skills"), "project level second");
    }

    mod active_skills {
        use super::*;

        fn resolver_with(tmp: &Path, name: &str, body: &str) -> SkillResolver {
            let dir = tmp.join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: fixture\nuser-invocable: true\n---\n\n{body}"),
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
                ActiveSkillEntry { name: "a".into(), body: "## Inner\n\nbody a\n".into() },
                ActiveSkillEntry { name: "b".into(), body: "plain body b\n".into() },
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
                &[ActiveSkillEntry { name: "s1".into(), body: "body one".into() }],
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
