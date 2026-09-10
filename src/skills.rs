//! App-side skills wiring.
//!
//! The skills capability (filesystem discovery, catalog injection + prompt
//! surgery, `skill` tool, per-turn refresh middleware, telemetry) lives in
//! agent-works' `skill` module and arrives through phi-agent's re-exports.
//! This module keeps only phimint's own policy: which directories phimint
//! scans by default. ops-agent/db-agent 各自提供自己的目录列表即可。

use std::path::PathBuf;

pub use phi_agent::{SkillCatalogRefreshMiddleware, SkillResolver, SkillTelemetry, SkillTool, render_catalog};

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
}
