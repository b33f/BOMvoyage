//! Self-update: release check against GitHub and an in-place binary upgrade.
//!
//! Design constraints:
//! - An update check must never break or delay the tool: it runs on a
//!   background thread, every failure is silent, and the network is only
//!   touched at most once per day (a small state file caches the last
//!   check and the latest version seen).
//! - Disabled by `--no-update-check` or `BOMVOYAGE_NO_UPDATE_CHECK` (set to
//!   `1`/`true`/`yes`/`on`).
//! - The downloaded archive is checksummed against the release's own
//!   `.sha256` asset before anything is replaced; all transport is HTTPS
//!   (rustls) pinned to github.com.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{ToolError, ToolResult};

const REPO_API: &str = "https://api.github.com/repos/b33f/BOMvoyage";
const USER_AGENT: &str = "bomvoyage-self-update";
/// Hard cap on downloaded release artifacts (they are ~10 MB today).
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

// ---------------------------------------------------------------- version

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// True when `a` is strictly newer than `b` (dotted numeric compare; any
/// leading `v` and a non-numeric tail like `-beta` are tolerated).
pub fn version_is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<(u64, &str)> {
        let v = v.trim().trim_start_matches('v');
        v.split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                let rest: &str = &p[digits.len()..];
                (
                    digits.parse::<u64>().unwrap_or(0),
                    // pre-release tail ("", "-beta", …); compared by the caller
                    if rest.is_empty() { "" } else { rest },
                )
            })
            .collect()
    }
    let a = parts(candidate);
    let b = parts(current);
    for i in 0..a.len().max(b.len()) {
        let (na, ta) = a.get(i).copied().unwrap_or((0, ""));
        let (nb, tb) = b.get(i).copied().unwrap_or((0, ""));
        match na.cmp(&nb) {
            std::cmp::Ordering::Equal => {}
            other => return other == std::cmp::Ordering::Greater,
        }
        // Numeric parts equal: a release (no tail) outranks any pre-release
        // tail of the same number — 0.1.1 > 0.1.1-beta.
        let ord = match (ta.is_empty(), tb.is_empty()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => ta.cmp(tb),
        };
        match ord {
            std::cmp::Ordering::Equal => continue,
            other => return other == std::cmp::Ordering::Greater,
        }
    }
    false
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Serialize)]
struct State {
    checked_at_secs: u64,
    latest: String,
}

/// The shared per-user cache directory (`~/.cache/bomvoyage`, or the
/// Windows `%LOCALAPPDATA%` equivalent), if the environment defines one.
/// Shared by the update-check state file and the detailed-mode caches.
pub(crate) fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    if home.is_empty() {
        return None;
    }
    let mut p = PathBuf::from(home);
    if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            p = PathBuf::from(local);
        } else {
            p.push("AppData");
            p.push("Local");
        }
    } else {
        p.push(".cache");
    }
    p.push("bomvoyage");
    Some(p)
}

fn state_path() -> Option<PathBuf> {
    cache_dir().map(|mut p| {
        p.push("update-check.json");
        p
    })
}

fn read_state() -> Option<State> {
    let path = state_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(State {
        checked_at_secs: v.get("checked_at_secs")?.as_u64()?,
        latest: v.get("latest")?.as_str()?.to_string(),
    })
}

fn write_state(state: &State) {
    if let Some(path) = state_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::json!({
            "checked_at_secs": state.checked_at_secs,
            "latest": state.latest,
        });
        let _ = std::fs::write(path, text.to_string());
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- opt-out

/// `--no-update-check` anywhere on the command line, or the
/// `BOMVOYAGE_NO_UPDATE_CHECK` environment variable.
pub fn disabled(no_update_check_flag: bool) -> bool {
    if no_update_check_flag {
        return true;
    }
    matches!(
        std::env::var("BOMVOYAGE_NO_UPDATE_CHECK")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

// ---------------------------------------------------------------- check

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
}

impl UpdateInfo {
    fn notice_text(&self) -> String {
        format!(
            "BOMvoyage {} is available (you have {}) — run `bomvoyage update apply` to upgrade",
            self.latest, self.current
        )
    }
}

/// Shared result of the background check, read by the server's /api/update.
static LAST_UPDATE: Mutex<Option<UpdateInfo>> = Mutex::new(None);

pub fn snapshot() -> serde_json::Value {
    let guard = LAST_UPDATE.lock().ok();
    match guard.and_then(|g| g.clone()) {
        Some(info) => serde_json::json!({
            "checking": false,
            "available": true,
            "current": info.current,
            "latest": info.latest,
        }),
        None => serde_json::json!({ "checking": true, "available": false }),
    }
}

/// The stderr one-liner for a pending update, if the check found one.
pub fn pending_notice() -> Option<String> {
    let guard = LAST_UPDATE.lock().ok()?;
    guard.as_ref().map(|i| i.notice_text())
}

fn record(info: Option<UpdateInfo>) {
    if let Ok(mut guard) = LAST_UPDATE.lock() {
        *guard = info;
    }
}

/// Fetch the latest published release tag from GitHub (one HTTPS call).
fn fetch_latest() -> ToolResult<UpdateInfo> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let response = get_allowlisted(&agent, &format!("{REPO_API}/releases/latest"))?;
    let body: serde_json::Value = response
        .into_json()
        .map_err(|e| ToolError::Io(std::io::Error::other(format!("release check: {e}"))))?;
    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Other("release check: response had no tag_name".into()))?
        .to_string();
    Ok(UpdateInfo {
        current: current_version().to_string(),
        latest: tag.trim_start_matches('v').to_string(),
    })
}

/// The automatic path: at most one network check per day; otherwise uses the
/// cached result. Runs on a background thread — never panic, never error out.
pub fn daily_check() {
    let do_check = || -> Option<UpdateInfo> {
        if let Some(state) = read_state() {
            if is_fresh(state.checked_at_secs, now_secs()) {
                return update_if_newer(&state.latest);
            }
        }
        match fetch_latest() {
            Ok(info) => {
                write_state(&State {
                    checked_at_secs: now_secs(),
                    latest: info.latest.clone(),
                });
                update_if_newer(&info.latest)
            }
            Err(_) => read_state().and_then(|s| update_if_newer(&s.latest)),
        }
    };
    record(do_check());
}

/// A cached check is trusted for one day, not a second longer — at exactly
/// 24h it goes stale and the next run re-queries.
fn is_fresh(checked_at_secs: u64, now_secs: u64) -> bool {
    now_secs.saturating_sub(checked_at_secs) < CHECK_INTERVAL_SECS
}

fn update_if_newer(latest: &str) -> Option<UpdateInfo> {
    if version_is_newer(latest, current_version()) {
        Some(UpdateInfo {
            current: current_version().to_string(),
            latest: latest.to_string(),
        })
    } else {
        None
    }
}

/// `bomvoyage update check`: force a network check and report in words.
pub fn check_command() -> ToolResult<()> {
    eprintln!("checking for a new BOMvoyage release …");
    let info = fetch_latest()?;
    write_state(&State {
        checked_at_secs: now_secs(),
        latest: info.latest.clone(),
    });
    match update_if_newer(&info.latest) {
        Some(info) => println!("{}", info.notice_text()),
        None => println!(
            "BOMvoyage {} is up to date (latest release: {})",
            info.current, info.latest
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------- apply

/// The only hosts an upgrade may ever talk to. Every request and every
/// redirect hop is checked against this list; anything else is refused
/// before a connection is made.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const MAX_REDIRECTS: usize = 5;

/// HTTPS to a GitHub host, or refuse. This is the whole policy.
fn allowlisted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // strip userinfo (`user@host`) if present
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host); // strip port
    ALLOWED_HOSTS.contains(&host)
}

/// GET with redirects followed manually so every hop re-passes the
/// GitHub-only allowlist (a release asset URL may legitimately redirect to
/// the CDN; it may never leave *.github.com / githubusercontent.com).
fn get_allowlisted(agent: &ureq::Agent, url: &str) -> ToolResult<ureq::Response> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        if !allowlisted(&current) {
            return Err(ToolError::Other(format!(
                "refusing to fetch {current} — upgrades may only talk to GitHub over HTTPS"
            )));
        }
        let response = agent
            .get(&current)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| ToolError::Other(format!("download failed ({current}): {e}")))?;
        let status = response.status();
        if (300..400).contains(&status) {
            let location = response.header("Location").ok_or_else(|| {
                ToolError::Other(format!("redirect from {current} had no Location"))
            })?;
            current = resolve_redirect(&current, location);
            continue;
        }
        return Ok(response);
    }
    Err(ToolError::Other(format!(
        "too many redirects fetching {url} (max {MAX_REDIRECTS})"
    )))
}

/// Join a possibly-relative `Location` onto the base URL.
fn resolve_redirect(base: &str, location: &str) -> String {
    if location.starts_with("https://") || location.starts_with("http://") {
        return location.to_string();
    }
    let origin = base
        .split(['/', '?', '#'])
        .take(3)
        .collect::<Vec<_>>()
        .join("/");
    // (scheme, empty, host) → "https://host"
    if location.starts_with('/') {
        format!("{origin}{location}")
    } else {
        format!("{origin}/{location}")
    }
}

/// Require that this exact artifact digest has a build-provenance
/// attestation in this repository (GitHub's attestations API). Release
/// assets and their checksums ship from the same place, so a checksum
/// alone proves transport integrity, not provenance; the attestation
/// exists only if the artifact was produced by this repo's release
/// workflow (`attest-build-provenance`, id-token: write) — it cannot be
/// forged by a hijacked maintainer token uploading a new asset.
fn require_attestation(agent: &ureq::Agent, archive_sha256_hex: &str) -> ToolResult<()> {
    let url = format!("{REPO_API}/attestations/sha256:{archive_sha256_hex}");
    let response = get_allowlisted(agent, &url)?;
    if response.status() == 404 {
        return Err(ToolError::Other(format!(
            "no build-provenance attestation for this artifact (sha256:{archive_sha256_hex}) — refusing to install.\n\
             The artifact was not produced by this repository's release workflow."
        )));
    }
    if response.status() != 200 {
        return Err(ToolError::Other(format!(
            "attestation lookup failed with HTTP {} — refusing to install",
            response.status()
        )));
    }
    let body: serde_json::Value = response
        .into_json()
        .map_err(|e| ToolError::Other(format!("attestation lookup unreadable: {e}")))?;
    // The API answers `{"attestations": [...]}`; older shapes carried a
    // `total_count`. Accept either, require at least one bundle.
    let count = body
        .get("attestations")
        .and_then(|a| a.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
        .max(
            body.get("total_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
        );
    if count == 0 {
        return Err(ToolError::Other(
            "no build-provenance attestation for this artifact — refusing to install".into(),
        ));
    }
    Ok(())
}

/// `bomvoyage update apply`: download the release archive for this platform,
/// verify it against the release's published SHA-256 and this repo's build
/// attestations, and replace the running binary in place. The new version
/// runs on the next launch.
pub fn apply_command() -> ToolResult<()> {
    eprintln!("checking for the latest release …");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(600))
        .build();

    // Latest release metadata: find this platform's assets.
    let response = get_allowlisted(&agent, &format!("{REPO_API}/releases/latest"))?;
    let release: serde_json::Value = response
        .into_json()
        .map_err(|e| ToolError::Other(format!("release lookup failed: {e}")))?;
    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Other("release lookup failed: no tag_name".into()))?
        .to_string();

    if !version_is_newer(tag.trim_start_matches('v'), current_version()) {
        println!(
            "BOMvoyage {} is already the latest release — nothing to apply",
            current_version()
        );
        return Ok(());
    }

    let target = target_triple();
    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let archive_name = format!("bomvoyage-{tag}-{target}.{ext}");
    let checksum_name = format!("{archive_name}.sha256");
    let asset_urls: Vec<String> = release
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?;
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    (name == archive_name || name == checksum_name).then_some(url)
                })
                .collect()
        })
        .unwrap_or_default();

    let archive_url = asset_urls
        .iter()
        .find(|u| u.ends_with(&archive_name))
        .ok_or_else(|| {
            ToolError::Other(format!(
                "release {tag} has no asset for this platform ({target})"
            ))
        })?;
    let checksum_url = asset_urls
        .iter()
        .find(|u| u.ends_with(&checksum_name))
        .ok_or_else(|| ToolError::Other(format!("release {tag} has no {checksum_name}")))?;

    eprintln!("downloading {archive_name} …");
    let archive = download_capped(&agent, archive_url)?;
    eprintln!("verifying checksum …");
    let checksum = download_capped(&agent, checksum_url)?;
    verify_sha256(&archive, &checksum, &archive_name)?;

    eprintln!("verifying build provenance …");
    require_attestation(&agent, &hex(&Sha256::digest(&archive)))?;

    eprintln!("extracting …");
    let binary = extract_binary(&archive, &archive_name, &tag)?;

    eprintln!("replacing the running binary …");
    let exe = std::env::current_exe()?;
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| {
        ToolError::Other(format!(
            "could not move the current binary aside ({}): {e}",
            exe.display()
        ))
    })?;
    if let Err(e) = install_binary(&binary, &exe) {
        // best effort rollback
        let _ = std::fs::rename(&old, &exe);
        return Err(e);
    }
    let _ = std::fs::remove_file(&old);

    println!("upgraded to {tag} — restart BOMvoyage to run the new version");
    Ok(())
}

fn target_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => {
            // unknown platform: no asset will match, say so plainly
            eprintln!("warning: unknown platform {os}/{arch}");
            "unknown"
        }
    }
}

fn download_capped(agent: &ureq::Agent, url: &str) -> ToolResult<Vec<u8>> {
    let response = get_allowlisted(agent, url)?;
    let mut reader = response.into_reader().take(MAX_DOWNLOAD_BYTES + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| ToolError::Other(format!("download failed ({url}): {e}")))?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(ToolError::Other(format!(
            "download from {url} exceeded the {} MiB cap",
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
        )));
    }
    Ok(bytes)
}

fn verify_sha256(bytes: &[u8], checksum_asset: &[u8], archive_name: &str) -> ToolResult<()> {
    let text = std::str::from_utf8(checksum_asset)
        .map_err(|_| ToolError::Other("checksum asset is not valid UTF-8".into()))?;
    let expected = text
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?;
            let file = it.next()?.rsplit('/').next()?; // tolerate full paths
            (file == archive_name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| ToolError::Other("checksum asset does not mention the archive".into()))?;
    let actual = hex(&Sha256::digest(bytes));
    if actual != expected {
        return Err(ToolError::Other(format!(
            "checksum mismatch for {archive_name}: expected {expected}, got {actual} — not installing"
        )));
    }
    Ok(())
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract the `bomvoyage[.exe]` entry from the downloaded archive into a
/// temporary file with executable permissions.
fn extract_binary(archive: &[u8], archive_name: &str, tag: &str) -> ToolResult<PathBuf> {
    let exe_name = if cfg!(target_os = "windows") {
        "bomvoyage.exe"
    } else {
        "bomvoyage"
    };
    let staging = std::env::temp_dir().join(format!(
        "bomvoyage-update-{}-{}",
        tag.trim_start_matches('v'),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    if archive_name.ends_with(".tar.gz") {
        let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(archive));
        let mut tar = tar::Archive::new(gz);
        for entry in tar
            .entries()
            .map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?
        {
            let mut entry =
                entry.map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?
                .to_path_buf();
            if path.file_name().is_some_and(|n| n == exe_name) {
                let dest = staging.join(exe_name);
                entry
                    .unpack(&dest)
                    .map_err(|e| ToolError::Other(format!("archive unpack failed: {e}")))?;
                return Ok(dest);
            }
        }
    } else {
        let reader = std::io::Cursor::new(archive);
        let mut zip = zip::ZipArchive::new(reader)
            .map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?;
            let name = entry
                .name()
                .map_err(|e| ToolError::Other(format!("archive unreadable: {e}")))?
                .into_owned();
            if name.ends_with(exe_name) {
                let dest = staging.join(exe_name);
                let mut out = std::fs::File::create(&dest)
                    .map_err(|e| ToolError::Other(format!("staging write failed: {e}")))?;
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| ToolError::Other(format!("staging write failed: {e}")))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
                }
                return Ok(dest);
            }
        }
    }
    Err(ToolError::Other(format!(
        "release archive did not contain {exe_name}"
    )))
}

fn install_binary(from: &PathBuf, to: &PathBuf) -> ToolResult<()> {
    std::fs::copy(from, to).map_err(|e| ToolError::Other(format!("install failed: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(version_is_newer("0.1.2", "0.1.1"));
        assert!(version_is_newer("v0.2.0", "0.1.11"));
        assert!(version_is_newer("1.0", "0.9.9"));
        assert!(!version_is_newer("0.1.1", "0.1.1"));
        assert!(!version_is_newer("0.1.0", "0.1.1"));
        assert!(!version_is_newer("0.1.1-beta", "0.1.1"));
        assert!(version_is_newer("0.1.2-beta", "0.1.1"));
        assert!(version_is_newer("0.10.0", "0.9.0"));
    }

    #[test]
    fn github_only_urls() {
        // allowed
        assert!(allowlisted(
            "https://api.github.com/repos/b33f/BOMvoyage/releases/latest"
        ));
        assert!(allowlisted(
            "https://github.com/b33f/BOMvoyage/releases/download/v0.2.0/x.tar.gz"
        ));
        assert!(allowlisted(
            "https://objects.githubusercontent.com/x/y?token=z"
        ));
        assert!(allowlisted(
            "https://release-assets.githubusercontent.com/x"
        ));
        assert!(allowlisted("https://github.com:443/a"));
        // refused: other hosts, other schemes, garbage
        assert!(!allowlisted("http://github.com/a")); // no plaintext
        assert!(!allowlisted("https://evil.example.com/x.tar.gz"));
        assert!(!allowlisted("https://github.com.evil.example.com/x.tar.gz"));
        assert!(!allowlisted("https://user@evil.example.com/x"));
        assert!(!allowlisted("ftp://github.com/a"));
        assert!(!allowlisted("file:///etc/passwd"));
        assert!(!allowlisted(""));
    }

    #[test]
    fn redirect_join() {
        assert_eq!(
            resolve_redirect(
                "https://github.com/a/b",
                "https://objects.githubusercontent.com/x"
            ),
            "https://objects.githubusercontent.com/x"
        );
        assert_eq!(
            resolve_redirect("https://api.github.com/repos/x", "/some/path?q=1"),
            "https://api.github.com/some/path?q=1"
        );
        assert_eq!(
            resolve_redirect("https://github.com/a", "rel/path"),
            "https://github.com/rel/path"
        );
        // a protocol-relative Location cannot escape the allowlisted host:
        // it joins as a *path* on the origin, not as a new authority
        assert_eq!(
            resolve_redirect("https://api.github.com/x", "//evil.example.com/y"),
            "https://api.github.com//evil.example.com/y"
        );
        assert!(allowlisted(&resolve_redirect(
            "https://api.github.com/x",
            "//evil.example.com/y"
        )));
    }

    #[test]
    fn checksum_verification() {
        let archive = b"archive-bytes";
        let digest = hex(&Sha256::digest(archive));
        // matching checksum, standard "<hash>  <name>" line
        let ok = format!("{digest}  bomvoyage-1.tar.gz\n");
        assert!(verify_sha256(archive, ok.as_bytes(), "bomvoyage-1.tar.gz").is_ok());
        // uppercase hex in the checksum file is accepted
        let upper = format!("{}  bomvoyage-1.tar.gz", digest.to_uppercase());
        assert!(verify_sha256(archive, upper.as_bytes(), "bomvoyage-1.tar.gz").is_ok());
        // checksum asset written from a different cwd (full path in the file)
        let pathy = format!("{digest}  /some/dir/bomvoyage-1.tar.gz");
        assert!(verify_sha256(archive, pathy.as_bytes(), "bomvoyage-1.tar.gz").is_ok());
        // mismatch → refuse
        let wrong = format!("{}  bomvoyage-1.tar.gz", hex(&Sha256::digest(b"other")));
        let err = verify_sha256(archive, wrong.as_bytes(), "bomvoyage-1.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        // wrong filename / unparsable / non-utf8 → refuse
        assert!(verify_sha256(
            archive,
            ok.replace("1.tar.gz", "2.zip").as_bytes(),
            "bomvoyage-1.tar.gz"
        )
        .is_err());
        assert!(verify_sha256(archive, b"garbage", "bomvoyage-1.tar.gz").is_err());
        assert!(verify_sha256(archive, &[0xff, 0xfe], "bomvoyage-1.tar.gz").is_err());
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn version_more_edges() {
        // missing parts mean zero: 1.0 == 1.0.0
        assert!(!version_is_newer("1.0", "1.0.0"));
        assert!(!version_is_newer("1.0.0", "1.0"));
        // four parts
        assert!(version_is_newer("1.2.3.4", "1.2.3.3"));
        assert!(!version_is_newer("1.2.3.4", "1.2.3.4"));
        // pre-release of the same number is never "newer"
        assert!(!version_is_newer("0.2.0-rc1", "0.2.0"));
        assert!(!version_is_newer("0.2.0-rc.2", "0.2.0-rc.2"));
        // whitespace and v-prefix tolerance
        assert!(version_is_newer("  v1.1.0 ", "1.0.9"));
        // numeric compare, not lexicographic: 2 < 10
        assert!(version_is_newer("0.1.10", "0.1.9"));
        assert!(!version_is_newer("0.1.9", "0.1.10"));
    }

    #[test]
    fn daily_check_freshness_window() {
        let day = CHECK_INTERVAL_SECS;
        assert!(is_fresh(0, day - 1), "just inside 24h is fresh");
        assert!(!is_fresh(0, day), "exactly 24h is stale");
        assert!(!is_fresh(0, day + 1));
        assert!(is_fresh(10_000, 10_000), "same instant is fresh");
        // a clock that went backwards never trips saturating_sub into panic
        assert!(is_fresh(day + 5, 10));
    }

    #[test]
    fn state_roundtrip_and_optout() {
        // state file: write + read through the real path (HOME-scoped)
        let state = State {
            checked_at_secs: 12345,
            latest: "9.9.9".into(),
        };
        write_state(&state);
        let read = read_state().expect("state readable");
        assert_eq!(read.checked_at_secs, 12345);
        assert_eq!(read.latest, "9.9.9");

        // opt-out env var
        std::env::set_var("BOMVOYAGE_NO_UPDATE_CHECK", "1");
        assert!(disabled(false));
        assert!(disabled(true)); // the flag alone is enough
        std::env::set_var("BOMVOYAGE_NO_UPDATE_CHECK", "off");
        assert!(!disabled(false));
        assert!(disabled(true)); // and still wins with a non-disabling value
        std::env::remove_var("BOMVOYAGE_NO_UPDATE_CHECK");
    }
}
