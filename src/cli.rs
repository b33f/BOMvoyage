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
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
