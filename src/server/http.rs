//! Minimal HTTP/1.1 over std::net — one request per connection, answered
//! with `Connection: close` (the relate-http surface's deliberate shape).

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

pub fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    // Read until the header terminator.
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    // httparse needs the terminator to know the head ended.
    let head = &buf[..header_end + 4];
    if request
        .parse(head)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        .is_partial()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "partial request",
        ));
    }
    let method = request.method.unwrap_or("GET").to_string();
    let target = request.path.unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // Body: Content-Length bytes after the header terminator.
    let content_length = request
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > 8 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body too large",
        ));
    }
    let body_start = header_end + 4;
    let mut body: Vec<u8> = buf[body_start.min(buf.len())..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n.min(content_length - body.len())]);
    }

    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

pub fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn respond_json(
    stream: &mut TcpStream,
    status: u16,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    respond(
        stream,
        status,
        "application/json",
        body.to_string().as_bytes(),
    )
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
