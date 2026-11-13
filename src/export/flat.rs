//! Flat exports: JSON rows and CSV — the honest raw form of the as-of query.

use crate::export::SbomRow;

pub fn render_json(rows: &[SbomRow]) -> String {
    serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string())
}

const COLUMNS: &[&str] = &[
    "purl",
    "name",
    "ecosystem",
    "version",
    "scope",
    "requirement",
    "manifest",
    "added_by",
    "added_date",
];

pub fn render_csv(rows: &[SbomRow]) -> String {
    let mut out = String::new();
    out.push_str(&COLUMNS.join(","));
    out.push('\n');
    for row in rows {
        let cells = [
            row.purl.as_str(),
            row.name.as_str(),
            row.ecosystem.as_str(),
            row.version.as_str(),
            row.scope.as_str(),
            row.requirement.as_str(),
            row.manifest.as_str(),
            row.added_by.as_str(),
            row.added_date.as_str(),
        ];
        let line: Vec<String> = cells.iter().map(|c| csv_cell(c)).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_cells_with_commas() {
        let row = SbomRow {
            purl: "pkg:cargo/serde@1.0.200".into(),
            name: "serde".into(),
            ecosystem: "rust".into(),
            version: "1.0.200".into(),
            scope: "direct".into(),
            requirement: "1.0".into(),
            manifest: "Cargo.toml".into(),
            added_by: "Ada Lovelace".into(),
            added_date: "2024-01-01T00:00:00Z".into(),
        };
        let csv = render_csv(std::slice::from_ref(&row));
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1]
            .starts_with("pkg:cargo/serde@1.0.200,serde,rust,1.0.200,direct,1.0,Cargo.toml,"));
    }
}
