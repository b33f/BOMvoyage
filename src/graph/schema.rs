//! Graph schema: index DDL. Nodes are immutable facts (a Package node IS a
//! name+ecosystem+version triple); all time lives on relationship validity
//! intervals `{since_sha, since_date, removed_sha, removed_date}`.

pub const INDEX_DDL: &[&str] = &[
    "CREATE INDEX sbom_pkg_purl IF NOT EXISTS FOR (p:Package) ON (p.purl)",
    "CREATE INDEX sbom_pkg_name IF NOT EXISTS FOR (p:Package) ON (p.name)",
    "CREATE INDEX sbom_manifest_path IF NOT EXISTS FOR (m:Manifest) ON (m.path)",
    "CREATE INDEX sbom_file_path IF NOT EXISTS FOR (f:File) ON (f.path)",
    "CREATE INDEX sbom_person_email IF NOT EXISTS FOR (x:Person) ON (x.email)",
    "CREATE INDEX sbom_commit_sha IF NOT EXISTS FOR (c:Commit) ON (c.sha)",
];
