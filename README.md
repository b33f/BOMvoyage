# relate-sbom

**A temporal SBOM (Software Bill of Materials) generator and explorer, backed by the
[Relate](https://crates.io/crates/relate) embedded graph database.**

One binary. Run it inside a git repository and it:

1. **Scans every dependency manifest** — Python (`requirements*.txt`, `pyproject.toml`,
   `poetry.lock`, `uv.lock`), Java (`pom.xml`, Gradle, `gradle.lockfile`), Rust
   (`Cargo.toml`, `Cargo.lock`), Go (`go.mod`, `go.sum`).
2. **Walks the full git history** of those manifests and replays it, deriving for every
   dependency: **who added it, when, every version bump, and when it was removed** —
   stored as occupancy intervals on graph edges.
3. **Maps usage sites**: which files import / call each dependency.
4. **Builds the dependency graph** — direct and transitive (full pkg→pkg edges from
   `Cargo.lock`; module topology from `go.mod`; flat lockfiles are honestly marked
   `flat`, never faked into wrong edges).
5. Writes it all to **one portable `.rl` file** (Parquet snapshot + WAL inside),
   then serves an **interactive web UI with time travel**.

## Quick start (the demo)

```console
$ cd some-git-repo
$ relate-sbom
scanning /some-git-repo …
412 packages across 87 commits, 634 dependency events, 1502 usage sites
wrote sbom.rl
SBOM explorer: http://127.0.0.1:8420
```

Open the URL: an interactive dependency graph with a **time-travel slider** — drag it
back to any commit and watch the SBOM shrink to what the repo depended on that day.
Click any package to see **who added it, when, its full version timeline, and every
file that uses it**. The raw-Cypher box at the bottom shows the temporal query behind
the view you're looking at.

## Time travel

Relate has no native point-in-time queries, so time is **modeled in the graph**: every
temporal edge carries `{since_sha, since_date, removed_sha, removed_date}`, and an
"as-of" query is a Cypher predicate:

```cypher
MATCH (m:Manifest)-[r:DECLARES]->(p:Package)
WHERE r.since_date <= $as_of AND (r.removed_date IS NULL OR r.removed_date > $as_of)
RETURN p.purl, r.version_requirement, r.added_by_name
```

One scan builds the whole history; the present and every past moment are queries over
the same graph. Dates are git `%aI` ISO-8601 instants normalized to UTC, so string
comparison is chronological.

## CLI

```text
relate-sbom                                  # scan + serve (the demo path)
relate-sbom scan   [PATH] [--db sbom.rl] [--force] [--max-commits N]
relate-sbom serve  [--db sbom.rl] [--port 8420] [--open]
relate-sbom export [--db sbom.rl] [--format spdx|cyclonedx|json|csv] [--as-of DATE|SHA] [-o FILE]
relate-sbom query  [--db sbom.rl] <CYPHER>
```

`--as-of` accepts an ISO date (`2024-06-01`), an ISO datetime, a commit-sha prefix, or
`HEAD`. `export` and `query` open the database read-only; a re-scan of an unchanged
HEAD is a no-op (`--force` rescans; rescans are idempotent MERGEs on natural keys).

### Export standards

```console
$ relate-sbom export --format spdx --as-of 2024-01-01 -o sbom-jan.spdx.json
$ relate-sbom export --format cyclonedx -o sbom-now.cdx.json
```

SPDX 2.3 and CycloneDX 1.5, with dependency relationships, at any point in history.

## Try it on this repo's own history

```console
$ cd relate          # the Relate engine repository
$ relate-sbom scan . --max-commits 300
$ relate-sbom query --db sbom.rl "MATCH (m:Manifest)-[r:DECLARES]->(p:Package {name: 'parquet'})
  RETURN p.purl, r.version_requirement, r.since_date, r.added_by_name ORDER BY r.since_date"
```

## Design notes

- **A `Package` node IS a (name, ecosystem, version) triple**; `purl` (Package URL) is
  the natural key. A version bump is a new node, and the version timeline is the set of
  `DECLARES` intervals — "when did this dep change" is a one-hop query.
- **Transitive honesty**: `Cargo.lock` yields true pkg→pkg edges; `go.mod` yields the
  module topology it actually expresses; `poetry.lock`/`uv.lock`/`gradle.lockfile` are
  flat lists, so edges are synthesized from the root and the project is badged
  `flat` in the UI — a wrong edge is worse than a flat-but-true one in a security tool.
- **Git access shells out to `git`** (no libgit2): a pinned `-z`/record-separator
  `git log` invocation, byte-capped stdout, malformed records are errors — the same
  hardened pattern the relate-git-to-graph importer uses.
- **No web framework**: the HTTP layer is hand-rolled over `std::net` (the
  relate-http surface's shape) and the UI (cytoscape.js) is embedded in the binary —
  fully offline, nothing to install.
- **Bounded by construction**: commit caps, 4 MiB manifest caps, file-count caps;
  hitting a cap warns loudly instead of truncating silently.

## Building

```console
$ cargo build --release      # Rust 1.88+ (edition 2021)
```

The only runtime dependency that matters is `relate` (crates.io) — the embedded
openCypher graph engine. Databases are single `.rl` files you can copy, archive, or
`scp` to a colleague.

## Tests

```console
$ cargo test                 # parser fixtures, interval replay, end-to-end git
                             # history → as-of queries, idempotent rescan
```
