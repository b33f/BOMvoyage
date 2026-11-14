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
- **The Cypher console is read-only by enforcement**, not convention: statements
  starting with mutation keywords are refused server-side before the engine
  sees them, and `export` / `query` open the database read-only.
- **Bounded by construction**: commit counts, manifest byte sizes and usage-scan
  file counts are capped, and hitting a cap warns loudly — a hostile repository
  cannot exhaust memory through a huge lockfile or history without a visible
  warning.
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
