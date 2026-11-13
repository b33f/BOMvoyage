//! The graph store: open a `.rl` database, ensure the schema, and write a
//! scan's output idempotently.

pub mod queries;
pub mod schema;
pub mod write;

use std::collections::BTreeMap;

use relate::{Database, OpenMode, OpenOptions, Value};

use crate::error::ToolResult;
use crate::scan::ScanOutput;

pub struct SbomGraph {
    pub db: Database,
}

/// Create the indexes on an opened database (idempotent).
pub fn ensure_schema(db: &Database) -> ToolResult<()> {
    let mut conn = db.connect()?;
    for ddl in schema::INDEX_DDL {
        conn.query(ddl)?;
    }
    Ok(())
}

/// Head of the last COMPLETED history walk recorded on the Project node.
pub fn stored_history_head(db: &Database) -> ToolResult<Option<String>> {
    let mut conn = db.connect()?;
    let result = conn.query(queries::META)?;
    if result.rows.is_empty() {
        return Ok(None);
    }
    let head = result
        .header
        .columns
        .iter()
        .position(|c| c == "history_head")
        .and_then(|i| result.rows[0].get(i))
        .and_then(|v| v.as_string().map(str::to_string))
        .filter(|s| !s.is_empty());
    Ok(head)
}

impl SbomGraph {
    /// Open (creating if needed) a writable graph database.
    pub fn open_writable(path: &std::path::Path) -> ToolResult<Self> {
        let db = Database::open(&path.to_string_lossy())?;
        ensure_schema(&db)?;
        Ok(Self { db })
    }

    /// Open read-only (for `export`/`query` beside a live writer, or after).
    pub fn open_read_only(path: &std::path::Path) -> ToolResult<Self> {
        let db = Database::open_with(
            &path.to_string_lossy(),
            OpenOptions {
                mode: OpenMode::ReadOnly,
                ..OpenOptions::default()
            },
        )?;
        Ok(Self { db })
    }

    /// History head recorded by the last COMPLETED history walk, if any.
    pub fn stored_history_head(&self) -> ToolResult<Option<String>> {
        let mut conn = self.db.connect()?;
        let result = conn.query(queries::META)?;
        if result.rows.is_empty() {
            return Ok(None);
        }
        let head = result
            .header
            .columns
            .iter()
            .position(|c| c == "history_head")
            .and_then(|i| result.rows[0].get(i))
            .and_then(|v| v.as_string().map(str::to_string))
            .filter(|s| !s.is_empty());
        Ok(head)
    }

    /// Head sha recorded by the last scan, if any.
    pub fn stored_head(&self) -> ToolResult<Option<String>> {
        let mut conn = self.db.connect()?;
        let result = conn.query(queries::META)?;
        if result.rows.is_empty() {
            return Ok(None);
        }
        let head = result
            .header
            .columns
            .iter()
            .position(|c| c == "head_sha")
            .and_then(|i| result.rows[0].get(i))
            .and_then(|v| v.as_string().map(str::to_string));
        Ok(head)
    }

    /// Write a full scan. Idempotent: every MERGE keys on natural identity,
    /// so replaying the same scan leaves the graph unchanged.
    pub fn write_scan(&self, out: &ScanOutput) -> ToolResult<()> {
        write_scan(&self.db, out)
    }
}

/// Write a scan output. Idempotent: every MERGE keys on natural identity, so
/// replaying the same scan leaves the graph unchanged. Callable from any
/// thread holding the `Database` — the background history walker shares the
/// same instance the HTTP server is serving from.
pub fn write_scan(db: &Database, out: &ScanOutput) -> ToolResult<()> {
    {
        let mut conn = db.connect()?;
        conn.begin()?;

        // A full scan replaces the provisional intervals a current-only scan
        // left behind (its real intervals carry real opening commits).
        if !out.provisional {
            conn.query(write::DELETE_PROVISIONAL)?;
            conn.query(write::DELETE_PROVISIONAL_ADDED)?;
        }

        // Nodes first.
        write::flush(
            &mut conn,
            write::MERGE_PACKAGES,
            write::package_rows(&out.packages),
        )?;

        let mut commit_rows = Vec::new();
        let mut person_rows: BTreeMap<String, write::Row> = BTreeMap::new();
        for c in &out.commits {
            let mut r = write::Row::new();
            write::str_row(&mut r, "sha", &c.sha);
            write::str_row(&mut r, "date", &c.date);
            write::str_row(&mut r, "email", &c.author_email);
            write::str_row(&mut r, "summary", &c.summary);
            commit_rows.push(r);
            let mut p = write::Row::new();
            write::str_row(&mut p, "email", &c.author_email);
            write::str_row(&mut p, "name", &c.author_name);
            person_rows.insert(c.author_email.clone(), p);
        }
        // Interval authors ride the same commits list, but collect from
        // intervals anyway to be safe (identity = email).
        for i in &out.intervals {
            let c = &i.since;
            let mut p = write::Row::new();
            write::str_row(&mut p, "email", &c.author_email);
            write::str_row(&mut p, "name", &c.author_name);
            person_rows.insert(c.author_email.clone(), p);
        }
        write::flush(&mut conn, write::MERGE_COMMITS, commit_rows)?;
        write::flush(
            &mut conn,
            write::MERGE_PEOPLE,
            person_rows.into_values().collect(),
        )?;

        // Project + manifests.
        let scan_quality = scan_quality_label(out);
        let mut proj = write::Row::new();
        write::str_row(&mut proj, "name", &out.repo_name);
        write::str_row(&mut proj, "head_sha", &out.head_sha);
        write::str_row(&mut proj, "scanned_at", &now_iso());
        write::str_row(&mut proj, "tool_version", env!("CARGO_PKG_VERSION"));
        write::str_row(&mut proj, "scan_quality", scan_quality);
        // The history pointer only advances when the walk is complete for
        // this head — a provisional scan leaves it pointing at the past (or
        // empty), which is what tells the next run (and the UI) to keep
        // loading history.
        let history_head = if out.history_complete {
            out.head_sha.clone()
        } else {
            String::new()
        };
        write::str_row(&mut proj, "history_head", &history_head);
        write::flush(&mut conn, write::MERGE_PROJECT, vec![proj])?;

        let mut manifests: BTreeMap<String, (&str, &'static str)> = BTreeMap::new();
        for i in &out.intervals {
            let Some(kind) = crate::scan::discover::kind_for(&i.manifest) else {
                continue;
            };
            manifests
                .entry(i.manifest.clone())
                .or_insert((write::eco_name(kind.ecosystem()), manifest_kind_label(kind)));
        }
        let mut hm_rows = Vec::new();
        for (path, (eco, kind)) in &manifests {
            let mut r = write::Row::new();
            write::str_row(&mut r, "name", &out.repo_name);
            write::str_row(&mut r, "path", path);
            write::str_row(&mut r, "eco", eco);
            write::str_row(&mut r, "kind", kind);
            hm_rows.push(r);
        }
        write::flush(&mut conn, write::MERGE_HAS_MANIFEST, hm_rows)?;

        // DECLARES rows: one per occupancy interval, keyed by the OPENING
        // commit so a re-scan merges the same edge and closes are in-place.
        let mut declares: Vec<write::Row> = Vec::new();
        for i in &out.intervals {
            let mut r = write::Row::new();
            write::str_row(&mut r, "manifest", &i.manifest);
            write::str_row(&mut r, "purl", &i.pkg.purl());
            write::str_row(&mut r, "since_sha", &i.since.sha);
            write::str_row(&mut r, "since_date", &i.since.date);
            write::str_row(
                &mut r,
                "requirement",
                i.pkg.requirement.as_deref().unwrap_or(""),
            );
            write::str_row(&mut r, "scope", write::scope_name(i.pkg.scope));
            write::str_row(&mut r, "email", &i.since.author_email);
            write::str_row(&mut r, "name", &i.since.author_name);
            let (removed_sha, removed_date) = match &i.until {
                Some(c) => (Some(c.sha.clone()), Some(c.date.clone())),
                None => (None, None),
            };
            write::opt_str_row(&mut r, "removed_sha", &removed_sha);
            write::opt_str_row(&mut r, "removed_date", &removed_date);
            r.insert(
                "key".into(),
                Value::String(format!("{}|{}|{}", i.manifest, i.pkg.purl(), i.since.sha).into()),
            );
            r.insert("provisional".into(), Value::Bool(out.provisional));
            declares.push(r);
        }
        write::flush(&mut conn, write::MERGE_DECLARES, declares)?;

        // DEPENDS_ON edges.
        let mut edges = Vec::new();
        for e in &out.dep_edges {
            let mut r = write::Row::new();
            write::str_row(&mut r, "from", &e.from);
            write::str_row(&mut r, "to", &e.to);
            write::str_row(&mut r, "source", e.source);
            write::str_row(&mut r, "since_sha", &e.since_sha);
            write::str_row(&mut r, "since_date", &e.since_date);
            write::opt_str_row(&mut r, "removed_sha", &e.removed_sha);
            write::opt_str_row(&mut r, "removed_date", &e.removed_date);
            r.insert(
                "key".into(),
                Value::String(format!("{}|{}|{}", e.from, e.to, e.since_sha).into()),
            );
            edges.push(r);
        }
        write::flush(&mut conn, write::MERGE_DEPENDS_ON, edges)?;

        // IMPORTS (worktree usage).
        let head_date = out
            .commits
            .last()
            .map(|c| c.date.clone())
            .unwrap_or_default();
        let mut imports = Vec::new();
        for i in &out.imports {
            let mut r = write::Row::new();
            write::str_row(&mut r, "file", &i.file);
            write::str_row(&mut r, "purl", &i.purl);
            write::str_row(&mut r, "symbol", &i.symbol);
            write::str_row(&mut r, "since_date", &head_date);
            let language = i
                .file
                .rsplit('.')
                .next()
                .map(|ext| match ext {
                    "py" => "python",
                    "rs" => "rust",
                    "go" => "go",
                    "java" | "kt" => "java",
                    _ => "other",
                })
                .unwrap_or("other")
                .to_string();
            write::str_row(&mut r, "language", &language);
            r.insert(
                "key".into(),
                Value::String(format!("{}|{}|{}", i.file, i.purl, i.symbol).into()),
            );
            imports.push(r);
        }
        write::flush(&mut conn, write::MERGE_IMPORTS, imports)?;

        // Person → Package attribution edges: ADDED at the opening commit,
        // REMOVED at the closing commit (if any).
        let mut added_rows = Vec::new();
        let mut removed_rows = Vec::new();
        for i in &out.intervals {
            let mut r = write::Row::new();
            write::str_row(&mut r, "email", &i.since.author_email);
            write::str_row(&mut r, "purl", &i.pkg.purl());
            write::str_row(&mut r, "sha", &i.since.sha);
            write::str_row(&mut r, "date", &i.since.date);
            r.insert("provisional".into(), Value::Bool(out.provisional));
            added_rows.push(r);
            if let Some(c) = &i.until {
                let mut r = write::Row::new();
                write::str_row(&mut r, "email", &c.author_email);
                write::str_row(&mut r, "purl", &i.pkg.purl());
                write::str_row(&mut r, "sha", &c.sha);
                write::str_row(&mut r, "date", &c.date);
                removed_rows.push(r);
            }
        }
        write::flush(&mut conn, write::MERGE_ADDED, added_rows)?;
        write::flush(&mut conn, write::MERGE_REMOVED, removed_rows)?;

        let mut touched = Vec::new();
        for (sha, path) in &out.touched {
            let mut r = write::Row::new();
            write::str_row(&mut r, "sha", sha);
            write::str_row(&mut r, "path", path);
            touched.push(r);
        }
        write::flush(&mut conn, write::MERGE_TOUCHED, touched)?;

        // Root package: the project itself, pinned at head.
        let root_purl = format!("pkg:generic/{}@{}", out.repo_name, out.head_sha);
        let mut root_rows: Vec<write::Row> = Vec::new();
        let mut rr = write::Row::new();
        write::str_row(&mut rr, "purl", &root_purl);
        write::str_row(&mut rr, "name", &out.repo_name);
        write::str_row(&mut rr, "eco", "generic");
        write::str_row(&mut rr, "version", &out.head_sha);
        write::str_row(&mut rr, "scope", "direct");
        root_rows.push(rr);
        write::flush(&mut conn, write::MERGE_PACKAGES, root_rows)?;
        let mut root_link = write::Row::new();
        write::str_row(&mut root_link, "name", &out.repo_name);
        write::str_row(&mut root_link, "purl", &root_purl);
        write::flush(&mut conn, write::MERGE_ROOT, vec![root_link])?;

        conn.commit()?;
        db.checkpoint()?;
        Ok(())
    }
}

fn manifest_kind_label(kind: crate::scan::ManifestKind) -> &'static str {
    match kind {
        crate::scan::ManifestKind::RequirementsTxt => "requirements",
        crate::scan::ManifestKind::PyProject => "pyproject",
        crate::scan::ManifestKind::PoetryLock => "poetry-lock",
        crate::scan::ManifestKind::UvLock => "uv-lock",
        crate::scan::ManifestKind::PomXml => "pom",
        crate::scan::ManifestKind::Gradle => "gradle",
        crate::scan::ManifestKind::GradleLockfile => "gradle-lockfile",
        crate::scan::ManifestKind::CargoToml => "cargo-toml",
        crate::scan::ManifestKind::CargoLock => "cargo-lock",
        crate::scan::ManifestKind::GoMod => "go-mod",
        crate::scan::ManifestKind::GoSum => "go-sum",
    }
}

/// Honest resolution-quality label surfaced in the UI.
fn scan_quality_label(out: &ScanOutput) -> &'static str {
    let has_lock_edges = out
        .dep_edges
        .iter()
        .any(|e| e.source == "lockfile" || e.source == "go-mod");
    if has_lock_edges {
        "transitive"
    } else {
        "flat"
    }
}

/// Current UTC instant as ISO-8601 (`YYYY-MM-DDTHH:MM:SSZ`), from the Unix
/// clock — no chrono dependency.
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days since the Unix epoch → civil (year, month, day). Howard Hinnant's
/// algorithm; the one date conversion the tool needs.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Resolve an `--as-of` token (date, datetime, sha prefix, or HEAD) to the
/// ISO instant the temporal predicates compare against.
pub fn resolve_as_of(conn: &mut relate::Connection, token: Option<&str>) -> ToolResult<String> {
    let token = token.unwrap_or("HEAD");
    if token.eq_ignore_ascii_case("HEAD") {
        let result = conn.query("MATCH (c:Commit) RETURN max(c.date) AS d")?;
        return Ok(cell_string(&result, 0).unwrap_or_else(|| "9999-12-31T23:59:59Z".to_string()));
    }
    // Date-only input: expand to end of day so the whole day is included.
    if token.len() == 10 && token.as_bytes()[4] == b'-' {
        return Ok(format!("{token}T23:59:59Z"));
    }
    if token.len() >= 20 {
        return Ok(token.to_string());
    }
    // Otherwise: a (partial) commit sha.
    let mut params = BTreeMap::new();
    params.insert("sha".to_string(), Value::String(token.to_string().into()));
    let result = conn.query_with_params(queries::SHA_DATE, params)?;
    match cell_string(&result, 0) {
        Some(date) => Ok(date),
        None => Err(crate::error::ToolError::Other(format!(
            "--as-of {token:?} is neither an ISO date nor a known commit prefix"
        ))),
    }
}

fn cell_string(result: &relate::QueryResult, col: usize) -> Option<String> {
    result
        .rows
        .first()
        .and_then(|r| r.get(col))
        .and_then(|v| v.as_string().map(str::to_string))
}
