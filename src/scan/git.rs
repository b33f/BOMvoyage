//! Git access by shelling out to the `git` CLI — no libgit2 dependency,
//! matching the relate-git-to-graph importer's precedent (ADR-0376
//! invocation shape). Records are separated by `0x1e`, header fields by
//! `0x1f`, paths by NUL; a malformed record is an error naming it, never a
//! silently dropped commit.

use std::io::Read;
use std::process::{Command, Stdio};

use super::{CommitInfo, ManifestHistory, ScanCaps};
use crate::error::{ScanWarning, ToolError, ToolResult};

const RECORD_SEP: u8 = 0x1e;
const FIELD_SEP: u8 = 0x1f;

fn git_child(repo: &std::path::Path, args: &[String]) -> ToolResult<std::process::Child> {
    Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg("log.showSignature=false")
        .arg("--no-pager")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::GitMissing("git executable not found on PATH".into())
            } else {
                ToolError::GitCommand(e.to_string())
            }
        })
}

/// Validate a `org/repo` GitHub spec and derive the cache path:
/// exactly one `/`, both segments non-empty, safe characters only, no
/// `..` — the spec becomes a URL path and a directory name.
pub fn validate_github_spec(spec: &str) -> ToolResult<(String, String)> {
    let parts: Vec<&str> = spec.split('/').collect();
    if parts.len() != 2 {
        return Err(crate::error::ToolError::Other(format!(
            "--repo {spec:?}: expected github org/repo (one slash)"
        )));
    }
    let (org, repo) = (parts[0], parts[1]);
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(org) || !ok(repo) {
        return Err(crate::error::ToolError::Other(format!(
            "--repo {spec:?}: org and repo may only contain letters, digits, '.', '-', '_'"
        )));
    }
    Ok((org.to_string(), repo.to_string()))
}

/// Clone (or refresh a cached clone of) a GitHub repo and return the
/// checkout path — full history (time travel needs it), via the git
/// subprocess, cached under the shared cache dir.
pub fn fetch_github_repo(spec: &str) -> ToolResult<std::path::PathBuf> {
    let (org, repo) = validate_github_spec(spec)?;
    let dir = crate::update::cache_dir()
        .ok_or_else(|| ToolError::Other("no cache directory available (set HOME)".into()))?
        .join("repos")
        .join(format!("{org}_{repo}"));
    if dir.exists() {
        // Refresh the cached clone: fetch, re-point origin/HEAD, land the
        // checkout hard on the remote's default branch.
        run_simple(&dir, &["fetch".into(), "origin".into(), "--prune".into()])?;
        run_simple(
            &dir,
            &[
                "remote".into(),
                "set-head".into(),
                "origin".into(),
                "--auto".into(),
            ],
        )?;
        let remote_head = {
            let out = run_simple(
                &dir,
                &[
                    "symbolic-ref".into(),
                    "--short".into(),
                    "refs/remotes/origin/HEAD".into(),
                ],
            )?;
            String::from_utf8_lossy(&out).trim().to_string() // "origin/main"
        };
        let branch = remote_head.trim_start_matches("origin/").to_string();
        run_simple(
            &dir,
            &[
                "checkout".into(),
                "-B".into(),
                branch.clone(),
                remote_head.clone(),
            ],
        )?;
        run_simple(&dir, &["reset".into(), "--hard".into(), remote_head])?;
    } else {
        let url = format!("https://github.com/{spec}.git");
        std::fs::create_dir_all(dir.parent().unwrap_or(std::path::Path::new("/")))?;
        run_simple(
            dir.parent().unwrap_or(std::path::Path::new("/")),
            &["clone".into(), url.clone(), dir.to_string_lossy().into()],
        )
        .map_err(|e| {
            // a 404/private repo makes git ask for credentials — surface
            // the real problem instead of a credential prompt
            let msg = e.to_string();
            if msg.contains("could not read Username") || msg.contains("terminal prompts disabled")
            {
                ToolError::Other(format!("no such repo (or private): {url}"))
            } else {
                e
            }
        })?;
    }
    Ok(dir)
}

pub fn head_sha(repo: &std::path::Path) -> ToolResult<String> {
    let out = run_simple(repo, &["rev-parse".into(), "HEAD".into()])?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// HEAD as a `CommitInfo` — the current snapshot's one history record.
pub fn head_commit(repo: &std::path::Path) -> ToolResult<CommitInfo> {
    let fmt = "%H%x1f%aN%x1f%aE%x1f%aI%x1f%s";
    let out = run_simple(
        repo,
        &["log".into(), "-1".into(), format!("--format={fmt}")],
    )?;
    let line = String::from_utf8_lossy(&out);
    let fields: Vec<&str> = line.trim_end_matches('\n').split('\x1f').collect();
    if fields.len() != 5 {
        return Err(ToolError::GitOutput(format!(
            "HEAD record has {} fields, expected 5",
            fields.len()
        )));
    }
    let date = fields[3].to_string();
    if !valid_author_date(&date) {
        return Err(ToolError::GitOutput(format!(
            "unexpected author date shape: {date}"
        )));
    }
    Ok(CommitInfo {
        sha: fields[0].to_string(),
        author_name: fields[1].to_string(),
        author_email: fields[2].to_string(),
        date: to_utc_iso(&date)?,
        summary: fields[4].to_string(),
    })
}

fn run_simple(repo: &std::path::Path, args: &[String]) -> ToolResult<Vec<u8>> {
    let mut child = git_child(repo, args)?;
    let stdout = capped_stdout(&mut child, 64 * 1024 * 1024)?;
    let stderr = drain_stderr_now(&mut child);
    let status = child.wait()?;
    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(ToolError::GitCommand(if msg.is_empty() {
            format!("git exited with {status}")
        } else {
            msg
        }));
    }
    Ok(stdout)
}

fn capped_stdout(child: &mut std::process::Child, cap: u64) -> ToolResult<Vec<u8>> {
    let mut buf = Vec::new();
    let n = child
        .stdout
        .take()
        .expect("child spawned with piped stdout has stdout")
        .take(cap)
        .read_to_end(&mut buf)?;
    if n as u64 >= cap {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ToolError::GitCommand(format!(
            "git output exceeded the {cap} byte cap"
        )));
    }
    Ok(buf)
}

fn drain_stderr_now(child: &mut std::process::Child) -> Vec<u8> {
    child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
        .unwrap_or_default()
}

/// Walk the repo's history newest-first and return, per commit
/// (chronological, oldest first), every manifest-shaped path it touched with
/// that file's content at the commit (`None` = deleted there). Bounded by
/// `caps.max_commits`.
pub fn manifest_history(
    repo: &std::path::Path,
    caps: ScanCaps,
    warnings: &mut Vec<ScanWarning>,
) -> ToolResult<ManifestHistory> {
    let fmt = "%x1e%H%x1f%aN%x1f%aE%x1f%aI%x1f%s";
    let args: Vec<String> = vec![
        "log".into(),
        "--no-merges".into(),
        "--no-color".into(),
        "--name-only".into(),
        "-z".into(),
        format!("--max-count={}", caps.max_commits),
        format!("--format={fmt}"),
    ];
    let mut child = git_child(repo, &args)?;
    let raw = capped_stdout(&mut child, 256 * 1024 * 1024)?;
    let stderr = drain_stderr_now(&mut child);
    let status = child.wait()?;
    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(ToolError::GitCommand(if msg.is_empty() {
            format!("git exited with {status}")
        } else {
            msg
        }));
    }

    let mut out = Vec::new();
    for record in raw.split(|&b| b == RECORD_SEP) {
        if record.is_empty() {
            continue;
        }
        // Header: fields up to the first NUL; body: "\n" + NUL-terminated paths.
        let (header, body) = match record.iter().position(|&b| b == 0) {
            Some(i) => (&record[..i], &record[i + 1..]),
            None => {
                return Err(ToolError::GitOutput(
                    "log record has no NUL-terminated header".into(),
                ))
            }
        };
        let fields: Vec<&[u8]> = header.split(|&b| b == FIELD_SEP).collect();
        if fields.len() != 5 {
            return Err(ToolError::GitOutput(format!(
                "log record has {} header fields, expected 5",
                fields.len()
            )));
        }
        let sha = field(fields[0])?;
        let author_name = field(fields[1])?;
        let author_email = field(fields[2])?;
        let date = field(fields[3])?;
        let summary = field(fields[4])?;
        if !valid_author_date(&date) {
            return Err(ToolError::GitOutput(format!(
                "unexpected author date shape: {date}"
            )));
        }
        // Normalize to UTC: the as-of machinery compares dates as strings,
        // which is only ordering-correct when every date is in one zone.
        let date = to_utc_iso(&date)?;
        // Body is a leading newline then NUL-terminated paths (empty for a
        // commit that touched no files — impossible with --name-only, but
        // kept as a guard).
        let paths: Vec<String> = body
            .strip_prefix(b"\n")
            .unwrap_or(body)
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).to_string())
            .filter(|p| crate::scan::discover::kind_for(p).is_some())
            .collect();
        if paths.is_empty() {
            continue;
        }
        let email = if author_email.is_empty() {
            warnings.push(ScanWarning::EmptyAuthor {
                commit: sha.clone(),
            });
            format!("{author_name}@unknown")
        } else {
            author_email
        };
        out.push((
            CommitInfo {
                sha,
                author_name,
                author_email: email,
                date,
                summary,
            },
            paths,
        ));
    }
    if out.len() as u64 >= caps.max_commits {
        warnings.push(ScanWarning::CapHit {
            what: "commits walked".into(),
            limit: caps.max_commits,
        });
    }

    // git log is newest-first; replay oldest-first.
    out.reverse();

    // Fetch content per (commit, manifest path) — in parallel. Each `git
    // show` is an independent subprocess, and one per blob is where long
    // histories spend their wall clock; the reads share nothing, so a small
    // worker pool over the flat job list turns process-spawn latency into
    // throughput. Results land in pre-sized slots keyed by job order, so the
    // output (and every warning) is identical to the serial walk, and the
    // lowest-indexed failure is reported, so even the error is deterministic.
    let mut starts = Vec::with_capacity(out.len());
    let mut total_jobs = 0usize;
    for (_, paths) in &out {
        starts.push(total_jobs);
        total_jobs += paths.len();
    }
    let results = std::sync::Mutex::new(
        out.iter()
            .map(|(_, paths)| vec![None; paths.len()])
            .collect::<Vec<_>>(),
    );
    let errors: std::sync::Mutex<Vec<(usize, ToolError)>> = std::sync::Mutex::new(Vec::new());
    let next_job = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(16); // a git subprocess per job; more than 16 spawns thrash
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if !errors.lock().expect("errors mutex").is_empty() {
                    return; // a sibling failed; stop feeding the pool
                }
                let job = next_job.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if job >= total_jobs {
                    return;
                }
                // Flat job index → (record, file): starts is strictly
                // increasing (empty records were skipped above).
                let r = starts.partition_point(|&st| st <= job) - 1;
                let f = job - starts[r];
                let (commit, paths) = &out[r];
                match show_opt(repo, &commit.sha, &paths[f], caps.max_manifest_bytes) {
                    Ok(content) => results.lock().expect("results mutex")[r][f] = content,
                    Err(e) => {
                        errors.lock().expect("errors mutex").push((job, e));
                        return;
                    }
                }
            });
        }
    });
    let mut errs = errors.into_inner().expect("errors mutex");
    if !errs.is_empty() {
        errs.sort_by_key(|(job, _)| *job);
        return Err(errs.remove(0).1);
    }
    let results = results.into_inner().expect("results mutex");
    let mut with_content = Vec::with_capacity(out.len());
    for ((commit, paths), files) in out.into_iter().zip(results) {
        with_content.push((commit, paths.into_iter().zip(files).collect()));
    }
    Ok(with_content)
}

/// `git show <sha>:<path>` capped to `cap` bytes; `Ok(None)` when the path
/// does not exist at that commit.
fn show_opt(
    repo: &std::path::Path,
    sha: &str,
    path: &str,
    cap: usize,
) -> ToolResult<Option<Vec<u8>>> {
    let mut child = git_child(repo, &["show".to_string(), format!("{sha}:{path}")])?;
    let data = capped_stdout(&mut child, cap as u64)?;
    let stderr = drain_stderr_now(&mut child);
    let status = child.wait()?;
    if status.success() {
        Ok(Some(data))
    } else {
        // git's failure here IS the deletion signal; keep its message for the
        // unexpected-failure case only when it is not the standard
        // "path does not exist" complaint.
        let msg = String::from_utf8_lossy(&stderr).trim().to_string();
        if msg.contains("does not exist") || msg.contains("exists on disk, but not in") {
            Ok(None)
        } else {
            Err(ToolError::GitCommand(if msg.is_empty() {
                format!("git show {sha}:{path} failed")
            } else {
                msg
            }))
        }
    }
}

fn field(b: &[u8]) -> ToolResult<String> {
    Ok(String::from_utf8_lossy(b).to_string())
}

/// Light shape check on `%aI` (strict ISO-8601): `YYYY-MM-DDTHH:MM:SS` plus
/// `Z` or `±HH:MM`. The as-of machinery compares dates as strings, which is
/// only correct when every date comes from this one format.
fn valid_author_date(v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() != 20 && b.len() != 25 {
        return false;
    }
    b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[17..19].iter().all(u8::is_ascii_digit)
        && matches!(b[19], b'Z' | b'+' | b'-')
}

/// Convert a `%aI` instant to `YYYY-MM-DDTHH:MM:SSZ` (UTC), so that string
/// comparison across commits is chronological. A zoned input adjusts by its
/// offset; a `Z` input passes through.
fn to_utc_iso(v: &str) -> ToolResult<String> {
    let year: i64 = v[0..4]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad year in {v}")))?;
    let month: i64 = v[5..7]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad month in {v}")))?;
    let day: i64 = v[8..10]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad day in {v}")))?;
    let hour: i64 = v[11..13]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad hour in {v}")))?;
    let minute: i64 = v[14..16]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad minute in {v}")))?;
    let second: i64 = v[17..19]
        .parse()
        .map_err(|_| ToolError::GitOutput(format!("bad second in {v}")))?;
    let offset: i64 = match v.as_bytes()[19] {
        b'Z' => 0,
        sign @ (b'+' | b'-') => {
            let oh: i64 = v[20..22]
                .parse()
                .map_err(|_| ToolError::GitOutput(format!("bad offset in {v}")))?;
            let om: i64 = v[23..25]
                .parse()
                .map_err(|_| ToolError::GitOutput(format!("bad offset in {v}")))?;
            let magnitude = oh * 3600 + om * 60;
            if sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return Err(ToolError::GitOutput(format!("bad zone in {v}"))),
    };
    // Days from civil (Howard Hinnant's algorithm), the inverse of the
    // writer's civil_from_days.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let utc_days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (uy, um, ud) = civil_from_days(utc_days);
    Ok(format!(
        "{uy:04}-{um:02}-{ud:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    ))
}

/// Days since the Unix epoch → civil (year, month, day).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_normalization_handles_offsets() {
        assert_eq!(
            to_utc_iso("2024-06-01T00:30:00+02:00").unwrap(),
            "2024-05-31T22:30:00Z"
        );
        assert_eq!(
            to_utc_iso("2024-06-01T23:30:00-01:00").unwrap(),
            "2024-06-02T00:30:00Z"
        );
        assert_eq!(
            to_utc_iso("2024-06-01T12:00:00Z").unwrap(),
            "2024-06-01T12:00:00Z"
        );
    }

    #[test]
    fn date_shape_rejects_loose_spellings() {
        assert!(valid_author_date("2024-06-01T12:00:00Z"));
        assert!(valid_author_date("2024-06-01T12:00:00+05:30"));
        assert!(!valid_author_date("2024-06-01 12:00:00"));
        assert!(!valid_author_date("2024-06-01T12:00:00"));
        assert!(!valid_author_date("not a date"));
    }
}

#[cfg(test)]
mod github_tests {
    use super::*;

    #[test]
    fn spec_validation() {
        assert!(validate_github_spec("neo4j/neo4j-java-driver").is_ok());
        assert!(validate_github_spec("b33f/BOMvoyage").is_ok());
        assert!(validate_github_spec("org.w3/util").is_ok()); // dots fine
                                                              // bad shapes
        assert!(validate_github_spec("just-a-name").is_err());
        assert!(validate_github_spec("a/b/c").is_err());
        assert!(validate_github_spec("/repo").is_err());
        assert!(validate_github_spec("org/").is_err());
        assert!(validate_github_spec("").is_err());
        // traversal and injection
        assert!(validate_github_spec("a/../..").is_err());
        assert!(validate_github_spec("../etc/passwd").is_err());
        assert!(validate_github_spec("a/b; rm -rf /").is_err());
        assert!(validate_github_spec("a/b/c/d").is_err());
        // cache path derivation is spec-derived and safe
        let (org, repo) = validate_github_spec("neo4j/neo4j-java-driver").unwrap();
        assert_eq!(
            (org.as_str(), repo.as_str()),
            ("neo4j", "neo4j-java-driver")
        );
    }
}
