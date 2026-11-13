//! go.mod (hand-parsed — it is NOT TOML despite the surface resemblance) and
//! go.sum parsing.

use std::collections::BTreeSet;

use super::super::{Eco, PkgRef, Scope};

fn pkg(module: &str, version: &str, scope: Scope) -> PkgRef {
    PkgRef {
        name: module.to_string(),
        version: version.to_string(),
        requirement: (!version.is_empty()).then(|| version.to_string()),
        eco: Eco::Go,
        scope,
    }
}

fn purl(module: &str, version: &str) -> String {
    if version.is_empty() {
        format!("pkg:golang/{module}")
    } else {
        format!("pkg:golang/{module}@{version}")
    }
}

/// go.mod: `module` line, `require` lines and `require ( ... )` blocks.
/// A `// indirect` marker marks the dependency transitive.
pub fn parse_go_mod(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if let Some(module) = trimmed.strip_prefix("module ") {
            let module = module.trim().trim_matches('"');
            out.push(pkg(module, "", Scope::Direct));
            continue;
        }
        let indirect = trimmed.contains("// indirect");
        // Strip the comment before parsing module/version tokens.
        let code = trimmed.split("//").next().unwrap_or("").trim();
        if code == "require (" {
            in_block = true;
            continue;
        }
        if in_block && code == ")" {
            in_block = false;
            continue;
        }
        let require_line = if in_block {
            Some(code)
        } else {
            code.strip_prefix("require ").map(|rest| rest.trim())
        };
        if let Some(rest) = require_line {
            let mut parts = rest.split_whitespace();
            let module = parts.next().unwrap_or_default().trim_matches('"');
            let version = parts.next().unwrap_or_default().trim_matches('"');
            if module.is_empty() {
                return Err("require entry without a module path".to_string());
            }
            let scope = if indirect {
                Scope::Transitive
            } else {
                Scope::Direct
            };
            out.push(pkg(module, version, scope));
        }
    }
    Ok(out)
}

/// go.mod EDGES: root module → every require entry (direct and indirect
/// alike). go.mod does not carry module-to-module edges, so this is the
/// honest topology the file itself expresses.
pub fn go_mod_edges(text: &str) -> Result<Vec<(String, String, &'static str)>, String> {
    let pkgs = parse_go_mod(text)?;
    let root = pkgs
        .first()
        .filter(|p| p.version.is_empty())
        .ok_or("go.mod has no module line")?;
    let root_purl = purl(&root.name, "");
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for dep in &pkgs {
        if dep.name == root.name {
            continue;
        }
        if seen.insert(purl(&dep.name, &dep.version)) {
            out.push((root_purl.clone(), purl(&dep.name, &dep.version), "go-mod"));
        }
    }
    Ok(out)
}

/// go.sum: `<module> <version> <hash>` lines (plus `<module> <version>/go.mod`
/// variants). Exact resolved versions for every module ever seen.
pub fn parse_go_sum(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let module = parts.next().unwrap_or_default();
        let version = parts.next().unwrap_or_default();
        if module.is_empty() || version.is_empty() {
            continue; // blank or malformed line
        }
        if version.ends_with("/go.mod") {
            continue;
        }
        if seen.insert((module, version)) {
            out.push(pkg(module, version, Scope::Transitive));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_mod_parses_direct_and_indirect() {
        let text = r#"
module example.com/app

go 1.22

require (
	github.com/pkg/errors v0.9.1
	golang.org/x/mod v0.17.0 // indirect
)

require golang.org/x/sys v0.20.0 // indirect
"#;
        let pkgs = parse_go_mod(text).expect("parses");
        assert_eq!(pkgs.len(), 4, "root module + 3 requires");
        let root = pkgs.first().expect("root first");
        assert_eq!(root.name, "example.com/app");
        assert_eq!(root.version, "");
        let errors = pkgs
            .iter()
            .find(|p| p.name == "github.com/pkg/errors")
            .expect("errors");
        assert_eq!(errors.version, "v0.9.1");
        assert_eq!(errors.scope, Scope::Direct);
        let xmod = pkgs
            .iter()
            .find(|p| p.name == "golang.org/x/mod")
            .expect("x/mod");
        assert_eq!(xmod.scope, Scope::Transitive);
        let xsys = pkgs
            .iter()
            .find(|p| p.name == "golang.org/x/sys")
            .expect("x/sys");
        assert_eq!(xsys.scope, Scope::Transitive);

        let edges = go_mod_edges(text).expect("edges");
        assert_eq!(edges.len(), 3, "root has three require entries");
        assert!(edges.iter().all(|e| e.0 == "pkg:golang/example.com/app"));
    }

    #[test]
    fn go_sum_dedups_and_skips_gomod_variants() {
        let text = "\
github.com/pkg/errors v0.9.1 h1:abc=
github.com/pkg/errors v0.9.1/go.mod h1:def=
github.com/pkg/errors v0.9.1 h1:ghi=
";
        let pkgs = parse_go_sum(text).expect("parses");
        assert_eq!(pkgs.len(), 1, "dedup module+version, skip /go.mod");
        assert_eq!(pkgs[0].version, "v0.9.1");
        assert_eq!(pkgs[0].scope, Scope::Transitive);
    }
}
