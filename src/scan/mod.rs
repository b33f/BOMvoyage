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
    Node,
}

impl Eco {
    pub fn purl_prefix(self) -> &'static str {
        match self {
            Eco::Python => "pypi",
            Eco::Java => "maven",
            Eco::Rust => "cargo",
            Eco::Go => "golang",
            Eco::Node => "npm",
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
    PackageJson,
    PackageLock,
    YarnLock,
    PnpmLock,
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
            ManifestKind::PackageJson
            | ManifestKind::PackageLock
            | ManifestKind::YarnLock
            | ManifestKind::PnpmLock => Eco::Node,
        }
    }

    /// True when this manifest yields dependency *edges* between packages,
    /// not just a flat list.
    pub fn is_graph_source(self) -> bool {
        matches!(
            self,
            ManifestKind::CargoLock
                | ManifestKind::GoMod
                | ManifestKind::PackageLock
                | ManifestKind::YarnLock
                | ManifestKind::PnpmLock
        )
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
        if self.eco == Eco::Node {
            return parse::npm::npm_purl(&self.name, &self.version);
        }
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
    /// True for a current-snapshot-only scan: its DECLARES/ADDED intervals are
    /// marked provisional and replaced when the full history lands.
    pub provisional: bool,
    /// True when the history walk is complete for `head_sha` — the writer
    /// records that on the Project node so a re-scan can skip the walk.
    pub history_complete: bool,
}

/// One history record: (commit, [(manifest path, content at that commit —
/// None when deleted)]).
pub type HistoryRecord = (CommitInfo, Vec<(String, Option<Vec<u8>>)>);

/// The full history walk, oldest first.
pub type ManifestHistory = Vec<HistoryRecord>;

/// Scan only the worktree's current state: no history walk. Intervals open
/// at HEAD and are marked provisional — the full scan replaces them. This is
/// the phase-1 fast path: the UI can serve the current SBOM while the
/// history walk fills in the time-travel data in the background.
pub fn current_only(repo: &std::path::Path) -> ToolResult<ScanOutput> {
    if !repo.join(".git").exists() {
        return Err(ToolError::NotARepository(repo.display().to_string()));
    }
    let mut warnings = Vec::new();
    let manifests = discover::discover_manifests(repo)?;
    if manifests.is_empty() {
        return Err(ToolError::NoManifests);
    }
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

    // One synthetic history record: HEAD. The replay machinery derives the
    // current dependency set, its edges and its intervals exactly as a full
    // walk would — the only difference is that everything opens at HEAD.
    let head = git::head_commit(repo)?;
    let mut files = Vec::new();
    for m in &manifests {
        let content = std::fs::read(repo.join(&m.path)).ok();
        files.push((m.path.clone(), content));
    }
    let history: ManifestHistory = vec![(head, files)];
    let replay = replay_history(history, &mut warnings);

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
        provisional: true,
        history_complete: false,
    })
}

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
            // Big monorepo package-lock.json files run to tens of MB.
            max_manifest_bytes: 64 * 1024 * 1024,
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
    let t = std::time::Instant::now();
    let history = git::manifest_history(repo, caps, &mut warnings)?;
    eprintln!("  phase: history fetch {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let replay = replay_history(history, &mut warnings);
    eprintln!("  phase: replay {:?}", t.elapsed());

    let all_packages: Vec<PkgRef> = replay.packages.values().cloned().collect();
    let t = std::time::Instant::now();
    let imports = usage::scan_usage(repo, &all_packages, &manifests, &mut warnings)?;
    eprintln!("  phase: usage {:?}", t.elapsed());

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
        provisional: false,
        history_complete: true,
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
///
/// Parsing (the expensive half on lockfile-churny repos) is fanned out
/// across cores in chunks before the sequential pass: it is pure per
/// (path, kind, content), while the diff needs the mutable open-interval
/// state and stays serial. Chunking keeps only one chunk's parsed sets in
/// memory at a time. Warnings are emitted from the serial pass in exactly
/// the order the inline parse used to produce.
fn replay_history(history: ManifestHistory, warnings: &mut Vec<ScanWarning>) -> ReplayState {
    let mut intervals: Vec<DepInterval> = Vec::new();
    let mut dep_edges: Vec<DepEdge> = Vec::new();
    let mut packages: BTreeMap<String, PkgRef> = BTreeMap::new();
    let mut commits = Vec::new();
    let mut touched = Vec::new();
    // Per manifest path: dep name -> (package, commit that opened the interval).
    let mut open: BTreeMap<String, BTreeMap<String, (PkgRef, CommitInfo)>> = BTreeMap::new();
    // Per graph-source path: open edge (from, to, source) -> index into
    // dep_edges. Edges are diffed like packages: an unchanged edge keeps its
    // interval across lockfile commits instead of being closed and reopened
    // (which multiplied history size by the number of lockfile commits).
    let mut open_edges: BTreeMap<String, OpenEdges> = BTreeMap::new();

    for chunk in history.chunks(PARSE_CHUNK) {
        let parsed = preparse_chunk(chunk);
        for ((commit, files), pre_list) in chunk.iter().zip(parsed) {
            commits.push(commit.clone());
            for (path, _content) in files {
                touched.push((commit.sha.clone(), path.clone()));
            }
            for ((path, _content), pre) in files.iter().zip(pre_list) {
                let Some(kind) = discover::kind_for(path) else {
                    continue;
                };
                match pre {
                    PreParsed::Deleted => {
                        // Manifest deleted at this commit: close every open
                        // interval on it.
                        if let Some(map) = open.get_mut(path) {
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
                        close_edge_interval(&mut dep_edges, open_edges.get_mut(path), commit);
                        continue;
                    }
                    PreParsed::Skip => continue,
                    PreParsed::Unparseable(reason) => {
                        warnings.push(ScanWarning::UnparseableManifest {
                            path: path.clone(),
                            commit: commit.sha.clone(),
                            reason,
                        });
                        continue;
                    }
                    PreParsed::Parsed {
                        pkgs: new_set,
                        edges,
                    } => {
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

                        // Graph-source manifests yield dependency EDGES for
                        // this interval.
                        if kind.is_graph_source() {
                            match edges {
                                Ok(edges) => {
                                    let new_edges: std::collections::BTreeSet<EdgeKey> =
                                        edges.into_iter().collect();
                                    let open_now = open_edges.entry(path.clone()).or_default();
                                    // Removed edges: close their interval.
                                    open_now.retain(|key, &mut i| {
                                        if new_edges.contains(key) {
                                            return true;
                                        }
                                        let e = &mut dep_edges[i];
                                        e.removed_sha = Some(commit.sha.clone());
                                        e.removed_date = Some(commit.date.clone());
                                        false
                                    });
                                    // New edges: open an interval.
                                    for key in new_edges {
                                        if open_now.contains_key(&key) {
                                            continue;
                                        }
                                        dep_edges.push(DepEdge {
                                            from: key.0.clone(),
                                            to: key.1.clone(),
                                            source: key.2,
                                            since_sha: commit.sha.clone(),
                                            since_date: commit.date.clone(),
                                            removed_sha: None,
                                            removed_date: None,
                                        });
                                        open_now.insert(key, dep_edges.len() - 1);
                                    }
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

/// How many history records get parsed per parallel batch — bounds the
/// memory held in parsed dep sets.
const PARSE_CHUNK: usize = 128;

/// One manifest file's parse, done ahead of the sequential diff.
enum PreParsed {
    /// Content is None — the manifest was deleted at this commit.
    Deleted,
    /// Not a manifest-shaped path (kept for zip-alignment).
    Skip,
    Unparseable(String),
    Parsed {
        pkgs: BTreeMap<String, PkgRef>,
        /// Edge set for graph-source manifests only.
        edges: Result<Vec<(String, String, &'static str)>, String>,
    },
}

/// Parse every manifest file in the chunk across the worker pool. Parsing is
/// pure per (path, content), so this shares nothing; results slot back into
/// record/file order.
fn preparse_chunk(chunk: &[HistoryRecord]) -> Vec<Vec<PreParsed>> {
    let mut starts = Vec::with_capacity(chunk.len());
    let mut total_jobs = 0usize;
    for (_, files) in chunk {
        starts.push(total_jobs);
        total_jobs += files.len();
    }
    let results = std::sync::Mutex::new(
        chunk
            .iter()
            .map(|(_, files)| {
                (0..files.len())
                    .map(|_| PreParsed::Skip)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
    );
    let next_job = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let job = next_job.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if job >= total_jobs {
                    return;
                }
                let r = starts.partition_point(|&st| st <= job) - 1;
                let f = job - starts[r];
                let (path, content) = &chunk[r].1[f];
                let parsed = match content {
                    None => PreParsed::Deleted,
                    Some(bytes) => match discover::kind_for(path) {
                        None => PreParsed::Skip,
                        Some(kind) => match parse::parse_manifest(path, kind, bytes) {
                            Err(reason) => PreParsed::Unparseable(reason),
                            Ok(refs) => {
                                let edges = if kind.is_graph_source() {
                                    parse::parse_dep_edges(path, kind, bytes)
                                        .map_err(|e| e.to_string())
                                } else {
                                    Ok(Vec::new())
                                };
                                PreParsed::Parsed {
                                    pkgs: keyed_by_name(refs),
                                    edges,
                                }
                            }
                        },
                    },
                };
                results.lock().expect("preparse results mutex")[r][f] = parsed;
            });
        }
    });
    results.into_inner().expect("preparse results mutex")
}

/// Close the currently open DEPENDS_ON interval for one manifest path.
type EdgeKey = (String, String, &'static str);
type OpenEdges = BTreeMap<EdgeKey, usize>;

/// Manifest deleted: close every open edge interval it carried.
fn close_edge_interval(
    dep_edges: &mut [DepEdge],
    open: Option<&mut OpenEdges>,
    commit: &CommitInfo,
) {
    if let Some(open) = open {
        for &i in open.values() {
            let e = &mut dep_edges[i];
            if e.removed_sha.is_none() {
                e.removed_sha = Some(commit.sha.clone());
                e.removed_date = Some(commit.date.clone());
            }
        }
        open.clear();
    }
}

/// Key a manifest's packages for interval diffing. The name is the key, so
/// a version change reads as a bump of the same dependency — except where
/// one lockfile holds several versions of a name at once (routine in npm
/// and Cargo), where each version is keyed by its purl so none is dropped.
fn keyed_by_name(refs: Vec<PkgRef>) -> BTreeMap<String, PkgRef> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in &refs {
        *counts.entry(p.name.clone()).or_default() += 1;
    }
    refs.into_iter()
        .map(|p| {
            let key = if counts[&p.name] > 1 {
                p.purl()
            } else {
                p.name.clone()
            };
            (key, p)
        })
        .collect()
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

    #[allow(clippy::type_complexity)] // test fixture shorthand
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

    fn lock(pkgs: &[(&str, &str, &[&str])]) -> Vec<u8> {
        let mut entries = serde_json::Map::new();
        for (path, version, deps) in pkgs {
            let deps: serde_json::Map<String, serde_json::Value> =
                deps.iter().map(|d| (d.to_string(), "*".into())).collect();
            entries.insert(
                path.to_string(),
                serde_json::json!({"version": version, "dependencies": deps}),
            );
        }
        serde_json::json!({"lockfileVersion": 3, "packages": entries})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn unchanged_edges_keep_one_interval_and_duplicate_versions_survive() {
        let v1 = lock(&[
            ("node_modules/a", "1.0.0", &["b"]),
            ("node_modules/b", "3.0.0", &[]),
            ("node_modules/c", "1.0.0", &["b"]),
            ("node_modules/c/node_modules/b", "2.0.0", &[]),
        ]);
        // c2: only `a` bumps; every other edge is unchanged.
        let v2 = lock(&[
            ("node_modules/a", "1.1.0", &["b"]),
            ("node_modules/b", "3.0.0", &[]),
            ("node_modules/c", "1.0.0", &["b"]),
            ("node_modules/c/node_modules/b", "2.0.0", &[]),
        ]);
        let history = vec![
            history_entry(
                "c1",
                "2024-01-01T00:00:00Z",
                vec![("package-lock.json", Some(v1))],
            ),
            history_entry(
                "c2",
                "2024-02-01T00:00:00Z",
                vec![("package-lock.json", Some(v2))],
            ),
        ];
        let mut warnings = Vec::new();
        let out = replay_history(history, &mut warnings);
        assert!(warnings.is_empty());

        // c -> b@2.0.0 never changed: exactly one interval, still open.
        let c_edges: Vec<&DepEdge> = out
            .dep_edges
            .iter()
            .filter(|e| e.from == "pkg:npm/c@1.0.0")
            .collect();
        assert_eq!(c_edges.len(), 1);
        assert_eq!(c_edges[0].to, "pkg:npm/b@2.0.0");
        assert_eq!(c_edges[0].since_sha, "c1");
        assert!(c_edges[0].removed_sha.is_none());
        // a@1.0.0's edge closed at c2; a@1.1.0's opened there.
        assert!(out
            .dep_edges
            .iter()
            .any(|e| e.from == "pkg:npm/a@1.0.0" && e.removed_sha.as_deref() == Some("c2")));
        assert!(out.dep_edges.iter().any(|e| e.from == "pkg:npm/a@1.1.0"
            && e.since_sha == "c2"
            && e.removed_sha.is_none()));
        assert_eq!(out.dep_edges.len(), 3);

        // Both versions of `b` are tracked, not just the last one parsed.
        let live_b: Vec<String> = out
            .intervals
            .iter()
            .filter(|i| i.pkg.name == "b" && i.until.is_none())
            .map(|i| i.pkg.version.clone())
            .collect();
        assert_eq!(live_b.len(), 2, "got {live_b:?}");
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
        let out = replay_history(history, &mut warnings);
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
        let out = replay_history(history, &mut warnings);
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
        let out = replay_history(history, &mut warnings);
        assert!(out.intervals.is_empty());
        assert_eq!(warnings.len(), 1, "one UnparseableManifest warning");
    }
}
