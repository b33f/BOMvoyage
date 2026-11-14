//! Node.js / TypeScript / JavaScript: `package.json`, `package-lock.json`
//! (and `npm-shrinkwrap.json`), `yarn.lock` (classic v1 and Berry), and
//! `pnpm-lock.yaml` (v5 / v6 / v9).
//!
//! Like Cargo, the plain manifest yields DIRECT, versionless packages
//! (`pkg:npm/react`) carrying the requirement as written, and the lockfiles
//! yield the exact, versioned transitive closure plus real pkg→pkg edges.
//! Lockfiles are parsed line-by-line where they are YAML-ish, so no YAML
//! dependency is pulled in; unknown shapes degrade to fewer packages, never
//! to wrong ones.

use std::collections::BTreeMap;

use super::super::{Eco, PkgRef, Scope};

fn pkg(name: &str, version: &str, requirement: Option<String>, scope: Scope) -> PkgRef {
    PkgRef {
        name: name.to_string(),
        version: version.to_string(),
        requirement,
        eco: Eco::Node,
        scope,
    }
}

/// purl for an npm package. Scoped names (`@types/node`) put the scope in
/// the purl namespace with its `@` percent-encoded, per the purl spec.
pub fn npm_purl(name: &str, version: &str) -> String {
    let base = match name.strip_prefix('@') {
        Some(scoped) => format!("pkg:npm/%40{scoped}"),
        None => format!("pkg:npm/{name}"),
    };
    if version.is_empty() {
        base
    } else {
        format!("{base}@{version}")
    }
}

const DEP_TABLES: &[&str] = &["dependencies", "devDependencies", "optionalDependencies"];

// ---------------------------------------------------------------- package.json

/// `dependencies` / `devDependencies` / `optionalDependencies`. Workspace and
/// local links (`workspace:`, `file:`, `link:`) are the repo's own code, not
/// third-party dependencies, and are skipped.
pub fn parse_package_json(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = BTreeMap::new();
    for table in DEP_TABLES {
        let Some(deps) = value.get(table).and_then(|v| v.as_object()) else {
            continue;
        };
        for (name, spec) in deps {
            let spec = spec.as_str().unwrap_or_default();
            if is_local_spec(spec) {
                continue;
            }
            seen.entry(name.clone())
                .or_insert_with(|| pkg(name, "", Some(spec.to_string()), Scope::Direct));
        }
    }
    Ok(seen.into_values().collect())
}

fn is_local_spec(spec: &str) -> bool {
    ["workspace:", "file:", "link:", "portal:"]
        .iter()
        .any(|p| spec.starts_with(p))
}

// ---------------------------------------------------------------- package-lock

/// Package name from a lockfile v2/v3 `packages` key:
/// `node_modules/a/node_modules/@s/b` → `@s/b`.
fn name_from_lock_path(path: &str) -> Option<&str> {
    let idx = path.rfind("node_modules/")?;
    Some(&path[idx + "node_modules/".len()..])
}

/// One resolved install location in a package-lock.
struct LockEntry {
    path: String,
    name: String,
    version: String,
    deps: Vec<String>,
}

fn lock_entries(value: &serde_json::Value) -> Vec<LockEntry> {
    let mut out = Vec::new();
    if let Some(packages) = value.get("packages").and_then(|v| v.as_object()) {
        // lockfileVersion 2/3: flat map of install paths.
        for (path, entry) in packages {
            if path.is_empty() || entry.get("link").and_then(|v| v.as_bool()) == Some(true) {
                continue; // the root project / workspace symlinks
            }
            let Some(name) = entry
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| name_from_lock_path(path))
            else {
                continue; // a workspace folder, not an installed package
            };
            let version = entry
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let mut deps = Vec::new();
            for table in ["dependencies", "optionalDependencies", "peerDependencies"] {
                if let Some(d) = entry.get(table).and_then(|v| v.as_object()) {
                    deps.extend(d.keys().cloned());
                }
            }
            out.push(LockEntry {
                path: path.clone(),
                name: name.to_string(),
                version: version.to_string(),
                deps,
            });
        }
    } else if let Some(deps) = value.get("dependencies").and_then(|v| v.as_object()) {
        // lockfileVersion 1: a nested tree; synthesize v2-style paths.
        fn walk(
            prefix: &str,
            deps: &serde_json::Map<String, serde_json::Value>,
            out: &mut Vec<LockEntry>,
        ) {
            for (name, entry) in deps {
                let path = format!("{prefix}node_modules/{name}");
                let version = entry
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if version.starts_with("file:") || version.starts_with("link:") {
                    continue;
                }
                let requires = entry
                    .get("requires")
                    .and_then(|v| v.as_object())
                    .map(|r| r.keys().cloned().collect())
                    .unwrap_or_default();
                out.push(LockEntry {
                    path: path.clone(),
                    name: name.clone(),
                    version: version.to_string(),
                    deps: requires,
                });
                if let Some(nested) = entry.get("dependencies").and_then(|v| v.as_object()) {
                    walk(&format!("{path}/"), nested, out);
                }
            }
        }
        walk("", deps, &mut out);
    }
    out
}

/// package-lock.json / npm-shrinkwrap.json: every installed package version.
pub fn parse_package_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut seen = BTreeMap::new();
    for e in lock_entries(&value) {
        if e.version.is_empty() {
            continue;
        }
        seen.entry(npm_purl(&e.name, &e.version))
            .or_insert_with(|| {
                pkg(
                    &e.name,
                    &e.version,
                    Some(e.version.clone()),
                    Scope::Transitive,
                )
            });
    }
    Ok(seen.into_values().collect())
}

/// package-lock edges, resolved the way Node resolves `require`: from the
/// dependent's install path, look in its own `node_modules`, then walk up.
pub fn package_lock_edges(text: &str) -> Result<Vec<(String, String, &'static str)>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let entries = lock_entries(&value);
    let by_path: BTreeMap<&str, &LockEntry> =
        entries.iter().map(|e| (e.path.as_str(), e)).collect();

    let mut out = Vec::new();
    let mut push = |from: String, dep: &str, base: &str| {
        let mut dir = base.to_string();
        loop {
            let candidate = if dir.is_empty() {
                format!("node_modules/{dep}")
            } else {
                format!("{dir}/node_modules/{dep}")
            };
            if let Some(t) = by_path.get(candidate.as_str()) {
                out.push((from.clone(), npm_purl(&t.name, &t.version), "lockfile"));
                return;
            }
            if dir.is_empty() {
                return; // unresolved (optional/peer not installed)
            }
            // Up one install level: strip the trailing `node_modules/<name>`.
            dir = match dir.rfind("/node_modules/") {
                Some(i) => dir[..i].to_string(),
                None => String::new(),
            };
        }
    };

    // The project root: edges from the root package to what it pulls in.
    if let Some(root) = root_identity(&value) {
        let root_deps: Vec<String> = value
            .get("packages")
            .and_then(|p| p.get(""))
            .map(|r| {
                DEP_TABLES
                    .iter()
                    .filter_map(|t| r.get(*t).and_then(|v| v.as_object()))
                    .flat_map(|m| m.keys().cloned())
                    .collect()
            })
            .unwrap_or_default();
        for dep in root_deps {
            push(root.clone(), &dep, "");
        }
    }
    for e in &entries {
        if e.version.is_empty() {
            continue;
        }
        let from = npm_purl(&e.name, &e.version);
        for dep in &e.deps {
            push(from.clone(), dep, &e.path);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn root_identity(value: &serde_json::Value) -> Option<String> {
    let name = value.get("name").and_then(|v| v.as_str())?;
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");
    Some(npm_purl(name, version))
}

// ---------------------------------------------------------------- yarn.lock

struct YarnEntry {
    /// Every `name@range` selector the block answers to.
    selectors: Vec<String>,
    name: String,
    version: String,
    /// (dep name, range) pairs.
    deps: Vec<(String, String)>,
}

/// Name part of a yarn selector: `@babel/core@^7.0.0` → `@babel/core`,
/// `lodash@npm:^4` → `lodash`.
fn selector_name(sel: &str) -> &str {
    let search_from = usize::from(sel.starts_with('@'));
    match sel[search_from..].find('@') {
        Some(i) => &sel[..i + search_from],
        None => sel,
    }
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches('"').trim_matches('\'')
}

/// Parse both lockfile dialects: classic (`version "1.2.3"`) and Berry
/// (`version: 1.2.3`, `"name@npm:^1"` selectors, `__metadata` block).
fn yarn_entries(text: &str) -> Vec<YarnEntry> {
    let mut out: Vec<YarnEntry> = Vec::new();
    let mut in_deps = false;
    for raw in text.lines() {
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if indent == 0 {
            in_deps = false;
            let header = line.trim_end_matches(':');
            if header == "__metadata" {
                out.push(YarnEntry {
                    selectors: vec![],
                    name: String::new(),
                    version: String::new(),
                    deps: vec![],
                });
                continue;
            }
            let selectors: Vec<String> =
                header.split(", ").map(|s| unquote(s).to_string()).collect();
            let name = selectors
                .first()
                .map(|s| selector_name(s).to_string())
                .unwrap_or_default();
            out.push(YarnEntry {
                selectors,
                name,
                version: String::new(),
                deps: vec![],
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        if indent <= 2 {
            in_deps = matches!(
                line.trim_end_matches(':'),
                "dependencies" | "optionalDependencies"
            ) && line.ends_with(':');
            if let Some(v) = line.strip_prefix("version ") {
                cur.version = unquote(v).to_string();
            } else if let Some(v) = line.strip_prefix("version:") {
                cur.version = unquote(v).to_string();
            }
            continue;
        }
        if in_deps {
            // classic: `name "^1.0"`; berry: `name: ^1.0` (names may be quoted).
            let (dep, range) = if let Some((d, r)) = split_berry_kv(line) {
                (d, r)
            } else if let Some((d, r)) = line.split_once(' ') {
                (unquote(d).to_string(), unquote(r).to_string())
            } else {
                continue;
            };
            cur.deps.push((dep, range));
        }
    }
    out.retain(|e| !e.name.is_empty() && !e.version.is_empty() && !e.version.contains("use.local"));
    // Berry marks the workspace itself with a `0.0.0-use.local` version.
    out.retain(|e| !e.selectors.iter().any(|s| s.contains("@workspace:")));
    out
}

/// `"@s/n": "npm:^1"` / `name: ^1` → (name, range). The `: ` separator must
/// come after any scoped `@` and quoted name.
fn split_berry_kv(line: &str) -> Option<(String, String)> {
    let (k, v) = if let Some(rest) = line.strip_prefix('"') {
        let close = rest.find('"')?;
        let after = rest[close + 1..].strip_prefix(':')?;
        (&rest[..close], after)
    } else {
        line.split_once(": ")?
    };
    Some((k.trim().to_string(), unquote(v).to_string()))
}

pub fn parse_yarn_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut seen = BTreeMap::new();
    for e in yarn_entries(text) {
        seen.entry(npm_purl(&e.name, &e.version))
            .or_insert_with(|| {
                pkg(
                    &e.name,
                    &e.version,
                    Some(e.version.clone()),
                    Scope::Transitive,
                )
            });
    }
    Ok(seen.into_values().collect())
}

pub fn yarn_lock_edges(text: &str) -> Result<Vec<(String, String, &'static str)>, String> {
    let entries = yarn_entries(text);
    let mut by_selector: BTreeMap<String, &YarnEntry> = BTreeMap::new();
    for e in &entries {
        for s in &e.selectors {
            by_selector.insert(s.clone(), e);
        }
    }
    let mut out = Vec::new();
    for e in &entries {
        let from = npm_purl(&e.name, &e.version);
        for (dep, range) in &e.deps {
            // Classic selectors are `name@range`; Berry's are `name@npm:range`
            // (the range in the deps list may already carry the `npm:`).
            let candidates = [format!("{dep}@{range}"), format!("{dep}@npm:{range}")];
            if let Some(t) = candidates.iter().find_map(|c| by_selector.get(c)) {
                out.push((from.clone(), npm_purl(&t.name, &t.version), "lockfile"));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------- pnpm-lock

/// Strip pnpm's peer-dependency suffixes: `1.2.3(react@18.2.0)` and the v5
/// `1.2.3_react@18.2.0` form.
fn pnpm_clean_version(v: &str) -> &str {
    let v = v.split('(').next().unwrap_or(v);
    v.split('_').next().unwrap_or(v)
}

/// A `packages:`/`snapshots:` key → (name, version). Handles v5 `/name/1.0.0`,
/// v6 `/name@1.0.0`, v9 `name@1.0.0`, scoped names, and peer suffixes.
fn pnpm_key(key: &str) -> Option<(String, String)> {
    let key = key.split(": ").next().unwrap_or(key);
    let key = unquote(key.trim_end_matches(':'));
    let key = key.strip_prefix('/').unwrap_or(key);
    let key = key.split('(').next().unwrap_or(key);
    let from = usize::from(key.starts_with('@'));
    // v5: name/version or @s/name/version, where the version segment starts
    // with a digit (its `_peer@x` suffix would otherwise look like v6).
    let name_slash = if from == 1 {
        key.find('/')
            .and_then(|s| key[s + 1..].find('/').map(|t| s + 1 + t))
    } else {
        key.find('/')
    };
    if let Some(i) = name_slash {
        if key[i + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            return Some((
                key[..i].to_string(),
                pnpm_clean_version(&key[i + 1..]).to_string(),
            ));
        }
    }
    // v6 / v9: name@version (skip the scope's leading '@').
    if let Some(i) = key[from..].find('@') {
        let (name, version) = (&key[..i + from], &key[i + from + 1..]);
        return Some((name.to_string(), pnpm_clean_version(version).to_string()));
    }
    // v5: name/version (scoped: @s/name/version).
    let i = key.rfind('/')?;
    Some((
        key[..i].to_string(),
        pnpm_clean_version(&key[i + 1..]).to_string(),
    ))
}

struct PnpmEntry {
    name: String,
    version: String,
    deps: Vec<(String, String)>,
}

fn pnpm_entries(text: &str) -> Vec<PnpmEntry> {
    // v9 splits metadata (`packages:`) from the dependency graph
    // (`snapshots:`); earlier versions keep both under `packages:`. Merge
    // by (name, version).
    let mut map: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    let mut section = "";
    let mut current: Option<(String, String)> = None;
    let mut in_deps = false;
    for raw in text.lines() {
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let line = raw.trim();
        if indent == 0 {
            section = match line {
                "packages:" => "packages",
                "snapshots:" => "snapshots",
                _ => "",
            };
            current = None;
            continue;
        }
        if section.is_empty() {
            continue;
        }
        if indent == 2 {
            in_deps = false;
            current = pnpm_key(line).filter(|(_, v)| !v.is_empty() && !v.starts_with("link:"));
            if let Some(k) = &current {
                map.entry(k.clone()).or_default();
            }
            continue;
        }
        let Some(k) = &current else { continue };
        if indent == 4 {
            in_deps = matches!(line, "dependencies:" | "optionalDependencies:");
            continue;
        }
        if in_deps && indent >= 6 {
            if let Some((dep, ver)) = split_berry_kv(line) {
                if ver.starts_with("link:") || ver.starts_with("file:") {
                    continue;
                }
                // A value is a version (possibly peer-suffixed) or, for npm
                // aliases, a package key (`/x/1.0.0`, `x@1.0.0`).
                let is_alias = ver.starts_with('/')
                    || ver.contains('@') && !ver.starts_with(|c: char| c.is_ascii_digit());
                let (dep, ver) = match is_alias.then(|| pnpm_key(&ver)).flatten() {
                    Some(aliased) => aliased,
                    None => (dep, pnpm_clean_version(&ver).to_string()),
                };
                map.entry(k.clone()).or_default().push((dep, ver));
            }
        }
    }
    map.into_iter()
        .map(|((name, version), deps)| PnpmEntry {
            name,
            version,
            deps,
        })
        .collect()
}

pub fn parse_pnpm_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    Ok(pnpm_entries(text)
        .into_iter()
        .map(|e| {
            pkg(
                &e.name,
                &e.version,
                Some(e.version.clone()),
                Scope::Transitive,
            )
        })
        .collect())
}

pub fn pnpm_lock_edges(text: &str) -> Result<Vec<(String, String, &'static str)>, String> {
    let mut out = Vec::new();
    for e in pnpm_entries(text) {
        let from = npm_purl(&e.name, &e.version);
        for (dep, ver) in &e.deps {
            out.push((from.clone(), npm_purl(dep, ver), "lockfile"));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purl_encodes_scopes() {
        assert_eq!(npm_purl("react", "18.2.0"), "pkg:npm/react@18.2.0");
        assert_eq!(npm_purl("@types/node", ""), "pkg:npm/%40types/node");
    }

    #[test]
    fn package_json_reads_all_dep_tables_and_skips_local() {
        let text = r#"{
          "name": "app",
          "dependencies": {"react": "^18.2.0", "shared": "workspace:*"},
          "devDependencies": {"typescript": "~5.4.0", "@types/node": "^20"},
          "optionalDependencies": {"fsevents": "2.3.3"}
        }"#;
        let pkgs = parse_package_json(text).unwrap();
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["@types/node", "fsevents", "react", "typescript"]
        );
        assert!(pkgs
            .iter()
            .all(|p| p.version.is_empty() && p.scope == Scope::Direct));
        assert_eq!(pkgs[2].requirement.as_deref(), Some("^18.2.0"));
    }

    #[test]
    fn package_lock_v3_packages_and_nested_resolution() {
        let text = r#"{
          "name": "app", "version": "1.0.0", "lockfileVersion": 3,
          "packages": {
            "": {"name": "app", "dependencies": {"a": "^1"}},
            "node_modules/a": {"version": "1.0.0", "dependencies": {"b": "^2"}},
            "node_modules/b": {"version": "3.0.0"},
            "node_modules/a/node_modules/b": {"version": "2.1.0"},
            "node_modules/@s/c": {"version": "0.1.0", "dependencies": {"b": "*"}},
            "packages/local": {"name": "local", "version": "0.0.1"},
            "node_modules/local": {"link": true, "resolved": "packages/local"}
          }
        }"#;
        let pkgs = parse_package_lock(text).unwrap();
        let purls: Vec<String> = pkgs.iter().map(|p| p.purl()).collect();
        assert!(purls.contains(&"pkg:npm/b@2.1.0".to_string()));
        assert!(purls.contains(&"pkg:npm/b@3.0.0".to_string()));
        assert!(purls.contains(&"pkg:npm/%40s/c@0.1.0".to_string()));
        let edges = package_lock_edges(text).unwrap();
        // a's nested b wins over the hoisted one.
        assert!(edges.contains(&(
            "pkg:npm/a@1.0.0".into(),
            "pkg:npm/b@2.1.0".into(),
            "lockfile"
        )));
        // @s/c has no nested b: resolves to the hoisted one.
        assert!(edges.contains(&(
            "pkg:npm/%40s/c@0.1.0".into(),
            "pkg:npm/b@3.0.0".into(),
            "lockfile"
        )));
        assert!(edges.contains(&(
            "pkg:npm/app@1.0.0".into(),
            "pkg:npm/a@1.0.0".into(),
            "lockfile"
        )));
    }

    #[test]
    fn package_lock_v1_tree() {
        let text = r#"{
          "name": "app", "version": "1.0.0", "lockfileVersion": 1,
          "dependencies": {
            "a": {"version": "1.0.0", "requires": {"b": "^2"},
                  "dependencies": {"b": {"version": "2.0.0"}}},
            "b": {"version": "3.0.0"}
          }
        }"#;
        assert_eq!(parse_package_lock(text).unwrap().len(), 3);
        let edges = package_lock_edges(text).unwrap();
        assert!(edges.contains(&(
            "pkg:npm/a@1.0.0".into(),
            "pkg:npm/b@2.0.0".into(),
            "lockfile"
        )));
    }

    #[test]
    fn yarn_classic_lock() {
        let text = r#"# THIS IS AN AUTOGENERATED FILE.
# yarn lockfile v1


"@babel/core@^7.0.0", "@babel/core@^7.1.0":
  version "7.24.0"
  resolved "https://registry.yarnpkg.com/@babel/core/-/core-7.24.0.tgz"
  dependencies:
    debug "^4.1.0"

debug@^4.1.0:
  version "4.3.4"
"#;
        let pkgs = parse_yarn_lock(text).unwrap();
        let purls: Vec<String> = pkgs.iter().map(|p| p.purl()).collect();
        assert_eq!(
            purls,
            vec!["pkg:npm/%40babel/core@7.24.0", "pkg:npm/debug@4.3.4"]
        );
        let edges = yarn_lock_edges(text).unwrap();
        assert_eq!(
            edges,
            vec![(
                "pkg:npm/%40babel/core@7.24.0".into(),
                "pkg:npm/debug@4.3.4".into(),
                "lockfile"
            )]
        );
    }

    #[test]
    fn yarn_berry_lock() {
        let text = r#"__metadata:
  version: 6

"app@workspace:.":
  version: 0.0.0-use.local
  dependencies:
    left-pad: ^1.3.0

"left-pad@npm:^1.3.0":
  version: 1.3.0
  dependencies:
    "@s/x": "npm:^2.0.0"

"@s/x@npm:^2.0.0":
  version: 2.0.1
"#;
        let pkgs = parse_yarn_lock(text).unwrap();
        let purls: Vec<String> = pkgs.iter().map(|p| p.purl()).collect();
        assert_eq!(
            purls,
            vec!["pkg:npm/%40s/x@2.0.1", "pkg:npm/left-pad@1.3.0"]
        );
        let edges = yarn_lock_edges(text).unwrap();
        assert_eq!(
            edges,
            vec![(
                "pkg:npm/left-pad@1.3.0".into(),
                "pkg:npm/%40s/x@2.0.1".into(),
                "lockfile"
            )]
        );
    }

    #[test]
    fn pnpm_v9_and_v6_locks() {
        let v9 = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      react-dom:
        specifier: ^18
        version: 18.2.0(react@18.2.0)

packages:
  react@18.2.0:
    resolution: {integrity: sha512-x}
  react-dom@18.2.0:
    resolution: {integrity: sha512-y}
  '@types/node@20.1.0':
    resolution: {integrity: sha512-z}

snapshots:
  react@18.2.0: {}
  react-dom@18.2.0(react@18.2.0):
    dependencies:
      react: 18.2.0
"#;
        let purls: Vec<String> = parse_pnpm_lock(v9)
            .unwrap()
            .iter()
            .map(|p| p.purl())
            .collect();
        assert_eq!(
            purls,
            vec![
                "pkg:npm/%40types/node@20.1.0",
                "pkg:npm/react@18.2.0",
                "pkg:npm/react-dom@18.2.0"
            ]
        );
        assert_eq!(
            pnpm_lock_edges(v9).unwrap(),
            vec![(
                "pkg:npm/react-dom@18.2.0".into(),
                "pkg:npm/react@18.2.0".into(),
                "lockfile"
            )]
        );

        let v5 = r#"lockfileVersion: 5.4

packages:

  /react-dom/18.2.0_react@18.2.0:
    resolution: {integrity: sha512-y}
    dependencies:
      loose-envify: 1.4.0
      react: 18.2.0

  /@babel/core/7.24.0:
    resolution: {integrity: sha512-q}
"#;
        let purls: Vec<String> = parse_pnpm_lock(v5)
            .unwrap()
            .iter()
            .map(|p| p.purl())
            .collect();
        assert_eq!(
            purls,
            vec!["pkg:npm/%40babel/core@7.24.0", "pkg:npm/react-dom@18.2.0"]
        );
        let edges = pnpm_lock_edges(v5).unwrap();
        assert!(edges.contains(&(
            "pkg:npm/react-dom@18.2.0".into(),
            "pkg:npm/react@18.2.0".into(),
            "lockfile"
        )));
    }
}
