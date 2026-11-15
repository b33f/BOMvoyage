//! `bomvoyage site`: export a fully static, self-contained explorer site
//! (Cloudflare Pages or any static host). The graph is pre-built here —
//! the scan needs `git` and cannot run in a browser — and the generated
//! page replays it into an in-browser RelateDb (WASM) at load time.

pub mod dump;
pub mod emit;

use std::path::Path;

use relate::Database;

use crate::error::{ToolError, ToolResult};

/// `bomvoyage site <path> --out dist/`: scan (or reuse `--db`), optionally
/// run detailed mode (the global flags), dump the graph, emit the static
/// site, print deploy instructions.
#[allow(clippy::too_many_arguments)]
pub fn site_command(
    path: &Path,
    db_path: &Path,
    out: &Path,
    repo_spec: Option<String>,
    force: bool,
    max_commits: u64,
    detailed: bool,
    vuln_details: bool,
    wasm_cdn: bool,
) -> ToolResult<()> {
    // Never clobber a directory that isn't ours to clobber.
    if out.exists() && out.read_dir()?.next().is_some() {
        if force {
            eprintln!(
                "dist {} is not empty — overwriting (--force)",
                out.display()
            );
        } else {
            return Err(ToolError::Other(format!(
                "{} is not empty (use --force to overwrite)",
                out.display()
            )));
        }
    }

    // Bring the graph current: reuse a current database, else a FULL
    // synchronous scan — the site ships without a background walker, so
    // partial history would be permanent.
    let needs_scan = !db_path.exists() || force || (graph_is_stale(path, db_path)?);
    if needs_scan {
        if !path.exists() {
            return Err(ToolError::Other(format!(
                "repository {} not found (and a scan is needed)",
                path.display()
            )));
        }
        eprintln!("scanning {} …", path.display());
        let out_scan = crate::scan::run(
            path,
            crate::scan::ScanCaps {
                max_commits,
                ..crate::scan::ScanCaps::default()
            },
        )?;
        for w in &out_scan.warnings {
            eprintln!("warning: {w}");
        }
        let graph = crate::graph::SbomGraph::open_writable(db_path)?;
        graph.write_scan(&out_scan)?;
    }

    let db = open_db(db_path)?;

    if detailed {
        eprintln!("detailed mode: fetching deps.dev data for the snapshot …");
        let opts = crate::depsdev::DetailedOpts {
            vuln_details,
            refresh: false,
        };
        let stats = crate::depsdev::detailed_graph(&db, &opts)?;
        eprintln!("{}", stats.summary_line());
    }

    warn_if_provisional(&db)?;

    // Dump + emit. (All reads go through the ONE open handle — a .rl
    // bundle allows no second one.)
    let (repo_name, head_sha) = {
        let mut conn = db.connect()?;
        let result = conn.query(crate::graph::queries::META)?;
        let col = |name: &str| result.header.columns.iter().position(|c| c == name);
        let get = |name: &str| {
            result
                .rows
                .first()
                .and_then(|r| r.get(col(name)?))
                .and_then(|v| v.as_string().map(str::to_string))
                .unwrap_or_default()
        };
        (get("name"), get("head_sha"))
    };
    let history_complete =
        crate::graph::stored_history_head(&db)?.as_deref() == Some(head_sha.as_str());
    let mut conn = db.connect()?;
    let mut snapshot = dump::dump(&mut conn, &repo_name, &head_sha, history_complete)?;
    snapshot.repo_spec = repo_spec;
    let stage_count = snapshot.stages.len();
    let written = emit::emit(out, &snapshot)?;

    eprintln!(
        "wrote {} ({} files, {stage_count} snapshot stages):",
        out.display(),
        written.len()
    );
    for (rel, bytes) in &written {
        eprintln!("  {rel} ({} KB)", bytes / 1024);
    }
    if !snapshot.history_complete {
        eprintln!(
            "warning: history walk incomplete — the site's time travel will be partial (scan fully, then re-export)"
        );
    }
    eprintln!(
        "privacy: the published site exposes this repository's full dependency history, including committer names and emails"
    );
    if wasm_cdn {
        eprintln!(
            "note: --wasm-cdn serves the graph engine from a third-party CDN — that is a supply-chain decision on YOUR site's page"
        );
    }
    eprintln!(
        "deploy to Cloudflare Pages:\n  npx wrangler pages deploy {}\nor upload the directory in the Pages dashboard (framework preset: None). URLs are relative — any static host works, under a subpath too.",
        out.display()
    );
    Ok(())
}

/// True when the database's stored history head is missing or behind the
/// repository's current HEAD.
fn graph_is_stale(path: &Path, db_path: &Path) -> ToolResult<bool> {
    let head = crate::scan::git::head_sha(path)?;
    let graph = crate::graph::SbomGraph::open_read_only(db_path)?;
    Ok(graph.stored_history_head()?.as_deref() != Some(head.as_str()))
}

fn open_db(db_path: &Path) -> ToolResult<Database> {
    Database::open(&db_path.to_string_lossy())
        .map_err(|e| ToolError::Other(format!("could not open {}: {e}", db_path.display())))
}

/// A site must not ship provisional (current-only scan) intervals — warn
/// loudly; they make time travel lie.
fn warn_if_provisional(db: &Database) -> ToolResult<()> {
    let mut conn = db.connect()?;
    let result =
        conn.query("MATCH ()-[r:DECLARES]->() WHERE r.provisional = true RETURN count(r) AS n")?;
    let n = result
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            relate::Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);
    if n > 0 {
        eprintln!(
            "warning: the graph has {n} provisional intervals (a partial scan) — the published site will show incomplete time travel"
        );
    }
    Ok(())
}
