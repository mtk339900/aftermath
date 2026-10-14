//! Analysis orchestration: turn a repository's current change into a
//! deterministic [`Report`].
//!
//! Pipeline (evidence-before-speculation, at every step):
//!
//! 1. Read the change set from Git (worktree / staged / revision).
//! 2. Index the workspace, parsing the *new* and *old* side of every changed
//!    file so symbols can be located on both sides of a diff.
//! 3. Resolve imports and build the impact engine.
//! 4. For each change, locate the enclosing declarations (innermost
//!    declarations covering the changed lines) and collect affected symbols.
//! 5. Discover tests; execute them only with explicit consent.
//! 6. Emit unknowns (things the analysis could *not* prove) and counterfactual
//!    test suggestions (never executed).
//! 7. Assemble and finalize the report (sorted, redacted, reproducible).
//!
//! The orchestrator produces no fabricated certainty: every confidence label
//! traces back to a [`resolved`](crate::workspace) reference or a documented
//! name-match fallback.

use std::collections::BTreeMap;

use crate::evidence::Consent;
use crate::evidence::{
    AffectedSymbol, AnalysisMethod, ChangeCategory, CoChange, Confidence, Evidence, EvidenceKind,
    EvidenceLocation, HistoryCommit, HistoryReport, LedgerEntry, Relationship, Reproducibility,
    SuggestionRule, SymbolRef, TestEvidence, TestSuggestion, UnknownItem, UnknownKind,
};
use crate::gitrepo::{Change, GitRepo};
use crate::impact::ImpactEngine;
use crate::report::{default_language_support, ChangeScope, Report, ScopeMode, Summary};
use crate::tests::{DiscoveredTest, TestRunnerConfig};
use crate::{config::Config, executor::Executor, time};

const MAX_SUGGESTIONS_PER_ENTRY: usize = 12;
const MAX_LEDGER_ENTRIES: usize = 2000;

/// Knobs for one analysis run.
#[derive(Clone, Debug, Default)]
pub struct AnalysisOptions {
    /// Which state to analyze. `Revision` requires `revision`.
    pub mode: ScopeMode,
    /// Commit-ish to analyze when `mode == Revision`.
    pub revision: Option<String>,
    /// Explicit consent to execute discovered integration tests.
    pub run_tests: bool,
    /// Opt-in history (co-change) analysis.
    pub history: bool,
    /// Pin `generated_at` for reproducible test output; `None` uses the clock.
    pub generated_at: Option<String>,
    /// Home directory for path redaction in report text.
    pub home: Option<std::path::PathBuf>,
}

impl AnalysisOptions {
    fn consent(&self) -> Consent {
        if self.run_tests {
            Consent::Explicit
        } else {
            Consent::NotGranted
        }
    }
}

/// Run a full analysis and return the finalized report.
///
/// `exec` is used only when `run_tests` is set; it is never invoked otherwise.
pub fn analyze(
    repo: &GitRepo,
    cfg: &Config,
    opts: &AnalysisOptions,
    cache: &mut crate::cache::ParseCache,
    exec: &mut dyn Executor,
) -> Result<Report, String> {
    let raw_changes = repo
        .changes(opts.mode, opts.revision.as_deref())
        .map_err(|e| format!("failed to read changes: {e}"))?;

    // Never analyze Aftermath's own working state (`.aftermath/`) or files the
    // configuration excludes — the cache we write must not look like a change.
    let changes: Vec<Change> = raw_changes
        .into_iter()
        .filter(|c| {
            !is_internal_path(&c.path)
                && !crate::config::path_excluded(&c.path, &cfg.analysis.exclude)
        })
        .collect();

    // Extra sources let the workspace parse both sides of a diff even when the
    // old/new side is not on disk (deleted files, revision scopes).
    let extra: Vec<(String, Option<String>, Option<String>)> = changes
        .iter()
        .map(|c| (c.path.clone(), c.old_content.clone(), c.new_content.clone()))
        .collect();

    let ws = crate::workspace::Workspace::build(repo, cfg, &mut *cache, &extra)?;
    let impact = ImpactEngine::new(&ws);
    let now = opts.generated_at.clone().unwrap_or_else(time::now_rfc3339);

    // -- tests: discover across the whole workspace (we parsed it anyway) -----
    let test_cfg = TestRunnerConfig::default();
    let parsed_pairs: Vec<(&str, &crate::lang::ParsedFile)> =
        ws.files.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let discovered = crate::tests::discover(&parsed_pairs, &test_cfg);
    let (test_evidence, test_runs) = crate::tests::collect(
        &discovered,
        opts.consent(),
        &test_cfg,
        exec,
        &now,
        repo.root(),
    );

    // -- ledger: one entry per changed symbol (or per file) -------------------
    let mut ledger: Vec<LedgerEntry> = Vec::new();
    for change in &changes {
        ledger.extend(ledger_for_change(&ws, &impact, change, cfg));
    }
    ledger.truncate(MAX_LEDGER_ENTRIES);

    // Attach test evidence to entries, and generate unknowns + suggestions.
    let mut unknowns: Vec<UnknownItem> = Vec::new();
    for entry in &mut ledger {
        attach_tests(
            &ws,
            &impact,
            entry,
            &test_evidence,
            &discovered,
            opts.consent(),
        );
        generate_suggestions(&ws, entry);
        collect_entry_unknowns(entry, &mut unknowns);
    }

    let history = if opts.history && cfg.history.enabled {
        let focus: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        build_history(repo, &focus, cfg).ok()
    } else {
        None
    };

    let mut report = Report {
        schema_version: crate::SCHEMA_VERSION.to_string(),
        reproducibility: Reproducibility {
            aftermath_version: crate::VERSION.to_string(),
            schema_version: crate::SCHEMA_VERSION.to_string(),
            repository_revision: repo.head_oid(),
            repository_dirty: repo.is_dirty(),
            config: cfg.summary_value(),
            language_support: default_language_support(),
            generated_at: now.clone(),
            commands_executed: test_runs.iter().map(|r| r.command.clone()).collect(),
            test_runs,
        },
        scope: ChangeScope {
            mode: opts.mode,
            revision: opts.revision.clone(),
            changed_files: changes.iter().map(|c| c.path.clone()).collect(),
        },
        ledger,
        tests: Vec::new(),
        unknowns,
        blind_spots: ws.blind_spots.clone(),
        history,
        summary: Summary::default(),
    };
    // Redact machine-specific paths from every human-readable field we filled.
    sanitize_report(&mut report, repo.root(), opts.home.as_deref());
    report.finalize();
    report.summary.changed_files = changes.len();
    Ok(report)
}

// ---------------------------------------------------------------------------
// Per-change ledger construction
// ---------------------------------------------------------------------------

fn ledger_for_change(
    ws: &crate::workspace::Workspace,
    impact: &ImpactEngine,
    change: &Change,
    _cfg: &Config,
) -> Vec<LedgerEntry> {
    if change.binary {
        return vec![file_level_entry(
            ws,
            change,
            Vec::new(),
            "binary file; contents not analyzed",
        )];
    }

    let new_parsed = ws.files.get(&change.path);
    let old_parsed = ws.old_files.get(&change.path);
    let symbols = changed_symbols(new_parsed, old_parsed, change);

    if symbols.is_empty() {
        // No declaration covers the change: report the change at file scope.
        return vec![file_level_entry(
            ws,
            change,
            Vec::new(),
            "no declaration located for the changed lines",
        )];
    }

    symbols
        .into_iter()
        .take(64)
        .map(|sym| {
            let (direct, transitive) = impact.affected(&sym, _cfg.analysis.depth);
            build_entry(change, Some(sym), &[], direct, transitive)
        })
        .collect()
}

/// Determine which declarations the diff actually touches.
///
/// Added files treat every declaration as new; deleted files use the old
/// side; modified files pick the innermost declarations covering a changed
/// line (falling back to old-side lines when only deletions occurred).
fn changed_symbols(
    new_parsed: Option<&crate::lang::ParsedFile>,
    old_parsed: Option<&crate::lang::ParsedFile>,
    change: &Change,
) -> Vec<SymbolRef> {
    match change.category {
        ChangeCategory::Added => new_parsed
            .map(|p| all_symbols(&change.path, p))
            .unwrap_or_default(),
        ChangeCategory::Deleted => old_parsed
            .map(|p| all_symbols(&change.path, p))
            .unwrap_or_default(),
        ChangeCategory::Modified | ChangeCategory::Renamed => {
            let mut out = innermost_symbols(new_parsed, &change.path, &change.new_lines);
            if out.is_empty() {
                // Only lines were removed; attribute through the old side.
                if let Some(old) = old_parsed {
                    for d in &old.decls {
                        if change
                            .old_lines
                            .iter()
                            .any(|l| d.start_line <= *l && *l <= d.end_line)
                        {
                            out.push(SymbolRef::new(&change.path, d.name.clone(), d.kind));
                        }
                    }
                }
            }
            out
        }
    }
}

fn all_symbols(file: &str, parsed: &crate::lang::ParsedFile) -> Vec<SymbolRef> {
    let mut v: Vec<SymbolRef> = parsed
        .decls
        .iter()
        .map(|d| SymbolRef::new(file, d.name.clone(), d.kind))
        .collect();
    v.sort_by(|a, b| (&a.name, a.kind).cmp(&(&b.name, b.kind)));
    v.dedup();
    v
}

/// Innermost declarations covering any of `lines` (smallest enclosing span).
fn innermost_symbols(
    parsed: Option<&crate::lang::ParsedFile>,
    file: &str,
    lines: &[u32],
) -> Vec<SymbolRef> {
    let Some(parsed) = parsed else {
        return Vec::new();
    };
    let mut best_for_line: BTreeMap<u32, SymbolRef> = BTreeMap::new();
    for &line in lines {
        let mut best: Option<&crate::lang::Decl> = None;
        for d in &parsed.decls {
            // Methods are recorded as `Class.method`; a changed line inside the
            // class body but outside any method still belongs to the class.
            if line >= d.start_line && line <= d.end_line {
                best = match best {
                    None => Some(d),
                    Some(b) if span(b) >= span(d) => Some(d),
                    keep => keep,
                };
            }
        }
        if let Some(b) = best {
            best_for_line.insert(line, SymbolRef::new(file, b.name.clone(), b.kind));
        }
    }
    let mut out: Vec<SymbolRef> = best_for_line.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup();
    out
}

fn span(d: &crate::lang::Decl) -> u32 {
    d.end_line.saturating_sub(d.start_line)
}

fn file_level_entry(
    _ws: &crate::workspace::Workspace,
    change: &Change,
    affected: Vec<AffectedSymbol>,
    note: &str,
) -> LedgerEntry {
    build_entry(change, None, &[], affected, Vec::new()).with_unverified(note)
}

fn build_entry(
    change: &Change,
    symbol: Option<SymbolRef>,
    methods: &[AnalysisMethod],
    direct: Vec<AffectedSymbol>,
    transitive: Vec<AffectedSymbol>,
) -> LedgerEntry {
    let id = symbol
        .as_ref()
        .map(|s| s.id())
        .unwrap_or_else(|| format!("{}::(file)", change.path));
    LedgerEntry {
        id,
        changed_file: change.path.clone(),
        changed_symbol: symbol,
        change_category: change.category,
        changed_lines: change.new_lines.clone(),
        direct_affected: direct,
        transitive_affected: transitive,
        methods: methods.to_vec(),
        confidence: Confidence::Unknown,
        tests: Vec::new(),
        unverified: Vec::new(),
        suggested_next_steps: Vec::new(),
        suggestions: Vec::new(),
    }
}

// Small ergonomic helper implemented via a trait so `build_entry` stays a plain fn.
trait WithUnverified {
    fn with_unverified(self, note: &str) -> Self;
}
impl WithUnverified for LedgerEntry {
    fn with_unverified(mut self, note: &str) -> Self {
        self.unverified.push(note.to_string());
        self
    }
}

// ---------------------------------------------------------------------------
// Tests, unknowns, suggestions
// ---------------------------------------------------------------------------

fn attach_tests(
    _ws: &crate::workspace::Workspace,
    impact: &ImpactEngine,
    entry: &mut LedgerEntry,
    all_tests: &[TestEvidence],
    discovered: &[DiscoveredTest],
    consent: Consent,
) {
    let mut methods = std::collections::BTreeSet::new();

    // Direct symbol-targeted evidence via the impact engine.
    if let Some(sym) = &entry.changed_symbol {
        for t in discovered {
            if let Some(ev) = impact.test_evidence(&t.file, sym) {
                if let Some(te) = all_tests.iter().find(|x| x.id == t.id) {
                    let mut te = te.clone();
                    te.symbol = Some(sym.clone());
                    if te.evidence.is_empty() {
                        te.evidence.push(ev.clone());
                    }
                    entry.tests.push(te);
                    methods.insert(AnalysisMethod::TestDiscovery);
                }
            }
        }
    }

    // Same-file tests (a change to a test file is evidence about itself).
    for t in all_tests {
        if t.file == entry.changed_file && !entry.tests.iter().any(|e| e.id == t.id) {
            entry.tests.push(t.clone());
            methods.insert(AnalysisMethod::TestDiscovery);
        }
    }

    // Entry-level verification state.
    let any_passed = entry
        .tests
        .iter()
        .any(|t| t.status == crate::evidence::TestStatus::Passed);
    if any_passed {
        entry.confidence = Confidence::VerifiedByExecutedTest;
    }

    if consent != Consent::NotGranted && entry.tests.iter().any(|t| t.id.contains("node --test")) {
        methods.insert(AnalysisMethod::TestExecution);
    }

    for m in methods {
        if !entry.methods.contains(&m) {
            entry.methods.push(m);
        }
    }

    // If we found no tests at all, say so without implying their absence.
    if entry.tests.is_empty() {
        entry.unverified.push(
            "no test referencing this symbol was identified by the available analysis \
             (this is not proof that none exists)"
                .to_string(),
        );
    }
}

fn collect_entry_unknowns(entry: &mut LedgerEntry, out: &mut Vec<UnknownItem>) {
    // Finalize per-entry confidence as the strongest impact found *unless* an
    // executed test already verified it (which is a stronger statement).
    let strongest = entry
        .direct_affected
        .iter()
        .chain(&entry.transitive_affected)
        .map(|a| a.confidence)
        .min_by_key(|c| c.impact_rank());

    if entry.confidence != Confidence::VerifiedByExecutedTest {
        entry.confidence = strongest.unwrap_or(Confidence::Unknown);
    }

    // Deterministic next steps (safe, non-destructive).
    if entry.confidence.is_impact() {
        entry.suggested_next_steps.push(
            "review the affected symbols listed above and run the project's test suite".to_string(),
        );
    } else if entry.changed_symbol.is_some() && entry.direct_affected.is_empty() {
        entry
            .suggested_next_steps
            .push("no callers/importers were resolved statically; consider a consumer-level integration test".to_string());
    }

    // Unknown: a changed public symbol with no identified test.
    for a in &entry.direct_affected {
        if a.confidence == Confidence::PossibleImpact {
            out.push(UnknownItem {
                id: format!("{}->{}:unconfirmed", entry.id, a.symbol.id()),
                kind: UnknownKind::UnresolvedCallSite,
                summary: format!(
                    "`{}` may be affected, but the link is a name match that could not be \
                     resolved to a declaration",
                    a.symbol.id()
                ),
                affected_symbol: Some(a.symbol.clone()),
                evidence: a.evidence.clone(),
                next_step: "confirm whether this reference resolves to the changed symbol"
                    .to_string(),
            });
        }
    }

    if entry.changed_symbol.is_some() && entry.tests.is_empty() {
        out.push(UnknownItem {
            id: format!("{}:no-identified-test", entry.id),
            kind: UnknownKind::ChangedPublicSymbolWithoutIdentifiedTest,
            summary: "no test referencing this changed symbol was identified".to_string(),
            affected_symbol: entry.changed_symbol.clone(),
            evidence: Vec::new(),
            next_step: "add or run a test that exercises this symbol".to_string(),
        });
    }

    // Unknown: tests identified but never executed (only when consent absent).
    let not_executed = entry
        .tests
        .iter()
        .any(|t| t.status == crate::evidence::TestStatus::NotExecuted);
    if not_executed {
        out.push(UnknownItem {
            id: format!("{}:tests-not-executed", entry.id),
            kind: UnknownKind::TestNotExecuted,
            summary: "tests were identified but not executed (no consent given)".to_string(),
            affected_symbol: entry.changed_symbol.clone(),
            evidence: Vec::new(),
            next_step: "re-run with test execution enabled to observe pass/fail results"
                .to_string(),
        });
    }
}

fn generate_suggestions(ws: &crate::workspace::Workspace, entry: &mut LedgerEntry) {
    let Some(sym) = entry.changed_symbol.as_ref() else {
        return;
    };
    let Some(parsed) = ws.files.get(&sym.file) else {
        return;
    };
    let Some(decl) = parsed.find_decl(&sym.name) else {
        return;
    };
    let old_decl = ws
        .old_files
        .get(&sym.file)
        .and_then(|p| p.find_decl(&sym.name));

    let mut suggestions: Vec<TestSuggestion> = Vec::new();
    let ev_line = EvidenceLocation::line(sym.file.clone(), decl.start_line);

    let mut push = |rule: SuggestionRule, title: &str, rationale: &str| {
        if suggestions.len() >= MAX_SUGGESTIONS_PER_ENTRY {
            return;
        }
        suggestions.push(TestSuggestion {
            id: format!("{}:{}", entry.id, title),
            title: title.to_string(),
            rationale: rationale.to_string(),
            rule,
            target_symbol: Some(sym.clone()),
            suggested_location: None,
            evidence: vec![Evidence::new(
                EvidenceKind::SyntaxTree,
                ev_line.clone(),
                "suggestion derived from the structure of the changed declaration",
            )],
            executed: false,
        });
    };

    if decl.has_numeric_param() {
        push(
            SuggestionRule::BoundaryValues,
            "boundary values",
            "the function takes a boundary-sensitive numeric value; probe 0, min, max, and \
             sign boundaries",
        );
    }
    if decl.branches >= 2 {
        push(
            SuggestionRule::BranchCoverage,
            "branch coverage",
            "the function contains multiple branches; ensure each condition is exercised",
        );
    }
    if decl.has_error_handling {
        push(
            SuggestionRule::ErrorPath,
            "error handling path",
            "the function has error handling; the failure path should be covered explicitly",
        );
    }
    let lname = decl.name.to_lowercase();
    if lname.contains("parse") || lname.contains("decode") || lname.contains("deserialize") {
        push(
            SuggestionRule::MalformedInput,
            "malformed input",
            "this looks like a parser/decoder; probe it with malformed and adversarial input",
        );
    }
    if decl.public_api {
        push(
            SuggestionRule::PublicApiConsumer,
            "public API consumer",
            "a public signature changed; a consumer-level test guards the contract",
        );
    }
    if count_direct_callers(entry) >= 2 {
        push(
            SuggestionRule::MultipleCallers,
            "multiple callers",
            "several call sites may hold different expectations of this symbol",
        );
    }
    if let Some(old) = old_decl {
        if old.params != decl.params {
            push(
                SuggestionRule::SignatureChange,
                "signature change",
                "the parameter list changed; callers and tests should be checked against the new shape",
            );
        }
    }

    entry.suggestions = suggestions;
}

fn count_direct_callers(entry: &LedgerEntry) -> usize {
    entry
        .direct_affected
        .iter()
        .filter(|a| a.relationship == Relationship::DirectCaller)
        .count()
}

// ---------------------------------------------------------------------------
// History (Feature F) — always correlation, never proof
// ---------------------------------------------------------------------------

fn build_history(repo: &GitRepo, focus: &[String], cfg: &Config) -> Result<HistoryReport, String> {
    let limit = cfg.history.limit;
    let commits = repo
        .recent_commits(limit)
        .map_err(|e| format!("history walk failed: {e}"))?;
    let truncated = commits.len() as u32 >= limit;

    let changed: std::collections::HashSet<&str> = focus.iter().map(|s| s.as_str()).collect();

    // Co-change: files that repeatedly appear in the same commits as a focus
    // file. Reported strictly as correlation.
    let mut pair_counts: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut relevant: Vec<HistoryCommit> = Vec::new();
    for commit in &commits {
        if commit.files.iter().any(|f| changed.contains(f.as_str())) {
            relevant.push(commit.clone());
        }
        for f in commit.files.iter().filter(|f| changed.contains(f.as_str())) {
            for other in &commit.files {
                if other == f {
                    continue;
                }
                let key = canonical_pair(f, other);
                *pair_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    // Merge each file's partners into one `CoChange` for readability.
    let mut merged: BTreeMap<String, CoChange> = BTreeMap::new();
    for ((a, b), n) in pair_counts {
        if !changed.contains(a.as_str()) || n < cfg.history.co_change_threshold {
            continue;
        }
        merged
            .entry(a.clone())
            .or_insert_with(|| CoChange {
                file: a.clone(),
                files: Vec::new(),
                shared_commits: 0,
            })
            .files
            .push(format!("{} ({}×)", b, n));
    }
    let co_changes: Vec<CoChange> = merged
        .into_values()
        .map(|mut c| {
            c.files.sort();
            c
        })
        .collect();

    Ok(HistoryReport {
        commits_examined: commits.len() as u32,
        truncated,
        co_changes,
        relevant_commits: relevant,
        disclaimer: HistoryReport::DISCLAIMER.to_string(),
    })
}

/// Standalone history analysis for the CLI `history` command: no change set is
/// required; the caller supplies the focus files.
pub fn history_report(
    repo: &GitRepo,
    cfg: &Config,
    focus: &[String],
) -> Result<HistoryReport, String> {
    build_history(repo, focus, cfg)
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// True for paths inside Aftermath's own working directory (`.aftermath/`),
/// which is machine-local state that must never be analyzed as a change.
fn is_internal_path(path: &str) -> bool {
    path == ".aftermath" || path.starts_with(".aftermath/")
}

// ---------------------------------------------------------------------------
// Redaction of human-readable report fields
// ---------------------------------------------------------------------------

fn sanitize_report(
    report: &mut Report,
    repo_root: &std::path::Path,
    home: Option<&std::path::Path>,
) {
    let fix = |s: &mut String| {
        let cleaned = crate::redact::sanitize(s, repo_root, home);
        *s = cleaned;
    };
    // Human-readable free-text fields that may contain machine paths.
    for entry in &mut report.ledger {
        entry.changed_file = crate::redact::sanitize(&entry.changed_file, repo_root, home);
        for u in &mut entry.unverified {
            fix(u);
        }
        for s in &mut entry.suggested_next_steps {
            fix(s);
        }
        for e in &mut entry.direct_affected {
            for ev in &mut e.evidence {
                fix(&mut ev.detail);
                if let Some(sn) = &mut ev.location.snippet {
                    fix(sn);
                }
            }
        }
        for e in entry.transitive_affected.iter_mut() {
            for ev in &mut e.evidence {
                fix(&mut ev.detail);
                if let Some(sn) = &mut ev.location.snippet {
                    fix(sn);
                }
            }
        }
    }
    for u in &mut report.unknowns {
        fix(&mut u.summary);
        fix(&mut u.next_step);
        for ev in &mut u.evidence {
            fix(&mut ev.detail);
        }
    }
    for b in &mut report.blind_spots {
        fix(&mut b.reason);
        fix(&mut b.impact);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::executor::LocalExecutor;
    use crate::gitrepo::GitRepo;

    fn fixture() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("src/math.ts", "export function add(a: number, b: number): number {\n  return a + b;\n}\n"),
            ("src/app.ts", "import { add } from './math';\n\nexport function run(): number {\n  return add(2, 3);\n}\n"),
            ("src/unused.ts", "export function helper() {\n  return 1;\n}\n"),
        ];
        let repo = crate::gitrepo::testkit::init_with(dir.path(), &files).unwrap();
        (dir, repo)
    }

    #[test]
    fn empty_scope_produces_empty_report() {
        let (_d, repo) = fixture();
        let cfg = Config::default();
        let mut cache = crate::cache::ParseCache::load(repo.root(), cfg.cache.enabled);
        let mut exec = LocalExecutor;
        let report = analyze(
            &repo,
            &cfg,
            &AnalysisOptions {
                generated_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
            &mut cache,
            &mut exec,
        )
        .unwrap();
        assert_eq!(report.summary.changed_files, 0);
    }

    #[test]
    fn modifying_add_identifies_caller() {
        let (dir, repo) = fixture();
        // Change add() to use division and re-run.
        std::fs::write(
            dir.path().join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a / b;\n}\n",
        )
        .unwrap();
        let cfg = Config::default();
        let mut cache = crate::cache::ParseCache::load(repo.root(), cfg.cache.enabled);
        let mut exec = LocalExecutor;
        let report = analyze(
            &repo,
            &cfg,
            &AnalysisOptions {
                mode: ScopeMode::Worktree,
                generated_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
            &mut cache,
            &mut exec,
        )
        .unwrap();

        assert_eq!(report.summary.changed_files, 1);
        let entry = report
            .ledger
            .iter()
            .find(|e| e.changed_symbol.as_ref().map(|s| s.name.as_str()) == Some("add"))
            .expect("add() must be located as a changed symbol");
        assert!(
            entry
                .direct_affected
                .iter()
                .any(|a| a.symbol.name == "run" && a.confidence.is_impact()),
            "run() calls add() and must be reported: {:#?}",
            entry.direct_affected
        );
        assert!(entry.confidence.is_impact());
    }

    #[test]
    fn no_consent_means_no_execution() {
        let (dir, repo) = fixture();
        std::fs::write(
            dir.path().join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a - b;\n}\n",
        )
        .unwrap();
        let cfg = Config::default();
        let mut cache = crate::cache::ParseCache::load(repo.root(), cfg.cache.enabled);
        let mut exec = LocalExecutor;
        let report = analyze(
            &repo,
            &cfg,
            &AnalysisOptions::default(),
            &mut cache,
            &mut exec,
        )
        .unwrap();
        assert!(report.reproducibility.test_runs.is_empty());
        assert_eq!(
            report.summary.tests_passed, 0,
            "no consent means no test ran"
        );
    }

    #[test]
    fn suggestions_include_boundary_for_numeric_fn() {
        let (dir, repo) = fixture();
        std::fs::write(
            dir.path().join("src/math.ts"),
            "export function add(a: number, b: number): number {\n  return a + b + 1;\n}\n",
        )
        .unwrap();
        let cfg = Config::default();
        let mut cache = crate::cache::ParseCache::load(repo.root(), cfg.cache.enabled);
        let mut exec = LocalExecutor;
        let report = analyze(
            &repo,
            &cfg,
            &AnalysisOptions::default(),
            &mut cache,
            &mut exec,
        )
        .unwrap();
        let entry = report
            .ledger
            .iter()
            .find(|e| e.changed_symbol.as_ref().map(|s| s.name.as_str()) == Some("add"))
            .unwrap();
        assert!(
            entry
                .suggestions
                .iter()
                .any(|s| s.rule == SuggestionRule::BoundaryValues),
            "numeric fn should get a boundary suggestion: {:#?}",
            entry.suggestions
        );
        // Suggestions are proposals, never executed.
        assert!(entry.suggestions.iter().all(|s| !s.executed));
    }
}
