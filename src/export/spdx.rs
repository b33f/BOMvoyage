//! SPDX 2.3 JSON rendering.

use crate::export::SbomRow;

/// SPDX ids must be `SPDXRef-[A-Za-z0-9.-]+`; the purl's `:` and `/` are not.
fn spdx_id(purl: &str) -> String {
    let cleaned: String = purl
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("SPDXRef-{cleaned}")
}

pub fn render(
    rows: &[SbomRow],
    edges: &[(String, String)],
    repo_name: &str,
    as_of: &str,
) -> String {
    let mut packages: Vec<serde_json::Value> = Vec::new();
    let mut id_of: std::collections::BTreeMap<&str, String> = Default::default();
    // The document itself is a package so edges can attach to it.
    let doc_id = "SPDXRef-Document".to_string();

    for row in rows {
        let id = id_of
            .entry(row.purl.as_str())
            .or_insert_with(|| spdx_id(&row.purl))
            .clone();
        packages.push(serde_json::json!({
            "name": row.name,
            "SPDXID": id,
            "versionInfo": row.version,
            "downloadLocation": "NOASSERTION",
            "externalRefs": [{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": row.purl,
            }],
            "filesAnalyzed": false,
        }));
    }

    let mut relationships: Vec<serde_json::Value> = Vec::new();
    for (from, to) in edges {
        let from_id = if rows.iter().any(|r| r.purl == *from) {
            spdx_id(from)
        } else {
            doc_id.clone()
        };
        if rows.iter().any(|r| r.purl == *to) {
            relationships.push(serde_json::json!({
                "spdxElementId": from_id,
                "relationshipType": "DEPENDS_ON",
                "relatedSpdxElement": spdx_id(to),
            }));
        }
    }

    let doc = serde_json::json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": doc_id,
        "name": repo_name,
        "documentNamespace": format!("https://relate.dev/sbom/{repo_name}/as-of/{as_of}"),
        "creationInfo": {
            "created": as_of,
            "creators": ["Tool: BOMvoyage"],
            "licenseListVersion": "3.23",
        },
        "packages": packages,
        "relationships": relationships,
        "comment": format!("Generated from git history as of {as_of}; 'created' reflects the as-of instant, not the generation time."),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}
