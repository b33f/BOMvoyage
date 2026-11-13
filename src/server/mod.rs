//! The explorer web server: hand-rolled HTTP/1.1 on std::net (no web
//! framework — the binary stays self-contained, mirroring relate-http),
//! thread-per-connection, static UI embedded in the binary.

pub mod api;
pub mod http;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use relate::Database;

use crate::server::api::Api;
use crate::server::http::{read_request, respond, Request};

pub const INDEX_HTML: &str = include_str!("../ui/index.html");
pub const APP_JS: &str = include_str!("../ui/app.js");
pub const STYLE_CSS: &str = include_str!("../ui/style.css");
pub const CYTOSCAPE_JS: &[u8] = include_bytes!("../ui/vendor/cytoscape.min.js");

/// Serve until SIGINT/SIGTERM. One thread per connection; each request opens
/// its own engine connection from the shared `Database`.
pub fn serve(db: Arc<Database>, repo_name: &str, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("BOMvoyage explorer: http://127.0.0.1:{port}");
    let api = Arc::new(Api {
        db,
        repo_name: repo_name.to_string(),
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let api = Arc::clone(&api);
        std::thread::spawn(move || handle_connection(stream, &api));
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
