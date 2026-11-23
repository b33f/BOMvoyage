//! Detailed mode: per-package data from the deps.dev API, written back as
//! `Package` node properties.
//!
//! Two opt-in tiers, both disk-cached (the data is a nearly immutable fact
//! about a package *version*, so entries are kept indefinitely; `--refresh`
//! busts the cache):
//! 1. `--detailed`: one GET per package version against
//!    `api.deps.dev/v3/systems/{S}/packages/{N}/versions/{V}` — project
//!    links, SPDX licenses, and advisory IDs.
//! 2. `--vuln-details`: one additional GET per distinct advisory against
//!    `api.osv.dev/v1/vulns/{id}` — severity label + short summary.
//!
//! Network posture mirrors `update.rs`: HTTPS to an allowlisted fixed host
//! set, User-Agent set, no redirects followed, bounded worker threads, a
//! global request pace, and exponential backoff on 429/5xx. Network trouble
//! never fails the run — per-item failures warn and count.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relate::Value;
use sha2::{Digest, Sha256};

use crate::error::ToolResult;
use crate::graph::write::{self, Row};

const DEPSDEV_API: &str = "https://api.deps.dev/v3";
/// Tier 2: per-advisory details. Documented as key-less and unlimited.
const OSV_API: &str = "https://api.osv.dev/v1";
/// The only hosts a detailed run may ever talk to.
const ALLOWED_HOSTS: &[&str] = &["api.deps.dev", "api.osv.dev"];
const USER_AGENT: &str = concat!("bomvoyage/", env!("CARGO_PKG_VERSION"));
/// Bounded tier-1 concurrency (deps.dev quota is shared).
const WORKERS: usize = 4;
const OSV_WORKERS: usize = 6;
/// Global pacing between request starts, per tier (~10 req/s).
const MIN_REQUEST_INTERVAL_MS: u64 = 100;
const MAX_RETRIES: u32 = 3;
const REQUEST_TIMEOUT_SECS: u64 = 15;
/// Cap on stored summary text (OSV summaries run long).
const MAX_SUMMARY_CHARS: usize = 200;

// ---------------------------------------------------------------- types

pub struct DetailedTarget {
    pub purl: String,
    pub system: &'static str,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DetailedOpts {
    /// Tier 2: also fetch per-advisory details from OSV.
    pub vuln_details: bool,
    /// Re-fetch even for cached entries.
    pub refresh: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DetailedStats {
    pub fetched: usize,
    pub cached: usize,
    pub not_found: usize,
    pub failed: usize,
    pub advisories_fetched: usize,
}

impl DetailedStats {
    pub fn summary_line(&self) -> String {
        let mut s = format!(
            "detailed: {} fetched, {} cached, {} without data, {} failed",
            self.fetched, self.cached, self.not_found, self.failed
        );
        if self.advisories_fetched > 0 {
            s.push_str(&format!(
                ", {} advisories detailed",
                self.advisories_fetched
            ));
        }
        s
    }
}

// ------------------------------------------------- systems, names, urls

/// Graph ecosystem spelling (`write::eco_name`) → deps.dev system.
pub fn depsdev_system(eco: &str) -> Option<&'static str> {
    match eco {
        "python" => Some("PYPI"),
        "java" => Some("MAVEN"),
        "rust" => Some("CARGO"),
        "go" => Some("GO"),
        "node" => Some("NPM"),
        _ => None,
    }
}

/// PEP 503 normalization for PyPI only (lowercase, runs of `-_.` → `-`).
/// npm is case-sensitive and cargo/go names pass through as stored; maven
/// keeps its `group:artifact` form, exactly what deps.dev expects.
pub fn normalize_name(system: &str, name: &str) -> String {
    if system == "PYPI" {
        let mut out = String::with_capacity(name.len());
        let mut prev_sep = false;
        for c in name.chars() {
            if matches!(c, '-' | '_' | '.') {
                if !prev_sep {
                    out.push('-');
                }
                prev_sep = true;
            } else {
                out.push(c.to_ascii_lowercase());
                prev_sep = false;
            }
        }
        out
    } else {
        name.to_string()
    }
}

/// `encodeURIComponent`-style escaping: unreserved ASCII passes, everything
/// else → %XX. This encodes `/`, `:` and `@` in one uniform rule — exactly
/// what the deps.dev docs prescribe for every path parameter, covering npm
/// scoped names (`@org/name`), maven coordinates (`group:artifact`) and go
/// module paths (`github.com/user/repo`) at once.
pub fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn api_key_suffix() -> String {
    match std::env::var("BOMVOYAGE_DEPSDEV_KEY") {
        Ok(key) if !key.trim().is_empty() => format!("?key={}", path_escape(key.trim())),
        _ => String::new(),
    }
}

pub fn version_info_url(t: &DetailedTarget) -> String {
    format!(
        "{DEPSDEV_API}/systems/{}/packages/{}/versions/{}{}",
        t.system,
        path_escape(&normalize_name(t.system, &t.name)),
        path_escape(&t.version),
        api_key_suffix()
    )
}

/// The human-facing deps.dev page for this exact package version.
pub fn depsdev_web_url(t: &DetailedTarget) -> String {
    format!(
        "https://deps.dev/{}/{}/{}",
        t.system.to_ascii_lowercase(),
        path_escape(&normalize_name(t.system, &t.name)),
        path_escape(&t.version)
    )
}

// ---------------------------------------------------------------- http

/// HTTPS to an allowlisted fixed host, or refuse — same policy as
/// `update.rs`, different host set. Belt and braces: we construct every URL
/// ourselves, but the check runs before any connection regardless.
fn allowlisted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    ALLOWED_HOSTS.contains(&host)
}

/// Serialize request starts so N workers together stay at ~10 req/s.
struct Pacer {
    next: Mutex<Instant>,
    min_interval: Duration,
}

impl Pacer {
    fn new() -> Self {
        Self {
            next: Mutex::new(Instant::now()),
            min_interval: Duration::from_millis(MIN_REQUEST_INTERVAL_MS),
        }
    }

    fn wait(&self) {
        let Ok(mut next) = self.next.lock() else {
            return;
        };
        let now = Instant::now();
        if now < *next {
            std::thread::sleep(*next - now);
        }
        *next = Instant::now() + self.min_interval;
    }
}

/// One GET expecting JSON. `Ok(Some(body))` on 200, `Ok(None)` on 404,
/// `Err` for anything else — after retrying 429/5xx with backoff.
fn get_json(agent: &ureq::Agent, url: &str) -> Result<Option<serde_json::Value>, String> {
    if !allowlisted(url) {
        return Err(format!(
            "refusing to fetch {url} — detailed mode may only talk to {} over HTTPS",
            ALLOWED_HOSTS.join(", ")
        ));
    }
    let mut backoff = 1u64;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(backoff));
            backoff *= 2;
        }
        let response = match agent.get(url).header("User-Agent", USER_AGENT).call() {
            Ok(resp) => resp,
            Err(e) => return Err(format!("request failed: {e}")),
        };
        let code = response.status().as_u16();
        match code {
            200 => {
                let body: serde_json::Value = response
                    .into_body()
                    .read_json()
                    .map_err(|e| format!("unreadable response: {e}"))?;
                return Ok(Some(body));
            }
            404 => return Ok(None),
            429 | 502 | 503 | 504 => {
                if let Some(ra) = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                {
                    backoff = backoff.max(ra.min(10));
                }
                continue;
            }
            _ => return Err(format!("HTTP {code}")),
        }
    }
    Err(format!(
        "gave up after {MAX_RETRIES} retries (429/5xx): {url}"
    ))
}

// ---------------------------------------------------------------- cache

struct CacheEntry {
    status: u16,
    body: Option<serde_json::Value>,
}

/// Cache directory for one namespace (`depsdev` / `osv`), if the
/// environment defines a per-user cache location.
fn cache_dir(ns: &str) -> Option<PathBuf> {
    crate::update::cache_dir().map(|dir| dir.join(ns))
}

/// Filenames are sha256 of the key (a purl or advisory id), so no
/// filesystem-escaping questions ever arise.
fn cache_get(dir: Option<&Path>, key: &str) -> Option<CacheEntry> {
    let path = dir?.join(format!(
        "{}.json",
        Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let status = v.get("status")?.as_u64()? as u16;
    let body = match v.get("body") {
        Some(serde_json::Value::Null) | None => None,
        Some(b) => Some(b.clone()),
    };
    Some(CacheEntry { status, body })
}

fn cache_put(dir: Option<&Path>, key: &str, status: u16, body: Option<&serde_json::Value>) {
    let Some(path) = dir.map(|d| {
        d.join(format!(
            "{}.json",
            Sha256::digest(key.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::json!({
        "key": key,
        "fetched_at_secs": now_secs(),
        "status": status,
        "body": body,
    });
    let _ = std::fs::write(path, json.to_string());
}

// In-flight fetch progress, readable from the server's /api/meta (the
// walker's detailed pass runs in another thread than the API).
static FETCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static FETCH_DONE: AtomicUsize = AtomicUsize::new(0);
static FETCH_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// Live progress of the running detailed fetch: (done, total), or None
/// when no fetch is in flight.
pub fn progress_snapshot() -> Option<(usize, usize)> {
    if FETCH_IN_FLIGHT.load(Ordering::Relaxed) {
        Some((
            FETCH_DONE.load(Ordering::Relaxed),
            FETCH_TOTAL.load(Ordering::Relaxed),
        ))
    } else {
        None
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ------------------------------------------------------------- tier 1

/// Result of one tier-1 lookup, distinguishing cache hits (for the stats)
/// from network fetches. `None` = the package/version has no deps.dev data.
enum Fetched {
    Cached(Option<serde_json::Value>),
    Fresh(Option<serde_json::Value>),
}

fn fetch_version_info(
    agent: &ureq::Agent,
    dir: Option<&Path>,
    t: &DetailedTarget,
    refresh: bool,
) -> Result<Fetched, String> {
    if !refresh {
        if let Some(entry) = cache_get(dir, &t.purl) {
            return Ok(Fetched::Cached(match entry.status {
                200 => entry.body,
                _ => None, // cached negative
            }));
        }
    }
    match get_json(agent, &version_info_url(t))? {
        Some(body) => {
            cache_put(dir, &t.purl, 200, Some(&body));
            Ok(Fetched::Fresh(Some(body)))
        }
        None => {
            cache_put(dir, &t.purl, 404, None);
            Ok(Fetched::Fresh(None))
        }
    }
}

/// Fetch every target with bounded concurrency and a global pace. Results
/// come back index-aligned with `targets`. Never panics on network trouble.
/// Prints in-place progress on a TTY (`\r`-updated single line, never
/// scrolling); piped stderr stays quiet until the summary.
fn fetch_versions(targets: &[DetailedTarget], refresh: bool) -> Vec<Result<Fetched, String>> {
    use std::io::IsTerminal as _;
    use std::io::Write as _;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            // 4xx/5xx arrive as Ok responses; get_json() matches on the
            // status itself, which keeps the 429 Retry-After header reachable.
            .http_status_as_error(false)
            .build(),
    );
    let dir = cache_dir("depsdev");
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<Fetched, String>)>> =
        Mutex::new(Vec::with_capacity(targets.len()));
    let pacer = Pacer::new();
    let total = targets.len();
    FETCH_TOTAL.store(total, Ordering::Relaxed);
    FETCH_DONE.store(0, Ordering::Relaxed);
    FETCH_IN_FLIGHT.store(true, Ordering::Relaxed);
    let done = AtomicUsize::new(0);
    let fetched = AtomicUsize::new(0);
    let cached = AtomicUsize::new(0);
    let not_found = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let is_tty = std::io::stderr().is_terminal();
    std::thread::scope(|s| {
        // In-place progress line: the fetch can take minutes; without this
        // it looks hung. Never printed when stderr is piped.
        s.spawn(|| {
            let mut seen = 0;
            while done.load(Ordering::Relaxed) < total {
                std::thread::sleep(Duration::from_millis(400));
                let d = done.load(Ordering::Relaxed);
                if is_tty && d != seen {
                    seen = d;
                    eprint!(
                        "\x1b[2K\r  detailed: {d}/{total} — {} fetched, {} cached, {} without data, {} failed",
                        fetched.load(Ordering::Relaxed),
                        cached.load(Ordering::Relaxed),
                        not_found.load(Ordering::Relaxed),
                        failed.load(Ordering::Relaxed),
                    );
                    let _ = std::io::stderr().flush();
                }
            }
            if is_tty {
                eprint!("\x1b[2K\r");
                let _ = std::io::stderr().flush();
            }
        });
        for _ in 0..WORKERS.min(targets.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= targets.len() {
                    break;
                }
                pacer.wait();
                let r = fetch_version_info(&agent, dir.as_deref(), &targets[i], refresh);
                match &r {
                    Ok(Fetched::Fresh(Some(_))) => {
                        fetched.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Fetched::Cached(Some(_))) => {
                        cached.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        not_found.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                done.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut guard) = results.lock() {
                    guard.push((i, r));
                }
            });
        }
    });
    FETCH_IN_FLIGHT.store(false, Ordering::Relaxed);
    let mut out: Vec<Option<Result<Fetched, String>>> = (0..targets.len()).map(|_| None).collect();
    for (i, r) in results.into_inner().unwrap_or_default() {
        out[i] = Some(r);
    }
    // Every index is pushed exactly once by construction; a poisoned mutex
    // would mark the whole run failed rather than hang.
    out.into_iter()
        .map(|slot| slot.unwrap_or_else(|| Err("worker crashed".into())))
        .collect()
}

// --------------------------------------------------------- row builders

/// deps.dev VersionInfo → a detailed-mode row (all keys the MERGE statement
/// names; `write::flush` UNWINDs these directly).
fn version_row(t: &DetailedTarget, info: &serde_json::Value, fetched_at: u64) -> Row {
    // v3 link labels observed in the wild: SOURCE_REPO, HOMEPAGE,
    // DOCUMENTATION (the v3alpha docs spell them SOURCE / PROJECT_WEBSITE —
    // accepted as fallbacks). First matching label wins.
    let link = |labels: &[&str]| {
        info.get("links")
            .and_then(|l| l.as_array())
            .and_then(|arr| {
                labels
                    .iter()
                    .filter_map(|label| {
                        arr.iter()
                            .find(|l| l.get("label").and_then(|v| v.as_str()) == Some(*label))
                            .and_then(|l| l.get("url"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .next()
            })
    };
    let license = info
        .get("licenses")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let advisories: Vec<serde_json::Value> = info
        .get("advisoryKeys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let id = a.get("id").and_then(|v| v.as_str())?;
                    let mut entry = serde_json::json!({ "id": id });
                    // `aliases` is undocumented on advisory keys; parse
                    // defensively and drop when absent or empty.
                    if let Some(aliases) = a.get("aliases").and_then(|v| v.as_array()) {
                        let al: Vec<&str> = aliases.iter().filter_map(|x| x.as_str()).collect();
                        if !al.is_empty() {
                            entry["aliases"] = serde_json::json!(al);
                        }
                    }
                    Some(entry)
                })
                .collect()
        })
        .unwrap_or_default();

    let mut r = Row::new();
    write::str_row(&mut r, "purl", &t.purl);
    write::opt_str_row(&mut r, "homepage", &link(&["HOMEPAGE", "PROJECT_WEBSITE"]));
    write::opt_str_row(&mut r, "repository", &link(&["SOURCE_REPO", "SOURCE"]));
    write::str_row(&mut r, "license", &license);
    write::str_row(
        &mut r,
        "advisories",
        &serde_json::to_string(&advisories).unwrap_or_else(|_| "[]".into()),
    );
    r.insert("vulns".into(), Value::Int(advisories.len() as i64));
    write::str_row(&mut r, "depsdev_url", &depsdev_web_url(t));
    write::str_row(&mut r, "depsdev_status", "ok");
    r.insert("fetched_at".into(), Value::Int(fetched_at as i64));
    r.insert(
        "deprecated".into(),
        Value::Bool(
            info.get("isDeprecated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
    );
    r
}

/// A 404 from deps.dev is still a written fact (`depsdev_status:
/// "not_found"` distinguishes looked-up-but-absent from never-looked-up).
fn not_found_row(purl: &str, fetched_at: u64) -> Row {
    let mut r = Row::new();
    write::str_row(&mut r, "purl", purl);
    write::str_row(&mut r, "homepage", "");
    write::str_row(&mut r, "repository", "");
    write::str_row(&mut r, "license", "");
    write::str_row(&mut r, "advisories", "[]");
    r.insert("vulns".into(), Value::Int(0));
    write::str_row(&mut r, "depsdev_url", "");
    write::str_row(&mut r, "depsdev_status", "not_found");
    r.insert("fetched_at".into(), Value::Int(fetched_at as i64));
    r.insert("deprecated".into(), Value::Bool(false));
    r
}

// ------------------------------------------------------------- tier 2

/// Per-advisory detail fetched from OSV (`--vuln-details`).
fn osv_detail(body: &serde_json::Value) -> (Option<String>, Option<String>) {
    let severity = body
        .get("database_specific")
        .and_then(|d| d.get("severity"))
        .and_then(|v| v.as_str())
        // A ready-made label (HIGH|MODERATE|LOW|CRITICAL) — CVSS vectors
        // from `severity[]` are deliberately not parsed.
        .map(str::to_string);
    let summary = body
        .get("summary")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(MAX_SUMMARY_CHARS).collect::<String>());
    (severity, summary)
}

/// Fold fetched OSV details into each row's `advisories` JSON in place.
/// Entries without a detail entry (404s, failures) are left untouched —
/// tier-1 data survives.
fn fold_osv_details(rows: &mut [Row], details: &OsvDetails) {
    for row in rows.iter_mut() {
        let Some(Value::String(advisories)) = row.get("advisories") else {
            continue;
        };
        let Ok(mut arr) = serde_json::from_str::<serde_json::Value>(advisories) else {
            continue;
        };
        let mut changed = false;
        if let Some(entries) = arr.as_array_mut() {
            for entry in entries.iter_mut() {
                let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some((sev, sum)) = details.get(id) else {
                    continue;
                };
                if let Some(sev) = sev {
                    entry["severity"] = serde_json::json!(sev);
                    changed = true;
                }
                if let Some(sum) = sum {
                    entry["summary"] = serde_json::json!(sum);
                    changed = true;
                }
            }
        }
        if changed {
            if let Some(slot) = row.get_mut("advisories") {
                *slot = Value::String(arr.to_string().into());
            }
        }
    }
}

/// Fetch OSV details for every distinct advisory id, then fold them into
/// the rows. Returns the number of advisories successfully detailed.
fn apply_osv_details(rows: &mut [Row], refresh: bool) -> usize {
    // Distinct ids across all rows: one fetch per id however many versions
    // share the advisory.
    let mut ids: Vec<String> = Vec::new();
    for row in rows.iter() {
        if let Some(Value::String(advisories)) = row.get("advisories") {
            collect_advisory_ids(advisories, &mut ids);
        }
    }
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return 0;
    }
    eprintln!(
        "detailed mode: fetching details for {} distinct advisories via OSV …",
        ids.len()
    );

    let details = fetch_osv_details(&ids, refresh);
    let detailed = details
        .values()
        .filter(|(sev, sum)| sev.is_some() || sum.is_some())
        .count();
    fold_osv_details(rows, &details);
    detailed
}

fn collect_advisory_ids(advisories_json: &str, out: &mut Vec<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(advisories_json) else {
        return;
    };
    if let Some(entries) = v.as_array() {
        for e in entries {
            if let Some(id) = e.get("id").and_then(|v| v.as_str()) {
                out.push(id.to_string());
            }
        }
    }
}

/// What one fetch produced: a body on 200, none on 404, or why it failed.
type FetchResult = Result<Option<serde_json::Value>, String>;
/// Tier-2 outcome per advisory id.
type OsvDetails = HashMap<String, (Option<String>, Option<String>)>;

fn fetch_osv_details(ids: &[String], refresh: bool) -> OsvDetails {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            // 4xx/5xx arrive as Ok responses; get_json() matches on the
            // status itself, which keeps the 429 Retry-After header reachable.
            .http_status_as_error(false)
            .build(),
    );
    let dir = cache_dir("osv");
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(String, FetchResult)>> = Mutex::new(Vec::with_capacity(ids.len()));
    let pacer = Pacer::new();
    std::thread::scope(|s| {
        for _ in 0..OSV_WORKERS.min(ids.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= ids.len() {
                    break;
                }
                pacer.wait();
                let id = &ids[i];
                let r = (|| -> Result<Option<serde_json::Value>, String> {
                    if !refresh {
                        if let Some(entry) = cache_get(dir.as_deref(), id) {
                            return Ok(match entry.status {
                                200 => entry.body,
                                _ => None,
                            });
                        }
                    }
                    let url = format!("{OSV_API}/vulns/{}", path_escape(id));
                    match get_json(&agent, &url)? {
                        Some(body) => {
                            cache_put(dir.as_deref(), id, 200, Some(&body));
                            Ok(Some(body))
                        }
                        None => {
                            cache_put(dir.as_deref(), id, 404, None);
                            Ok(None)
                        }
                    }
                })();
                if let Ok(mut guard) = results.lock() {
                    guard.push((id.clone(), r));
                }
            });
        }
    });
    let mut out = HashMap::new();
    for (id, r) in results.into_inner().unwrap_or_default() {
        if let Ok(Some(body)) = r {
            out.insert(id, osv_detail(&body));
        }
    }
    out
}

// ------------------------------------------------------------ pipeline

/// The whole detailed-mode pass: read targets from the graph, fetch tier 1
/// (cache-aware, bounded, paced), optionally tier 2, write back. Network
/// trouble never fails the run — per-item failures warn and count.
pub fn detailed_graph(db: &relate::Database, opts: &DetailedOpts) -> ToolResult<DetailedStats> {
    let targets: Vec<DetailedTarget> = {
        let mut conn = db.connect()?;
        let result = conn.query(crate::graph::queries::DETAILED_TARGETS)?;
        let col = |name: &str| result.header.columns.iter().position(|c| c == name);
        let (c_purl, c_eco, c_name, c_version) =
            (col("purl"), col("eco"), col("name"), col("version"));
        let pick = |row: &[Value], i: Option<usize>| -> String {
            i.and_then(|i| row.get(i))
                .and_then(|v| v.as_string().map(str::to_string))
                .unwrap_or_default()
        };
        result
            .rows
            .iter()
            .filter_map(|row| {
                let system = depsdev_system(&pick(row, c_eco))?;
                Some(DetailedTarget {
                    purl: pick(row, c_purl),
                    system,
                    name: pick(row, c_name),
                    version: pick(row, c_version),
                })
            })
            .collect()
    };

    if targets.is_empty() {
        eprintln!("detailed mode: no versioned packages to look up");
        return Ok(DetailedStats::default());
    }
    eprintln!(
        "detailed mode: fetching data for {} package versions via deps.dev …",
        targets.len()
    );

    let results = fetch_versions(&targets, opts.refresh);
    let fetched_at = now_secs();
    let mut stats = DetailedStats::default();
    let mut rows: Vec<Row> = Vec::with_capacity(targets.len());
    for (t, r) in targets.iter().zip(&results) {
        match r {
            Ok(Fetched::Fresh(Some(body))) => {
                stats.fetched += 1;
                rows.push(version_row(t, body, fetched_at));
            }
            Ok(Fetched::Cached(Some(body))) => {
                stats.cached += 1;
                rows.push(version_row(t, body, fetched_at));
            }
            Ok(f @ (Fetched::Fresh(None) | Fetched::Cached(None))) => {
                stats.not_found += 1;
                if matches!(f, Fetched::Fresh(None)) {
                    stats.fetched += 1; // the 404 was a real network lookup
                }
                rows.push(not_found_row(&t.purl, fetched_at));
            }
            Err(e) => {
                stats.failed += 1;
                eprintln!("warning: {} detailed-mode lookup failed: {e}", t.purl);
            }
        }
    }

    if opts.vuln_details {
        stats.advisories_fetched = apply_osv_details(&mut rows, opts.refresh);
    }

    if !rows.is_empty() {
        let mut conn = db.connect()?;
        apply_rows(&mut conn, rows)?;
        db.checkpoint()?;
    }
    Ok(stats)
}

/// Write detailed rows to the graph in the house batched style. Public +
/// network-free so the integration tests can drive it directly.
pub fn apply_rows(conn: &mut relate::Connection, rows: Vec<Row>) -> ToolResult<()> {
    write::flush(conn, write::MERGE_PACKAGE_DETAILED, rows)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(system: &'static str, name: &str, version: &str) -> DetailedTarget {
        DetailedTarget {
            purl: format!("pkg:{system}/{name}@{version}"),
            system,
            name: name.into(),
            version: version.into(),
        }
    }

    #[test]
    fn path_escape_cases() {
        // npm scoped name
        assert_eq!(path_escape("@types/node"), "%40types%2Fnode");
        // maven coordinates
        assert_eq!(
            path_escape("org.apache.logging.log4j:log4j-core"),
            "org.apache.logging.log4j%3Alog4j-core"
        );
        // go module path
        assert_eq!(
            path_escape("github.com/user/repo"),
            "github.com%2Fuser%2Frepo"
        );
        // unreserved names unchanged
        assert_eq!(path_escape("serde"), "serde");
        assert_eq!(path_escape("lodash"), "lodash");
    }

    #[test]
    fn pypi_names_normalize_pep503() {
        assert_eq!(
            normalize_name("PYPI", "typing_extensions"),
            "typing-extensions"
        );
        assert_eq!(normalize_name("PYPI", "Foo.Bar"), "foo-bar");
        assert_eq!(normalize_name("PYPI", "Django--x"), "django-x");
        // other ecosystems pass through
        assert_eq!(normalize_name("NPM", "@types/Node"), "@types/Node");
        assert_eq!(normalize_name("CARGO", "serde"), "serde");
        assert_eq!(
            normalize_name("MAVEN", "org.apache.logging.log4j:log4j-core"),
            "org.apache.logging.log4j:log4j-core"
        );
    }

    #[test]
    fn ecosystem_mapping() {
        assert_eq!(depsdev_system("python"), Some("PYPI"));
        assert_eq!(depsdev_system("java"), Some("MAVEN"));
        assert_eq!(depsdev_system("rust"), Some("CARGO"));
        assert_eq!(depsdev_system("go"), Some("GO"));
        assert_eq!(depsdev_system("node"), Some("NPM"));
        assert_eq!(depsdev_system("generic"), None);
        assert_eq!(depsdev_system(""), None);
    }

    #[test]
    fn urls_escape_name_and_version() {
        let t = DetailedTarget {
            purl: "pkg:npm/@types/node@20.1.0".into(),
            system: "NPM",
            name: "@types/node".into(),
            version: "20.1.0".into(),
        };
        assert_eq!(
            version_info_url(&t),
            "https://api.deps.dev/v3/systems/NPM/packages/%40types%2Fnode/versions/20.1.0"
        );
        assert_eq!(
            depsdev_web_url(&t),
            "https://deps.dev/npm/%40types%2Fnode/20.1.0"
        );
        let m = target("MAVEN", "org.example:lib", "1.0");
        assert!(version_info_url(&m).contains("/packages/org.example%3Alib/versions/1.0"));
    }

    #[test]
    fn version_row_maps_fields() {
        let info: serde_json::Value = serde_json::from_str(
            r#"{
                "links": [
                    {"label": "HOMEPAGE", "url": "https://serde.rs"},
                    {"label": "SOURCE_REPO", "url": "https://github.com/serde-rs/serde"},
                    {"label": "DOCUMENTATION", "url": "https://docs.rs/serde"}
                ],
                "licenses": ["MIT OR Apache-2.0"],
                "advisoryKeys": [
                    {"id": "GHSA-xxxx-yyyy-zzzz", "aliases": ["CVE-2024-0001"]},
                    {"id": "OSV-2024-1"}
                ],
                "isDeprecated": true,
                "isDefault": false
            }"#,
        )
        .unwrap();
        let t = target("CARGO", "serde", "1.0.100");
        let row = version_row(&t, &info, 1234);
        assert_eq!(
            row.get("purl"),
            Some(&Value::String("pkg:CARGO/serde@1.0.100".into()))
        );
        assert_eq!(
            row.get("homepage").unwrap().as_string(),
            Some("https://serde.rs")
        );
        assert_eq!(
            row.get("repository").unwrap().as_string(),
            Some("https://github.com/serde-rs/serde")
        );
        assert_eq!(
            row.get("license").unwrap().as_string(),
            Some("MIT OR Apache-2.0")
        );
        assert_eq!(row.get("vulns"), Some(&Value::Int(2)));
        assert_eq!(row.get("depsdev_status").unwrap().as_string(), Some("ok"));
        assert_eq!(row.get("deprecated"), Some(&Value::Bool(true)));
        assert_eq!(row.get("fetched_at"), Some(&Value::Int(1234)));
        let advisories = row.get("advisories").unwrap().as_string().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(advisories).unwrap();
        assert_eq!(parsed[0]["id"], "GHSA-xxxx-yyyy-zzzz");
        assert_eq!(parsed[0]["aliases"][0], "CVE-2024-0001");
        assert_eq!(parsed[1]["id"], "OSV-2024-1");
        assert!(parsed[1].get("aliases").is_none());
    }

    #[test]
    fn version_row_handles_missing_fields() {
        let info: serde_json::Value = serde_json::from_str(r#"{"licenses": []}"#).unwrap();
        let t = target("NPM", "left-pad", "1.0.0");
        let row = version_row(&t, &info, 5);
        assert_eq!(row.get("homepage"), Some(&Value::Null));
        assert_eq!(row.get("repository"), Some(&Value::Null));
        assert_eq!(row.get("license"), Some(&Value::String("".into())));
        assert_eq!(row.get("vulns"), Some(&Value::Int(0)));
        assert_eq!(row.get("advisories").unwrap().as_string(), Some("[]"));
        assert_eq!(row.get("deprecated"), Some(&Value::Bool(false)));
    }

    #[test]
    fn not_found_row_shape() {
        let row = not_found_row("pkg:npm/missing@0.0.1", 7);
        assert_eq!(
            row.get("depsdev_status").unwrap().as_string(),
            Some("not_found")
        );
        assert_eq!(row.get("vulns"), Some(&Value::Int(0)));
        // every key the MERGE statement names is present
        for k in [
            "purl",
            "depsdev_url",
            "depsdev_status",
            "fetched_at",
            "homepage",
            "repository",
            "license",
            "vulns",
            "advisories",
            "deprecated",
        ] {
            assert!(row.contains_key(k), "missing key {k}");
        }
    }

    #[test]
    fn osv_detail_prefers_database_specific_severity() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "id": "GHSA-a",
                "summary": "sql injection in the query builder",
                "severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/AC:N"}],
                "database_specific": {"severity": "HIGH"}
            }"#,
        )
        .unwrap();
        let (sev, sum) = osv_detail(&body);
        assert_eq!(sev.as_deref(), Some("HIGH"));
        assert_eq!(sum.as_deref(), Some("sql injection in the query builder"));

        let bare: serde_json::Value = serde_json::from_str(r#"{"id": "GHSA-b"}"#).unwrap();
        let (sev, sum) = osv_detail(&bare);
        assert_eq!(sev, None);
        assert_eq!(sum, None);
    }

    #[test]
    fn osv_summary_is_capped() {
        let long = "x".repeat(500);
        let body: serde_json::Value =
            serde_json::from_str(&format!(r#"{{"summary": "{long}"}}"#)).unwrap();
        let (_, sum) = osv_detail(&body);
        assert_eq!(sum.unwrap().chars().count(), MAX_SUMMARY_CHARS);
    }

    #[test]
    fn fold_osv_details_augments_and_preserves() {
        let mut rows = vec![not_found_row("pkg:npm/lodash@4.17.15", 0)];
        rows[0].insert(
            "advisories".into(),
            Value::String(
                r#"[{"id":"GHSA-xxxx-yyyy-zzzz","aliases":["CVE-2020-8203"]},{"id":"GHSA-no-detail"}]"#
                    .into(),
            ),
        );
        rows[0].insert("vulns".into(), Value::Int(2));
        let details = HashMap::from([(
            "GHSA-xxxx-yyyy-zzzz".to_string(),
            (
                Some("HIGH".to_string()),
                Some("prototype pollution".to_string()),
            ),
        )]);
        fold_osv_details(&mut rows, &details);
        let parsed: serde_json::Value =
            serde_json::from_str(rows[0].get("advisories").unwrap().as_string().unwrap()).unwrap();
        assert_eq!(parsed[0]["severity"], "HIGH");
        assert_eq!(parsed[0]["summary"], "prototype pollution");
        assert_eq!(parsed[0]["aliases"][0], "CVE-2020-8203");
        // an advisory with no detail entry is left untouched
        assert!(parsed[1].get("severity").is_none());
        assert_eq!(parsed[1]["id"], "GHSA-no-detail");
    }

    #[test]
    fn collect_ids_dedups_via_caller() {
        let mut ids = Vec::new();
        collect_advisory_ids(r#"[{"id":"A"},{"id":"B"},{"id":"A"}]"#, &mut ids);
        ids.sort();
        ids.dedup();
        assert_eq!(ids, ["A", "B"]);
        // malformed JSON is tolerated
        let mut ids2 = Vec::new();
        collect_advisory_ids("not json", &mut ids2);
        assert!(ids2.is_empty());
    }

    #[test]
    fn cache_roundtrip_including_negative() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        cache_put(Some(dir), "k1", 200, Some(&serde_json::json!({"v": 1})));
        let entry = cache_get(Some(dir), "k1").expect("positive entry cached");
        assert_eq!(entry.status, 200);
        assert_eq!(entry.body.unwrap()["v"], 1);

        cache_put(Some(dir), "k2", 404, None);
        let entry = cache_get(Some(dir), "k2").expect("negative entry cached");
        assert_eq!(entry.status, 404);
        assert_eq!(entry.body, None);

        assert!(
            cache_get(Some(dir), "k3").is_none(),
            "unknown key is a miss"
        );
        // no cache dir configured → everything is a miss, writes are no-ops
        assert!(cache_get(None, "k1").is_none());
        cache_put(None, "k1", 200, None);
    }

    #[test]
    fn host_allowlist() {
        assert!(allowlisted(
            "https://api.deps.dev/v3/systems/NPM/packages/x"
        ));
        assert!(allowlisted("https://api.osv.dev/v1/vulns/GHSA-a"));
        assert!(allowlisted("https://api.deps.dev:443/x"));
        assert!(!allowlisted("http://api.deps.dev/v3")); // no plaintext
        assert!(!allowlisted("https://evil.example.com/x"));
        assert!(!allowlisted("https://api.deps.dev.evil.example.com/x"));
        assert!(!allowlisted("https://user@evil.example.com/x"));
        assert!(!allowlisted("ftp://api.deps.dev/x"));
        assert!(!allowlisted(""));
    }
}
