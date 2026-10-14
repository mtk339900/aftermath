//! The Aftermath evidence model.
//!
//! Every claim produced by the analyzer is represented as an [`Evidence`]-bearing
//! structure so that reports can always answer: *what do we know, what supports it,
//! and what remains unknown?*
//!
//! The types here are the stable contract behind the JSON report schema
//! (`schemas/aftermath-report.schema.json`, schema version [`crate::SCHEMA_VERSION`]).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Kind of a declared symbol, as recovered from syntax.
///
/// Deliberately coarse: Aftermath does not perform type checking, so it does not
/// distinguish subtle categories it cannot prove.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    TypeAlias,
    Enum,
    Variable,
    Module,
    /// The declaration form was recognized as a symbol but its category was not.
    Unknown,
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Enum => "enum",
            SymbolKind::Variable => "variable",
            SymbolKind::Module => "module",
            SymbolKind::Unknown => "symbol",
        };
        f.write_str(s)
    }
}

/// A reference to a declared symbol.
///
/// `file` is always repository-relative and uses `/` separators so that reports
/// are reproducible across machines and platforms.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolRef {
    /// Repository-relative path with forward slashes.
    pub file: String,
    /// Declared name; for methods, `Class.method`.
    pub name: String,
    pub kind: SymbolKind,
}

impl SymbolRef {
    pub fn new(file: impl Into<String>, name: impl Into<String>, kind: SymbolKind) -> Self {
        Self {
            file: file.into(),
            name: name.into(),
            kind,
        }
    }

    /// Stable identifier used as a ledger key and in snapshots.
    pub fn id(&self) -> String {
        format!("{}::{}", self.file, self.name)
    }
}

impl std::fmt::Display for SymbolRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.file)
    }
}

// ---------------------------------------------------------------------------
// Confidence
// ---------------------------------------------------------------------------

/// Classification attached to every significant finding.
///
/// These are plain-language classes, not fabricated percentages. The exact
/// meaning of each class is documented in `docs/EVIDENCE_MODEL.md` and is part
/// of the schema contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A resolved, structural link exists: a call to a locally declared or
    /// import-resolved symbol, or a diff hunk that covers the declaration itself.
    ConfirmedRelationship,
    /// Multiple independent signals converge (for example a resolved import plus
    /// a call site), or a chain of exclusively confirmed links reached through
    /// bounded transitive traversal.
    StronglySupportedImpact,
    /// Plausible but unproven: name-only matches, or member calls that cannot be
    /// attributed without type information.
    PossibleImpact,
    /// Resolution was blocked (dynamic dispatch, parse errors, unresolved
    /// imports). Absence of a relationship was *not* established.
    Unknown,
    /// An executed test exercises this behavior and its recorded result is pass.
    VerifiedByExecutedTest,
    /// No relevant test was identified by the available analysis, or identified
    /// tests were not executed.
    NotVerified,
}

impl Confidence {
    /// Rank used to pick the strongest impact classification.
    /// Test-state variants (`VerifiedByExecutedTest`, `NotVerified`) rank lowest
    /// because they describe verification, not relationship strength.
    pub fn impact_rank(&self) -> u8 {
        match self {
            Confidence::ConfirmedRelationship => 0,
            Confidence::StronglySupportedImpact => 1,
            Confidence::PossibleImpact => 2,
            Confidence::Unknown => 3,
            Confidence::VerifiedByExecutedTest | Confidence::NotVerified => 4,
        }
    }

    /// Short label used in terminal output.
    pub fn label(&self) -> &'static str {
        match self {
            Confidence::ConfirmedRelationship => "confirmed relationship",
            Confidence::StronglySupportedImpact => "strongly supported impact",
            Confidence::PossibleImpact => "possible impact",
            Confidence::Unknown => "unknown",
            Confidence::VerifiedByExecutedTest => "verified by executed test",
            Confidence::NotVerified => "not verified",
        }
    }

    /// True when the classification asserts an actual relationship.
    pub fn is_impact(&self) -> bool {
        matches!(
            self,
            Confidence::ConfirmedRelationship
                | Confidence::StronglySupportedImpact
                | Confidence::PossibleImpact
        )
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// Where a piece of evidence came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// Changed lines observed in a Git diff.
    GitDiffHunk,
    /// Declaration or reference located by parsing source text.
    SyntaxTree,
    /// A call expression referencing a symbol.
    CallSite,
    /// An import/export binding, including aliases and re-exports.
    ImportEdge,
    /// An `extends`/`implements` relationship.
    InheritanceEdge,
    /// A type annotation referencing a declared type.
    TypeReference,
    /// A test file references the symbol.
    TestReference,
    /// A test that was actually executed, with its recorded result.
    TestExecution,
    /// Files or symbols changed together in Git history. Correlation only —
    /// never proof of a technical dependency.
    HistoryCorrelation,
    /// A bare name match that could not be resolved to a declaration.
    NameMatch,
    /// Project configuration states a fact (for example, a generated-file marker).
    ConfigDeclaration,
}

impl std::fmt::Display for EvidenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EvidenceKind::GitDiffHunk => "git diff",
            EvidenceKind::SyntaxTree => "syntax tree",
            EvidenceKind::CallSite => "call site",
            EvidenceKind::ImportEdge => "import",
            EvidenceKind::InheritanceEdge => "inheritance",
            EvidenceKind::TypeReference => "type reference",
            EvidenceKind::TestReference => "test reference",
            EvidenceKind::TestExecution => "test execution",
            EvidenceKind::HistoryCorrelation => "history correlation",
            EvidenceKind::NameMatch => "name match",
            EvidenceKind::ConfigDeclaration => "configuration",
        };
        f.write_str(s)
    }
}

/// A location for an evidence item. Paths are repository-relative with `/`
/// separators; snippets are single, trimmed lines capped in length.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceLocation {
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// Optional 1-based column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// Short source excerpt, already secret-redacted and length-capped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

impl EvidenceLocation {
    pub fn line(file: impl Into<String>, line: u32) -> Self {
        Self {
            file: file.into(),
            line,
            column: None,
            snippet: None,
        }
    }

    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        let mut s = snippet.into();
        const MAX: usize = 120;
        if s.len() > MAX {
            let mut cut = MAX;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s.truncate(cut);
            s.push('…');
        }
        self.snippet = Some(s);
        self
    }
}

/// A single, sourced observation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub location: EvidenceLocation,
    /// Human-readable statement of what this evidence shows.
    pub detail: String,
}

impl Evidence {
    pub fn new(kind: EvidenceKind, location: EvidenceLocation, detail: impl Into<String>) -> Self {
        Self {
            kind,
            location,
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Change description
// ---------------------------------------------------------------------------

/// How a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCategory {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl std::fmt::Display for ChangeCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ChangeCategory::Added => "added",
            ChangeCategory::Modified => "modified",
            ChangeCategory::Deleted => "deleted",
            ChangeCategory::Renamed => "renamed",
        };
        f.write_str(s)
    }
}

/// The analysis method that produced a finding. Recorded per ledger entry so
/// reports state *how* conclusions were reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisMethod {
    /// Parsing source text into a syntax tree.
    SyntaxAnalysis,
    /// Resolving import/export bindings between modules.
    ImportResolution,
    /// Scanning call expressions for references.
    CallSiteScan,
    /// Breadth-first traversal over resolved edges with a depth bound.
    BoundedTransitiveTraversal,
    /// Matching test files against symbols.
    TestDiscovery,
    /// Actually running a test command.
    TestExecution,
    /// Correlating file changes across Git history.
    HistoryCorrelation,
    /// Falling back to bare name matching where resolution failed.
    NameHeuristic,
}

impl std::fmt::Display for AnalysisMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AnalysisMethod::SyntaxAnalysis => "syntax analysis",
            AnalysisMethod::ImportResolution => "import resolution",
            AnalysisMethod::CallSiteScan => "call-site scan",
            AnalysisMethod::BoundedTransitiveTraversal => "bounded transitive traversal",
            AnalysisMethod::TestDiscovery => "test discovery",
            AnalysisMethod::TestExecution => "test execution",
            AnalysisMethod::HistoryCorrelation => "history correlation",
            AnalysisMethod::NameHeuristic => "name heuristic",
        };
        f.write_str(s)
    }
}

/// How an affected symbol relates to the changed symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    /// Calls the changed symbol directly.
    DirectCaller,
    /// Imports the changed symbol (aliases resolved).
    ImportsSymbol,
    /// Extends or implements the changed symbol.
    InheritsFrom,
    /// References the changed symbol in a type position.
    TypeConsumer,
    /// Reached through further resolved links.
    Transitive,
    /// A test file that references the changed symbol.
    TestConsumer,
    /// Reads or otherwise uses the symbol outside a call.
    UsesSymbol,
}

impl std::fmt::Display for Relationship {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Relationship::DirectCaller => "calls",
            Relationship::ImportsSymbol => "imports",
            Relationship::InheritsFrom => "inherits",
            Relationship::TypeConsumer => "uses type",
            Relationship::Transitive => "reaches transitively",
            Relationship::TestConsumer => "tests",
            Relationship::UsesSymbol => "uses",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Outcome of a test or test run. The four states are kept distinct: a tool
/// that did not run tests must never report them as passing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// Executed and observed to pass.
    Passed,
    /// Executed and observed to fail.
    Failed,
    /// Identified but not executed during this analysis.
    NotExecuted,
    /// Could not be executed (missing runner, broken command, no consent).
    Unavailable,
    /// Explicitly skipped by the test runner.
    Skipped,
}

impl std::fmt::Display for TestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TestStatus::Passed => "passed",
            TestStatus::Failed => "failed",
            TestStatus::NotExecuted => "not executed",
            TestStatus::Unavailable => "unavailable",
            TestStatus::Skipped => "skipped",
        };
        f.write_str(s)
    }
}

/// A test identified by discovery, with its evidence and (optionally) its
/// executed result.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestEvidence {
    /// Test name as reported by the runner, or the file path for file-level records.
    pub id: String,
    /// Repository-relative test file.
    pub file: String,
    /// Symbol this test relates to, when identified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolRef>,
    pub status: TestStatus,
    /// How the relation was established.
    pub evidence: Vec<Evidence>,
    /// Honest note, e.g. "no test command configured" or runner output excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A recorded test execution (Feature H: reproducibility).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestRunRecord {
    /// Executed command as an argv array — never a shell string.
    pub command: Vec<String>,
    /// Repository-relative working directory.
    pub cwd: String,
    pub status: TestStatus,
    /// Process exit code, when a process actually ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// RFC 3339 timestamp of execution.
    pub executed_at: String,
    /// Duration in milliseconds, when measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Extracted summary line(s) from runner output, if recognizable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// How consent was obtained.
    pub consent: Consent,
}

/// Whether a potentially risky operation was authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consent {
    /// The user asked for this exact operation on the command line.
    Explicit,
    /// The user confirmed interactively.
    Confirmed,
    /// The operation was pre-authorized in configuration.
    Configured,
    /// No execution occurred.
    NotGranted,
}

// ---------------------------------------------------------------------------
// Affected symbols and ledger entries (Feature A)
// ---------------------------------------------------------------------------

/// A symbol affected by the change, with the evidence that connects it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AffectedSymbol {
    pub symbol: SymbolRef,
    pub relationship: Relationship,
    /// Hop distance from the changed symbol (1 = direct).
    pub depth: u32,
    pub confidence: Confidence,
    pub evidence: Vec<Evidence>,
    /// Full chain of symbols traversed, changed symbol first (depth > 1 only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<SymbolRef>,
}

/// One entry of the Change Consequence Ledger.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Stable identifier: `<file>::<symbol>` or `<file>::(file)` for file-level changes.
    pub id: String,
    pub changed_file: String,
    /// The symbol whose declaration the diff covers; `None` when no declaration
    /// was located (the report then says so explicitly).
    pub changed_symbol: Option<SymbolRef>,
    pub change_category: ChangeCategory,
    /// 1-based new-side line numbers touched by the diff.
    pub changed_lines: Vec<u32>,
    pub direct_affected: Vec<AffectedSymbol>,
    pub transitive_affected: Vec<AffectedSymbol>,
    pub methods: Vec<AnalysisMethod>,
    /// Strongest *impact* classification across affected symbols.
    pub confidence: Confidence,
    pub tests: Vec<TestEvidence>,
    /// Behaviors the analysis could not verify — first-class output.
    pub unverified: Vec<String>,
    /// Deterministic, structural next steps.
    pub suggested_next_steps: Vec<String>,
    /// Counterfactual test suggestions (Feature C).
    pub suggestions: Vec<TestSuggestion>,
}

// ---------------------------------------------------------------------------
// Counterfactual test suggestions (Feature C)
// ---------------------------------------------------------------------------

/// The structural rule that produced a suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionRule {
    /// Function takes a boundary-sensitive numeric value.
    BoundaryValues,
    /// Function contains multiple branches.
    BranchCoverage,
    /// An error-handling path changed or exists untested.
    ErrorPath,
    /// A public signature changed.
    SignatureChange,
    /// Multiple callers with potentially different expectations.
    MultipleCallers,
    /// Parser-like function should be probed with malformed input.
    MalformedInput,
    /// A changed public export needs a consumer-level test.
    PublicApiConsumer,
}

impl std::fmt::Display for SuggestionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SuggestionRule::BoundaryValues => "boundary values",
            SuggestionRule::BranchCoverage => "branch coverage",
            SuggestionRule::ErrorPath => "error path",
            SuggestionRule::SignatureChange => "signature change",
            SuggestionRule::MultipleCallers => "multiple callers",
            SuggestionRule::MalformedInput => "malformed input",
            SuggestionRule::PublicApiConsumer => "public API consumer",
        };
        f.write_str(s)
    }
}

/// A deterministic suggestion for a test that does not yet exist.
///
/// Suggestions are *proposals*: they have never been executed, and reports say so.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestSuggestion {
    pub id: String,
    pub title: String,
    /// Why the structure of the code implies this test.
    pub rationale: String,
    pub rule: SuggestionRule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_symbol: Option<SymbolRef>,
    /// Existing test file where the test would naturally live, when identified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_location: Option<String>,
    pub evidence: Vec<Evidence>,
    /// Always false; recorded so no consumer mistakes a suggestion for a result.
    pub executed: bool,
}

// ---------------------------------------------------------------------------
// Unknowns (Feature B)
// ---------------------------------------------------------------------------

/// Category of unknown behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownKind {
    ChangedPublicSymbolWithoutIdentifiedTest,
    UnresolvedCallSite,
    UnresolvedImport,
    DynamicImport,
    RuntimeDispatch,
    ChangedInterfaceWithoutConsumerInfo,
    TestNotExecuted,
    TestUnavailable,
    SkippedInput,
}

impl std::fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            UnknownKind::ChangedPublicSymbolWithoutIdentifiedTest => {
                "changed public symbol without identified test"
            }
            UnknownKind::UnresolvedCallSite => "unresolved call site",
            UnknownKind::UnresolvedImport => "unresolved import",
            UnknownKind::DynamicImport => "dynamic import",
            UnknownKind::RuntimeDispatch => "runtime dispatch",
            UnknownKind::ChangedInterfaceWithoutConsumerInfo => {
                "changed interface without consumer information"
            }
            UnknownKind::TestNotExecuted => "test not executed",
            UnknownKind::TestUnavailable => "test unavailable",
            UnknownKind::SkippedInput => "skipped input",
        };
        f.write_str(s)
    }
}

/// An explicitly reported unknown. Wording separates *absence of discovered
/// coverage* from *proven absence of coverage*.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnknownItem {
    pub id: String,
    pub kind: UnknownKind,
    /// Neutral statement of what is not known.
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_symbol: Option<SymbolRef>,
    pub evidence: Vec<Evidence>,
    /// Concrete, safe next investigation.
    pub next_step: String,
}

// ---------------------------------------------------------------------------
// Blind spots (Feature E)
// ---------------------------------------------------------------------------

/// Why part of the repository was not analyzed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlindSpotKind {
    UnsupportedLanguage,
    ParseErrors,
    UnresolvedImport,
    DynamicDispatch,
    GeneratedCode,
    IgnoredDirectory,
    OversizedFile,
    BinaryFile,
    MissingLanguageConfig,
    IncompleteSymbolResolution,
}

impl std::fmt::Display for BlindSpotKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BlindSpotKind::UnsupportedLanguage => "unsupported language",
            BlindSpotKind::ParseErrors => "syntax errors in file",
            BlindSpotKind::UnresolvedImport => "unresolved import",
            BlindSpotKind::DynamicDispatch => "dynamic dispatch",
            BlindSpotKind::GeneratedCode => "generated code",
            BlindSpotKind::IgnoredDirectory => "ignored directory",
            BlindSpotKind::OversizedFile => "file exceeds size limit",
            BlindSpotKind::BinaryFile => "binary file",
            BlindSpotKind::MissingLanguageConfig => "missing language configuration",
            BlindSpotKind::IncompleteSymbolResolution => "incomplete symbol resolution",
        };
        f.write_str(s)
    }
}

/// A recorded limitation of analysis completeness. An analysis is never called
/// "complete" while relevant inputs are skipped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlindSpot {
    pub id: String,
    pub kind: BlindSpotKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// What was skipped and why.
    pub reason: String,
    /// What this prevents the analysis from proving.
    pub impact: String,
}

// ---------------------------------------------------------------------------
// Repository archaeology (Feature F)
// ---------------------------------------------------------------------------

/// A commit examined by history analysis.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HistoryCommit {
    /// Full object id (hex).
    pub id: String,
    /// Sanitized subject line (first line only).
    pub subject: String,
    /// Repository-relative files touched.
    pub files: Vec<String>,
}

/// A pair of files that changed together repeatedly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CoChange {
    pub file: String,
    pub files: Vec<String>,
    pub shared_commits: u32,
}

/// Results of optional history analysis. Always labeled as correlation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HistoryReport {
    pub commits_examined: u32,
    /// History walk was truncated at the configured limit.
    pub truncated: bool,
    pub co_changes: Vec<CoChange>,
    pub relevant_commits: Vec<HistoryCommit>,
    /// Standing disclaimer carried in every history section.
    pub disclaimer: String,
}

impl HistoryReport {
    pub const DISCLAIMER: &'static str =
        "Historical co-change is correlation, not a proven technical dependency.";
}

// ---------------------------------------------------------------------------
// Evidence diff (Feature G)
// ---------------------------------------------------------------------------

/// A compact description of one relationship for diffing two reports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RelationshipSummary {
    /// `<entry id> -> <affected symbol id>`.
    pub from: String,
    pub to: String,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestSummary {
    pub id: String,
    pub file: String,
    pub status: TestStatus,
}

/// How the evidence changed between two analysis runs (Feature G).
///
/// Removed evidence is *not* proof of a regression; the structure carries that
/// disclaimer explicitly.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EvidenceDiff {
    pub before_revision: Option<String>,
    pub after_revision: Option<String>,
    pub relationships_added: Vec<RelationshipSummary>,
    pub relationships_removed: Vec<RelationshipSummary>,
    pub tests_added: Vec<TestSummary>,
    pub tests_removed: Vec<TestSummary>,
    pub tests_status_changed: Vec<TestStatusChange>,
    pub unknowns_added: Vec<String>,
    pub unknowns_removed: Vec<String>,
    pub blindspots_added: Vec<String>,
    pub blindspots_removed: Vec<String>,
    pub disclaimer: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestStatusChange {
    pub id: String,
    pub file: String,
    pub before: TestStatus,
    pub after: TestStatus,
}

impl EvidenceDiff {
    pub const DISCLAIMER: &'static str =
        "Removed evidence means the analysis no longer observes it; it is not proof of a regression.";
}

// ---------------------------------------------------------------------------
// Reproducibility (Feature H)
// ---------------------------------------------------------------------------

/// Static description of what one language adapter can and cannot do.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LanguageSupportNote {
    pub language: String,
    pub extensions: Vec<String>,
    pub supported: Vec<String>,
    pub limitations: Vec<String>,
}

/// Metadata that makes a report explainable and comparable across runs.
///
/// Note: no `Ord` derive — it contains `serde_json::Value`, and ordering on
/// configuration would be meaningless anyway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reproducibility {
    pub aftermath_version: String,
    pub schema_version: String,
    /// HEAD object id at analysis time, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_revision: Option<String>,
    /// True when uncommitted changes were part of the analyzed state.
    pub repository_dirty: bool,
    /// Redacted, machine-independent configuration actually used.
    pub config: serde_json::Value,
    pub language_support: Vec<LanguageSupportNote>,
    /// RFC 3339 timestamp; the only intentionally varying field, which is why
    /// comparisons normalize it.
    pub generated_at: String,
    /// Commands Aftermath itself executed (never shell-interpolated).
    pub commands_executed: Vec<Vec<String>>,
    /// Tests that were actually executed, with real results.
    pub test_runs: Vec<TestRunRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_ranks_impact_strongest_first() {
        assert!(
            Confidence::ConfirmedRelationship.impact_rank()
                < Confidence::StronglySupportedImpact.impact_rank()
        );
        assert!(
            Confidence::StronglySupportedImpact.impact_rank()
                < Confidence::PossibleImpact.impact_rank()
        );
        assert!(Confidence::PossibleImpact.impact_rank() < Confidence::Unknown.impact_rank());
    }

    #[test]
    fn confidence_serde_uses_snake_case_labels() {
        let json = serde_json::to_string(&Confidence::ConfirmedRelationship).unwrap();
        assert_eq!(json, "\"confirmed_relationship\"");
        let back: Confidence = serde_json::from_str("\"not_verified\"").unwrap();
        assert_eq!(back, Confidence::NotVerified);
    }

    #[test]
    fn symbol_ids_are_stable() {
        let s = SymbolRef::new("src/a.ts", "parse", SymbolKind::Function);
        assert_eq!(s.id(), "src/a.ts::parse");
    }

    #[test]
    fn evidence_snippet_is_capped() {
        let long = "x".repeat(500);
        let loc = EvidenceLocation::line("a.ts", 3).with_snippet(long);
        let s = loc.snippet.unwrap();
        assert!(s.chars().count() <= 121, "snippet must be capped");
    }
}
