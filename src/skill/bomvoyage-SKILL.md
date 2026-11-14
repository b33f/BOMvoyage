---
name: bomvoyage
description: Temporal SBOM for a git repository — BOMvoyage scans dependency manifests, replays git history into a graph, and serves a time-travel explorer. Use when asked about a repo's dependencies, SBOM, supply chain, who added a dependency and when, version history, or exporting SPDX/CycloneDX.
---

# BOMvoyage

BOMvoyage is a single-binary temporal SBOM (Software Bill of Materials)
generator and explorer, powered by the embedded RelateDB graph engine. It
walks a git repository's dependency manifests (Python, Java, Rust, Go,
Node.js), replays their history, and writes one portable `.rl` graph file.

## Commands

```console
bomvoyage                          # scan current repo + serve explorer UI (opens browser, port 8334)
bomvoyage scan [DIR] --db FILE     # scan only (--force to rescan unchanged HEAD, --max-commits N)
bomvoyage serve --db FILE [--port 8334] [--open]
bomvoyage query --db FILE "CYPHER" # read-only openCypher against the graph, JSON out
bomvoyage export --db FILE --format spdx|cyclonedx|json|csv [--as-of DATE|SHA] [-o FILE]
bomvoyage update check             # is there a newer release?
bomvoyage update apply             # upgrade the binary in place (checksum + provenance verified)
```

## Graph model

- `(:Manifest)-[:DECLARES]->(:Package)` — `DECLARES` edges carry temporal
  intervals: `{since_sha, since_date, removed_sha, removed_date,
  version_requirement, added_by_name, …}`. A missing `removed_date` means
  the dependency is still live.
- A `Package` node is a (name, ecosystem, version) triple keyed by `purl`;
  a version bump is a new node. `DEPENDS_ON` edges give transitive
  structure where the lockfile expresses it.
- "As of" is a Cypher predicate on those intervals:
  `WHERE r.since_date <= $as_of AND (r.removed_date IS NULL OR r.removed_date > $as_of)`
  — dates are UTC ISO-8601 and string-compare chronologically.

## Useful queries

Most-declared dependencies:

```cypher
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
RETURN p.name AS name, count(*) AS versions ORDER BY versions DESC LIMIT 10
```

Who added a dependency and when (every version bump):

```cypher
MATCH (m:Manifest)-[r:DECLARES]->(p:Package {name: 'serde'})
RETURN p.purl, r.version_requirement, r.since_date, r.added_by_name
ORDER BY r.since_date
```

Dependencies live at a past date:

```cypher
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE r.since_date <= '2024-06-01' AND (r.removed_date IS NULL OR r.removed_date > '2024-06-01')
RETURN p.purl ORDER BY p.purl
```

A name that is not in the repo returns zero rows by design — first run the
top-deps query to learn the names present.

## Notes for agent workflows

- The `.rl` file **is** the database (portable single file). A re-scan of an
  unchanged HEAD is a no-op; `--force` rescans. `query` and `export` open it
  read-only and can run while the explorer serves it.
- `--as-of` accepts an ISO date, ISO datetime, commit-sha prefix, or `HEAD`.
- Shallow clones produce truncated history (the scan warns); fetch first
  when full history matters.
- When scripting many runs, disable the daily update check with
  `--no-update-check` or `BOMVOYAGE_NO_UPDATE_CHECK=1`.
- The explorer binds `127.0.0.1` only; kill the process to stop serving.
- Scan caps (commits, manifest sizes) warn loudly rather than truncating
  silently — surface those warnings to the user.
