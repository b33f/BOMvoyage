//! Integration test: a temp git repo with scripted commits (add serde →
//! bump serde → add tokio → remove serde), scanned into a `.rl` database,
//! then the as-of queries are asserted at each point — plus scan-twice
//! idempotency.

use std::path::Path;
use std::process::Command;

use relate::Value;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One scripted commit: set Cargo.toml content, commit with a fixed author
/// and date so attribution asserts are deterministic.
fn commit(dir: &Path, name: &str, email: &str, date: &str, message: &str, cargo_toml: &str) {
    commit_full(
        dir,
        name,
        email,
        date,
        message,
        &[("Cargo.toml", cargo_toml)],
    );
}

/// Multi-file variant: writes every (`path`, `content`) pair, stages, and
/// commits with a fixed author/date.
fn commit_full(
    dir: &Path,
    name: &str,
    email: &str,
    date: &str,
    message: &str,
    files: &[(&str, &str)],
) {
    for (path, content) in files {
        std::fs::write(dir.join(path), content).unwrap();
        git(dir, &["add", path]);
    }
    git(
        dir,
        &[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "--no-gpg-sign",
            &format!("--date={date}"),
            "-m",
            message,
        ],
    );
}

fn cargo_toml(deps: &[(&str, &str)]) -> String {
    let mut s = String::from("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\n");
    for (n, v) in deps {
        s.push_str(&format!("{n} = \"{v}\"\n"));
    }
    s
}

fn string_cell(conn: &mut relate::Connection, cypher: &str, col: &str) -> String {
    let result = conn.query(cypher).expect("query runs");
    let i = result
        .header
        .columns
        .iter()
        .position(|c| c == col)
        .expect("column present");
    result
        .rows
        .first()
        .and_then(|r| r.get(i))
        .and_then(|v| v.as_string().map(str::to_string))
        .unwrap_or_default()
}

#[test]
fn temporal_sbom_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("demo");
    std::fs::create_dir(&repo).expect("create repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);

    commit(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-01-01T12:00:00+00:00",
        "add serde",
        &cargo_toml(&[("serde", "1.0.100")]),
    );
    commit(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-02-01T12:00:00+00:00",
        "bump serde",
        &cargo_toml(&[("serde", "1.0.200")]),
    );
    commit(
        &repo,
        "Grace",
        "grace@example.com",
        "2024-03-01T12:00:00+00:00",
        "add tokio",
        &cargo_toml(&[("serde", "1.0.200"), ("tokio", "1.0.0")]),
    );
    commit(
        &repo,
        "Grace",
        "grace@example.com",
        "2024-04-01T12:00:00+00:00",
        "remove serde",
        &cargo_toml(&[("tokio", "1.0.0")]),
    );

    let db = tmp.path().join("bomvoyage.rl");
    let out = bomvoyage_scan(&repo, &db);
    assert_eq!(out.repo_name, "demo");

    // As of March: serde 1.0.200 + tokio live; the serde bump closed 1.0.100.
    let live_at = |conn: &mut relate::Connection, as_of: &str| -> Vec<String> {
        let mut params = std::collections::BTreeMap::new();
        params.insert("as_of".to_string(), Value::String(as_of.to_string().into()));
        let result = conn
            .query_with_params(bomvoyage::graph::queries::LIVE_PACKAGES, params)
            .expect("live query");
        let i = result
            .header
            .columns
            .iter()
            .position(|c| c == "purl")
            .unwrap();
        result
            .rows
            .iter()
            .filter_map(|r| r.get(i).and_then(|v| v.as_string().map(str::to_string)))
            .collect()
    };

    // (A `.rl` bundle holds one open handle at a time — scope each reader.)
    {
        let graph = bomvoyage::graph::SbomGraph::open_read_only(&db).expect("db opens");
        let mut conn = graph.db.connect().expect("connect");
        let march = live_at(&mut conn, "2024-03-15T00:00:00Z");
        assert!(
            march.iter().any(|p| p.contains("serde")),
            "serde live in March: {march:?}"
        );
        assert!(
            march.iter().any(|p| p.contains("tokio")),
            "tokio live in March: {march:?}"
        );
        let april = live_at(&mut conn, "2024-04-15T00:00:00Z");
        assert!(
            !april.iter().any(|p| p.contains("serde")),
            "serde removed in April: {april:?}"
        );
        assert!(
            april.iter().any(|p| p.contains("tokio")),
            "tokio still live: {april:?}"
        );
        let january = live_at(&mut conn, "2024-01-15T00:00:00Z");
        assert_eq!(january.len(), 1, "only serde in January: {january:?}");

        // Who added serde 1.0.100: Ada, on her commit.
        let who = string_cell(
            &mut conn,
            "MATCH (m:Manifest)-[r:DECLARES]->(p:Package) WHERE r.version_requirement = '1.0.100' RETURN r.added_by_email AS by",
            "by",
        );
        assert_eq!(who, "ada@example.com");

        // serde's version timeline has two intervals (1.0.100 closed, 1.0.200
        // closed by the removal).
        let result = conn
            .query(
                "MATCH (m:Manifest)-[r:DECLARES]->(p:Package {name: 'serde'}) RETURN count(r) AS n",
            )
            .expect("count");
        assert_eq!(result.rows[0][0], Value::Int(2), "two serde intervals");
    }

    // Idempotency: scan again, counts unchanged.
    bomvoyage_scan(&repo, &db);
    {
        let graph = bomvoyage::graph::SbomGraph::open_read_only(&db).expect("db reopens");
        let mut conn = graph.db.connect().expect("connect");
        let r = conn
            .query("MATCH ()-[r:DECLARES]->() RETURN count(r) AS n")
            .expect("count");
        // serde@1.0.100 (c1→c2), serde@1.0.200 (c2→c4), tokio (c3→HEAD).
        assert_eq!(
            &r.rows[0][0],
            &Value::Int(3),
            "idempotent rescan: same 3 intervals"
        );
    }
}

/// Run the scanner as a library call (the binary's `scan` path).
fn bomvoyage_scan(repo: &Path, db: &Path) -> bomvoyage::scan::ScanOutput {
    let out = bomvoyage::scan::run(repo, bomvoyage::scan::ScanCaps::default()).expect("scan runs");
    let graph = bomvoyage::graph::SbomGraph::open_writable(db).expect("db opens");
    graph.write_scan(&out).expect("write runs");
    out
}

/// Detailed-mode properties (deps.dev data): round-trip through the graph,
/// survive a full rescan, and overwrite idempotently. No network — rows are
/// hand-built and applied through the same write path the client uses.
#[test]
fn detailed_mode_properties_survive_rescan() {
    use bomvoyage::graph::write::{self, Row};

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("demo");
    std::fs::create_dir(&repo).expect("create repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);

    // A minimal Cargo.lock: versioned purls with real DECLARES intervals —
    // what LIVE_PACKAGES / DETAILED_TARGETS / the graph endpoint need.
    let cargo_lock = |pkgs: &[(&str, &str)]| {
        let mut s = String::new();
        for (name, version) in pkgs {
            s.push_str(&format!(
                "[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n\n"
            ));
        }
        s
    };
    commit_full(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-01-01T12:00:00+00:00",
        "add serde",
        &[
            ("Cargo.toml", &cargo_toml(&[("serde", "1.0.100")])),
            ("Cargo.lock", &cargo_lock(&[("serde", "1.0.100")])),
        ],
    );
    commit_full(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-02-01T12:00:00+00:00",
        "bump serde",
        &[
            ("Cargo.toml", &cargo_toml(&[("serde", "1.0.200")])),
            ("Cargo.lock", &cargo_lock(&[("serde", "1.0.200")])),
        ],
    );
    commit_full(
        &repo,
        "Grace",
        "grace@example.com",
        "2024-03-01T12:00:00+00:00",
        "add tokio",
        &[
            (
                "Cargo.toml",
                &cargo_toml(&[("serde", "1.0.200"), ("tokio", "1.0.0")]),
            ),
            (
                "Cargo.lock",
                &cargo_lock(&[("serde", "1.0.200"), ("tokio", "1.0.0")]),
            ),
        ],
    );

    let db = tmp.path().join("bomvoyage.rl");
    bomvoyage_scan(&repo, &db);

    // An undetailed graph reports zero detailed packages — the UI keys the
    // vulnerable-deps console preset off this field.
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::RwLock::new(std::sync::Arc::new(
                relate::Database::open(&db.to_string_lossy()).expect("open"),
            )),
            db_path: db.clone(),
            repo_spec: std::sync::RwLock::new(None),
            repo_name: "demo".into(),
        };
        let meta = api.meta().expect("meta");
        assert_eq!(
            meta["detailed_packages"], 0,
            "no detailed data yet: count must be 0"
        );
    }

    fn detailed_row(purl: &str, vulns: i64, advisories: &str) -> Row {
        let mut r = Row::new();
        write::str_row(&mut r, "purl", purl);
        write::str_row(
            &mut r,
            "depsdev_url",
            &format!(
                "https://deps.dev/cargo/serde/{}",
                purl.rsplit('@').next().unwrap_or("")
            ),
        );
        write::str_row(&mut r, "depsdev_status", "ok");
        r.insert("fetched_at".into(), Value::Int(1234));
        write::str_row(&mut r, "homepage", "https://serde.rs");
        write::str_row(&mut r, "repository", "https://github.com/serde-rs/serde");
        write::str_row(&mut r, "license", "MIT OR Apache-2.0");
        r.insert("vulns".into(), Value::Int(vulns));
        write::str_row(&mut r, "advisories", advisories);
        r.insert("deprecated".into(), Value::Bool(false));
        r
    }

    const SERDE_100: &str = "pkg:cargo/serde@1.0.100";
    const SERDE_200: &str = "pkg:cargo/serde@1.0.200";
    const GHSA_ROW: &str = r#"[{"id":"GHSA-xxxx-yyyy-zzzz","aliases":["CVE-2024-0001"]}]"#;

    // (A `.rl` bundle holds one open handle at a time — scope each writer.)
    {
        let graph = bomvoyage::graph::SbomGraph::open_writable(&db).expect("db opens");
        let mut conn = graph.db.connect().expect("connect");

        // Before: no detailed data anywhere.
        let before = conn
            .query(&format!(
                "MATCH (p:Package {{purl: '{SERDE_100}'}}) RETURN p.vulns AS v"
            ))
            .expect("query");
        assert_eq!(before.rows[0][0], Value::Null, "no vulns property yet");

        // Apply hand-built detailed rows through the production write path:
        // serde 1.0.100 carries advisories, 1.0.200 is clean.
        bomvoyage::depsdev::apply_rows(
            &mut conn,
            vec![
                detailed_row(SERDE_100, 2, GHSA_ROW),
                detailed_row(SERDE_200, 0, "[]"),
            ],
        )
        .expect("apply runs");

        let result = conn
            .query(&format!(
                "MATCH (p:Package {{purl: '{SERDE_100}'}})
                 RETURN p.vulns AS vulns, p.license AS license, p.advisories AS advisories, p.homepage AS home"
            ))
            .expect("query");
        let col = |name: &str| {
            result
                .header
                .columns
                .iter()
                .position(|c| c == name)
                .unwrap()
        };
        assert_eq!(result.rows[0][col("vulns")], Value::Int(2));
        assert_eq!(
            result.rows[0][col("license")].as_string(),
            Some("MIT OR Apache-2.0")
        );
        assert_eq!(
            result.rows[0][col("advisories")].as_string(),
            Some(GHSA_ROW)
        );
        assert_eq!(
            result.rows[0][col("home")].as_string(),
            Some("https://serde.rs")
        );
        // conn and graph drop here: a .rl bundle holds one open handle.
    }

    // --- GAP 1: time-correctness of the detailed columns in
    // LIVE_PACKAGES — a version live at as-of reports THAT version's
    // advisories, and a superseded version drops out.
    let live_at = |conn: &mut relate::Connection, as_of: &str, purl: &str| -> (Value, Value) {
        let mut params = std::collections::BTreeMap::new();
        params.insert("as_of".to_string(), Value::String(as_of.to_string().into()));
        let result = conn
            .query_with_params(bomvoyage::graph::queries::LIVE_PACKAGES, params)
            .expect("live query");
        let cpurl = result
            .header
            .columns
            .iter()
            .position(|c| c == "purl")
            .unwrap();
        let cv = result
            .header
            .columns
            .iter()
            .position(|c| c == "vulns")
            .unwrap();
        let cadv = result
            .header
            .columns
            .iter()
            .position(|c| c == "advisories")
            .unwrap();
        for row in &result.rows {
            if row.get(cpurl).and_then(|v| v.as_string()) == Some(purl) {
                return (
                    row.get(cv).cloned().unwrap_or(Value::Null),
                    row.get(cadv).cloned().unwrap_or(Value::Null),
                );
            }
        }
        (Value::Null, Value::Null)
    };
    {
        let graph = bomvoyage::graph::SbomGraph::open_read_only(&db).expect("reopen");
        let mut conn = graph.db.connect().expect("connect");
        // January: serde 1.0.100 live, carrying its 2 advisories.
        let (v, adv) = live_at(&mut conn, "2024-01-15T00:00:00Z", SERDE_100);
        assert_eq!(
            v,
            Value::Int(2),
            "serde 1.0.100 live in January with vulns=2"
        );
        assert_eq!(adv.as_string(), Some(GHSA_ROW));
        let (v2, _) = live_at(&mut conn, "2024-01-15T00:00:00Z", SERDE_200);
        assert_eq!(
            v2,
            Value::Null,
            "serde 1.0.200 does not exist yet in January"
        );
        // March: 1.0.200 live and clean; 1.0.100 gone from the live set.
        let (v, adv) = live_at(&mut conn, "2024-03-15T00:00:00Z", SERDE_200);
        assert_eq!(v, Value::Int(0), "serde 1.0.200 live in March, clean");
        assert_eq!(adv.as_string(), Some("[]"));
        let (v, _) = live_at(&mut conn, "2024-03-15T00:00:00Z", SERDE_100);
        assert_eq!(v, Value::Null, "serde 1.0.100 no longer live in March");
    }

    // --- GAP 2: DETAILED_TARGETS excludes the generic root node and
    // version-less packages (they cannot hit the per-version endpoint).
    {
        let graph = bomvoyage::graph::SbomGraph::open_read_only(&db).expect("reopen");
        let mut conn = graph.db.connect().expect("connect");
        let result = conn
            .query(bomvoyage::graph::queries::DETAILED_TARGETS)
            .expect("targets query");
        let cp = result
            .header
            .columns
            .iter()
            .position(|c| c == "purl")
            .unwrap();
        let purls: Vec<String> = result
            .rows
            .iter()
            .filter_map(|r| r.get(cp).and_then(|v| v.as_string().map(str::to_string)))
            .collect();
        assert!(
            purls.iter().any(|p| p == SERDE_100),
            "targets include serde 1.0.100"
        );
        assert!(
            purls.iter().any(|p| p == SERDE_200),
            "targets include serde 1.0.200"
        );
        assert!(
            !purls.iter().any(|p| p.starts_with("pkg:generic/")),
            "the root node is never a target: {purls:?}"
        );
        assert!(
            purls
                .iter()
                .all(|p| p.rsplit('@').next().map(|v| !v.is_empty()).unwrap_or(false)),
            "every target has a version (manifest-only deps excluded): {purls:?}"
        );
    }

    // --- GAP 3: /api/graph node data carries the detailed fields —
    // vulns as a number, advisories parsed into a real JSON array only
    // when vulns > 0, license/license links present, clean nodes null.
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::RwLock::new(std::sync::Arc::new(
                relate::Database::open(&db.to_string_lossy()).expect("open"),
            )),
            db_path: db.clone(),
            repo_spec: std::sync::RwLock::new(None),
            repo_name: "demo".into(),
        };
        let g = api
            .graph(Some("2024-01-15T23:59:59Z"), None)
            .expect("graph query");
        let nodes = g["elements"]["nodes"]
            .as_array()
            .expect("nodes array")
            .clone();
        let serde100 = nodes
            .iter()
            .find(|n| n["data"]["id"] == SERDE_100)
            .unwrap_or_else(|| panic!("serde 1.0.100 in the as-of graph: {nodes:?}"));
        assert_eq!(serde100["data"]["vulns"], 2, "vulns is a JSON number");
        assert_eq!(serde100["data"]["license"], "MIT OR Apache-2.0");
        assert_eq!(
            serde100["data"]["advisories"][0]["id"], "GHSA-xxxx-yyyy-zzzz",
            "advisories must be a parsed array, not a JSON string"
        );
        assert_eq!(
            serde100["data"]["depsdev_url"],
            "https://deps.dev/cargo/serde/1.0.100"
        );
        // conn and graph drop here: a .rl bundle holds one open handle.
    }
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::RwLock::new(std::sync::Arc::new(
                relate::Database::open(&db.to_string_lossy()).expect("open"),
            )),
            db_path: db.clone(),
            repo_spec: std::sync::RwLock::new(None),
            repo_name: "demo".into(),
        };
        let g = api
            .graph(Some("2024-03-15T23:59:59Z"), None)
            .expect("graph query");
        let nodes = g["elements"]["nodes"]
            .as_array()
            .expect("nodes array")
            .clone();
        let serde200 = nodes
            .iter()
            .find(|n| n["data"]["id"] == SERDE_200)
            .expect("serde 1.0.200 in the March graph");
        assert_eq!(serde200["data"]["vulns"], 0);
        assert!(
            serde200["data"]["advisories"].is_null(),
            "a clean node carries no advisories payload: {}",
            serde200["data"]["advisories"]
        );
    }

    // /api/meta now reports the detailed packages — the UI condition for
    // offering the vulnerable-deps preset.
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::RwLock::new(std::sync::Arc::new(
                relate::Database::open(&db.to_string_lossy()).expect("open"),
            )),
            db_path: db.clone(),
            repo_spec: std::sync::RwLock::new(None),
            repo_name: "demo".into(),
        };
        let meta = api.meta().expect("meta");
        assert_eq!(meta["detailed_packages"], 2, "two detailed packages now");
    }

    // A full rescan (what serve's background walker does) must preserve the
    // detailed properties: MERGE_PACKAGES never clears unknown properties.
    bomvoyage_scan(&repo, &db);
    {
        let graph = bomvoyage::graph::SbomGraph::open_writable(&db).expect("db reopens");
        let mut conn = graph.db.connect().expect("connect");
        let result = conn
            .query(&format!(
                "MATCH (p:Package {{purl: '{SERDE_100}'}}) RETURN p.vulns AS v"
            ))
            .expect("query");
        assert_eq!(
            result.rows[0][0],
            Value::Int(2),
            "detailed data survives rescan"
        );

        // Re-applying with different values overwrites (re-runnable).
        let mut r = Row::new();
        write::str_row(&mut r, "purl", SERDE_100);
        write::str_row(&mut r, "depsdev_url", "");
        write::str_row(&mut r, "depsdev_status", "not_found");
        r.insert("fetched_at".into(), Value::Int(99));
        write::str_row(&mut r, "homepage", "");
        write::str_row(&mut r, "repository", "");
        write::str_row(&mut r, "license", "");
        write::str_row(&mut r, "advisories", "[]");
        r.insert("vulns".into(), Value::Int(0));
        r.insert("deprecated".into(), Value::Bool(false));
        bomvoyage::depsdev::apply_rows(&mut conn, vec![r]).expect("re-apply runs");
        let result = conn
            .query(&format!(
                "MATCH (p:Package {{purl: '{SERDE_100}'}}) RETURN p.vulns AS v, p.depsdev_status AS s"
            ))
            .expect("query");
        assert_eq!(result.rows[0][0], Value::Int(0), "overwritten");
        assert_eq!(result.rows[0][1].as_string(), Some("not_found"));
    }
}

/// `bomvoyage site`'s core promise: a snapshot dumped from a scanned graph
/// replays into a SECOND database and builds the IDENTICAL graph — the same
/// proof the browser's WASM replay needs, made natively. Asserted over the
/// read queries the generated site's UI runs, at HEAD and at a past date.
#[test]
fn site_snapshot_roundtrip_replays_identical_graph() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("demo");
    std::fs::create_dir(&repo).expect("create repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let lock = |pkgs: &[(&str, &str)]| {
        let mut s = String::new();
        for (n, v) in pkgs {
            s.push_str(&format!(
                "[[package]]\nname = \"{n}\"\nversion = \"{v}\"\n\n"
            ));
        }
        s
    };
    commit_full(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-01-01T12:00:00+00:00",
        "add serde",
        &[
            ("Cargo.toml", &cargo_toml(&[("serde", "1.0.100")])),
            ("Cargo.lock", &lock(&[("serde", "1.0.100")])),
        ],
    );
    commit_full(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-02-01T12:00:00+00:00",
        "bump serde",
        &[
            ("Cargo.toml", &cargo_toml(&[("serde", "1.0.200")])),
            ("Cargo.lock", &lock(&[("serde", "1.0.200")])),
        ],
    );

    // source graph
    let source_db = tmp.path().join("source.rl");
    let out = bomvoyage::scan::run(&repo, bomvoyage::scan::ScanCaps::default()).expect("scan");
    let g = bomvoyage::graph::SbomGraph::open_writable(&source_db).expect("open");
    g.write_scan(&out).expect("write");

    // dump
    let head = g.stored_head().expect("head").unwrap_or_default();
    let snapshot = {
        let mut conn = g.db.connect().expect("connect");
        bomvoyage::site::dump::dump(&mut conn, "demo", &head, true).expect("dump")
    };
    assert!(snapshot.stages.contains_key("packages"));
    assert!(snapshot.stages.contains_key("declares"));
    drop(g);

    // replay into a second, empty database
    let replay_db = tmp.path().join("replayed.rl");
    let g = bomvoyage::graph::SbomGraph::open_writable(&replay_db).expect("open");
    {
        let mut conn = g.db.connect().expect("connect");
        bomvoyage::site::dump::replay_snapshot(&mut conn, &snapshot).expect("replay");
    }
    drop(g);

    // both graphs answer the site's read queries identically
    let rows_of = |db: &Path, query: &str, as_of: Option<&str>| -> Vec<String> {
        let graph = bomvoyage::graph::SbomGraph::open_read_only(db).expect("open");
        let mut conn = graph.db.connect().expect("connect");
        let result = {
            // HEAD (None) resolves to max commit date, exactly as the
            // server's resolve_as_of does
            let as_of = match as_of {
                Some(a) => a.to_string(),
                None => {
                    let r = conn
                        .query("MATCH (c:Commit) RETURN max(c.date) AS d")
                        .expect("head");
                    r.rows[0][0].as_string().expect("head date").to_string()
                }
            };
            let mut params = std::collections::BTreeMap::new();
            params.insert("as_of".to_string(), Value::String(as_of.into()));
            conn.query_with_params(query, params).expect("query")
        };
        let mut rows: Vec<String> = result
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| {
                        v.as_string()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        rows.sort();
        rows
    };
    use bomvoyage::graph::queries as q;
    for (query, as_of) in [
        (q::LIVE_PACKAGES, Some("2024-01-15T00:00:00Z")),
        (q::LIVE_PACKAGES, Some("2024-03-01T00:00:00Z")),
        (q::LIVE_PACKAGES, None),
        (q::LIVE_DEPENDS_ON, Some("2024-03-01T00:00:00Z")),
        (q::COMMITS, None),
        (q::META, None),
    ] {
        let a = rows_of(&source_db, query, as_of);
        let b = rows_of(&replay_db, query, as_of);
        assert_eq!(
            a, b,
            "replayed graph must answer {query} (as_of {as_of:?}) identically"
        );
    }
}

/// `scan --detailed` on a repo whose HEAD was already scanned must NOT
/// silently no-op: the "history is current" short-circuit used to return
/// before the detailed pass ran, so a graph scanned without the flag could
/// never get it (the reported "empty, not enriching" bug). This drives the
/// real binary — the logic lives in it — and needs no network: offline
/// lookups warn per package and still exit 0.
#[test]
fn detailed_mode_runs_even_when_history_is_current() {
    let bin = env!("CARGO_BIN_EXE_bomvoyage");
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("demo");
    std::fs::create_dir(&repo).expect("create repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-01-01T12:00:00+00:00",
        "add serde",
        &cargo_toml(&[("serde", "1.0.100")]),
    );
    let db = tmp.path().join("bomvoyage.rl");

    let run = |args: &[&str]| {
        let out = std::process::Command::new(bin)
            .args(args)
            .env("BOMVOYAGE_NO_UPDATE_CHECK", "1")
            .output()
            .expect("binary runs");
        assert!(
            out.status.success(),
            "bomvoyage {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).to_string()
    };

    // First scan: plain, no detailed flag.
    let stderr = run(&[
        "scan",
        repo.to_str().unwrap(),
        "--db",
        &db.to_string_lossy(),
    ]);
    assert!(db.exists(), "first scan writes the database");
    assert!(!stderr.contains("applying detailed mode"));

    // Second scan with --detailed: history is current, the scan
    // short-circuits — but the detailed pass must still run.
    let stderr = run(&[
        "scan",
        repo.to_str().unwrap(),
        "--db",
        &db.to_string_lossy(),
        "--detailed",
    ]);
    assert!(
        stderr.contains("history is current"),
        "sanity: second scan short-circuits: {stderr}"
    );
    assert!(
        stderr.contains("applying detailed mode"),
        "--detailed must apply to an already-scanned graph: {stderr}"
    );

    // `--more-detailed` is the both-tiers shorthand: it IMPLIES --detailed,
    // so it enriches an already-scanned graph and never warns about a
    // missing --detailed.
    let stderr = run(&[
        "scan",
        repo.to_str().unwrap(),
        "--db",
        &db.to_string_lossy(),
        "--more-detailed",
    ]);
    assert!(
        stderr.contains("history is current"),
        "sanity: third scan short-circuits: {stderr}"
    );
    assert!(
        stderr.contains("applying detailed mode"),
        "--more-detailed must imply --detailed: {stderr}"
    );
    assert!(
        !stderr.contains("--vuln-details has no effect"),
        "--more-detailed must not demand --detailed: {stderr}"
    );

    // Plain --vuln-details alone still warns — it has no effect without
    // --detailed.
    let stderr = run(&[
        "scan",
        repo.to_str().unwrap(),
        "--db",
        &db.to_string_lossy(),
        "--vuln-details",
    ]);
    assert!(
        stderr.contains("--vuln-details has no effect without --detailed"),
        "--vuln-details alone must warn: {stderr}"
    );
}
