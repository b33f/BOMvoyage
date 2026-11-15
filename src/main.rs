//! BOMvoyage binary: CLI dispatch over the library.
//!
//! BOMvoyage is powered by RelateDB — https://relatedb.ai/

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use relate::{Database, OpenMode, OpenOptions};

use bomvoyage::cli::{Cli, Command, SkillAction, UpdateAction};
use bomvoyage::depsdev::DetailedOpts;
use bomvoyage::error::{ToolError, ToolResult};
use bomvoyage::graph::SbomGraph;
use bomvoyage::{depsdev, export, graph, scan, server, skill, update};

#[cfg(test)]
mod port_tests {
    use super::*;

    #[test]
    fn finds_own_listening_socket() {
        // bind an ephemeral port, then find ourselves holding it
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let holder = find_port_holder(port).expect("the listener must be found");
        assert_eq!(holder.pid, std::process::id());
        assert!(!holder.path.is_empty(), "a path/command is always reported");
        drop(listener);
        assert!(
            find_port_holder(port).is_none(),
            "released port has no holder"
        );
    }

    #[test]
    fn unbound_port_has_no_holder() {
        // an ephemeral port we never bind: extremely likely to be free
        let free = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            l.local_addr().expect("addr").port()
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(find_port_holder(free).is_none());
    }
}

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
    // `--more-detailed` is the both-tiers shorthand: it *implies* --detailed
    // rather than erroring without it. Effective values are derived once, so
    // every path (demo, scan, serve, detailed) sees the same thing.
    let detailed = cli.detailed || cli.more_detailed;
    let vuln_details = cli.vuln_details || cli.more_detailed;
    // `--repo org/repo`: clone (or refresh) a GitHub repo into the cache
    // and use that checkout wherever a local folder would be scanned.
    let repo_path = match &cli.repo {
        Some(spec) => Some(scan::git::fetch_github_repo(spec)?),
        None => None,
    };
    let effective_path = |default: &Path| -> ToolResult<std::path::PathBuf> {
        Ok(repo_path.clone().unwrap_or_else(|| default.to_path_buf()))
    };
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
        None => all_in_one(
            &effective_path(&cli.path)?,
            &cli.db,
            cli.repo.clone(),
            8334,
            5000,
            detailed,
            vuln_details,
            cli.no_update_check,
        ),
        Some(Command::Scan {
            path,
            db,
            force,
            max_commits,
        }) => {
            spawn_daily_update_check(cli.no_update_check);
            warn_vuln_details_without_detailed(detailed, vuln_details);
            cmd_scan(
                &effective_path(path)?,
                db,
                *force,
                *max_commits,
                detailed,
                vuln_details,
            )
        }
        Some(Command::Serve {
            path,
            db,
            port,
            open,
            max_commits,
        }) => {
            let db_path = db.clone();
            let opened = Arc::new(Database::open(&db.to_string_lossy())?);
            graph::ensure_schema(&opened)?;
            warn_vuln_details_without_detailed(detailed, vuln_details);
            scan_and_serve(
                &effective_path(path)?,
                opened,
                &db_path,
                cli.repo.clone(),
                *port,
                *open,
                *max_commits,
                detailed,
                vuln_details,
                cli.no_update_check,
            )
        }
        Some(Command::Detailed { db, refresh }) => {
            spawn_daily_update_check(cli.no_update_check);
            // Detailed mode is the subcommand's whole job; --vuln-details is
            // meaningful on its own here (the global --detailed is ignored).
            cmd_detailed(db, vuln_details, *refresh)
        }
        Some(Command::Site {
            path,
            db,
            out,
            force,
            max_commits,
            wasm_cdn,
        }) => {
            spawn_daily_update_check(cli.no_update_check);
            warn_vuln_details_without_detailed(detailed, vuln_details);
            cmd_site(
                &effective_path(path)?,
                db,
                out,
                cli.repo.clone(),
                *force,
                *max_commits,
                detailed,
                vuln_details,
                *wasm_cdn,
            )
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

/// A process found listening on the wanted port.
struct PortHolder {
    pid: u32,
    path: String,
}

/// Best effort: who is listening on this port? Uses `lsof` (macOS/Linux;
/// absent on some minimal Linux installs → None). Returns the first
/// LISTENING pid with its full executable path.
fn find_port_holder(port: u16) -> Option<PortHolder> {
    let out = std::process::Command::new("lsof")
        .args([
            "-nP".to_string(),
            format!("-iTCP:{port}"),
            "-sTCP:LISTEN".to_string(),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines().skip(1) {
        // columns: COMMAND PID USER ... — COMMAND may be a full path
        let mut cols = line.split_whitespace();
        let command = cols.next()?;
        let pid = cols.next()?.parse::<u32>().ok()?;
        let path = if command.contains('/') {
            command.to_string()
        } else {
            // bare command name: resolve to the full invocation via ps
            std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "args="])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .map(|s| {
                    s.chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect::<String>()
                })
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| command.to_string())
        };
        return Some(PortHolder { pid, path });
    }
    None
}

/// Bind the serve port, or fail fast — and when the port is held, show the
/// holder's full path and (interactive terminals only) offer to kill it
/// and take the port. Non-interactive runs report and decline.
fn bind_serve_port(port: u16) -> ToolResult<std::net::TcpListener> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let holder = find_port_holder(port);
            if let Some(h) = &holder {
                eprintln!("port {port} is in use: pid {} — {}", h.pid, h.path);
            }
            let interactive = std::io::stdin().is_terminal();
            if interactive {
                if let Some(h) = &holder {
                    eprint!("kill pid {} and take port {port}? [y/N] ", h.pid);
                    std::io::Write::flush(&mut std::io::stderr())?;
                    let mut answer = String::new();
                    if std::io::stdin().read_line(&mut answer).is_ok()
                        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                    {
                        let killed = std::process::Command::new("kill")
                            .arg(h.pid.to_string())
                            .status()
                            .is_ok_and(|s| s.success());
                        if killed {
                            // wait for the port to free up
                            for _ in 0..10 {
                                if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                                    eprintln!("took port {port}");
                                    return Ok(l);
                                }
                                std::thread::sleep(std::time::Duration::from_millis(300));
                            }
                        }
                    }
                }
            }
            let holder_note = holder
                .map(|h| format!(" (held by pid {} — {})", h.pid, h.path))
                .unwrap_or_default();
            Err(ToolError::Other(format!(
                "port {port} is already in use{holder_note} — stop that process, or pick another \
                 port with --port <port>"
            )))
        }
        Err(e) => Err(ToolError::Io(e)),
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
#[allow(clippy::too_many_arguments)]
fn all_in_one(
    path: &Path,
    db: &Path,
    repo_spec: Option<String>,
    port: u16,
    max_commits: u64,
    detailed: bool,
    vuln_details: bool,
    no_update_check: bool,
) -> ToolResult<()> {
    let opened = Arc::new(Database::open(&db.to_string_lossy())?);
    graph::ensure_schema(&opened)?;
    scan_and_serve(
        path,
        opened,
        db,
        repo_spec,
        port,
        true,
        max_commits,
        detailed,
        vuln_details,
        no_update_check,
    )
}

/// Shared serve-path logic: bring the database's CURRENT state up to date
/// fast (worktree-only, provisional intervals), then walk the git history in
/// a background thread while the explorer is already being served.
#[allow(clippy::too_many_arguments)]
fn scan_and_serve(
    path: &Path,
    db: Arc<Database>,
    db_path: &Path,
    repo_spec: Option<String>,
    port: u16,
    open_browser: bool,
    max_commits: u64,
    detailed: bool,
    vuln_details: bool,
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

    // Ctrl+C: graceful shutdown. The first press lets the scan (if one is
    // running) and the history walk finish and get saved; the second force-
    // quits. Without this, SIGINT kills the process mid-WAL-write.
    server::install_shutdown_handler();

    // Bind the port BEFORE any scanning: a busy port must fail fast with a
    // friendly message — and, interactively, an offer to kill the holder.
    let listener = bind_serve_port(port)?;

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
        if detailed {
            run_detailed(&db, vuln_details, false);
        }
    } else {
        eprintln!(
            "history is current (HEAD {}); serving",
            &head[..head.len().min(12)]
        );
        // The scan short-circuits on a current HEAD, but detailed mode is
        // its own pass over the existing graph — a graph scanned without
        // the flag must still get it now (the cache keeps re-runs cheap).
        if detailed {
            run_detailed(&db, vuln_details, false);
        }
    }

    // The walker signals completion so a Ctrl+C during serving can wait,
    // bounded, for its final batches and checkpoint to land.
    let history_done = if !history_current {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
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
                    } else {
                        // Versions first revealed by the full history walk get
                        // their detailed data now; the cache makes this pass
                        // nearly free when the flag ran before serving.
                        if detailed {
                            run_detailed(&bg_db, vuln_details, false);
                        }
                        eprintln!(
                            "background: history complete — {} packages across {} commits, time travel ready",
                            out.packages.len(),
                            out.commits.len()
                        );
                    }
                }
                Err(e) => eprintln!("background history scan failed: {e}"),
            }
            let _ = done_tx.send(());
        });
        Some(done_rx)
    } else {
        None
    };

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
    // The db file's path rides along: on-demand repo scans (the UI's URL
    // box) write each repo to its own sibling database file and hot-swap.
    server::serve(listener, db, db_path, repo_spec, &repo_name, history_done)?;
    Ok(())
}

fn cmd_scan(
    path: &Path,
    db: &Path,
    force: bool,
    max_commits: u64,
    detailed: bool,
    vuln_details: bool,
) -> ToolResult<()> {
    eprintln!("scanning {} …", path.display());
    let caps = scan::ScanCaps {
        max_commits,
        ..scan::ScanCaps::default()
    };

    if db.exists() && !force {
        // A .rl bundle holds one open handle at a time: scope the read-only
        // probe so the writable open below never trips "storage locked".
        let stored_head = {
            let existing = SbomGraph::open_read_only(db)?;
            existing.stored_history_head()?
        };
        if let Some(head) = stored_head {
            let current = scan::git::head_sha(path)?;
            if head == current {
                eprintln!(
                    "history is current (HEAD {head}); nothing to scan (use --force to rescan)"
                );
                // The scan short-circuits on a current HEAD, but detailed
                // mode is its own pass over the existing graph — a graph
                // scanned without the flag must still get it now (the
                // cache keeps re-runs cheap).
                if detailed {
                    eprintln!("applying detailed mode to the existing graph …");
                    let graph = SbomGraph::open_writable(db)?;
                    run_detailed(&graph.db, vuln_details, false);
                    eprintln!("wrote {}", db.display());
                }
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
    if detailed {
        run_detailed(&graph.db, vuln_details, false);
    }
    Ok(())
}

/// Detailed-mode pass over an already-opened database. Never fails the
/// calling command — network trouble is warned and counted inside.
fn run_detailed(db: &Database, vuln_details: bool, refresh: bool) {
    let opts = DetailedOpts {
        vuln_details,
        refresh,
    };
    if let Err(e) = depsdev::detailed_graph(db, &opts) {
        eprintln!("warning: detailed mode failed: {e}");
    }
}

/// `--vuln-details` only does something together with `--detailed`.
fn warn_vuln_details_without_detailed(detailed: bool, vuln_details: bool) {
    if vuln_details && !detailed {
        eprintln!("warning: --vuln-details has no effect without --detailed");
    }
}

/// `bomvoyage site`: scan (or reuse the db) + optional detailed pass +
/// dump + emit the static site directory.
#[allow(clippy::too_many_arguments)]
fn cmd_site(
    path: &Path,
    db: &Path,
    out: &Path,
    repo_spec: Option<String>,
    force: bool,
    max_commits: u64,
    detailed: bool,
    vuln_details: bool,
    wasm_cdn: bool,
) -> ToolResult<()> {
    bomvoyage::site::site_command(
        path,
        db,
        out,
        repo_spec,
        force,
        max_commits,
        detailed,
        vuln_details,
        wasm_cdn,
    )
}

/// `bomvoyage detailed`: add detailed data to an existing graph in place,
/// no rescan needed.
fn cmd_detailed(db: &Path, vuln_details: bool, refresh: bool) -> ToolResult<()> {
    let graph = SbomGraph::open_writable(db)?;
    let opts = DetailedOpts {
        vuln_details,
        refresh,
    };
    let stats = depsdev::detailed_graph(&graph.db, &opts)?;
    eprintln!("{}", stats.summary_line());
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
