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
            // npm names are the import specifier verbatim (`@types/node`,
            // `lodash.merge`); never strip the scope.
            Eco::Node => p.name.clone(),
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
                Eco::Node => "js",
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

    // Per-file scan in parallel: reading and line-scanning a source file
    // shares nothing with the others, and monorepos run to tens of thousands
    // of them. Workers pull indices off a shared counter and tag their finds
    // with the file index, so the merged order matches the serial walk.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);
    let next_file = std::sync::atomic::AtomicUsize::new(0);
    let partial: std::sync::Mutex<Vec<(usize, Vec<ImportSite>)>> =
        std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut local: Vec<(usize, Vec<ImportSite>)> = Vec::new();
                loop {
                    let i = next_file.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    let (path, language) = &files[i];
                    let Ok(content) = std::fs::read(repo.join(path)) else {
                        continue;
                    };
                    let Ok(text) = String::from_utf8(content) else {
                        continue;
                    };
                    let mut sites = Vec::new();
                    for (key, symbol) in extract_symbols(&text, language) {
                        if let Some(purl) = by_key.get(&normalize(&key)) {
                            sites.push(ImportSite {
                                file: path.clone(),
                                purl: purl.clone(),
                                symbol,
                            });
                        } else if *language == "go" {
                            // Go import paths are module-path prefixes: match
                            // the longest.
                            if let Some(purl) = longest_prefix(&by_key, &key) {
                                sites.push(ImportSite {
                                    file: path.clone(),
                                    purl,
                                    symbol,
                                });
                            }
                        }
                    }
                    if !sites.is_empty() {
                        local.push((i, sites));
                    }
                }
                if !local.is_empty() {
                    partial.lock().expect("usage partial mutex").extend(local);
                }
            });
        }
    });
    let mut tagged = partial.into_inner().expect("usage partial mutex");
    tagged.sort_by_key(|(i, _)| *i);
    let out = tagged.into_iter().flat_map(|(_, sites)| sites).collect();
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
            "js" => {
                for spec in js_specifiers(line) {
                    if let Some(pkg) = js_package_of(&spec) {
                        out.push((pkg.to_string(), spec));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Module specifiers on one JS/TS line: `import … from 'x'`, `import 'x'`,
/// `export … from 'x'`, `require('x')`, `import('x')` (string literals only),
/// and the closing `} from 'x'` line of a multi-line import.
fn js_specifiers(line: &str) -> Vec<String> {
    if line.starts_with("//") || line.starts_with('*') || line.starts_with("/*") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let quoted_after = |rest: &str| -> Option<String> {
        let rest = rest.trim_start();
        let q = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"' | '`'))?;
        let body = &rest[1..];
        let end = body.find(q)?;
        let spec = &body[..end];
        (!spec.is_empty() && !spec.contains("${")).then(|| spec.to_string())
    };
    let is_statement = line.starts_with("import")
        || line.starts_with("export")
        || line.starts_with('}')
        || line.contains(" from ");
    if is_statement {
        if let Some(i) = line.rfind(" from ") {
            out.extend(quoted_after(&line[i + 6..]));
        } else if let Some(rest) = line.strip_prefix("}from") {
            out.extend(quoted_after(rest));
        } else if let Some(rest) = line.strip_prefix("import ") {
            out.extend(quoted_after(rest)); // side-effect import 'x'
        }
    }
    for call in ["require(", "import("] {
        let mut from = 0;
        while let Some(i) = line[from..].find(call) {
            let start = from + i;
            // `foo.require(` / `myimport(` are not module loads.
            let prev = line[..start].chars().next_back();
            if !prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '$') {
                out.extend(quoted_after(&line[start + call.len()..]));
            }
            from = start + call.len();
        }
    }
    out
}

/// The npm package a bare specifier resolves to (`@s/n/sub` → `@s/n`,
/// `lodash/fp` → `lodash`); None for relative/absolute paths, URLs, path
/// aliases (`@/x`, `~/x`, `#x`) and Node built-ins.
fn js_package_of(spec: &str) -> Option<&str> {
    if spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with('~')
        || spec.starts_with('#')
        || spec.starts_with("@/")
        || spec.starts_with("node:")
        || spec.contains("://")
    {
        return None;
    }
    let mut parts = spec.splitn(3, '/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        let end = first.len() + 1 + second.len();
        Some(&spec[..end])
    } else {
        Some(first)
    }
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
        "js" => {
            [
                ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".vue", ".svelte",
            ]
            .iter()
            .any(|ext| path.ends_with(ext))
                && !path.ends_with(".min.js")
        }
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
    fn js_imports_extract_package_names() {
        let src = r#"
import React, { useState } from 'react';
import type { Foo } from "@acme/types/sub";
import './styles.css';
import 'reflect-metadata';
const _ = require("lodash/fp");
const lazy = await import('chart.js');
export { x } from "zod";
import fs from 'node:fs';
import { a } from '@/components/a';
import {
  b,
} from 'date-fns';
// import nope from 'commented-out';
const r = myrequire('not-a-module');
"#;
        let keys: Vec<String> = extract_symbols(src, "js")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            vec![
                "react",
                "@acme/types",
                "reflect-metadata",
                "lodash",
                "chart.js",
                "zod",
                "date-fns"
            ]
        );
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
