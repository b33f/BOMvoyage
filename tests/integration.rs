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
    std::fs::write(dir.join("Cargo.toml"), cargo_toml).unwrap();
    git(dir, &["add", "Cargo.toml"]);
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
    commit(
        &repo,
        "Ada",
        "ada@example.com",
        "2024-01-01T12:00:00+00:00",
        "add serde",
        &cargo_toml(&[("serde", "1.0.100")]),
    );

    let db = tmp.path().join("bomvoyage.rl");
    bomvoyage_scan(&repo, &db);

    // An undetailed graph reports zero detailed packages — the UI keys the
    // vulnerable-deps console preset off this field.
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::Arc::new(relate::Database::open(&db.to_string_lossy()).expect("open")),
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
            "https://deps.dev/cargo/serde/1.0.100",
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

    // (A `.rl` bundle holds one open handle at a time — scope each writer.)
    {
        let graph = bomvoyage::graph::SbomGraph::open_writable(&db).expect("db opens");
        let mut conn = graph.db.connect().expect("connect");

        // Before: no detailed data anywhere. The test repo has no lockfile,
        // so its serde package has an empty version — create the versioned
        // node detailed mode would find on a lockfile-derived repo (the
        // detailed write MATCHes; it never creates Package nodes).
        conn.query(
            "MERGE (p:Package {purl: 'pkg:cargo/serde@1.0.100'})
             SET p.name = 'serde', p.ecosystem = 'cargo', p.version = '1.0.100'",
        )
        .expect("seed node");
        let before = conn
            .query("MATCH (p:Package {purl: 'pkg:cargo/serde@1.0.100'}) RETURN p.vulns AS v")
            .expect("query");
        assert_eq!(before.rows[0][0], Value::Null, "no vulns property yet");

        // Apply a hand-built detailed row through the production write path.
        bomvoyage::depsdev::apply_rows(
            &mut conn,
            vec![detailed_row(
                "pkg:cargo/serde@1.0.100",
                2,
                r#"[{"id":"GHSA-xxxx-yyyy-zzzz","aliases":["CVE-2024-0001"]}]"#,
            )],
        )
        .expect("apply runs");

        let result = conn
            .query(
                "MATCH (p:Package {purl: 'pkg:cargo/serde@1.0.100'})
                 RETURN p.vulns AS vulns, p.license AS license, p.advisories AS advisories, p.homepage AS home",
            )
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
            Some(r#"[{"id":"GHSA-xxxx-yyyy-zzzz","aliases":["CVE-2024-0001"]}]"#)
        );
        assert_eq!(
            result.rows[0][col("home")].as_string(),
            Some("https://serde.rs")
        );
        // conn and graph drop here: a .rl bundle holds one open handle.
    }

    // /api/meta now reports the detailed package — the UI condition for
    // offering the vulnerable-deps preset.
    {
        let api = bomvoyage::server::api::Api {
            db: std::sync::Arc::new(relate::Database::open(&db.to_string_lossy()).expect("open")),
            repo_name: "demo".into(),
        };
        let meta = api.meta().expect("meta");
        assert_eq!(meta["detailed_packages"], 1, "one detailed package now");
    }

    // A full rescan (what serve's background walker does) must preserve the
    // detailed properties: MERGE_PACKAGES never clears unknown properties.
    bomvoyage_scan(&repo, &db);
    {
        let graph = bomvoyage::graph::SbomGraph::open_writable(&db).expect("db reopens");
        let mut conn = graph.db.connect().expect("connect");
        let result = conn
            .query("MATCH (p:Package {purl: 'pkg:cargo/serde@1.0.100'}) RETURN p.vulns AS v")
            .expect("query");
        assert_eq!(
            result.rows[0][0],
            Value::Int(2),
            "detailed data survives rescan"
        );

        // Re-applying with different values overwrites (re-runnable).
        let mut r = Row::new();
        write::str_row(&mut r, "purl", "pkg:cargo/serde@1.0.100");
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
            .query("MATCH (p:Package {purl: 'pkg:cargo/serde@1.0.100'}) RETURN p.vulns AS v, p.depsdev_status AS s")
            .expect("query");
        assert_eq!(result.rows[0][0], Value::Int(0), "overwritten");
        assert_eq!(result.rows[0][1].as_string(), Some("not_found"));
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
