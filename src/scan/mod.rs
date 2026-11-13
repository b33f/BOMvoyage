//! The scan pipeline: discover manifests → walk git history → derive
//! add/bump/remove events with attribution → scan worktree usage sites.

pub mod discover;
pub mod git;
pub mod parse;
pub mod usage;

use std::collections::BTreeMap;

use crate::error::{ScanWarning, ToolError, ToolResult};

/// Supported ecosystems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Eco {
    Python,
    Java,
    Rust,
    Go,
}

impl Eco {
    pub fn purl_prefix(self) -> &'static str {
        match self {
            Eco::Python => "pypi",
            Eco::Java => "maven",
            Eco::Rust => "cargo",
            Eco::Go => "golang",
        }
    }
}

/// What kind of manifest a file is. Lockfile kinds carry exact resolved
/// versions; plain manifests carry version *requirements*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    RequirementsTxt,
    PyProject,
    PoetryLock,
    UvLock,
    PomXml,
    Gradle,
    GradleLockfile,
    CargoToml,
    CargoLock,
    GoMod,
    GoSum,
}

impl ManifestKind {
    pub fn ecosystem(self) -> Eco {
        match self {
            ManifestKind::RequirementsTxt
            | ManifestKind::PyProject
            | ManifestKind::PoetryLock
            | ManifestKind::UvLock => Eco::Python,
            ManifestKind::PomXml | ManifestKind::Gradle | ManifestKind::GradleLockfile => Eco::Java,
            ManifestKind::CargoToml | ManifestKind::CargoLock => Eco::Rust,
            ManifestKind::GoMod | ManifestKind::GoSum => Eco::Go,
        }
    }

    /// True when this manifest yields dependency *edges* between packages,
    /// not just a flat list.
    pub fn is_graph_source(self) -> bool {
        matches!(self, ManifestKind::CargoLock | ManifestKind::GoMod)
    }
}

/// A discovered manifest file (repo-relative path).
#[derive(Debug, Clone)]
pub struct ManifestRef {
    pub path: String,
    pub kind: ManifestKind,
}

/// One dependency as parsed from a manifest/lockfile at a point in history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PkgRef {
    pub name: String,
    /// Exact resolved version when known (lockfiles, go.sum); otherwise empty.
    pub version: String,
    /// The requirement as written (`^1.0`, `>=2,<3`), when known.
    pub requirement: Option<String>,
    pub eco: Eco,
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Direct,
    Transitive,
}

impl PkgRef {
    /// Package URL: the natural identity of a package version. Unpinned
    /// packages get a versionless purl (`pkg:pypi/requests`). Maven names
    /// carry `group:artifact`, rendered as the purl namespace `group/artifact`.
    pub fn purl(&self) -> String {
        let (ns, name) = match self.eco {
            Eco::Java => match self.name.split_once(':') {
                Some((g, a)) => (g.to_string(), a.to_string()),
                None => (String::new(), self.name.clone()),
            },
            _ => (String::new(), self.name.clone()),
        };
        let base = if ns.is_empty() {
            format!("pkg:{}/{}", self.eco.purl_prefix(), name)
        } else {
            format!("pkg:{}/{}/{}", self.eco.purl_prefix(), ns, name)
        };
        if self.version.is_empty() {
            base
        } else {
            format!("{base}@{}", self.version)
        }
    }

    /// The identity compared when diffing history: the exact version when we
    /// have one, else the requirement string.
    pub fn identity(&self) -> String {
        if !self.version.is_empty() {
            self.version.clone()
        } else {
            self.requirement.clone().unwrap_or_default()
        }
    }
}

/// A commit as read from `git log`.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    /// Strict ISO-8601 author date (`%aI`) — string comparison is ordering-correct.
    pub date: String,
    pub summary: String,
}

/// One dependency's occupancy interval on one manifest, derived by replaying
/// history: the package was declared from `since` until `until` (None = still
/// live at HEAD). Every add/bump/remove is expressible as intervals — a bump
/// closes one interval and opens another.
#[derive(Debug, Clone)]
pub struct DepInterval {
    pub manifest: String,
    pub pkg: PkgRef,
    pub since: CommitInfo,
    pub until: Option<CommitInfo>,
}

/// A worktree usage site: `file` references package `purl` via `symbol`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportSite {
    pub file: String,
    pub purl: String,
    pub symbol: String,
}

/// A dependency edge with a validity interval.
#[derive(Debug, Clone)]
pub struct DepEdge {
    pub from: String,
    pub to: String,
    pub source: &'static str,
    pub since_sha: String,
    pub since_date: String,
    pub removed_sha: Option<String>,
    pub removed_date: Option<String>,
}

/// Everything one scan produces, before writing to the graph.
#[derive(Debug, Default)]
pub struct ScanOutput {
    pub repo_name: String,
    pub head_sha: String,
    /// DECLARES occupancy intervals over the whole history.
    pub intervals: Vec<DepInterval>,
    /// DEPENDS_ON edges (lock-derived or synthesized) with validity intervals.
    pub dep_edges: Vec<DepEdge>,
    /// Every package version ever seen.
    pub packages: Vec<PkgRef>,
    pub imports: Vec<ImportSite>,
    pub warnings: Vec<ScanWarning>,
    /// Commits that touched manifests (the UI time slider).
    pub commits: Vec<CommitInfo>,
    /// (commit sha, manifest path) pairs from the history walk.
    pub touched: Vec<(String, String)>,
}

/// One history record: (commit, [(manifest path, content at that commit —
/// None when deleted)]).
pub type HistoryRecord = (CommitInfo, Vec<(String, Option<Vec<u8>>)>);

/// The full history walk, oldest first.
pub type ManifestHistory = Vec<HistoryRecord>;

/// Caps for bounded scanning; hitting one warns, never truncates silently.
#[derive(Debug, Clone, Copy)]
pub struct ScanCaps {
    pub max_commits: u64,
    pub max_manifest_bytes: usize,
}

impl Default for ScanCaps {
    fn default() -> Self {
        Self {
            max_commits: 5000,
            max_manifest_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Run the full scan of `repo` and return everything to write.
pub fn run(repo: &std::path::Path, caps: ScanCaps) -> ToolResult<ScanOutput> {
    if !repo.join(".git").exists() {
        return Err(ToolError::NotARepository(repo.display().to_string()));
    }
    let mut warnings = Vec::new();
    if repo.join(".git").join("shallow").exists() {
        warnings.push(ScanWarning::ShallowClone);
    }

    let manifests = discover::discover_manifests(repo)?;
    if manifests.is_empty() {
        return Err(ToolError::NoManifests);
    }
    // Flat lockfiles carry no dependency graph: say so up front (UI badge),
    // never imply transitive resolution that did not happen.
    for m in &manifests {
        let flat = matches!(
            m.kind,
            ManifestKind::PoetryLock
                | ManifestKind::UvLock
                | ManifestKind::GradleLockfile
                | ManifestKind::GoSum
        );
        if flat {
            warnings.push(ScanWarning::FlatResolution {
                path: m.path.clone(),
                note: "this lockfile is a flat list; transitive structure comes from the manifests, edges are synthesized from the root".into(),
            });
        }
    }

    // Walk history over all paths; keep commits touching manifest-shaped files
    // (this covers manifests deleted before HEAD). For each such commit, fetch
    // each touched manifest's content at that commit.
    let history = git::manifest_history(repo, caps, &mut warnings)?;
    let replay = replay_history(history, caps, &mut warnings);

    let all_packages: Vec<PkgRef> = replay.packages.values().cloned().collect();
    let imports = usage::scan_usage(repo, &all_packages, &manifests, &mut warnings)?;

    Ok(ScanOutput {
        repo_name: repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| repo.display().to_string()),
        head_sha: git::head_sha(repo)?,
        intervals: replay.intervals,
        dep_edges: replay.dep_edges,
        packages: all_packages,
        imports,
        warnings,
        commits: replay.commits,
        touched: replay.touched,
    })
}

/// State accumulated while replaying history.
struct ReplayState {
    intervals: Vec<DepInterval>,
    dep_edges: Vec<DepEdge>,
    packages: BTreeMap<String, PkgRef>,
    commits: Vec<CommitInfo>,
    touched: Vec<(String, String)>,
}

/// Replay per-manifest history from the empty set, diffing each commit's
/// dependency set against the previous one to derive events. Graph-source
/// manifests (Cargo.lock, go.mod) also emit DEPENDS_ON edge intervals: the
/// edges from one version of the file are closed when the file next changes
/// (or left open at HEAD).
fn replay_history(
    history: ManifestHistory,
    _caps: ScanCaps,
    warnings: &mut Vec<ScanWarning>,
) -> ReplayState {
    let mut intervals: Vec<DepInterval> = Vec::new();
    let mut dep_edges: Vec<DepEdge> = Vec::new();
    let mut packages: BTreeMap<String, PkgRef> = BTreeMap::new();
    let mut commits = Vec::new();
    let mut touched = Vec::new();
    // Per manifest path: dep name -> (package, commit that opened the interval).
    let mut open: BTreeMap<String, BTreeMap<String, (PkgRef, CommitInfo)>> = BTreeMap::new();
    // Per graph-source path: indices into dep_edges of the currently open interval.
    let mut open_edges: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (commit, files) in history {
        commits.push(commit.clone());
        for (path, _content) in &files {
            touched.push((commit.sha.clone(), path.clone()));
        }
        for (path, content) in files {
            let Some(kind) = discover::kind_for(&path) else {
                continue;
            };

            let Some(content) = content else {
                // Manifest deleted at this commit: close every open interval on it.
                if let Some(map) = open.get_mut(&path) {
                    for (pkg, since) in map.values() {
                        intervals.push(DepInterval {
                            manifest: path.clone(),
                            pkg: pkg.clone(),
                            since: since.clone(),
                            until: Some(commit.clone()),
                        });
                    }
                    map.clear();
                }
                close_edge_interval(&mut dep_edges, open_edges.get_mut(&path), &commit);
                continue;
            };

            let parsed = parse::parse_manifest(&path, kind, &content);
            let new_set: BTreeMap<String, PkgRef> = match parsed {
                Ok(refs) => refs.into_iter().map(|p| (p.name.clone(), p)).collect(),
                Err(reason) => {
                    warnings.push(ScanWarning::UnparseableManifest {
                        path: path.clone(),
                        commit: commit.sha.clone(),
                        reason,
                    });
                    continue;
                }
            };

            let entry = open.entry(path.clone()).or_default();
            // Bumps: close the old interval, open a new one. Removes: close.
            let names: Vec<String> = entry.keys().cloned().collect();
            for name in names {
                let (old, since) = entry.get(&name).expect("key was just read").clone();
                match new_set.get(&name) {
                    Some(new_pkg) if new_pkg.identity() != old.identity() => {
                        intervals.push(DepInterval {
                            manifest: path.clone(),
                            pkg: old,
                            since,
                            until: Some(commit.clone()),
                        });
                        entry.insert(name.clone(), (new_pkg.clone(), commit.clone()));
                    }
                    Some(_) => {}
                    None => {
                        intervals.push(DepInterval {
                            manifest: path.clone(),
                            pkg: old,
                            since,
                            until: Some(commit.clone()),
                        });
                        entry.remove(&name);
                    }
                }
            }
            // Adds: open new intervals.
            for (name, pkg) in &new_set {
                if !entry.contains_key(name) {
                    entry.insert(name.clone(), (pkg.clone(), commit.clone()));
                }
            }
            for pkg in new_set.into_values() {
                packages.insert(pkg.purl(), pkg);
            }

            // Graph-source manifests yield dependency EDGES for this interval.
            if kind.is_graph_source() {
                close_edge_interval(&mut dep_edges, open_edges.get_mut(&path), &commit);
                match parse::parse_dep_edges(&path, kind, &content) {
                    Ok(edges) => {
                        let mut idxs = Vec::with_capacity(edges.len());
                        for (from, to, source) in edges {
                            dep_edges.push(DepEdge {
                                from,
                                to,
                                source,
                                since_sha: commit.sha.clone(),
                                since_date: commit.date.clone(),
                                removed_sha: None,
                                removed_date: None,
                            });
                            idxs.push(dep_edges.len() - 1);
                        }
                        open_edges.insert(path, idxs);
                    }
                    Err(reason) => warnings.push(ScanWarning::UnparseableManifest {
                        path: path.clone(),
                        commit: commit.sha.clone(),
                        reason,
                    }),
                }
            }
        }
    }

    // Intervals still open at HEAD become live (until = None).
    for (path, map) in open {
        for (pkg, since) in map.into_values() {
            intervals.push(DepInterval {
                manifest: path.clone(),
                pkg,
                since,
                until: None,
            });
        }
    }

    ReplayState {
        intervals,
        dep_edges,
        packages,
        commits,
        touched,
    }
}

/// Close the currently open DEPENDS_ON interval for one manifest path.
fn close_edge_interval(
    dep_edges: &mut [DepEdge],
    open: Option<&mut Vec<usize>>,
    commit: &CommitInfo,
) {
    if let Some(idxs) = open {
        for &i in idxs.iter() {
            let e = &mut dep_edges[i];
            if e.removed_sha.is_none() {
                e.removed_sha = Some(commit.sha.clone());
                e.removed_date = Some(commit.date.clone());
            }
        }
        idxs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, date: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            author_name: "Ada".into(),
            author_email: "ada@example.com".into(),
            date: date.to_string(),
            summary: "s".into(),
        }
    }

    fn cargo_toml(deps: &[(&str, &str)]) -> Vec<u8> {
        let mut s =
            String::from("[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\n");
        for (n, v) in deps {
            s.push_str(&format!("{n} = \"{v}\"\n"));
        }
        s.into_bytes()
    }

    fn history_entry(
        sha: &str,
        date: &str,
        files: Vec<(&str, Option<Vec<u8>>)>,
    ) -> (CommitInfo, Vec<(String, Option<Vec<u8>>)>) {
        (
            commit(sha, date),
            files.into_iter().map(|(p, c)| (p.to_string(), c)).collect(),
        )
    }

    #[test]
    fn replay_derives_intervals() {
        let history = vec![
            history_entry(
                "c1",
                "2024-01-01T00:00:00Z",
                vec![("Cargo.toml", Some(cargo_toml(&[("serde", "1.0.100")])))],
            ),
            history_entry(
                "c2",
                "2024-02-01T00:00:00Z",
                vec![(
                    "Cargo.toml",
                    Some(cargo_toml(&[("serde", "1.0.200"), ("tokio", "1.0.0")])),
                )],
            ),
            history_entry(
                "c3",
                "2024-03-01T00:00:00Z",
                vec![("Cargo.toml", Some(cargo_toml(&[("tokio", "1.0.0")])))],
            ),
        ];
        let mut warnings = Vec::new();
        let out = replay_history(history, ScanCaps::default(), &mut warnings);
        assert!(warnings.is_empty());

        // serde: two intervals (1.0.100 closed at c2, 1.0.200 closed at c3).
        // tokio: one interval, open at HEAD.
        let serde_ivls: Vec<&DepInterval> = out
            .intervals
            .iter()
            .filter(|i| i.pkg.name == "serde")
            .collect();
        assert_eq!(serde_ivls.len(), 2);
        assert_eq!(serde_ivls[0].pkg.identity(), "1.0.100");
        assert_eq!(serde_ivls[0].since.sha, "c1");
        assert_eq!(
            serde_ivls[0].until.as_ref().expect("closed by bump").sha,
            "c2"
        );
        assert_eq!(serde_ivls[1].pkg.identity(), "1.0.200");
        assert_eq!(serde_ivls[1].since.sha, "c2");
        assert_eq!(
            serde_ivls[1].until.as_ref().expect("closed by removal").sha,
            "c3"
        );
        let tokio = out
            .intervals
            .iter()
            .find(|i| i.pkg.name == "tokio")
            .expect("tokio interval");
        assert_eq!(tokio.since.sha, "c2");
        assert!(tokio.until.is_none(), "tokio still live at HEAD");

        // Package nodes ever seen: versionless manifest deps collapse by purl
        // (pkg:cargo/serde, pkg:cargo/tokio) — the per-requirement history
        // lives on the DECLARES intervals, not on the node.
        assert_eq!(out.packages.len(), 2);
    }

    #[test]
    fn replay_handles_deleted_manifest() {
        let history = vec![
            history_entry(
                "c1",
                "2024-01-01T00:00:00Z",
                vec![("Cargo.toml", Some(cargo_toml(&[("serde", "1.0.100")])))],
            ),
            history_entry("c2", "2024-02-01T00:00:00Z", vec![("Cargo.toml", None)]),
        ];
        let mut warnings = Vec::new();
        let out = replay_history(history, ScanCaps::default(), &mut warnings);
        assert_eq!(out.intervals.len(), 1);
        assert_eq!(
            out.intervals[0]
                .until
                .as_ref()
                .expect("deleted closes it")
                .sha,
            "c2"
        );
    }

    #[test]
    fn replay_unparseable_manifest_warns_not_fails() {
        let history = vec![history_entry(
            "c1",
            "2024-01-01T00:00:00Z",
            vec![("Cargo.toml", Some(b"\x00\xff garbage".to_vec()))],
        )];
        let mut warnings = Vec::new();
        let out = replay_history(history, ScanCaps::default(), &mut warnings);
        assert!(out.intervals.is_empty());
        assert_eq!(warnings.len(), 1, "one UnparseableManifest warning");
    }
}
