//! CycloneDX 1.5 JSON rendering.

use std::collections::BTreeMap;

use crate::export::SbomRow;

pub fn render(
    rows: &[SbomRow],
    edges: &[(String, String)],
    repo_name: &str,
    as_of: &str,
) -> String {
    let mut components: Vec<serde_json::Value> = Vec::new();
    // purl -> bom-ref (the purl itself is a valid bom-ref and unique).
    let mut refs: BTreeMap<&str, String> = BTreeMap::new();
    for row in rows {
        refs.insert(row.purl.as_str(), row.purl.clone());
        let mut component = serde_json::json!({
            "type": "library",
            "bom-ref": row.purl,
            "name": row.name,
            "purl": row.purl,
        });
        if !row.version.is_empty() {
            component["version"] = serde_json::Value::String(row.version.clone());
        }
        components.push(component);
    }

    // dependencies[]: the document root plus every component that has edges.
    let mut dep_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (from, to) in edges {
        let from_ref = if rows.iter().any(|r| r.purl == *from) {
            from.clone()
        } else {
            // Root references outside the package list attach to the metadata root.
            format!("root:{from}")
        };
        dep_map.entry(from_ref).or_default().push(to.clone());
    }
    let mut dependencies: Vec<serde_json::Value> = dep_map
        .into_iter()
        .map(|(r, depends_on)| serde_json::json!({ "ref": r, "dependsOn": depends_on }))
        .collect();
    // The root component entry, so the tree anchors even with no edges.
    if !dependencies
        .iter()
        .any(|d| d["ref"].as_str().unwrap_or("").starts_with("root:"))
    {
        dependencies.push(serde_json::json!({ "ref": "root", "dependsOn": [] }));
    }

    let doc = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": format!("urn:uuid:bomvoyage-{as_of}"),
        "version": 1,
        "metadata": {
            "timestamp": as_of,
            "component": {
                "type": "application",
                "name": repo_name,
            },
            "tools": [{
                "vendor": "BOMvoyage",
                "name": "BOMvoyage",
                "version": env!("CARGO_PKG_VERSION"),
            }],
            "properties": [{
                "name": "bomvoyage:as-of",
                "value": as_of,
            }],
        },
        "components": components,
        "dependencies": dependencies,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}
