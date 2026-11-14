//! SBOM export: SPDX 2.3, CycloneDX 1.5, and flat JSON/CSV — each at the
//! current HEAD or any past point in time (`--as-of`).

pub mod cyclonedx;
pub mod flat;
pub mod spdx;

use std::collections::BTreeMap;

use relate::Value;

use crate::error::ToolResult;
use crate::graph::queries;

/// One dependency row as of a point in time, the shared input of every
/// exporter.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SbomRow {
    pub purl: String,
    pub name: String,
    pub ecosystem: String,
    pub version: String,
    pub scope: String,
    pub requirement: String,
    pub manifest: String,
    pub added_by: String,
    pub added_date: String,
}

/// Load the live package list at `as_of`.
pub fn live_rows(conn: &mut relate::Connection, as_of: &str) -> ToolResult<Vec<SbomRow>> {
    let mut params: BTreeMap<String, Value> = BTreeMap::new();
    params.insert("as_of".to_string(), Value::String(as_of.to_string().into()));
    let result = conn.query_with_params(queries::LIVE_PACKAGES, params)?;
    let col = |name: &str| result.header.columns.iter().position(|c| c == name);
    let (c_purl, c_name, c_eco, c_ver, c_scope, c_req, c_man, c_by, c_date) = (
        col("purl"),
        col("name"),
        col("ecosystem"),
        col("version"),
        col("scope"),
        col("requirement"),
        col("manifest"),
        col("added_by"),
        col("added_date"),
    );
    let get = |row: &[Value], idx: Option<usize>| -> String {
        idx.and_then(|i| row.get(i))
            .and_then(|v| v.as_string().map(str::to_string))
            .unwrap_or_default()
    };
    Ok(result
        .rows
        .iter()
        .map(|row| SbomRow {
            purl: get(row, c_purl),
            name: get(row, c_name),
            ecosystem: get(row, c_eco),
            version: get(row, c_ver),
            scope: get(row, c_scope),
            requirement: get(row, c_req),
            manifest: get(row, c_man),
            added_by: get(row, c_by),
            added_date: get(row, c_date),
        })
        .collect())
}

/// Dependency edges live at `as_of`, as (from, to) purl pairs.
pub fn live_edges(conn: &mut relate::Connection, as_of: &str) -> ToolResult<Vec<(String, String)>> {
    let mut params: BTreeMap<String, Value> = BTreeMap::new();
    params.insert("as_of".to_string(), Value::String(as_of.to_string().into()));
    let result = conn.query_with_params(queries::LIVE_DEPENDS_ON, params)?;
    let c_from = result.header.columns.iter().position(|c| c == "from");
    let c_to = result.header.columns.iter().position(|c| c == "to");
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let f = c_from
                .and_then(|i| row.get(i))
                .and_then(|v| v.as_string().map(str::to_string));
            let t = c_to
                .and_then(|i| row.get(i))
                .and_then(|v| v.as_string().map(str::to_string));
            f.zip(t)
        })
        .collect())
}

/// Render in the requested format.
pub fn render(
    format: crate::cli::Format,
    rows: &[SbomRow],
    edges: &[(String, String)],
    repo_name: &str,
    as_of: &str,
) -> String {
    match format {
        crate::cli::Format::Spdx => spdx::render(rows, edges, repo_name, as_of),
        crate::cli::Format::Cyclonedx => cyclonedx::render(rows, edges, repo_name, as_of),
        crate::cli::Format::Json => flat::render_json(rows),
        crate::cli::Format::Csv => flat::render_csv(rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<SbomRow> {
        vec![
            SbomRow {
                purl: "pkg:cargo/serde@1.0.2".into(),
                name: "serde".into(),
                ecosystem: "cargo".into(),
                version: "1.0.2".into(),
                scope: "direct".into(),
                requirement: "1.0".into(),
                manifest: "Cargo.toml".into(),
                added_by: "B33F".into(),
                added_date: "2026-01-01T00:00:00Z".into(),
            },
            SbomRow {
                purl: "pkg:cargo/serde_core@1.0.2".into(),
                name: "serde_core".into(),
                ecosystem: "cargo".into(),
                version: "1.0.2".into(),
                scope: "transitive".into(),
                requirement: "1.0".into(),
                manifest: "Cargo.lock".into(),
                added_by: "B33F".into(),
                added_date: "2026-01-01T00:00:00Z".into(),
            },
        ]
    }

    #[test]
    fn cyclonedx_render_is_valid_and_complete() {
        let edges = vec![(
            "pkg:cargo/serde@1.0.2".into(),
            "pkg:cargo/serde_core@1.0.2".into(),
        )];
        let text = render(
            crate::cli::Format::Cyclonedx,
            &rows(),
            &edges,
            "demo",
            "2026-11-14",
        );
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(doc["bomFormat"], "CycloneDX");
        assert_eq!(doc["specVersion"], "1.5");
        let components = doc["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);
        assert!(doc["dependencies"].as_array().unwrap().len() >= 2);
        // every component carries its purl
        assert!(components
            .iter()
            .all(|c| c["purl"].as_str().unwrap_or("").starts_with("pkg:")));
    }

    #[test]
    fn spdx_render_is_valid_and_links_relationships() {
        let edges = vec![(
            "pkg:cargo/serde@1.0.2".into(),
            "pkg:cargo/serde_core@1.0.2".into(),
        )];
        let text = render(
            crate::cli::Format::Spdx,
            &rows(),
            &edges,
            "demo",
            "2026-11-14",
        );
        let doc: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(doc["spdxVersion"], "SPDX-2.3");
        let packages = doc["packages"].as_array().expect("packages array");
        assert_eq!(packages.len(), 2);
        let rels = doc["relationships"].as_array().expect("relationships");
        assert!(
            rels.iter().any(|r| r["relationshipType"] == "DEPENDS_ON"),
            "the serde → serde_core edge must appear"
        );
        // SPDX ids are sanitized (no ':' or '/')
        assert!(packages
            .iter()
            .all(|p| p["SPDXID"].as_str().unwrap().starts_with("SPDXRef-")));
    }

    #[test]
    fn render_dispatches_all_formats_to_parseable_output() {
        let edges = vec![];
        let rows = rows();
        for fmt in [
            crate::cli::Format::Json,
            crate::cli::Format::Csv,
            crate::cli::Format::Spdx,
            crate::cli::Format::Cyclonedx,
        ] {
            let text = render(fmt, &rows, &edges, "demo", "2026-11-14");
            assert!(!text.is_empty(), "{fmt:?} rendered nothing");
        }
        assert!(serde_json::from_str::<serde_json::Value>(&render(
            crate::cli::Format::Json,
            &rows,
            &edges,
            "demo",
            "2026-11-14"
        ))
        .is_ok());
        assert!(
            render(crate::cli::Format::Csv, &rows, &edges, "d", "x")
                .lines()
                .count()
                >= 3,
            "csv has a header + rows"
        );
    }
}
