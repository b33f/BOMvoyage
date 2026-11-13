//! Python manifests: requirements*.txt, pyproject.toml (PEP 621 +
//! poetry-style), poetry.lock and uv.lock (both TOML).

use super::super::{Eco, PkgRef, Scope};

fn pkg(name: &str, version: &str, requirement: Option<String>, scope: Scope) -> PkgRef {
    PkgRef {
        // PEP 503 normalization for the graph name; the requirement keeps
        // the original spelling.
        name: normalize(name),
        version: version.to_string(),
        requirement,
        eco: Eco::Python,
        scope,
    }
}

/// PEP 503 name normalization: lowercase, runs of `-_.` collapse to `-`.
/// Import-site matching un-names with `_` (see usage.rs).
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        }
    }
    out
}

/// requirements.txt: `name[extra] op version` lines, `#` comments, `-r`
/// includes noted and skipped.
pub fn parse_requirements(text: &str) -> Result<Vec<PkgRef>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let stripped = line.split('#').next().unwrap_or("").trim();
        if stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('-') {
            // -r include, -e editable, --index-url: not a dependency row.
            continue;
        }
        // name[extra1,extra2]>=1.0,<2.0
        let (name, requirement) = split_requirement(stripped);
        if name.is_empty() {
            return Err(format!("unparseable requirement line: {line:?}"));
        }
        out.push(pkg(&name, "", requirement, Scope::Direct));
    }
    Ok(out)
}

/// Split `name[extras]<spec>` into (name, spec). Extras are stripped from
/// the name; the spec keeps the version operators.
fn split_requirement(spec: &str) -> (String, Option<String>) {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut name = String::new();
    let mut in_extras = false;
    while i < chars.len() {
        match chars[i] {
            '[' => in_extras = true,
            ']' => in_extras = false,
            c if "=<>!~;".contains(c) => break,
            c if !in_extras => name.push(c),
            _ => {}
        }
        i += 1;
    }
    let req: String = chars[i..].iter().collect();
    let req = req.trim();
    (
        name.trim().to_string(),
        (!req.is_empty()).then(|| req.to_string()),
    )
}

/// pyproject.toml: PEP 621 `[project].dependencies` and
/// `[tool.poetry.dependencies`]. `path`/`git` deps are skipped (not from a
/// registry).
pub fn parse_pyproject(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    if let Some(deps) = value
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in deps {
            let Some(s) = dep.as_str() else { continue };
            // Skip direct URL dependencies (pip PEP 508 `name @ url`).
            if s.contains(" @ ") {
                continue;
            }
            let (name, requirement) = split_requirement(s.split('#').next().unwrap_or(s).trim());
            if name.is_empty() {
                continue;
            }
            out.push(pkg(&name, "", requirement, Scope::Direct));
        }
    }
    if let Some(deps) = value
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, spec) in deps {
            if name == "python" {
                continue;
            }
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
    }
    Ok(out)
}

/// poetry.lock: `[[package]] { name, version }` entries — flat, exact.
pub fn parse_poetry_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    lock_packages(value.as_table(), "package")
}

/// uv.lock: `[[package]]` entries with `name` + `version` (version absent for
/// the root/virtual packages — those are skipped).
pub fn parse_uv_lock(text: &str) -> Result<Vec<PkgRef>, String> {
    let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    lock_packages(value.as_table(), "package")
}

fn lock_packages(root: Option<&toml::Table>, key: &str) -> Result<Vec<PkgRef>, String> {
    let Some(packages) = root.and_then(|r| r.get(key)).and_then(|p| p.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for p in packages {
        let name = match p.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return Err("lockfile package without a name".to_string()),
        };
        // Root/virtual entries carry no version — skip them, they are not
        // dependencies.
        let Some(version) = p.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        // Source check: registry packages only. git/path sources skip.
        if let Some(source) = p.get("source").and_then(|s| s.as_table()) {
            let kind = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if matches!(kind, "git" | "directory" | "path" | "editable") {
                continue;
            }
        }
        out.push(pkg(
            name,
            version,
            Some(version.to_string()),
            Scope::Transitive,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_parse_pins_comments_and_includes() {
        let text = "\
# runtime deps
requests[security]>=2.31,<3
flask==3.0.1
-r dev-requirements.txt
click
";
        let pkgs = parse_requirements(text).expect("parses");
        assert_eq!(pkgs.len(), 3);
        let requests = &pkgs[0];
        assert_eq!(requests.name, "requests");
        assert_eq!(requests.requirement.as_deref(), Some(">=2.31,<3"));
        assert!(requests.version.is_empty());
        assert_eq!(pkgs[2].name, "click");
        assert_eq!(pkgs[2].requirement, None);
    }

    #[test]
    fn pyproject_parses_pep621_and_poetry() {
        let text = r#"
[project]
name = "x"
dependencies = ["requests>=2.31", "flask @ https://example.com/flask.whl"]

[tool.poetry.dependencies]
python = "^3.11"
httpx = "^0.27"
"#;
        let pkgs = parse_pyproject(text).expect("parses");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"requests"));
        assert!(!names.contains(&"flask"), "URL deps skipped");
        let httpx = pkgs.iter().find(|p| p.name == "httpx").expect("httpx");
        assert_eq!(httpx.requirement.as_deref(), Some("^0.27"));
        assert!(!names.contains(&"python"), "python pin is not a dependency");
    }

    #[test]
    fn poetry_lock_parses_exact_versions() {
        let text = r#"
[[package]]
name = "requests"
version = "2.31.0"

[[package]]
name = "local-thing"
version = "1.0.0"
source = { type = "directory", url = "." }
"#;
        let pkgs = parse_poetry_lock(text).expect("parses");
        assert_eq!(pkgs.len(), 1, "directory source skipped");
        assert_eq!(pkgs[0].version, "2.31.0");
        assert_eq!(pkgs[0].scope, Scope::Transitive);
    }

    #[test]
    fn normalize_is_pep503() {
        assert_eq!(normalize("PyYAML"), "pyyaml");
        assert_eq!(normalize("my_pkg.lib"), "my-pkg-lib");
    }
}
