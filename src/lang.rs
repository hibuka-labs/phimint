//! Language registry (Phase 6c): the single source of truth for per-language
//! knowledge.
//!
//! The multi-language architecture is "注册制 + 通用兜底" (registry + generic
//! fallback):
//!
//! - The **fallback** layer is language-agnostic — file listing, ripgrep, running
//!   any verify command, and showing a raw stderr tail — so *any* language is
//!   covered at zero cost (the path Codex proves works).
//! - The **registry** layer adds per-language data on demand: extension →
//!   language mapping, a default verify command, and manifest file names (what
//!   the verify gate needs). The error parser is derived from the command string
//!   in `verify.rs`; tree-sitter grammar and LSP specs join in later phases.
//!
//! Everything here is pure data plus a cheap bounded directory scan — no I/O
//! beyond [`detect_languages`].

use std::collections::HashSet;
use std::path::Path;

/// Optional LSP diagnostics server for a language. `None` means no LSP support —
/// the `diagnostics` tool then degrades to `verify`. (Java's `jdtls` needs a
/// project import step, so it's left out for now.)
#[derive(Debug, Clone)]
pub struct LspSpec {
    /// Server argv (binary + args), e.g. `["rust-analyzer"]` or
    /// `["typescript-language-server", "--stdio"]`.
    pub command: &'static [&'static str],
}

/// Per-language knowledge. Pure data — no I/O.
#[derive(Debug, Clone)]
pub struct LanguageSpec {
    /// File extensions (no leading dot) mapped to this language.
    pub extensions: &'static [&'static str],
    /// Default `verify` command when the agent passes none.
    pub verify_command: &'static str,
    /// Manifest/lock file *names* that count as code (editing them dirties the gate).
    pub manifests: &'static [&'static str],
    /// LSP diagnostics server, if any (used by the `diagnostics` tool).
    pub lsp: Option<LspSpec>,
}

/// The registry, in detection-priority order ([`detect_languages`] returns
/// matches in this order; the first is the dominant language for the default
/// verify command).
pub static LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        extensions: &["rs"],
        verify_command: "cargo check",
        manifests: &["Cargo.toml", "Cargo.lock"],
        lsp: Some(LspSpec {
            command: &["rust-analyzer"],
        }),
    },
    LanguageSpec {
        extensions: &["java"],
        verify_command: "mvn -q compile",
        manifests: &["pom.xml", "build.gradle", "build.gradle.kts"],
        // jdtls needs a project import step; deferred (degrade to `verify`).
        lsp: None,
    },
    LanguageSpec {
        extensions: &["ts", "tsx"],
        verify_command: "npx tsc --noEmit",
        manifests: &["tsconfig.json"],
        lsp: Some(LspSpec {
            command: &["typescript-language-server", "--stdio"],
        }),
    },
    LanguageSpec {
        extensions: &["js", "jsx", "mjs", "cjs"],
        verify_command: "npm run build",
        manifests: &["package.json"],
        lsp: Some(LspSpec {
            command: &["typescript-language-server", "--stdio"],
        }),
    },
    LanguageSpec {
        extensions: &["c", "h"],
        verify_command: "make",
        manifests: &["Makefile"],
        lsp: Some(LspSpec {
            command: &["clangd"],
        }),
    },
    LanguageSpec {
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        verify_command: "make",
        manifests: &["CMakeLists.txt"],
        lsp: Some(LspSpec {
            command: &["clangd"],
        }),
    },
];

/// Directories skipped by the bounded scan in [`detect_languages`] and the
/// diagnostics tool's file walk.
pub(crate) const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build", "out"];
/// Max files scanned before [`detect_languages`] gives up (bounds startup cost).
const MAX_SCAN_FILES: usize = 2000;

/// The language whose extensions include `path`'s extension, if any.
pub fn language_for_path(path: &str) -> Option<&'static LanguageSpec> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES.iter().find(|l| l.extensions.contains(&ext.as_str()))
}

/// LSP `languageId` for a path (per-extension, so finer than the language group:
/// `.tsx` → `typescriptreact`, `.jsx` → `javascriptreact`).
pub fn lsp_language_id(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "java" => "java",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        _ => return None,
    })
}

/// The LSP server spec for a path's language, if that language has one.
pub fn lsp_spec_for_path(path: &str) -> Option<&'static LspSpec> {
    language_for_path(path).and_then(|l| l.lsp.as_ref())
}

/// Whether a path is "code" for the verify gate: a known source extension, or a
/// manifest/lock file name (Cargo.toml, pom.xml, package.json, …).
pub fn is_code_path(path: &str) -> bool {
    if language_for_path(path).is_some() {
        return true;
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    LANGUAGES.iter().any(|l| l.manifests.contains(&name))
}

/// Languages present in `root`, in registry order. A bounded directory walk
/// collects the extension set (skipping build output / vendored deps / dotdirs),
/// then each language whose extensions show up is returned.
pub fn detect_languages(root: &Path) -> Vec<&'static LanguageSpec> {
    let mut exts: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    collect_extensions(root, &mut exts, &mut count);
    LANGUAGES
        .iter()
        .filter(|l| l.extensions.iter().any(|e| exts.contains(*e)))
        .collect()
}

/// Default `verify` command: the dominant detected language's command, or
/// `cargo check` when nothing is recognized.
pub fn default_verify_command(root: &Path) -> &'static str {
    detect_languages(root)
        .into_iter()
        .next()
        .map(|l| l.verify_command)
        .unwrap_or("cargo check")
}

/// Bounded recursive scan collecting file extensions into `exts`.
fn collect_extensions(dir: &Path, exts: &mut HashSet<String>, count: &mut usize) {
    if *count >= MAX_SCAN_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *count >= MAX_SCAN_FILES {
            return;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_extensions(&path, exts, count);
        } else if ft.is_file() {
            *count += 1;
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                exts.insert(ext.to_ascii_lowercase());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_for_path_matches_extensions() {
        assert_eq!(language_for_path("src/lib.rs").unwrap().verify_command, "cargo check");
        assert_eq!(language_for_path("Foo.java").unwrap().verify_command, "mvn -q compile");
        assert_eq!(language_for_path("a.ts").unwrap().verify_command, "npx tsc --noEmit");
        assert_eq!(language_for_path("b.tsx").unwrap().verify_command, "npx tsc --noEmit");
        assert_eq!(language_for_path("c.js").unwrap().verify_command, "npm run build");
        assert_eq!(language_for_path("d.mjs").unwrap().verify_command, "npm run build");
        assert!(language_for_path("e.cpp").unwrap().extensions.contains(&"cpp"));
        assert!(language_for_path("f.hpp").unwrap().extensions.contains(&"hpp"));
        assert!(language_for_path("g.h").unwrap().extensions.contains(&"h"));
        assert!(language_for_path("README.md").is_none());
        assert!(language_for_path("src/").is_none());
    }

    #[test]
    fn is_code_path_covers_source_and_manifests() {
        for p in [
            "src/lib.rs",
            "Foo.java",
            "a.tsx",
            "b.cpp",
            "Cargo.toml",
            "Cargo.lock",
            "pom.xml",
            "package.json",
            "tsconfig.json",
            "CMakeLists.txt",
            "Makefile",
            "docs/Cargo.toml",
        ] {
            assert!(is_code_path(p), "{p} should be code");
        }
        for p in ["README.md", "docs/notes.txt", "src/"] {
            assert!(!is_code_path(p), "{p} should not be code");
        }
    }

    #[test]
    fn detect_languages_scans_extensions_and_skips_build_dirs() {
        let root = std::env::temp_dir().join("phiforge_lang_detect_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/Foo.java"), "class Foo {}").unwrap();
        std::fs::write(root.join("target/Ignored.java"), "class Ignored {}").unwrap();

        let langs = detect_languages(&root);
        assert_eq!(langs.len(), 1);
        assert!(langs[0].extensions.contains(&"java"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_verify_command_falls_back_to_cargo() {
        let empty = std::env::temp_dir().join("phiforge_lang_empty_test");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(default_verify_command(&empty), "cargo check");
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn lsp_language_id_maps_extensions() {
        assert_eq!(lsp_language_id("a.rs"), Some("rust"));
        assert_eq!(lsp_language_id("a.java"), Some("java"));
        assert_eq!(lsp_language_id("a.ts"), Some("typescript"));
        assert_eq!(lsp_language_id("a.tsx"), Some("typescriptreact"));
        assert_eq!(lsp_language_id("a.js"), Some("javascript"));
        assert_eq!(lsp_language_id("a.jsx"), Some("javascriptreact"));
        assert_eq!(lsp_language_id("a.cpp"), Some("cpp"));
        assert_eq!(lsp_language_id("a.h"), Some("c"));
        assert_eq!(lsp_language_id("README.md"), None);
    }

    #[test]
    fn lsp_spec_for_path_routes_to_server() {
        assert_eq!(lsp_spec_for_path("a.rs").unwrap().command[0], "rust-analyzer");
        assert_eq!(
            lsp_spec_for_path("a.ts").unwrap().command[0],
            "typescript-language-server"
        );
        assert_eq!(lsp_spec_for_path("a.cpp").unwrap().command[0], "clangd");
        // Java deferred: no LSP server registered.
        assert!(lsp_spec_for_path("a.java").is_none());
    }
}
