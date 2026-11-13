//! BOMvoyage binary: CLI dispatch over the library.
//!
//! BOMvoyage is powered by RelateDB — https://relatedb.ai/

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use relate::{Database, OpenMode, OpenOptions};

use bomvoyage::cli::{Cli, Command};
use bomvoyage::error::{ToolError, ToolResult};
use bomvoyage::graph::SbomGraph;
use bomvoyage::{export, graph, scan, server};

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: &Cli) -> ToolResult<()> {
    match &cli.command {
        None => all_in_one(&cli.path, Path::new("bomvoyage.rl"), 8334, 5000),
        Some(Command::Scan {
            path,
            db,
            force,
            max_commits,
        }) => cmd_scan(path, db, *force, *max_commits),
        Some(Command::Serve {
            path,
            db,
            port,
            open,
            max_commits,
        }) => {
            if !db.exists() {
                eprintln!("no graph database at {}; scanning first", db.display());
                cmd_scan(path, db, false, *max_commits)?;
            }
            cmd_serve(db, *port, *open)
        }
        Some(Command::Export {
            db,
            format,
            as_of,
            output,
        }) => cmd_export(db, *format, as_of.as_deref(), output.as_deref()),
        Some(Command::Query { db, cypher }) => cmd_query(db, cypher),
    }
}

/// The demo path: `bomvoyage` in a repo = scan + serve + open the browser.
fn all_in_one(path: &Path, db: &Path, port: u16, max_commits: u64) -> ToolResult<()> {
    cmd_scan(path, db, false, max_commits)?;
    cmd_serve(db, port, true)
}

fn cmd_scan(path: &Path, db: &Path, force: bool, max_commits: u64) -> ToolResult<()> {
    eprintln!("scanning {} …", path.display());
    let caps = scan::ScanCaps {
        max_commits,
        ..scan::ScanCaps::default()
    };

    if db.exists() && !force {
        let existing = SbomGraph::open_read_only(db)?;
        if let Some(head) = existing.stored_head()? {
            let current = scan::git::head_sha(path)?;
            if head == current {
                eprintln!("graph is current (HEAD {head}); nothing to do (use --force to rescan)");
                return Ok(());
            }
            eprintln!(
                "HEAD moved ({} → {current}); re-scanning (idempotent replay)",
                &head[..head.len().min(12)]
            );
        }
    }

    let out = scan::run(path, caps)?;
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    eprintln!(
        "{} packages across {} commits, {} dependency events, {} usage sites",
        out.packages.len(),
        out.commits.len(),
        out.intervals.len(),
        out.imports.len()
    );

    let graph = SbomGraph::open_writable(db)?;
    graph.write_scan(&out)?;
    eprintln!("wrote {}", db.display());
    Ok(())
}

fn cmd_serve(db: &Path, port: u16, open_browser: bool) -> ToolResult<()> {
    let graph = SbomGraph::open_writable(db)?;
    let repo_name = project_name(&graph)?;
    if open_browser {
        let url = format!("http://127.0.0.1:{port}");
        let _ = std::process::Command::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        })
        .arg(&url)
        .spawn();
    }
    server::serve(Arc::new(graph.db), &repo_name, port)?;
    Ok(())
}

fn project_name(graph: &SbomGraph) -> ToolResult<String> {
    let mut conn = graph.db.connect()?;
    let result = conn.query(graph::queries::META)?;
    Ok(result
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.as_string().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string()))
}

fn cmd_export(
    db: &Path,
    format: bomvoyage::cli::Format,
    as_of: Option<&str>,
    output: Option<&Path>,
) -> ToolResult<()> {
    let graph = SbomGraph::open_read_only(db)?;
    let mut conn = graph.db.connect()?;
    let as_of_instant = graph::resolve_as_of(&mut conn, as_of)?;
    let rows = export::live_rows(&mut conn, &as_of_instant)?;
    let edges = export::live_edges(&mut conn, &as_of_instant)?;
    let repo_name = project_name(&graph)?;
    let text = export::render(format, &rows, &edges, &repo_name, &as_of_instant);
    match output {
        Some(path) => {
            std::fs::write(path, text)?;
            eprintln!(
                "wrote {} ({} dependencies, as of {as_of_instant})",
                path.display(),
                rows.len()
            );
        }
        None => {
            std::io::stdout().write_all(text.as_bytes())?;
            println!();
        }
    }
    Ok(())
}

fn cmd_query(db: &Path, cypher: &str) -> ToolResult<()> {
    let graph = open_read_only_or_ephemeral(db)?;
    let mut conn = graph.db.connect()?;
    let result = conn.query(cypher)?;
    let wire = relate::query_result_to_wire(&result);
    println!("{}", serde_json::to_string_pretty(&wire)?);
    Ok(())
}

/// Read-only open that also works while another process holds the writer
/// lease: the ephemeral turn-into releases the source after loading.
fn open_read_only_or_ephemeral(db: &Path) -> ToolResult<SbomGraph> {
    match SbomGraph::open_read_only(db) {
        Ok(g) => Ok(g),
        Err(ToolError::Relate(_)) | Err(ToolError::Io(_)) => {
            let d = Database::open_with(
                &db.to_string_lossy(),
                OpenOptions {
                    mode: OpenMode::Ephemeral,
                    ..OpenOptions::default()
                },
            )?;
            Ok(SbomGraph { db: d })
        }
        Err(e) => Err(e),
    }
}
