//! JSON API handlers. Every endpoint takes an optional `as_of` parameter
//! (ISO date, datetime, or commit-sha prefix); the server resolves it once
//! and feeds the same temporal Cypher the CLI uses.

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::sync::Arc;

use relate::{Database, Value};

use crate::graph::{self, queries};
use crate::server::http::{respond_json, Request};

pub struct Api {
    pub db: Arc<Database>,
    pub repo_name: String,
}

/// Query parameter extraction: the first value of `name`.
fn qp(req: &Request, name: &str) -> Option<String> {
    req.query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == name).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

impl Api {
    pub fn handle(&self, stream: &mut TcpStream, req: &Request) -> std::io::Result<()> {
        let result: std::io::Result<serde_json::Value> =
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/api/meta") => self.meta(),
                ("GET", "/api/graph") => {
                    self.graph(qp(req, "as_of").as_deref(), qp(req, "depth").as_deref())
                }
                ("GET", "/api/history") => self.history(qp(req, "name").as_deref()),
                ("GET", "/api/usage") => self.usage(qp(req, "purl").as_deref()),
                ("GET", "/api/diff") => {
                    self.diff(qp(req, "from").as_deref(), qp(req, "to").as_deref())
                }
                ("GET", "/api/commits") => self.commits(),
                ("POST", "/api/query") => self.raw_query(&req.body),
                ("GET", _) | ("POST", _) => Ok(serde_json::json!({"error": "not found"})),
                _ => Ok(serde_json::json!({"error": "method not allowed"})),
            };
        match result {
            Ok(value) => {
                let is_error = value.get("error").is_some();
                respond_json(stream, if is_error { 400 } else { 200 }, &value)
            }
            Err(e) => respond_json(stream, 500, &serde_json::json!({"error": e.to_string()})),
        }
    }

    fn connect(&self) -> std::io::Result<relate::Connection> {
        self.db.connect().map_err(rel_io)
    }

    fn meta(&self) -> std::io::Result<serde_json::Value> {
        let mut conn = self.connect()?;
        let result = conn.query(queries::META).map_err(rel_io)?;
        let head_sha = cell(&result, 0, "head_sha");
        let scanned_at = cell(&result, 0, "scanned_at");
        let scan_quality = cell(&result, 0, "scan_quality");
        let history_head = cell(&result, 0, "history_head");
        // The time-travel data is ready when the history walk has completed
        // for exactly the head the current scan recorded.
        let history_current = !history_head.is_empty() && history_head == head_sha;

        let counts = conn
            .query("MATCH (p:Package) RETURN p.ecosystem AS eco, count(DISTINCT p.name) AS n ORDER BY n DESC")
            .map_err(rel_io)?;
        let c_eco = counts.header.columns.iter().position(|c| *c == "eco");
        let c_n = counts.header.columns.iter().position(|c| *c == "n");
        let ecosystems: Vec<serde_json::Value> = counts
            .rows
            .iter()
            .filter_map(|row| {
                let eco = c_eco
                    .and_then(|i| row.get(i))
                    .and_then(|v| v.as_string().map(str::to_string));
                let n = c_n.and_then(|i| row.get(i)).and_then(|v| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                });
                eco.zip(n)
                    .map(|(eco, n)| serde_json::json!({"ecosystem": eco, "names": n}))
            })
            .collect();

        let total = conn
            .query("MATCH (p:Package) RETURN count(p) AS n")
            .map_err(rel_io)?;
        let total_packages = total
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| match v {
                Value::Int(i) => Some(*i),
                _ => None,
            })
            .unwrap_or(0);

        Ok(serde_json::json!({
            "name": self.repo_name,
            "head_sha": head_sha,
            "last_scanned_at": scanned_at,
            "scan_quality": scan_quality,
            "history_current": history_current,
            "total_packages": total_packages,
            "ecosystems": ecosystems,
        }))
    }

    /// The as-of dependency graph in cytoscape's elements shape.
    fn graph(
        &self,
        as_of: Option<&str>,
        _depth: Option<&str>,
    ) -> std::io::Result<serde_json::Value> {
        let mut conn = self.connect()?;
        let as_of = graph::resolve_as_of(&mut conn, as_of).map_err(rel_io)?;

        let mut params: BTreeMap<String, Value> = BTreeMap::new();
        params.insert("as_of".into(), Value::String(as_of.clone().into()));
        let pkgs = conn
            .query_with_params(queries::LIVE_PACKAGES, params.clone())
            .map_err(rel_io)?;
        let edges = conn
            .query_with_params(queries::LIVE_DEPENDS_ON, params)
            .map_err(rel_io)?;

        let col =
            |r: &relate::QueryResult, name: &str| r.header.columns.iter().position(|c| c == name);
        let pick = |row: &[Value], i: Option<usize>| -> String {
            i.and_then(|i| row.get(i))
                .and_then(|v| v.as_string().map(str::to_string))
                .unwrap_or_default()
        };

        let nodes: Vec<serde_json::Value> = pkgs
            .rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "data": {
                        "id": pick(row, col(&pkgs, "purl")),
                        "label": format!("{}{}", pick(row, col(&pkgs, "name")), version_suffix(&pick(row, col(&pkgs, "version")))),
                        "ecosystem": pick(row, col(&pkgs, "ecosystem")),
                        "scope": pick(row, col(&pkgs, "scope")),
                        "manifest": pick(row, col(&pkgs, "manifest")),
                        "added_by": pick(row, col(&pkgs, "added_by")),
                        "added_date": pick(row, col(&pkgs, "added_date")),
                        "requirement": pick(row, col(&pkgs, "requirement")),
                    }
                })
            })
            .collect();
        let edges_json: Vec<serde_json::Value> = edges
            .rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "data": {
                        "source": pick(row, col(&edges, "from")),
                        "target": pick(row, col(&edges, "to")),
                        "source_kind": pick(row, col(&edges, "source")),
                    }
                })
            })
            .collect();

        Ok(serde_json::json!({
            "as_of": as_of,
            "elements": {"nodes": nodes, "edges": edges_json},
        }))
    }

    fn history(&self, name: Option<&str>) -> std::io::Result<serde_json::Value> {
        let Some(name) = name else {
            return Ok(serde_json::json!({"error": "missing ?name="}));
        };
        let mut conn = self.connect()?;
        let mut params: BTreeMap<String, Value> = BTreeMap::new();
        params.insert("name".into(), Value::String(name.into()));
        let result = conn
            .query_with_params(queries::HISTORY, params)
            .map_err(rel_io)?;
        Ok(query_to_json(&result))
    }

    fn usage(&self, purl: Option<&str>) -> std::io::Result<serde_json::Value> {
        let Some(purl) = purl else {
            return Ok(serde_json::json!({"error": "missing ?purl="}));
        };
        let mut conn = self.connect()?;
        let mut params: BTreeMap<String, Value> = BTreeMap::new();
        params.insert("purl".into(), Value::String(purl.into()));
        let result = conn
            .query_with_params(queries::USAGE, params)
            .map_err(rel_io)?;
        Ok(query_to_json(&result))
    }

    fn diff(&self, from: Option<&str>, to: Option<&str>) -> std::io::Result<serde_json::Value> {
        let (Some(from), Some(to)) = (from, to) else {
            return Ok(serde_json::json!({"error": "missing ?from= and ?to="}));
        };
        let mut conn = self.connect()?;
        let from = graph::resolve_as_of(&mut conn, Some(from)).map_err(rel_io)?;
        let to = graph::resolve_as_of(&mut conn, Some(to)).map_err(rel_io)?;
        let mut params: BTreeMap<String, Value> = BTreeMap::new();
        params.insert("d1".into(), Value::String(from.into()));
        params.insert("d2".into(), Value::String(to.into()));
        let result = conn
            .query_with_params(queries::DIFF, params)
            .map_err(rel_io)?;
        Ok(query_to_json(&result))
    }

    fn commits(&self) -> std::io::Result<serde_json::Value> {
        let mut conn = self.connect()?;
        let result = conn.query(queries::COMMITS).map_err(rel_io)?;
        Ok(query_to_json(&result))
    }

    /// Raw read-only Cypher: mutation prefixes are refused before the engine
    /// sees them, so the UI's query box cannot mutate the graph.
    fn raw_query(&self, body: &[u8]) -> std::io::Result<serde_json::Value> {
        let parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let cypher = parsed
            .get("cypher")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "missing cypher")
            })?;
        let upper = cypher.trim_start().to_ascii_uppercase();
        for banned in [
            "CREATE", "MERGE", "DELETE", "SET ", "REMOVE", "DROP", "DETACH", "CALL {", "LOAD CSV",
            "FOREACH",
        ] {
            if upper.starts_with(banned) {
                return Ok(serde_json::json!({
                    "error": format!("read-only endpoint: refusing statements starting with {}", banned.trim())
                }));
            }
        }
        let mut conn = self.connect()?;
        let result = if let Some(params_json) = parsed.get("params") {
            let mut params: BTreeMap<String, Value> = BTreeMap::new();
            if let Some(map) = params_json.as_object() {
                for (k, v) in map {
                    params.insert(k.clone(), json_to_value(v));
                }
            }
            conn.query_with_params(cypher, params)
        } else {
            conn.query(cypher)
        }
        .map_err(rel_io)?;
        let mut out = query_to_json(&result);
        out["columns"] = serde_json::Value::Array(
            result
                .header
                .columns
                .iter()
                .map(|c| serde_json::Value::String(c.clone()))
                .collect(),
        );
        Ok(out)
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone().into()),
        serde_json::Value::Array(a) => {
            Value::List(a.iter().map(json_to_value).collect::<Vec<_>>().into())
        }
        serde_json::Value::Object(o) => {
            let mut m = BTreeMap::new();
            for (k, v) in o {
                m.insert(k.clone(), json_to_value(v));
            }
            Value::Map(m.into())
        }
    }
}

/// Generic QueryResult → JSON rows (named maps).
fn query_to_json(result: &relate::QueryResult) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = result
        .rows
        .iter()
        .map(|row| {
            let mut m = serde_json::Map::new();
            for (i, col) in result.header.columns.iter().enumerate() {
                let v = row.get(i).cloned().unwrap_or(Value::Null);
                m.insert(col.clone(), value_to_json(&v));
            }
            serde_json::Value::Object(m)
        })
        .collect();
    serde_json::json!({ "rows": rows })
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Value::from(*f),
        Value::String(s) => serde_json::Value::String(s.to_string()),
        Value::List(l) => serde_json::Value::Array(l.iter().map(value_to_json).collect()),
        Value::Map(m) => {
            let mut o = serde_json::Map::new();
            for (k, v) in m.iter() {
                o.insert(k.clone(), value_to_json(v));
            }
            serde_json::Value::Object(o)
        }
        other => serde_json::Value::String(other.to_string()),
    }
}

fn cell(result: &relate::QueryResult, row: usize, col_name: &str) -> String {
    let col = result.header.columns.iter().position(|c| c == col_name);
    result
        .rows
        .get(row)
        .and_then(|r| col.and_then(|i| r.get(i)))
        .and_then(|v| v.as_string().map(str::to_string))
        .unwrap_or_default()
}

fn version_suffix(version: &str) -> String {
    if version.is_empty() {
        String::new()
    } else {
        format!("@{version}")
    }
}

/// Any engine or tool error becomes a 500 with its Display text.
fn rel_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
