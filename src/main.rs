//! BOMvoyage binary: CLI dispatch over the library.
//!
//! BOMvoyage is powered by RelateDB — https://relatedb.ai/

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use relate::{Database, OpenMode, OpenOptions};

use bomvoyage::cli::{Cli, Command, SkillAction, UpdateAction};
use bomvoyage::error::{ToolError, ToolResult};
use bomvoyage::graph::SbomGraph;
use bomvoyage::{export, graph, scan, server, skill, update};

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
        Some(Command::Update { action }) => match action {
            UpdateAction::Check => update::check_command(),
            UpdateAction::Apply => update::apply_command(),
        },
        Some(Command::Skill { action }) => match action {
            SkillAction::Install { skill, agent } => {
                skill::install_command(skill, agent.as_deref())
            }
            SkillAction::List => skill::list_command(),
        },
        None => all_in_one(&cli.path, &cli.db, 8334, 5000, cli.no_update_check),
        Some(Command::Scan {
            path,
            db,
            force,
            max_commits,
        }) => {
            spawn_daily_update_check(cli.no_update_check);
            cmd_scan(path, db, *force, *max_commits)
        }
        Some(Command::Serve {
            path,
            db,
            port,
            open,
            max_commits,
        }) => {
            let db = Arc::new(Database::open(&db.to_string_lossy())?);
            graph::ensure_schema(&db)?;
            scan_and_serve(path, db, *port, *open, *max_commits, cli.no_update_check)
        }
        Some(Command::Export {
            db,
            format,
            as_of,
            output,
        }) => {
            spawn_daily_update_check(cli.no_update_check);
            cmd_export(db, *format, as_of.as_deref(), output.as_deref())
        }
        Some(Command::Query { db, cypher }) => {
            spawn_daily_update_check(cli.no_update_check);
            cmd_query(db, cypher)
        }
    }
}

/// The automatic daily release check: background thread, silent on failure,
/// prints a one-line notice to stderr when a newer release exists. Used by
/// every non-serve invocation; the serve path passes the result to the UI.
fn spawn_daily_update_check(no_update_check: bool) {
    if update::disabled(no_update_check) {
        return;
    }
    std::thread::spawn(|| {
        update::daily_check();
        if let Some(notice) = update::pending_notice() {
            eprintln!("notice: {notice}");
        }
    });
}

/// The demo path: `bomvoyage` in a repo = fast current scan, serve
/// immediately with the browser opened, and keep walking git history in the
/// background to fill in the time-travel data.
fn all_in_one(
    path: &Path,
    db: &Path,
    port: u16,
    max_commits: u64,
    no_update_check: bool,
) -> ToolResult<()> {
    let db = Arc::new(Database::open(&db.to_string_lossy())?);
    graph::ensure_schema(&db)?;
    scan_and_serve(path, db, port, true, max_commits, no_update_check)
}

/// Shared serve-path logic: bring the database's CURRENT state up to date
/// fast (worktree-only, provisional intervals), then walk the git history in
/// a background thread while the explorer is already being served.
fn scan_and_serve(
    path: &Path,
    db: Arc<Database>,
    port: u16,
    open_browser: bool,
    max_commits: u64,
    no_update_check: bool,
) -> ToolResult<()> {
    // The daily release check: background thread feeding /api/update (the
    // UI shows a banner when a new release exists) + a stderr notice.
    if !update::disabled(no_update_check) {
        std::thread::spawn(|| {
            update::daily_check();
            if let Some(notice) = update::pending_notice() {
                eprintln!("notice: {notice}");
            }
        });
    }

    let head = scan::git::head_sha(path)?;
    let history_current = graph::stored_history_head(&db)?.as_deref() == Some(head.as_str());

    if !history_current {
        eprintln!("scanning current dependencies of {} …", path.display());
        let out = scan::current_only(path)?;
        for w in &out.warnings {
            eprintln!("warning: {w}");
        }
        eprintln!(
            "{} current packages, {} usage sites — exploring now; walking git history in the background",
            out.packages.len(),
            out.imports.len()
        );
        graph::write_scan(&db, &out)?;
    } else {
        eprintln!(
            "history is current (HEAD {}); serving",
            &head[..head.len().min(12)]
        );
    }

    if !history_current {
        let bg_path = path.to_path_buf();
        let bg_db = Arc::clone(&db);
        std::thread::spawn(move || {
            eprintln!("background: walking git history for time travel …");
            match scan::run(
                &bg_path,
                scan::ScanCaps {
                    max_commits,
                    ..scan::ScanCaps::default()
                },
            ) {
                Ok(out) => {
                    if let Err(e) = graph::write_scan(&bg_db, &out) {
                        eprintln!("background history write failed: {e}");
                        return;
                    }
                    eprintln!(
                        "background: history complete — {} packages across {} commits, time travel ready",
                        out.packages.len(),
                        out.commits.len()
                    );
                }
                Err(e) => eprintln!("background history scan failed: {e}"),
            }
        });
    }

    let repo_name = project_name(&db)?;
    if open_browser {
        let url = format!("http://127.0.0.1:{port}");
        let opened = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg(&url).spawn()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("cmd")
                .args(["/C", "start", &url])
                .spawn()
        } else {
            std::process::Command::new("xdg-open").arg(&url).spawn()
        };
        let _ = opened;
    }
    server::serve(db, &repo_name, port)?;
    Ok(())
}

fn cmd_scan(path: &Path, db: &Path, force: bool, max_commits: u64) -> ToolResult<()> {
    eprintln!("scanning {} …", path.display());
    let caps = scan::ScanCaps {
        max_commits,
        ..scan::ScanCaps::default()
    };

    if db.exists() && !force {
        let existing = SbomGraph::open_read_only(db)?;
        if let Some(head) = existing.stored_history_head()? {
            let current = scan::git::head_sha(path)?;
            if head == current {
                eprintln!(
                    "history is current (HEAD {head}); nothing to do (use --force to rescan)"
                );
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
    let t = std::time::Instant::now();
    graph.write_scan(&out)?;
    eprintln!("  phase: write {:?}", t.elapsed());
    eprintln!("wrote {}", db.display());
    Ok(())
}

fn project_name(db: &Database) -> ToolResult<String> {
    let mut conn = db.connect()?;
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
    let repo_name = project_name(&graph.db)?;
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
