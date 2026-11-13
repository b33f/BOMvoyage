//! Typed errors and non-fatal scan warnings. Everything user-visible goes
//! through one of these — nothing fails silently.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("not a git repository: {0}")]
    NotARepository(String),
    #[error("git not found on PATH: {0}")]
    GitMissing(String),
    #[error("no dependency manifests found (looked for requirements*.txt, pyproject.toml, poetry.lock, uv.lock, pom.xml, *.gradle*, gradle.lockfile, Cargo.toml, Cargo.lock, go.mod, go.sum)")]
    NoManifests,
    #[error("git command failed: {0}")]
    GitCommand(String),
    #[error("git output malformed: {0}")]
    GitOutput(String),
    #[error("database error: {0}")]
    Relate(#[from] relate::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// A non-fatal degradation discovered during a scan. Collected, printed to
/// stderr, surfaced in `/api/meta` and the UI — never dropped silently.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanWarning {
    UnparseableManifest {
        path: String,
        commit: String,
        reason: String,
    },
    FlatResolution {
        path: String,
        note: String,
    },
    ShallowClone,
    CapHit {
        what: String,
        limit: u64,
    },
    EmptyAuthor {
        commit: String,
    },
}

impl fmt::Display for ScanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanWarning::UnparseableManifest {
                path,
                commit,
                reason,
            } => {
                write!(f, "unparseable manifest {path} at {commit}: {reason}")
            }
            ScanWarning::FlatResolution { path, note } => {
                write!(f, "flat resolution for {path}: {note}")
            }
            ScanWarning::ShallowClone => write!(
                f,
                "shallow clone: history may be truncated (run `git fetch --unshallow`)"
            ),
            ScanWarning::CapHit { what, limit } => write!(f, "cap hit: {what} limited to {limit}"),
            ScanWarning::EmptyAuthor { commit } => write!(
                f,
                "commit {commit} has no author email; synthesized an identity"
            ),
        }
    }
}

pub type ToolResult<T> = Result<T, ToolError>;
