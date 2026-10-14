//! Aftermath core: evidence model and analysis engine.
//!
//! This crate is deliberately independent from the command-line interface.
//! It exposes domain types, a Git change reader, a language adapter layer,
//! an impact analyzer, an orchestration entry point, and deterministic
//! JSON/Markdown report writers.

pub mod analyze;
pub mod cache;
pub mod config;
pub mod evidence;
pub mod evidencediff;
pub mod executor;
pub mod gitrepo;
pub mod impact;
pub mod lang;
pub mod redact;
pub mod report;
pub mod tests;
pub mod time;
pub mod workspace;

pub use evidence::{Confidence, Evidence, EvidenceKind, SymbolRef};

/// Version of the Aftermath binary that produced a report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the analysis report schema. Bumped on breaking schema changes.
pub const SCHEMA_VERSION: &str = "1.0.0";
