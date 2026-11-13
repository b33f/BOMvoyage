//! Batched, idempotent writes. Every MERGE keys on natural identity — purl,
//! path, sha, email, or a composite relationship key — so re-scanning the
//! same repo twice leaves the graph unchanged. Rows ride UNWIND payloads
//! (batches of 500); each stage runs in one transaction.

use std::collections::BTreeMap;

use relate::Value;

/// Batch size for UNWIND writes.
const BATCH: usize = 500;

pub type Row = BTreeMap<String, Value>;

pub fn rows_param(rows: &[Row]) -> BTreeMap<String, Value> {
    let list: Vec<Value> = rows
        .iter()
        .map(|row| Value::Map(row.clone().into()))
        .collect();
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), Value::List(list.into()));
    params
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn opt_s(v: &Option<String>) -> Value {
    match v {
        Some(x) => s(x),
        None => Value::Null,
    }
}

pub fn flush(conn: &mut relate::Connection, cypher: &str, rows: Vec<Row>) -> relate::Result<()> {
    for batch in rows.chunks(BATCH) {
        conn.query_with_params(cypher, rows_param(batch))?;
    }
    Ok(())
}

/// Nodes for every package version ever seen.
pub const MERGE_PACKAGES: &str = r#"
UNWIND $rows AS row
MERGE (p:Package {purl: row.purl})
SET p.name = row.name, p.ecosystem = row.eco, p.version = row.version, p.scope = row.scope
"#;

pub fn package_rows(packages: &[super::super::scan::PkgRef]) -> Vec<Row> {
    packages
        .iter()
        .map(|p| {
            let mut r = Row::new();
            r.insert("purl".into(), s(&p.purl()));
            r.insert("name".into(), s(&p.name));
            r.insert("eco".into(), s(eco_name(p.eco)));
            r.insert("version".into(), s(&p.version));
            r.insert("scope".into(), s(scope_name(p.scope)));
            r
        })
        .collect()
}

/// Commits that touched manifests.
pub const MERGE_COMMITS: &str = r#"
UNWIND $rows AS row
MERGE (c:Commit {sha: row.sha})
SET c.date = row.date, c.author_email = row.email, c.summary = row.summary
"#;

/// Persons.
pub const MERGE_PEOPLE: &str = r#"
UNWIND $rows AS row
MERGE (x:Person {email: row.email})
SET x.name = row.name
"#;

/// DECLARES intervals, keyed `(manifest_path, purl, since_sha)`.
pub const MERGE_DECLARES: &str = r#"
UNWIND $rows AS row
MATCH (m:Manifest {path: row.manifest})
MATCH (p:Package {purl: row.purl})
MERGE (m)-[r:DECLARES {key: row.key}]->(p)
SET r.since_sha = row.since_sha, r.since_date = row.since_date,
    r.removed_sha = row.removed_sha, r.removed_date = row.removed_date,
    r.version_requirement = row.requirement, r.scope = row.scope,
    r.added_by_email = row.email, r.added_by_name = row.name
"#;

/// DEPENDS_ON intervals, keyed `(from_purl, to_purl, since_sha)`.
pub const MERGE_DEPENDS_ON: &str = r#"
UNWIND $rows AS row
MATCH (a:Package {purl: row.from})
MATCH (b:Package {purl: row.to})
MERGE (a)-[r:DEPENDS_ON {key: row.key}]->(b)
SET r.since_sha = row.since_sha, r.since_date = row.since_date,
    r.removed_sha = row.removed_sha, r.removed_date = row.removed_date,
    r.source = row.source
"#;

/// IMPORTS (worktree usage sites), keyed `(file, purl, symbol)`.
pub const MERGE_IMPORTS: &str = r#"
UNWIND $rows AS row
MERGE (f:File {path: row.file})
SET f.language = row.language
MATCH (p:Package {purl: row.purl})
MERGE (f)-[r:IMPORTS {key: row.key}]->(p)
SET r.symbol = row.symbol, r.since_date = row.since_date
"#;

/// Project node and its links to manifests + the root package.
pub const MERGE_PROJECT: &str = r#"
UNWIND $rows AS row
MERGE (pr:Project {name: row.name})
SET pr.head_sha = row.head_sha, pr.last_scanned_at = row.scanned_at,
    pr.tool_version = row.tool_version, pr.scan_quality = row.scan_quality
"#;

pub const MERGE_HAS_MANIFEST: &str = r#"
UNWIND $rows AS row
MATCH (pr:Project {name: row.name})
MERGE (pr)-[r:HAS_MANIFEST]->(m:Manifest {path: row.path})
SET m.ecosystem = row.eco, m.kind = row.kind
"#;

pub const MERGE_ROOT: &str = r#"
UNWIND $rows AS row
MATCH (pr:Project {name: row.name})
MATCH (p:Package {purl: row.purl})
MERGE (pr)-[r:ROOT]->(p)
"#;

pub const MERGE_ADDED: &str = r#"
UNWIND $rows AS row
MATCH (x:Person {email: row.email})
MATCH (p:Package {purl: row.purl})
MERGE (x)-[r:ADDED {at_sha: row.sha}]->(p)
SET r.at_date = row.date
"#;

pub const MERGE_REMOVED: &str = r#"
UNWIND $rows AS row
MATCH (x:Person {email: row.email})
MATCH (p:Package {purl: row.purl})
MERGE (x)-[r:REMOVED {at_sha: row.sha}]->(p)
SET r.at_date = row.date
"#;

pub const MERGE_TOUCHED: &str = r#"
UNWIND $rows AS row
MATCH (c:Commit {sha: row.sha})
MATCH (m:Manifest {path: row.path})
MERGE (c)-[r:TOUCHED]->(m)
"#;

pub fn eco_name(eco: super::super::scan::Eco) -> &'static str {
    match eco {
        super::super::scan::Eco::Python => "python",
        super::super::scan::Eco::Java => "java",
        super::super::scan::Eco::Rust => "rust",
        super::super::scan::Eco::Go => "go",
    }
}

pub fn scope_name(scope: super::super::scan::Scope) -> &'static str {
    match scope {
        super::super::scan::Scope::Direct => "direct",
        super::super::scan::Scope::Transitive => "transitive",
    }
}

pub fn opt_str_row(row: &mut Row, key: &str, v: &Option<String>) {
    row.insert(key.to_string(), opt_s(v));
}

pub fn str_row(row: &mut Row, key: &str, v: &str) {
    row.insert(key.to_string(), s(v));
}
