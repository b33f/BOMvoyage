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
