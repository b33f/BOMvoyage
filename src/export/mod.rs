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
