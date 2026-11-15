# Security Policy

## Reporting a vulnerability

Please do **not** open a public GitHub issue for a security vulnerability.

Use GitHub's **private vulnerability reporting** for this repository:
[https://github.com/b33f/BOMvoyage/security/advisories/new](https://github.com/b33f/BOMvoyage/security/advisories/new)
(under **Security → Report a vulnerability**). If that is not possible, contact the
maintainer directly and include:

- A description of the issue and its impact.
- Steps to reproduce, or a proof of concept.
- The version / commit you tested against.

You will get an acknowledgement within a few days, and updates as the fix progresses.
Please allow a reasonable disclosure window (aiming for 90 days) before publicizing
details.

## Scope

### In scope

- The `bomvoyage` binary: the scan pipeline, the embedded explorer web server,
  the export and query commands, and the graph schema they write.
- The bundled web UI served at `http://127.0.0.1:<port>`.
- The repository's CI workflows and release artifacts.

### Out of scope

- The [RelateDB](https://relatedb.ai/) engine itself (the `relate` crate). Report
  issues in the engine to its maintainers.
- Vulnerabilities in `git` or the toolchains needed to build BOMvoyage.
- Reports against older unsupported versions (see below).

## Supported versions

Only the latest release receives security fixes.

## Security model and hardening notes

BOMvoyage is a local development tool. Knowing what it trusts helps you assess
findings:

- **It executes `git` as a subprocess.** Git access shells out to the `git`
  binary on `PATH` — no library parsing of repository objects. Any code
  execution or spoofing by a malicious `git` binary is outside BOMvoyage's
  threat model. History content (file contents at past commits, author names,
  summaries) is treated as **untrusted data**: it is parsed with byte-capped,
  record-separated output, malformed records are errors (never silently
  ignored), and text is HTML-escaped before reaching the UI.
- **The explorer binds `127.0.0.1` only** and serves the current directory's
  graph. It performs no authentication — anything that can reach your loopback
  interface can read it, so don't expose the port.
- **Detailed mode makes third-party network calls and reveals your dependency
  list.** Opt-in only (`--detailed`, plus `--vuln-details` for tier 2), off by
  default. When enabled, BOMvoyage sends each dependency's **name and version**
  to the deps.dev API (`api.deps.dev`, a service operated by Google) and, with
  `--vuln-details`, each advisory's ID to the OSV API (`api.osv.dev`). Those
  services therefore learn which packages — and, in aggregate, which
  dependency ecosystem and versions — the scanned repository uses. **What is
  never sent**: source code, file contents, file paths, git history, author
  information, repository identity, or anything beyond the (ecosystem, name,
  version) triples already public on package registries. The transport is
  HTTPS to an allowlisted fixed host set (`api.deps.dev`, `api.osv.dev`) —
  every URL is constructed by the tool and checked against that list before
  any connection; redirects are never followed. Responses are cached locally
  under `~/.cache/bomvoyage/` (delete it to remove all traces). If your
  organization treats dependency metadata as sensitive (air-gapped builds,
  confidential projects), leave detailed mode off — every default path is
  network-free apart from the once-a-day update check below. An API key for
  deps.dev quota may be supplied via the `BOMVOYAGE_DEPSDEV_KEY` environment
  variable; it is sent only to `api.deps.dev`.
- **The Cypher console is read-only by enforcement**, not convention: statements
  starting with mutation keywords are refused server-side before the engine
  sees them, and `export` / `query` open the database read-only.
- **Bounded by construction**: commit counts, manifest byte sizes and usage-scan
  file counts are capped, and hitting a cap warns loudly — a hostile repository
  cannot exhaust memory through a huge lockfile or history without a visible
  warning.
- **Ctrl+C on the serve path is a graceful shutdown, not a signal kill**: the
  first press stops the explorer accepting and waits up to 2 minutes for the
  background history walk to commit its last batches and checkpoint; the
  second press force-quits. An interrupted walk leaves the graph honestly
  marked incomplete (the next scan completes it) — never half-claimed.
- **Release artifacts are reproducible-ish and checksummed**: every release
  ships a SHA-256 checksum per platform archive, built by the tagged CI
  workflow from a clean checkout.
- **The self-update path is locked to GitHub by construction**: the automatic
  daily release check is a single HTTPS (rustls) call to the GitHub API on a
  background thread, silent on failure, disabled by `--no-update-check` or
  `BOMVOYAGE_NO_UPDATE_CHECK=1`. Every request the updater makes — including
  **each redirect hop, followed manually** — must be HTTPS to an allowlisted
  GitHub host (`github.com`, `api.github.com`, `*.githubusercontent.com`);
  anything else is refused before a connection is made. `update apply`
  downloads the platform archive, verifies it against the release's
  published SHA-256 (transport integrity), then **requires a build-provenance
  attestation for the exact artifact digest** in this repository's
  attestations API — a release asset replaced by a hijacked maintainer token
  fails this check, because only the release workflow (id-token: write) can
  attest. Downloads are size-capped, nothing from the download is ever
  executed, and the binary is installed with a rename dance that rolls back
  on failure.
- **`--repo org/repo` cloning is allowlisted and validated**: the spec must
  be exactly `org/repo` of safe characters (letters, digits, `.`, `-`, `_`;
  no `..`, no whitespace, no shells metacharacters) — it becomes the only
  URL built (`https://github.com/<spec>.git`) and the only cache directory
  name. Cloning goes through the `git` subprocess with arguments passed as
  `argv` (never through a shell), HTTPS-only, to `github.com` only. Cached
  clones refresh with `git fetch` + a hard reset to the remote's default
  branch.
- **Generated static sites (`bomvoyage site`) make three kinds of network
  call, each bounded**:
  - **The visitor's browser** fetches the site's own static files, and —
    only if the visitor uses the on-demand GitHub box — the **public
    GitHub API and raw content hosts** (`api.github.com`,
    `raw.githubusercontent.com`) for the org/repo they typed (spec
    validated client-side; requests HTTPS-only to GitHub hosts; manifest
    paths percent-encoded per segment). No credentials, tokens, or visitor
    data are sent; GitHub sees the visitor's IP and the requested repo,
    like any GitHub page view.
  - **Detailed data is baked in at export time** (the site owner's
    `--detailed` run sends dependency names/versions to deps.dev/OSV once,
    per the detailed-mode note above) — the site's visitors never call
    deps.dev; the data is static in the snapshot.
  - **The graph engine** (`@relate/relate-wasm`) is vendored into the site
    pinned and SHA-256-verified at export; `--wasm-cdn` opts into a
    pinned CDN instead, which is the deployer's own supply-chain decision.
- **The in-browser Cypher console is read-only by the same enforcement** as
  the server (mutation-prefix refusal), and all snapshot replay and
  on-demand data flows are **parameterized** (rows as JSON parameters to
  fixed MERGE statements) — hostile characters in dependency names, emails
  or manifest paths can never become Cypher, HTML or JavaScript. UI text is
  HTML-escaped before rendering.
- **CI is supply-chain hardened**: every GitHub Action is pinned to a **full
  commit SHA** (never a movable tag or branch), workflows run least-privilege
  (`contents: read` everywhere except the release job's `contents: write`),
  checkouts do not persist git credentials, jobs run under timeouts, and
  Dependabot watches both the actions and the Rust crate graph (Cargo.lock is
  committed, so builds are `--locked` against pinned, checksummed versions).
  A `cargo-deny` CI job enforces RustSec advisories and restricts dependency
  sources to crates.io (a git dependency cannot sneak into the graph).
  CodeQL scans the Rust code weekly for security findings.
- **Release binaries carry signed build provenance**: every release artifact is
  attested with a Sigstore-based build provenance statement from GitHub's
  public OIDC. Anyone can verify a downloaded binary was built by this
  repository's workflow from a specific commit:
  `gh attestation verify <archive> -R b33f/BOMvoyage`.
