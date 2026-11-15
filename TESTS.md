# Tests

BOMvoyage's build acceptance is: `cargo test` (this suite, run by CI on
Linux, macOS and Windows), `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo deny check`, and a CI-only smoke scan+query against a real checkout.
This file lists every test in the suite and the feature each one pins down.

Run everything locally with:

```console
$ cargo test
```

The suite is 77 tests: 73 unit tests in the library plus four end-to-end
integration tests (`tests/integration.rs`) that build a real git repository
commit by commit and run the full scan → graph → temporal-query pipeline.

## Repository discovery (`src/scan/discover.rs`)

| Test | What it tests |
|---|---|
| `kind_for_matches_the_table` | Manifest → ecosystem classification: each discovered filename maps to the right parser/kind |

## Manifest parsers (`src/scan/parse/`)

| Test | What it tests |
|---|---|
| `cargo_toml_parses_string_and_table_deps` | Rust `Cargo.toml`: both dependency spellings (`dep = "1"` and `[dependencies.x]` tables) become packages |
| `cargo_lock_parses_packages_and_edges` | `Cargo.lock`: exact pinned versions **and** true pkg→pkg transitive `DEPENDS_ON` edges |
| `package_json_reads_all_dep_tables_and_skips_local` | `package.json`: deps/devDeps/peerDeps/optionalDeps all read; local `file:` paths skipped |
| `package_lock_v1_tree` | npm lockfile v1: nested dependency tree resolution |
| `package_lock_v3_packages_and_nested_resolution` | npm lockfile v3: the `packages` block, including nested paths |
| `pnpm_v9_and_v6_locks` | `pnpm-lock.yaml` both v6 and v9 formats |
| `yarn_classic_lock` | classic `yarn.lock` (v1) parsing |
| `yarn_berry_lock` | Yarn Berry (v2+) lockfile parsing |
| `purl_encodes_scopes` | scoped npm names (`@scope/name`) encode correctly into purls |
| `requirements_parse_pins_comments_and_includes` | `requirements*.txt`: version pins, comments, `-r` includes |
| `pyproject_parses_pep621_and_poetry` | `pyproject.toml`: PEP 621 and Poetry dependency sections |
| `poetry_lock_parses_exact_versions` | `poetry.lock`: exact pinned versions |
| `normalize_is_pep503` | Python name normalization is PEP 503 (so `typing_extensions` == `typing-extensions`) |
| `pom_parses_dependencies` | Maven `pom.xml`: `<dependency>` GAV extraction, parent-managed (version-less) deps stay versionless |
| `gradle_parses_configurations` | Gradle build scripts: `implementation(...)`, `api`, `testImplementation`, quoted and unquoted |
| `gradle_lockfile_parses_tsv` | `gradle.lockfile` TSV format, marked transitive |
| `go_mod_parses_direct_and_indirect` | `go.mod`: direct vs `// indirect` requirements |
| `go_sum_dedups_and_skips_gomod_variants` | `go.sum`: duplicate hashes dedup, `/go.mod` verification lines skipped |

## Usage scan — which files use which dependency (`src/scan/usage.rs`)

| Test | What it tests |
|---|---|
| `python_imports_extract_module_roots` | Python `import x` / `from x.y import z` → top-level module roots |
| `js_imports_extract_package_names` | JS/TS `import … from 'pkg'` and `require('pkg')` → package names |
| `rust_use_skips_internal_paths` | Rust `use` statements: crate-internal paths never become dependencies |
| `go_imports_extract_paths` | Go import paths → module usage |
| `longest_prefix_prefers_deepest_module` | Import→package matching picks the deepest (most specific) module prefix |

## Git history and temporal replay (`src/scan/`)

| Test | What it tests |
|---|---|
| `utc_normalization_handles_offsets` | Commit timestamps with non-UTC offsets normalize to UTC instants (string-comparable dates) |
| `spec_validation` | `--repo org/repo` validation: exactly one slash, safe characters, `..`/traversal/injection rejected; cache-path derivation |
| `date_shape_rejects_loose_spellings` | Only strict ISO date/datetime shapes are accepted; loose spellings are errors, never guesses |
| `replay_derives_intervals` | History replay derives `{since, removed}` occupancy intervals on `DECLARES` edges — who added what, every bump |
| `replay_handles_deleted_manifest` | A manifest deleted in a commit closes its intervals (dependency removal is recorded, not lost) |
| `replay_unparseable_manifest_warns_not_fails` | A corrupt manifest in history produces a loud warning and continues, never a failed scan |
| `unchanged_edges_keep_one_interval_and_duplicate_versions_survive` | Replays are idempotent: unchanged edges keep exactly one interval; the same version re-added later becomes a second interval, not a merge |

## Export (`src/export/`)

| Test | What it tests |
|---|---|
| `csv_quotes_cells_with_commas` | CSV output quotes cells containing commas |
| `cyclonedx_render_is_valid_and_complete` | CycloneDX 1.5: parses as JSON, right `bomFormat`/`specVersion`, every package a component with purl, dependency refs present |
| `spdx_render_is_valid_and_links_relationships` | SPDX 2.3: parses as JSON, sanitized `SPDXRef-` ids, `DEPENDS_ON` relationships link the graph edges |
| `render_dispatches_all_formats_to_parseable_output` | All four formats (`spdx`, `cyclonedx`, `json`, `csv`) produce non-empty, correctly-shaped output |

## Graph model and as-of resolution (`src/graph/`)

| Test | What it tests |
|---|---|
| `date_tokens_expand_and_datetimes_pass_through` | `--as-of 2024-06-01` expands to end-of-day UTC (whole day included); full datetimes pass through; sha prefixes are *not* temporal tokens |
| `civil_date_conversion_known_values` | The day-number → civil-date algorithm pinned against an independent calendar (epoch, leap day, pre-epoch) |

## Server API plumbing (`src/server/`)

| Test | What it tests |
|---|---|
| `url_decode_percent_plus_and_multibyte` | Query-string decoding: `%XX` escapes, `+` as space, multibyte UTF-8; malformed input passes through literally, never panics |
| `query_param_takes_first_value` | Repeated query params take the first value; absent params return `None` |
| `embedded_ui_scripts_parse` | Every embedded UI script (`app.js`, `search.js`) parses under `node --check` — the hand-rolled JS has no build step, so a syntax error ships silently and blanks the explorer (skipped where `node` is absent; CI always has it, plus an explicit lint-job gate) |
| `index_references_only_embedded_assets` | `index.html` references only scripts/styles the server actually embeds and routes |
| `serve_shuts_down_on_flag` | The serve accept loop observes the shutdown flag and returns promptly — what makes Ctrl+C a clean stop (stop accepting → wait for the history walk) instead of a default signal kill; no signals in the test, the flag is set directly |
| `embedded_ui_wiring_smoke` | Behavior of the real `app.js` run in node (`tests/ui_smoke.mjs`, stubbed DOM + fetch, offline, two worlds): on a detailed graph `init()` executes end-to-end and the "Vulnerable deps + who added them" preset appears **exactly once** (splicing it into the preset array *and* button-injecting it once duplicated it); packages render green (`#3fb950`) in **every** mode, with red triangles marking vulnerable ones; the version chip renders `v1.2.3` from `/api/meta`; `diffElements` (identity-preserving graph diff) and the seeded-vs-first-render layout options are pinned by fixture, including the 900ms base animation and the 4× toggle dividing it by four; the static backend's endpoint contracts are pinned against a stubbed engine; the on-demand GitHub scan's classifier, parsers (package.json/Cargo.toml/Cargo.lock/go.mod/requirements), purl building and spec validation are unit-pinned |

## Self-update (`src/update.rs`)

| Test | What it tests |
|---|---|
| `version_comparison` | Release version comparison: `v` prefix, numeric (not lexicographic) parts, pre-release ordering |
| `version_more_edges` | Comparison edge cases: missing parts (`1.0` == `1.0.0`), four parts, `0.1.10` > `0.1.9`, whitespace tolerance |
| `github_only_urls` | The upgrade transport policy: HTTPS to GitHub hosts only; look-alike hosts (`github.com.evil.example.com`), plaintext, other schemes refused |
| `redirect_join` | Redirect `Location` joining — including that a protocol-relative redirect cannot escape the allowlisted host |
| `checksum_verification` | Downloaded archives verify against the release's SHA-256 asset (case-insensitive hex, full-path lines); mismatch, wrong filename, garbage or non-UTF8 all refuse to install |
| `sha256_known_vector` | SHA-256 known-answer test (empty digest) |
| `daily_check_freshness_window` | The once-a-day cache: fresh inside 24h, stale at exactly 24h, safe against a backwards clock |
| `state_roundtrip_and_optout` | The update-check state file writes and reads back; opt-out works via `BOMVOYAGE_NO_UPDATE_CHECK` and `--no-update-check` (flag wins) |

## Agent skill (`src/skill.rs`)

| Test | What it tests |
|---|---|
| `positional_rejects_agent_names` | The CLI contract: an agent name as the `skill install` positional is a hard error pointing at `--agent`; `self` and `bomvoyage` are both accepted |
| `detection_by_config_dir_presence` | Agents are detected purely by config-directory presence, including the alternate roots (`.opencode` vs `.config/opencode`) with the documented precedence |
| `install_writes_skill_and_reports_replacement` | Skill installation writes valid SKILL.md content and reports "updated" (not "installed") when overwriting |
| `agent_names_normalize` | `--agent` matching tolerates case and separators (`Claude Code`, `Gemini_CLI`) |
| `agent_table_is_sane` | The supported-agent table: unique keys, no shared config roots, matches the documented 12-agent set |
| `embedded_skill_has_valid_frontmatter` | The embedded SKILL.md has valid frontmatter and its body teaches the commands it advertises |

## Detailed mode — deps.dev data (`src/depsdev.rs`)

Detailed mode is opt-in (`--detailed`, plus `--vuln-details` for tier 2) and
the only tests here that touch the network are none: every one runs against
fixture JSON or a temp-directory cache.

| Test | What it tests |
|---|---|
| `path_escape_cases` | `encodeURIComponent`-style path escaping: npm scoped names (`@types/node` → `%40types%2Fnode`), maven coordinates (`:` → `%3A`), go module paths (`/` → `%2F`), unreserved names unchanged |
| `pypi_names_normalize_pep503` | PyPI name normalization is PEP 503 (matching the deps.dev key space); npm/cargo/maven names pass through untouched |
| `ecosystem_mapping` | Graph ecosystem spellings map to deps.dev systems (`python`→`PYPI` … `node`→`NPM`); `generic` (the root node) and unknowns are excluded |
| `urls_escape_name_and_version` | The per-version API URL and the human-facing deps.dev page URL escape name and version correctly |
| `version_row_maps_fields` | deps.dev VersionInfo → node-property row: `HOMEPAGE`/`SOURCE_REPO` links, licenses joined, `advisoryKeys` → advisories JSON (+ defensive `aliases`), `vulns` count, `isDeprecated`, timestamp |
| `version_row_handles_missing_fields` | A minimal VersionInfo produces a complete row (empty license, zero vulns, `[]` advisories) — never a missing key |
| `not_found_row_shape` | A 404 still writes a complete row (`depsdev_status: "not_found"`, `vulns: 0`) so looked-up-but-absent is distinguishable from never-looked-up |
| `osv_detail_prefers_database_specific_severity` | OSV advisory mapping: `database_specific.severity` wins (CVSS vectors deliberately unparsed), summary extracted, absent fields → None |
| `osv_summary_is_capped` | Stored advisory summaries are capped at 200 chars (char-boundary safe) |
| `fold_osv_details_augments_and_preserves` | Folding OSV details into a row's advisories JSON adds severity/summary and leaves untouched entries (no detail fetched) intact |
| `collect_ids_dedups_via_caller` | Distinct advisory-id collection across packages; malformed JSON tolerated |
| `cache_roundtrip_including_negative` | The disk cache writes and reads back, including cached 404 negatives; a missing cache dir is a universal miss and a no-op write |
| `host_allowlist` | The transport policy: HTTPS to `api.deps.dev`/`api.osv.dev` only — look-alike hosts, plaintext, other schemes, userinfo tricks refused |

## Static-site export (`src/site/`)

| Test | What it tests |
|---|---|
| `site_snapshot_roundtrip_replays_identical_graph` | The load-bearing one: a snapshot dumped from a scanned graph replays into a second database and answers `LIVE_PACKAGES`/`LIVE_DEPENDS_ON`/`COMMITS`/`META` **identically** at HEAD and at past dates — the native twin of the browser's WASM replay |
| `replay_order_matches_merge_constants` | The replay order maps each stage to a distinct MERGE statement |
| `index_transform_injects_backend_and_stays_relative` | The generated index injects the backend before app.js and keeps relative asset URLs (subpath-safe) |
| `queries_js_covers_every_constant_the_backend_needs` | The generated `queries.js` carries every read query + MERGE template the backend dispatches (hardcoded key list), and parses as JSON |

The backend shim's endpoint contracts (meta/graph/history/usage/commits/update/query —
advisories parsed only when `vulns > 0`, write-prefix refusal, `history_current: true`)
are pinned in the `embedded_ui_wiring_smoke` node harness with a stubbed engine.

## End-to-end (`tests/integration.rs`)

| Test | What it tests |
|---|---|
| `temporal_sbom_end_to_end` | The whole product: a real git repository built commit by commit → `scan` → graph write → temporal Cypher (who added a dep, when, as-of queries at past dates) → idempotent rescan |
| `detailed_mode_properties_survive_rescan` | Detailed-mode rows round-trip through the production write path, **survive a full rescan** (MERGE never clears unknown properties), and re-applying overwrites idempotently; also pins `/api/meta`'s `detailed_packages` (0 on an undetailed graph, 2 after) which the UI uses to offer the vulnerable-deps console preset; **time-correctness**: `LIVE_PACKAGES` at a past date reports the then-live version's own vulns/advisories (superseded versions drop out); `DETAILED_TARGETS` excludes the generic root node and version-less packages; `/api/graph` node data carries `vulns` as a number, `license`, `depsdev_url`, and `advisories` as a parsed array only when `vulns > 0` (clean nodes null) — no network, rows hand-built |
| `detailed_mode_runs_even_when_history_is_current` | Drives the real binary: `scan --detailed` on a repo whose HEAD was already scanned must still run the detailed pass (the "history is current" short-circuit once returned before it — the silently-empty-enrichment bug); `--more-detailed` **implies** `--detailed` (enriches alone, never demands the other flag); plain `--vuln-details` alone still warns. Network-free: offline lookups warn and exit 0 |

## Adding a test

Add it next to the code it pins down (`#[cfg(test)]` module) or to
`tests/integration.rs` for whole-pipeline behavior, make CI green, and add a
row to the matching table above. A fix for a bug should come with the test
that would have caught it.
