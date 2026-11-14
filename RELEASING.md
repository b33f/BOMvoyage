# Releasing BOMvoyage

How to cut a release, and how to build the way CI does. Read this before
tagging — every step here exists because it was once missed.

## Build from source (the CI way)

Requirements: Rust 1.88+ (`rustup`), `git` on PATH. Full setup per-OS is in
the README's [Building](README.md#building) section.

```console
$ git clone https://github.com/b33f/BOMvoyage && cd BOMvoyage
$ cargo build --release --locked        # binary: target/release/bomvoyage
$ cargo test --release --locked         # 32 tests: parsers, replay, e2e git
$ cargo clippy --locked --all-targets -- -D warnings
$ cargo fmt --all -- --check
```

`--locked` is not optional: CI builds with it, and it guarantees you're
building the pinned, checksummed dependency set from `Cargo.lock`.

### The Intel Mac build is a cross-compile

There are no free Intel macOS runners anymore. The
`x86_64-apple-darwin` release artifact is **cross-compiled on an Apple-silicon
runner** (`macos-latest`) — no special linker setup needed, Apple's toolchain
builds both slices:

```console
$ rustup target add x86_64-apple-darwin
$ cargo build --release --target x86_64-apple-darwin
```

Do **not** change that matrix entry back to a `macos-13`-style runner — those
labels are retired and the job sits queued forever, silently stalling the
whole release (this happened to both v0.1.0 and the first v0.1.1 run).

## Release checklist (do these in order)

1. **Bump the version in `Cargo.toml`** to match the tag you're about to cut.
   This is the step that got missed on v0.1.1: the tag said one thing, the
   binary (`bomvoyage --version`, the `Compiling bomvoyage vX.Y.Z` line)
   still said another. The version is compiled in — a tag alone does nothing.
   `Cargo.lock` updates itself on the next `cargo` command.
2. **Verify everything green:**
   ```console
   $ cargo test --release --locked
   $ cargo clippy --locked --all-targets -- -D warnings
   $ cargo deny check        # advisories, licenses, sources, bans
   ```
3. Commit: `chore: bump version to X.Y.Z`.
4. Tag and push — the tag push is what triggers the release workflow:
   ```console
   $ git tag vX.Y.Z
   $ git push origin main vX.Y.Z
   ```
   Re-pointing a tag later (`git tag -f` + `git push --force`) also
   re-triggers the workflow — a tag's run uses the workflow and source **as
   of the tag**, so fixes made after tagging don't count until you re-tag.
5. **Watch the run** (Actions tab, or `gh run watch`). It builds 5 targets:
   linux x86_64 + aarch64, macOS Apple-silicon + Intel, Windows x86_64, then
   publishes archives + SHA-256 checksums and signs them with build
   provenance attestations. One stuck-queued job stalls the release — check
   the run didn't sit in `queued` for more than a few minutes.
6. **Verify the release** after it lands:
   ```console
   $ gh release view vX.Y.Z --repo b33f/BOMvoyage          # 10 assets expected
   $ gh attestation verify <downloaded archive> -R b33f/BOMvoyage
   $ shasum -a 256 -c <archive>.sha256                      # (or certutil on Windows)
   $ # and: unpack the archive, run `bomvoyage --version` — it must say X.Y.Z
   ```

## Adding dependencies

CI's `deps` job (`cargo-deny`, config in `deny.toml`) enforces:

- dependencies come from **crates.io only** — adding a `git` dependency breaks
  the build by design (supply-chain rule)
- no crate with an unknown/non-permissive license
- no known RustSec advisories — if one is unavoidable (pinned by an upstream
  chain), add it to `deny.toml`'s `ignore` list **with a comment** explaining
  why and the fix path, like the existing `thrift`/`paste` entries
