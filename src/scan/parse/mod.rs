//! Manifest parsers: pure functions from manifest bytes to package refs and
//! dependency edges. Each returns `Err(reason)` for unparseable content —
//! the caller turns that into a warning, never a silent skip.

pub mod cargo;
pub mod gomod;
pub mod java;
pub mod python;

use super::{ManifestKind, PkgRef, Scope};

/// Parse one manifest into its dependency list at a point in time.
pub fn parse_manifest(
    path: &str,
    kind: ManifestKind,
    content: &[u8],
) -> Result<Vec<PkgRef>, String> {
    let text = std::str::from_utf8(content).map_err(|_| "not valid UTF-8".to_string())?;
    match kind {
        ManifestKind::CargoToml => cargo::parse_cargo_toml(text),
        ManifestKind::CargoLock => cargo::parse_cargo_lock(text),
        ManifestKind::GoMod => gomod::parse_go_mod(text),
        ManifestKind::GoSum => gomod::parse_go_sum(text),
        ManifestKind::RequirementsTxt => python::parse_requirements(text),
        ManifestKind::PyProject => python::parse_pyproject(text),
        ManifestKind::PoetryLock => python::parse_poetry_lock(text),
        ManifestKind::UvLock => python::parse_uv_lock(text),
        ManifestKind::PomXml => java::parse_pom(text),
        ManifestKind::Gradle => java::parse_gradle(text),
        ManifestKind::GradleLockfile => java::parse_gradle_lockfile(text),
    }
    .map(|pkgs| {
        pkgs.into_iter()
            .map(|mut p| {
                p.scope = scope_for(kind);
                p
            })
            .collect()
    })
    .map_err(|e| format!("{path}: {e}"))
}

/// Lockfile-only packages are transitive; everything a plain manifest
/// declares is direct (refinement happens in the writer via lockfile edges).
fn scope_for(kind: ManifestKind) -> Scope {
    match kind {
        ManifestKind::CargoLock
        | ManifestKind::GoSum
        | ManifestKind::PoetryLock
        | ManifestKind::UvLock
        | ManifestKind::GradleLockfile => Scope::Transitive,
        _ => Scope::Direct,
    }
}

/// Parse dependency EDGES from a graph-source manifest (Cargo.lock, go.mod).
/// Returns (from_purl_base, to_purl, source) where `from_purl_base` is
/// either a full purl (`pkg:cargo/serde@1.0.1`) or, for go.mod's root module,
/// the module path used as a pseudo-package name with empty version.
pub fn parse_dep_edges(
    _path: &str,
    kind: ManifestKind,
    content: &[u8],
) -> Result<Vec<(String, String, &'static str)>, String> {
    let text = std::str::from_utf8(content).map_err(|_| "not valid UTF-8".to_string())?;
    match kind {
        ManifestKind::CargoLock => cargo::cargo_lock_edges(text),
        ManifestKind::GoMod => gomod::go_mod_edges(text),
        _ => Ok(Vec::new()),
    }
}
