//! Snapshot dump: read the graph back out as the same rows the native
//! write path sends, so a generated site can replay it into an in-browser
//! RelateDb (WASM) through the very same MERGE statements.

use std::collections::BTreeMap;

use relate::{Connection, Value};
use serde::Serialize;

use crate::error::ToolResult;

/// Read-back mirrors of each `write::MERGE_*` — same fields, same names,
/// same keys. Order matters only for the replay (MATCH targets must
/// exist), which `site::replay_snapshot` controls.
pub mod dump_queries {
    pub const PACKAGES: &str = r#"
MATCH (p:Package)
RETURN p.purl AS purl, p.name AS name, p.ecosystem AS eco, p.version AS version, p.scope AS scope"#;

    /// Detailed-mode (deps.dev) properties — replayed through
    /// `MERGE_PACKAGE_DETAILED`; only packages that carry the data.
    pub const DETAILED: &str = r#"
MATCH (p:Package)
WHERE p.depsdev_status IS NOT NULL
RETURN p.purl AS purl, p.depsdev_url AS depsdev_url, p.depsdev_status AS depsdev_status,
       p.depsdev_fetched_at AS fetched_at, p.homepage AS homepage, p.repository AS repository,
       p.license AS license, p.vulns AS vulns, p.advisories AS advisories, p.deprecated AS deprecated"#;

    pub const COMMITS: &str = r#"
MATCH (c:Commit)
RETURN c.sha AS sha, c.date AS date, c.author_email AS email, c.summary AS summary"#;

    pub const PEOPLE: &str = r#"
MATCH (x:Person) RETURN x.email AS email, x.name AS name"#;

    pub const PROJECT: &str = r#"
MATCH (pr:Project)
RETURN pr.name AS name, pr.head_sha AS head_sha, pr.last_scanned_at AS scanned_at,
       pr.tool_version AS tool_version, pr.scan_quality AS scan_quality,
       pr.history_head_sha AS history_head"#;

    pub const HAS_MANIFEST: &str = r#"
MATCH (pr:Project)-[r:HAS_MANIFEST]->(m:Manifest)
RETURN pr.name AS name, m.path AS path, m.ecosystem AS eco, m.kind AS kind"#;

    pub const ROOT: &str = r#"
MATCH (pr:Project)-[r:ROOT]->(p:Package)
RETURN pr.name AS name, p.purl AS purl"#;

    pub const DECLARES: &str = r#"
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
RETURN m.path AS manifest, p.purl AS purl, r.key AS key,
       r.since_sha AS since_sha, r.since_date AS since_date,
       r.removed_sha AS removed_sha, r.removed_date AS removed_date,
       r.version_requirement AS requirement, r.scope AS scope,
       r.added_by_email AS email, r.added_by_name AS name, r.provisional AS provisional"#;

    pub const DEPENDS_ON: &str = r#"
MATCH (a:Package)-[r:DEPENDS_ON]->(b:Package)
RETURN a.purl AS from, b.purl AS to, r.key AS key,
       r.since_sha AS since_sha, r.since_date AS since_date,
       r.removed_sha AS removed_sha, r.removed_date AS removed_date, r.source AS source"#;

    pub const IMPORTS: &str = r#"
MATCH (f:File)-[r:IMPORTS]->(p:Package)
RETURN f.path AS file, p.purl AS purl, r.key AS key,
       r.symbol AS symbol, r.since_date AS since_date, f.language AS language"#;

    pub const ADDED: &str = r#"
MATCH (x:Person)-[r:ADDED]->(p:Package)
RETURN x.email AS email, p.purl AS purl, r.at_sha AS sha, r.at_date AS date, r.provisional AS provisional"#;

    pub const REMOVED: &str = r#"
MATCH (x:Person)-[r:REMOVED]->(p:Package)
RETURN x.email AS email, p.purl AS purl, r.at_sha AS sha, r.at_date AS date"#;

    pub const TOUCHED: &str = r#"
MATCH (c:Commit)-[r:TOUCHED]->(m:Manifest)
RETURN c.sha AS sha, m.path AS path"#;

    /// Dump order (SELECT side): each stage's read-back query. Fields and
    /// names mirror the matching MERGE statement exactly.
    pub const DUMP_ORDER: &[(&str, &str)] = &[
        ("people", PEOPLE),
        ("packages", PACKAGES),
        ("detailed", DETAILED),
        ("project", PROJECT),
        ("has_manifest", HAS_MANIFEST),
        ("root", ROOT),
        ("commits", COMMITS),
        ("declares", DECLARES),
        ("depends_on", DEPENDS_ON),
        ("imports", IMPORTS),
        ("added", ADDED),
        ("removed", REMOVED),
        ("touched", TOUCHED),
    ];

    /// Replay order: every MERGE's MATCH targets must exist before its
    /// edges/rows land (mirrors `write_scan`'s order in graph/mod.rs).
    pub const REPLAY_ORDER: &[(&str, &str)] = &[
        ("packages", crate::graph::write::MERGE_PACKAGES),
        ("detailed", crate::graph::write::MERGE_PACKAGE_DETAILED),
        ("commits", crate::graph::write::MERGE_COMMITS),
        ("people", crate::graph::write::MERGE_PEOPLE),
        ("project", crate::graph::write::MERGE_PROJECT),
        ("has_manifest", crate::graph::write::MERGE_HAS_MANIFEST),
        ("root", crate::graph::write::MERGE_ROOT),
        ("declares", crate::graph::write::MERGE_DECLARES),
        ("depends_on", crate::graph::write::MERGE_DEPENDS_ON),
        ("imports", crate::graph::write::MERGE_IMPORTS),
        ("added", crate::graph::write::MERGE_ADDED),
        ("removed", crate::graph::write::MERGE_REMOVED),
        ("touched", crate::graph::write::MERGE_TOUCHED),
    ];
}

pub type RowJson = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub format: u32,
    pub tool_version: String,
    pub generated_at: String,
    pub repo_name: String,
    pub head_sha: String,
    pub history_complete: bool,
    /// The github spec (--repo org/repo) the export came from, if any —
    /// the generated page prepopulates its repo box from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_spec: Option<String>,
    /// Rows per stage, keyed by the stage name; stages with no rows are
    /// omitted (leaner JSON, and the replay skips them).
    pub stages: BTreeMap<String, Vec<RowJson>>,
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Value::from(*f),
        Value::String(s) => serde_json::Value::String(s.to_string()),
        // dump rows are scalar-only; anything else degrades to its display
        other => serde_json::Value::String(other.to_string()),
    }
}

/// Read one stage's rows as named JSON maps (column name → value).
fn dump_stage(conn: &mut Connection, query: &str) -> ToolResult<Vec<RowJson>> {
    let result = conn.query(query)?;
    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut m = BTreeMap::new();
        for (i, col) in result.header.columns.iter().enumerate() {
            m.insert(
                col.clone(),
                value_to_json(row.get(i).unwrap_or(&Value::Null)),
            );
        }
        out.push(m);
    }
    Ok(out)
}

/// Read the whole graph back as a replayable snapshot.
pub fn dump(
    conn: &mut Connection,
    repo_name: &str,
    head_sha: &str,
    history_complete: bool,
) -> ToolResult<Snapshot> {
    let mut stages = BTreeMap::new();
    for (name, query) in dump_queries::DUMP_ORDER {
        let rows = dump_stage(conn, query)?;
        if !rows.is_empty() {
            stages.insert(name.to_string(), rows);
        }
    }
    Ok(Snapshot {
        format: 1,
        repo_spec: None,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: crate::graph::now_iso(),
        repo_name: repo_name.to_string(),
        head_sha: head_sha.to_string(),
        history_complete,
        stages,
    })
}

/// Replay a snapshot into a database — the native twin of the browser's
/// replay (same MERGE constants, same 500-row batches). Used by the
/// round-trip test to prove a replayed snapshot builds the identical graph.
pub fn replay_snapshot(conn: &mut Connection, snapshot: &Snapshot) -> ToolResult<()> {
    for (name, statement) in dump_queries::REPLAY_ORDER {
        let Some(rows) = snapshot.stages.get(*name) else {
            continue;
        };
        // RowJson → relate Row
        let to_row = |m: &RowJson| -> crate::graph::write::Row {
            let mut r = crate::graph::write::Row::new();
            for (k, v) in m {
                let value = match v {
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
                    other => Value::String(other.to_string().into()),
                };
                r.insert(k.clone(), value);
            }
            r
        };
        let rows: Vec<crate::graph::write::Row> = rows.iter().map(&to_row).collect();
        crate::graph::write::flush(conn, statement, rows)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_order_matches_merge_constants() {
        // every stage maps to a distinct MERGE statement
        let mut seen = std::collections::BTreeSet::new();
        for (name, stmt) in dump_queries::REPLAY_ORDER {
            assert!(seen.insert(*stmt), "duplicate MERGE for {name}");
        }
        assert_eq!(dump_queries::REPLAY_ORDER.len(), 13);
    }
}
