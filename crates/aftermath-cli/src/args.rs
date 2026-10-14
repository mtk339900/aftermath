//! Command-line argument definitions (clap).
//!
//! Every analysis command shares the [`Global`] flags. Global flags are marked
//! `global = true` so they can appear before or after the subcommand.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Aftermath — every change leaves a trace.
///
/// Evidence-driven, local-first change analysis. Aftermath reads your Git
/// change, resolves what it touches with structural evidence, and separates
/// what it knows from what it cannot see. It never runs tests or mutates your
/// source without explicit consent.
#[derive(Debug, Parser)]
#[command(
    name = "aftermath",
    version,
    about,
    long_about = None,
    disable_colored_help = false,
    propagate_version = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub global: Global,
}

/// Flags shared by every subcommand.
#[derive(Debug, Args)]
pub struct Global {
    /// Path to the repository to analyze (defaults to the current directory
    /// or the nearest enclosing Git repository).
    #[arg(long, short = 'C', global = true, value_name = "PATH")]
    pub repo: Option<PathBuf>,

    /// Path to a specific `aftermath.toml` (defaults to `<repo>/aftermath.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Emit machine-readable JSON instead of terminal text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable colored output (also honors the `NO_COLOR` environment variable).
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Suppress non-essential output; only errors and requested results print.
    #[arg(long, short, global = true)]
    pub quiet: bool,

    /// Print additional diagnostics to stderr.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Never use the network (Aftermath is offline by default; this is asserted).
    #[arg(long, global = true)]
    pub offline: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an `aftermath.toml` configuration and the `.aftermath/` directory.
    Init {
        /// Overwrite an existing `aftermath.toml`.
        #[arg(long)]
        force: bool,
        /// Also initialize a Git repository if none exists (opt-in).
        #[arg(long)]
        git: bool,
    },

    /// Analyze a change and print the consequence ledger (the default command).
    Inspect {
        /// Analyze the staged index instead of the working tree.
        #[arg(long)]
        staged: bool,

        /// Analyze a specific commit (its change against its parent).
        #[arg(long, value_name = "REV")]
        revision: Option<String>,

        /// Diff this run against a previously saved JSON report.
        #[arg(long, value_name = "FILE")]
        baseline: Option<PathBuf>,

        /// Restrict analysis to one file (repeatable).
        #[arg(long, value_name = "PATH")]
        file: Vec<String>,
    },

    /// Explain one symbol's change consequences in detail.
    Why {
        /// The symbol to explain (name or `path::name`).
        symbol: String,
        #[arg(long)]
        staged: bool,
        #[arg(long, value_name = "REV")]
        revision: Option<String>,
    },

    /// Show what the analysis could not prove (unknowns and blind spots).
    Gaps {
        #[arg(long)]
        staged: bool,
        #[arg(long, value_name = "REV")]
        revision: Option<String>,
    },

    /// Report the verification state: which tests exist, which ran, results.
    Verify {
        #[arg(long)]
        staged: bool,
        #[arg(long, value_name = "REV")]
        revision: Option<String>,
        /// Explicitly consent to executing discovered integration tests.
        #[arg(long)]
        run_tests: bool,
    },

    /// Emit the full deterministic report.
    Report {
        #[arg(long, value_enum, default_value_t = Format::Terminal)]
        format: Format,
        /// Write the report to a file instead of standard output.
        #[arg(long, short, value_name = "FILE")]
        out: Option<PathBuf>,
        #[arg(long)]
        staged: bool,
        #[arg(long, value_name = "REV")]
        revision: Option<String>,
    },

    /// Correlate file changes across Git history (correlation, not proof).
    History {
        /// Focus on one given file (repeatable).
        #[arg(long, value_name = "PATH")]
        file: Vec<String>,
        /// Maximum number of commits to examine.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },

    /// Manage the advisory change-witness Git hook (`pre-commit`).
    Watch {
        /// Remove Aftermath's hook instead of installing it.
        #[arg(long)]
        remove: bool,
    },

    /// Check the environment and configuration.
    Doctor,

    /// Print the JSON schema for Aftermath reports.
    Schema {
        /// Write the schema to a file instead of standard output.
        #[arg(long, short, value_name = "FILE")]
        out: Option<PathBuf>,
    },
}

/// Output format for `aftermath report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Format {
    Terminal,
    Json,
    Markdown,
}
