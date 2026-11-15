//! CLI definition. Default (no subcommand) = scan + serve: the demo path.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// SPDX 2.3 JSON
    Spdx,
    /// CycloneDX 1.5 JSON
    Cyclonedx,
    /// Flat JSON rows (one dependency per row, with temporal metadata)
    Json,
    /// CSV rows
    Csv,
}

#[derive(Debug, Parser)]
#[command(
    name = "bomvoyage",
    version,
    about = "BOMvoyage — temporal SBOM generator and explorer, powered by RelateDB",
    long_about = "Scans a git repository's dependency manifests (Python, Java, Rust, Go, Node.js/TypeScript), \
builds a graph of every dependency, transitive dependency, usage site and the full \
git history of who added what and when versions changed — then serves an interactive \
web UI with time travel: view, query and export the SBOM as of any commit or date.\n\n\
Run with no subcommand inside a repo to scan it and open the explorer.\n\n\
Powered by RelateDB — https://relatedb.ai/"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Repository path to scan (default command only)
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub path: std::path::PathBuf,

    /// Graph database file (default command only)
    #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
    pub db: std::path::PathBuf,

    /// Never check GitHub for a newer release (also: env
    /// BOMVOYAGE_NO_UPDATE_CHECK=1). The automatic check runs at most once
    /// a day and never blocks.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    /// Detailed mode: fetch per-package data from deps.dev (homepage, repo,
    /// license, security advisories) and write it onto the graph. Slower —
    /// one API call per package version. NOTE: this contacts a third-party
    /// service and reveals the scanned repository's dependency names and
    /// versions to it. Off by default; see SECURITY.md.
    #[arg(long, global = true)]
    pub detailed: bool,

    /// With --detailed: also fetch per-advisory details (severity, summary)
    /// from the OSV API — one extra call per distinct advisory.
    #[arg(long, global = true)]
    pub vuln_details: bool,

    /// Shorthand for `--detailed --vuln-details`: full detailed mode with
    /// per-advisory severity and summaries. Implies --detailed.
    #[arg(long, global = true)]
    pub more_detailed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UpdateAction {
    /// Check GitHub for a newer release and report
    Check,
    /// Download the newest release for this platform, verify its checksum,
    /// and replace the running binary in place
    Apply,
}

#[derive(Debug, Subcommand)]
pub enum SkillAction {
    /// Install the self-skill. With no `--agent`, installs for every coding
    /// agent detected on this machine (by the presence of its config
    /// directory) in one go.
    Install {
        /// The skill to install: `self` (canonical) or `bomvoyage`.
        /// Passing an agent name here is a hard error — use --agent.
        #[arg(value_name = "SKILL", default_value = "self")]
        skill: String,
        /// Install only for this agent (e.g. `--agent claude-code`);
        /// supported: claude-code, cursor, windsurf, copilot, antigravity,
        /// gemini-cli, cline, codex, pi, opencode, junie, kiro
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
    },
    /// Show which agents are detected and whether the skill is installed
    List,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check for a new release (`update check`) or upgrade in place (`update apply`)
    Update {
        /// What to do
        #[arg(value_enum, default_value = "check")]
        action: UpdateAction,
    },
    /// Install this tool's agent skill for coding agents on this machine
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Scan the repo (or refresh an existing graph database)
    Scan {
        /// Repository path
        #[arg(value_name = "DIR", default_value = ".")]
        path: std::path::PathBuf,
        /// Graph database file (Relate .rl bundle)
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// Re-scan even if the repository HEAD is unchanged
        #[arg(long)]
        force: bool,
        /// Cap on commits walked
        #[arg(long, value_name = "N", default_value_t = 5000)]
        max_commits: u64,
    },
    /// Serve the interactive BOMvoyage explorer (auto-scans if the database is missing)
    Serve {
        /// Repository path (used when a scan is needed)
        #[arg(long, value_name = "DIR", default_value = ".")]
        path: std::path::PathBuf,
        /// Graph database file
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// Port to listen on
        #[arg(long, value_name = "PORT", default_value_t = 8334)]
        port: u16,
        /// Open the browser after starting
        #[arg(long)]
        open: bool,
        /// Cap on commits walked (used when a scan is needed)
        #[arg(long, value_name = "N", default_value_t = 5000)]
        max_commits: u64,
    },
    /// Add detailed data (deps.dev links, license, security advisories) to
    /// an existing graph database in place, without rescanning
    Detailed {
        /// Graph database file (Relate .rl bundle)
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// Re-fetch even for already-detailed packages (busts the local cache)
        #[arg(long)]
        refresh: bool,
    },
    /// Export a fully static, self-contained explorer site (deployable to
    /// Cloudflare Pages or any static host; the graph is explored in-browser
    /// via the WASM build of RelateDB)
    Site {
        /// Repository path (a scan runs if the database is missing or stale)
        #[arg(value_name = "DIR", default_value = ".")]
        path: std::path::PathBuf,
        /// Graph database file (created/updated if a scan is needed)
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// Output directory for the static site
        #[arg(long, short, value_name = "DIR", default_value = "dist")]
        out: std::path::PathBuf,
        /// Re-scan even if the repository HEAD is unchanged, and overwrite
        /// a non-empty output directory
        #[arg(long)]
        force: bool,
        /// Cap on commits walked
        #[arg(long, value_name = "N", default_value_t = 5000)]
        max_commits: u64,
        /// Serve the graph engine from a pinned public CDN instead of
        /// bundling it into the site (adds a third-party script to YOUR page)
        #[arg(long)]
        wasm_cdn: bool,
    },
    /// Export the SBOM (optionally as of a past commit or date)
    Export {
        /// Graph database file
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// Output format
        #[arg(long, short, value_enum, default_value = "spdx")]
        format: Format,
        /// Point in time: ISO date (2024-06-01), ISO datetime, commit sha (prefix ok), or `HEAD`
        #[arg(long)]
        as_of: Option<String>,
        /// Write to a file instead of stdout
        #[arg(long, short, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
    },
    /// Run a read-only Cypher query against the graph
    Query {
        /// Graph database file
        #[arg(long, short, value_name = "FILE", default_value = "bomvoyage.rl")]
        db: std::path::PathBuf,
        /// The Cypher statement
        #[arg(value_name = "CYPHER")]
        cypher: String,
    },
}
