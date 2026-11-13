//! Worktree usage scan: line-level import extraction per language, mapped
//! onto the known package universe. Honest heuristic — a symbol maps to a
//! package when the (normalized) package name matches the import root, or
//! the module path prefix matches (Go). No treesitter, no AST.

use std::collections::BTreeMap;

use super::discover::SKIP_DIRS;
use super::parse::python::normalize;
use super::{Eco, ImportSite, ManifestRef, PkgRef};
use crate::error::{ScanWarning, ToolResult};

const MAX_FILES: usize = 20000;

/// Scan the worktree for usage sites of `packages`. Returns IMPORTS triples.
/// Only the languages of the discovered manifests are walked — a Python-only
/// repo does not get its stray `.rs` files scanned.
pub fn scan_usage(
    repo: &std::path::Path,
    packages: &[PkgRef],
    manifests: &[ManifestRef],
    warnings: &mut Vec<ScanWarning>,
) -> ToolResult<Vec<ImportSite>> {
    // lookup key -> purl. Keys: normalized package base name (py/rs/java)
    // plus the full module path for Go modules.
    let mut by_key: BTreeMap<String, String> = BTreeMap::new();
    for p in packages {
        let base = match p.eco {
            Eco::Java => p.name.split(':').next_back().unwrap_or(&p.name).to_string(),
            _ => p.name.rsplit('/').next().unwrap_or(&p.name).to_string(),
        };
        by_key.entry(normalize(&base)).or_insert_with(|| p.purl());
        if matches!(p.eco, Eco::Go) && p.name.contains('/') {
            by_key.entry(p.name.clone()).or_insert_with(|| p.purl());
        }
    }

    let languages: Vec<&'static str> = {
        let mut langs: Vec<&'static str> = manifests
            .iter()
            .map(|m| match m.kind.ecosystem() {
                Eco::Python => "py",
                Eco::Java => "java",
                Eco::Rust => "rs",
                Eco::Go => "go",
            })
            .collect();
        langs.sort_unstable();
        langs.dedup();
        langs
    };

    let mut files = Vec::new();
    walk_sources(repo, repo, 0, &languages, &mut files)?;
    if files.len() > MAX_FILES {
        warnings.push(ScanWarning::CapHit {
            what: "files usage-scanned".into(),
            limit: MAX_FILES as u64,
        });
        files.truncate(MAX_FILES);
    }

    let mut out = Vec::new();
    for (path, language) in files {
        let Ok(content) = std::fs::read(repo.join(&path)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(content) else {
            continue;
        };
        for (key, symbol) in extract_symbols(&text, language) {
            if let Some(purl) = by_key.get(&normalize(&key)) {
                out.push(ImportSite {
                    file: path.clone(),
                    purl: purl.clone(),
                    symbol,
                });
            } else if language == "go" {
                // Go import paths are module-path prefixes: match the longest.
                if let Some(purl) = longest_prefix(&by_key, &key) {
                    out.push(ImportSite {
                        file: path.clone(),
                        purl,
                        symbol,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// (lookup key, display symbol) pairs found in one file.
fn extract_symbols(text: &str, language: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        match language {
            "py" => {
                let rest = if let Some(r) = line.strip_prefix("import ") {
                    r
                } else if let Some(r) = line.strip_prefix("from ") {
                    match r.split_once(" import ") {
                        Some((module, _)) => module,
                        None => continue,
                    }
                } else {
                    continue;
                };
                let module = rest.split_whitespace().next().unwrap_or("");
                let root = module.split('.').next().unwrap_or("");
                if !root.is_empty() {
                    out.push((root.to_string(), module.to_string()));
                }
            }
            "rs" => {
                let Some(rest) = line.strip_prefix("use ") else {
                    continue;
                };
                let path_expr = rest.split(';').next().unwrap_or("").trim();
                let root = path_expr.split("::").next().unwrap_or("");
                // `crate`/`self`/`super` are internal paths, not crates.
                if !root.is_empty() && !matches!(root, "crate" | "self" | "super") {
                    out.push((root.to_string(), path_expr.to_string()));
                }
            }
            "go" => {
                // import "path" or `alias "path"` (inside import blocks too).
                let Some(q) = line.find('"') else { continue };
                let Some(close) = line[q + 1..].find('"') else {
                    continue;
                };
                let module = &line[q + 1..q + 1 + close];
                if module.contains('/') || module.contains('.') {
                    out.push((module.to_string(), module.to_string()));
                }
            }
            "java" => {
                let Some(rest) = line.strip_prefix("import ") else {
                    continue;
                };
                let rest = rest.trim().trim_end_matches(';').trim();
                let rest = rest.strip_prefix("static ").unwrap_or(rest).trim();
                let root = rest.split('.').take(2).collect::<Vec<_>>().join(".");
                if !root.is_empty() {
                    out.push((root, rest.to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

/// Longest registered module path that is a prefix of `import_path`.
fn longest_prefix(by_key: &BTreeMap<String, String>, import_path: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for (key, purl) in by_key {
        if !key.contains('/') {
            continue;
        }
        let is_prefix = import_path == key.as_str() || import_path.starts_with(&format!("{key}/"));
        let is_longer = best
            .as_ref()
            .map(|(len, _)| key.len() > *len)
            .unwrap_or(true);
        if is_prefix && is_longer {
            best = Some((key.len(), purl.clone()));
        }
    }
    best.map(|(_, purl)| purl)
}

fn walk_sources(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    languages: &[&'static str],
    files: &mut Vec<(String, &'static str)>,
) -> ToolResult<()> {
    if depth > 8 || files.len() >= MAX_FILES {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) || file_type.is_symlink() {
                continue;
            }
            walk_sources(root, &path, depth + 1, languages, files)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            for language in languages {
                if extension_matches(&rel, language) {
                    files.push((rel, language));
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Language → source extension mapping (one table, consulted by the walker).
fn extension_matches(path: &str, language: &str) -> bool {
    match language {
        "py" => path.ends_with(".py"),
        "java" => path.ends_with(".java") || path.ends_with(".kt"),
        "rs" => path.ends_with(".rs"),
        "go" => path.ends_with(".go"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_imports_extract_module_roots() {
        let out = extract_symbols(
            "import os\nfrom flask.templating import render\nimport numpy as np\n",
            "py",
        );
        assert!(out.contains(&("os".to_string(), "os".to_string())));
        assert!(out.contains(&("flask".to_string(), "flask.templating".to_string())));
        assert!(out.contains(&("numpy".to_string(), "numpy".to_string())));
    }

    #[test]
    fn rust_use_skips_internal_paths() {
        let out = extract_symbols(
            "use serde::Serialize;\nuse crate::foo;\nuse std::collections::HashMap;\n",
            "rs",
        );
        assert!(out.contains(&("serde".to_string(), "serde::Serialize".to_string())));
        assert!(!out.iter().any(|(k, _)| k == "crate"));
        assert!(out.contains(&("std".to_string(), "std::collections::HashMap".to_string())));
    }

    #[test]
    fn go_imports_extract_paths() {
        let out = extract_symbols("\t\"fmt\"\n\t\"github.com/pkg/errors\"\n", "go");
        assert!(
            !out.iter().any(|(k, _)| k == "fmt"),
            "std import without dot/slash skipped"
        );
        assert!(out.contains(&(
            "github.com/pkg/errors".to_string(),
            "github.com/pkg/errors".to_string()
        )));
    }

    #[test]
    fn longest_prefix_prefers_deepest_module() {
        let mut by_key = BTreeMap::new();
        by_key.insert(
            "github.com/foo".to_string(),
            "pkg:golang/github.com/foo@v1".to_string(),
        );
        by_key.insert(
            "github.com/foo/bar".to_string(),
            "pkg:golang/github.com/foo/bar@v2".to_string(),
        );
        assert_eq!(
            longest_prefix(&by_key, "github.com/foo/bar/baz").as_deref(),
            Some("pkg:golang/github.com/foo/bar@v2")
        );
        assert_eq!(
            longest_prefix(&by_key, "github.com/foo").as_deref(),
            Some("pkg:golang/github.com/foo@v1")
        );
        assert_eq!(longest_prefix(&by_key, "github.com/nope"), None);
    }
}
