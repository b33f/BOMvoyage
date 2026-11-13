//! The named as-of Cypher queries. `$as_of` is an ISO-8601 instant (or the
//! end-of-day expansion of a date-only input); every temporal edge carries
//! the same four fields, so the same predicate works everywhere.
//!
//! The one as-of predicate, spelled inline in each statement:
//! `r.since_date <= $as_of AND (r.removed_date IS NULL OR r.removed_date > $as_of)`
//! Dates are git `%aI` ISO-8601 strings — string comparison is
//! ordering-correct for that one format.

/// All DECLARES intervals live at as-of (the SBOM package list).
pub const LIVE_PACKAGES: &str = r#"
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE r.since_date <= $as_of AND (r.removed_date IS NULL OR r.removed_date > $as_of)
RETURN DISTINCT p.purl AS purl, p.name AS name, p.ecosystem AS ecosystem, p.version AS version,
       p.scope AS scope, r.version_requirement AS requirement, m.path AS manifest,
       r.added_by_name AS added_by, r.since_sha AS added_sha, r.since_date AS added_date
ORDER BY purl
"#;

/// All DEPENDS_ON edges live at as-of.
pub const LIVE_DEPENDS_ON: &str = r#"
MATCH (a:Package)-[r:DEPENDS_ON]->(b:Package)
WHERE r.since_date <= $as_of AND (r.removed_date IS NULL OR r.removed_date > $as_of)
RETURN DISTINCT a.purl AS from, b.purl AS to, r.source AS source
"#;

/// Who added a package, and when (with removals for forensics).
pub const HISTORY: &str = r#"
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE p.name = $name OR p.purl = $name
RETURN p.purl AS purl, m.path AS manifest, r.version_requirement AS requirement,
       r.since_sha AS added_sha, r.since_date AS added_date,
       r.added_by_name AS added_by_name, r.added_by_email AS added_by_email,
       r.removed_sha AS removed_sha, r.removed_date AS removed_date
ORDER BY added_date
"#;

/// Usage sites of a package.
pub const USAGE: &str = r#"
MATCH (f:File)-[r:IMPORTS]->(p:Package {purl: $purl})
RETURN f.path AS file, r.symbol AS symbol
ORDER BY file
"#;

/// Changes between two points in time.
pub const DIFF: &str = r#"
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE r.since_date > $d1 AND r.since_date <= $d2
RETURN 'added' AS change, p.purl AS purl, r.since_date AS at_date, r.added_by_name AS by
UNION
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE r.removed_date > $d1 AND r.removed_date <= $d2
RETURN 'removed' AS change, p.purl AS purl, r.removed_date AS at_date, r.added_by_name AS by
"#;

/// Commits that touched manifests, for the time slider.
pub const COMMITS: &str = r#"
MATCH (c:Commit) RETURN c.sha AS sha, c.date AS date, c.summary AS summary,
       c.author_name AS author ORDER BY date
"#;

/// Meta: project row.
pub const META: &str = r#"
MATCH (pr:Project)
RETURN pr.name AS name, pr.head_sha AS head_sha, pr.last_scanned_at AS scanned_at,
       pr.scan_quality AS scan_quality, pr.history_head_sha AS history_head
"#;

/// Resolve an as-of token that looks like a sha prefix to its commit date.
pub const SHA_DATE: &str = r#"
MATCH (c:Commit)
WHERE c.sha STARTS WITH $sha
RETURN c.date AS date ORDER BY date LIMIT 1
"#;
