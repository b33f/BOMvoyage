//! Cargo.toml and Cargo.lock parsing (TOML via the `toml` crate).

use super::super::{Eco, PkgRef, Scope};

fn pkg(name: &str, version: &str, requirement: Option<String>, scope: Scope) -> PkgRef {
    PkgRef {
        name: name.to_string(),
        version: version.to_string(),
        requirement,
        eco: Eco::Rust,
        scope,
    }
}

/// `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` tables,
/// plus `[target.'<cfg>'.dependencies]` variants.
pub fn parse_cargo_toml(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let push_table = |deps: &toml::Table, out: &mut Vec<PkgRef>| {
        for (name, spec) in deps {
            let requirement = match spec {
                toml::Value::String(s) => Some(s.clone()),
                toml::Value::Table(t) => t
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                _ => None,
            };
            out.push(pkg(name, "", requirement, Scope::Direct));
        }
    };
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = value.get(table).and_then(|v| v.as_table()) {
            push_table(deps, &mut out);
        }
    }
    if let Some(targets) = value.get("target").and_then(|v| v.as_table()) {
        for target in targets.values() {
            for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = target.get(table).and_then(|v| v.as_table()) {
                    push_table(deps, &mut out);
                }
            }
        }
    }
    Ok(out)
}

/// Cargo.lock: every `[[package]]` with its exact version — the full
/// transitive closure as a flat list.
pub fn parse_cargo_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let Some(packages) = value.get("package").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for p in packages {
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("lockfile package without a name")?;
        let version = p
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        out.push(pkg(
            name,
            version,
            Some(version.to_string()),
            Scope::Transitive,
        ));
    }
    Ok(out)
}

/// Cargo.lock dependency EDGES: for each `[[package]]`, an edge to every name
/// in its `dependencies` list (version resolved from the lock's own entries).
pub fn cargo_lock_edges(text: &str) -> Result<Vec<(String, String, &'static str)>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let Some(packages) = value.get("package").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    // name -> (version, purl), resolved once. A lockfile can carry multiple
    // versions of one crate (name+version is the real key); keep them all.
    let mut by_name: Vec<(String, String)> = Vec::new();
    let mut entries: Vec<(String, String, Vec<String>)> = Vec::new();
    for p in packages {
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let version = p
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let deps: Vec<String> = p
            .get("dependencies")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str())
                    // Entries look like `serde`, `serde 1.0.200`, or
                    // `serde 1.0.200 (registry+...)` — keep the name and the
                    // version when present.
                    .map(|d| {
                        let mut parts = d.split_whitespace();
                        (
                            parts.next().unwrap_or("").to_string(),
                            parts.next().unwrap_or("").to_string(),
                        )
                    })
                    .collect::<Vec<(String, String)>>()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(n, _v)| n)
            .collect();
        by_name.push((name.clone(), version.clone()));
        entries.push((name, version, deps));
    }
    let purl_of = |name: &str, version: &str| format!("pkg:cargo/{name}@{version}");
    let mut out = Vec::new();
    for (name, version, deps) in &entries {
        let from = purl_of(name, version);
        for dep in deps {
            // Resolve the dep to a version via the lock's own entries; when
            // several versions exist, emit an edge to each (the graph shows
            // the true multi-version reality).
            let mut matched = false;
            for (dn, dv) in &by_name {
                if dn == dep {
                    out.push((from.clone(), purl_of(dn, dv), "lockfile"));
                    matched = true;
                }
            }
            if !matched {
                // Dep not in the lock's package list (renamed/optional edge):
                // emit a versionless edge so the name is never lost.
                out.push((from.clone(), format!("pkg:cargo/{dep}"), "lockfile"));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml_parses_string_and_table_deps() {
        let text = r#"
[package]
name = "x"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1.38", features = ["full"] }
[target.'cfg(unix)'.dependencies]
libc = "0.2"
"#;
        let pkgs = parse_cargo_toml(text).expect("parses");
        let mut names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["libc", "serde", "tokio"]);
        let tokio = pkgs
            .iter()
            .find(|p| p.name == "tokio")
            .expect("tokio present");
        assert_eq!(tokio.requirement.as_deref(), Some("1.38"));
        assert!(
            tokio.version.is_empty(),
            "manifests carry requirements, not versions"
        );
        assert_eq!(tokio.scope, Scope::Direct);
    }

    #[test]
    fn cargo_lock_parses_packages_and_edges() {
        let text = r#"
[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["serde_derive 1.0.200"]

[[package]]
name = "serde_derive"
version = "1.0.200"

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["serde"]
"#;
        let pkgs = parse_cargo_lock(text).expect("parses");
        assert_eq!(pkgs.len(), 3);
        let edges = cargo_lock_edges(text).expect("edges");
        assert!(edges.contains(&(
            "pkg:cargo/serde@1.0.200".to_string(),
            "pkg:cargo/serde_derive@1.0.200".to_string(),
            "lockfile"
        )));
        assert!(edges.contains(&(
            "pkg:cargo/app@0.1.0".to_string(),
            "pkg:cargo/serde@1.0.200".to_string(),
            "lockfile"
        )));
    }
}
