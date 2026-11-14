//! Worktree manifest discovery and the filename → ManifestKind mapping
//! (also applied to historical paths from git log).

use std::collections::BTreeSet;

use super::{ManifestKind, ManifestRef};
use crate::error::ToolResult;

pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "bower_components",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".yarn",
    "coverage",
];

/// The manifest filename → kind mapping, one table.
pub fn kind_for(path: &str) -> Option<ManifestKind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let kind = match name {
        "Cargo.toml" => ManifestKind::CargoToml,
        "Cargo.lock" => ManifestKind::CargoLock,
        "go.mod" => ManifestKind::GoMod,
        "go.sum" => ManifestKind::GoSum,
        "pyproject.toml" => ManifestKind::PyProject,
        "poetry.lock" => ManifestKind::PoetryLock,
        "uv.lock" => ManifestKind::UvLock,
        "pom.xml" => ManifestKind::PomXml,
        "gradle.lockfile" => ManifestKind::GradleLockfile,
        "package.json" => ManifestKind::PackageJson,
        "package-lock.json" | "npm-shrinkwrap.json" => ManifestKind::PackageLock,
        "yarn.lock" => ManifestKind::YarnLock,
        "pnpm-lock.yaml" => ManifestKind::PnpmLock,
        _ if name == "requirements.txt"
            || name.starts_with("requirements-") && name.ends_with(".txt") =>
        {
            ManifestKind::RequirementsTxt
        }
        _ if name.ends_with(".gradle") || name.ends_with(".gradle.kts") => ManifestKind::Gradle,
        _ => return None,
    };
    Some(kind)
}

/// Walk the worktree and find every manifest. Bounded depth keeps monorepo
/// node trees from exploding; symlinked directories are not followed.
pub fn discover_manifests(repo: &std::path::Path) -> ToolResult<Vec<ManifestRef>> {
    let mut found = BTreeSet::new();
    walk(repo, repo, 0, &mut found)?;
    Ok(found
        .into_iter()
        .filter_map(|path| kind_for(&path).map(|kind| ManifestRef { path, kind }))
        .collect())
}

fn walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    found: &mut BTreeSet<String>,
) -> ToolResult<()> {
    if depth > 6 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) || file_type.is_symlink() {
                continue;
            }
            walk(root, &path, depth + 1, found)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if kind_for(&rel).is_some() {
                found.insert(rel);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_for_matches_the_table() {
        assert_eq!(kind_for("Cargo.toml"), Some(ManifestKind::CargoToml));
        assert_eq!(
            kind_for("crates/foo/Cargo.lock"),
            Some(ManifestKind::CargoLock)
        );
        assert_eq!(kind_for("server/go.mod"), Some(ManifestKind::GoMod));
        assert_eq!(
            kind_for("requirements.txt"),
            Some(ManifestKind::RequirementsTxt)
        );
        assert_eq!(
            kind_for("requirements-dev.txt"),
            Some(ManifestKind::RequirementsTxt)
        );
        assert_eq!(kind_for("build.gradle.kts"), Some(ManifestKind::Gradle));
        assert_eq!(kind_for("app/build.gradle"), Some(ManifestKind::Gradle));
        assert_eq!(
            kind_for("web/package.json"),
            Some(ManifestKind::PackageJson)
        );
        assert_eq!(
            kind_for("package-lock.json"),
            Some(ManifestKind::PackageLock)
        );
        assert_eq!(kind_for("yarn.lock"), Some(ManifestKind::YarnLock));
        assert_eq!(kind_for("pnpm-lock.yaml"), Some(ManifestKind::PnpmLock));
        assert_eq!(kind_for("src/main.rs"), None);
        assert_eq!(kind_for("requirements.txt.bak"), None);
    }
}
