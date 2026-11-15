//! The explorer web server: hand-rolled HTTP/1.1 on std::net (no web
//! framework — the binary stays self-contained, mirroring relate-http),
//! thread-per-connection, static UI embedded in the binary.

pub mod api;
pub mod http;

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use relate::Database;

use crate::server::api::Api;
use crate::server::http::{read_request, respond, Request};

pub const INDEX_HTML: &str = include_str!("../ui/index.html");
pub const APP_JS: &str = include_str!("../ui/app.js");
pub const SEARCH_JS: &str = include_str!("../ui/search.js");
pub const STYLE_CSS: &str = include_str!("../ui/style.css");
pub const CYTOSCAPE_JS: &[u8] = include_bytes!("../ui/vendor/cytoscape.min.js");

/// Set by the first Ctrl+C; the serve loop polls it between accepts.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Install the Ctrl+C / SIGTERM handler (the serve path only). First press
/// requests a graceful shutdown: the explorer stops accepting and waits,
/// bounded, for the background history walk to finish — a deliberate exit
/// instead of the default signal kill, which is the safer mid-WAL-write
/// default. A second press force-quits.
pub fn install_shutdown_handler() {
    let result = ctrlc::set_handler(|| {
        if SHUTDOWN.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
        eprintln!("shutting down — press Ctrl+C again to force quit");
    });
    if let Err(e) = result {
        eprintln!("warning: could not install the Ctrl+C handler ({e}); default kill applies");
    }
}

/// Serve until SIGINT/SIGTERM (or `SHUTDOWN`, which the tests set directly).
/// One thread per connection; each request opens its own engine connection
/// from the shared `Database`.
///
/// `history_done` is the completion signal of the background history walker
/// (see `scan_and_serve`); when shutting down we wait, bounded, for the walk
/// to finish so its last batches and the final checkpoint land in the file.
/// In-flight connection threads are not joined — each already has a 30s
/// read timeout, and the process exit reclaims them.
pub fn serve(
    db: Arc<Database>,
    repo_name: &str,
    port: u16,
    history_done: Option<mpsc::Receiver<()>>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    eprintln!("BOMvoyage explorer: http://127.0.0.1:{}", addr.port());
    let api = Arc::new(Api {
        db,
        repo_name: repo_name.to_string(),
    });
    // Non-blocking accept + poll: the blocking `incoming()` iterator cannot
    // observe the shutdown flag while it waits.
    listener.set_nonblocking(true)?;
    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let api = Arc::clone(&api);
                std::thread::spawn(move || handle_connection(stream, &api));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
    eprintln!("explorer stopped");
    if let Some(done) = history_done {
        eprintln!("waiting up to 2 minutes for the background history walk to finish …");
        match done.recv_timeout(Duration::from_secs(120)) {
            Ok(()) => eprintln!("history walk finished — everything is saved"),
            Err(_) => eprintln!(
                "warning: the history walk is still running — progress committed so far is \
                 saved, and the next scan completes the rest"
            ),
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, api: &Api) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let result = read_request(&mut stream).and_then(|req| route(&mut stream, &req, api));
    if let Err(e) = result {
        eprintln!("request failed: {e}");
        let _ = respond(
            &mut stream,
            500,
            "application/json",
            br#"{"error": "internal server error"}"#,
        );
    }
}

fn route(stream: &mut TcpStream, req: &Request, api: &Api) -> std::io::Result<()> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond(
            stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        ("GET", "/app.js") => respond(stream, 200, "application/javascript", APP_JS.as_bytes()),
        ("GET", "/search.js") => {
            respond(stream, 200, "application/javascript", SEARCH_JS.as_bytes())
        }
        ("GET", "/style.css") => respond(stream, 200, "text/css", STYLE_CSS.as_bytes()),
        ("GET", "/cytoscape.min.js") => {
            respond(stream, 200, "application/javascript", CYTOSCAPE_JS)
        }
        ("GET", "/favicon.ico") => respond(stream, 404, "text/plain", b""),
        (_, p) if p.starts_with("/api/") => api.handle(stream, req),
        _ => respond(
            stream,
            404,
            "application/json",
            br#"{"error": "not found"}"#,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded UI is hand-rolled JS with no build step or bundler, so a
    /// syntax error ships silently and blanks the explorer (a premature
    /// array close once did exactly that — empty UI, stuck on "Loading…").
    /// When `node` is available, parse-check every embedded script exactly
    /// as a browser would. CI runners always have node; a machine without
    /// one skips rather than fails.
    #[test]
    fn embedded_ui_scripts_parse() {
        let node = match std::process::Command::new("node").arg("--version").output() {
            Ok(out) if out.status.success() => "node".to_string(),
            _ => {
                eprintln!("node not available — skipping UI parse check");
                return;
            }
        };
        for (name, source) in [("app.js", APP_JS), ("search.js", SEARCH_JS)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(name);
            std::fs::write(&path, source).expect("write script");
            let out = std::process::Command::new(&node)
                .arg("--check")
                .arg(&path)
                .output()
                .expect("node runs");
            assert!(
                out.status.success(),
                "embedded {name} has a syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The static UI must stay wired to the server: every script/style the
    /// page loads must be among the embedded, routed files.
    #[test]
    fn index_references_only_embedded_assets() {
        for src in ["/app.js", "/search.js", "/style.css", "/cytoscape.min.js"] {
            assert!(
                INDEX_HTML.contains(src.trim_start_matches('/')),
                "index.html references {src} but the check didn't find it"
            );
        }
    }

    /// Behavior, not just syntax: run the real app.js in node against a
    /// stubbed DOM + fetch and assert the wiring — most notably that the
    /// detailed-mode vulnerable-deps console preset exists exactly once
    /// (a double-insert once shipped: the preset was both spliced into the
    /// array and button-injected, and the wiring loop appended it again).
    #[test]
    fn embedded_ui_wiring_smoke() {
        let node = match std::process::Command::new("node").arg("--version").output() {
            Ok(out) if out.status.success() => "node".to_string(),
            _ => {
                eprintln!("node not available — skipping UI wiring smoke");
                return;
            }
        };
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ui_smoke.mjs");
        let out = std::process::Command::new(&node)
            .arg(&script)
            .output()
            .expect("node runs");
        assert!(
            out.status.success(),
            "embedded UI wiring smoke failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The serve loop must observe the shutdown flag and return promptly —
    /// this is what makes Ctrl+C a clean stop instead of a signal kill.
    /// (No signals in the test: the flag is set directly, CI-portably.)
    #[test]
    fn serve_shuts_down_on_flag() {
        let worker = std::thread::spawn(|| {
            let db = relate::Database::open_with(
                ":memory:",
                relate::OpenOptions {
                    mode: relate::OpenMode::Ephemeral,
                    ..relate::OpenOptions::default()
                },
            )
            .expect("ephemeral db opens");
            super::serve(
                std::sync::Arc::new(db),
                "shutdown-test",
                0, // ephemeral port; no request is ever made
                None,
            )
            .expect("serve returns cleanly on shutdown");
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        SHUTDOWN.store(true, Ordering::SeqCst);
        worker.join().expect("serve returns on shutdown");
    }
}
