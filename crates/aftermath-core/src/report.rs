//! Report envelope: the stable, machine-readable result of an analysis run,
//! plus deterministic JSON and Markdown renderers.
//!
//! Determinism rules applied by [`Report::finalize`]:
//! - every collection is sorted by a stable key;
//! - paths are repository-relative with `/` separators;
//! - the only intentionally varying field is `reproducibility.generated_at`,
//!   which [`Report::normalized`] removes for run-to-run comparison.

use serde::{Deserialize, Serialize};

use crate::evidence::{
    AffectedSymbol, BlindSpot, Confidence, EvidenceDiff, HistoryReport, LanguageSupportNote,
    LedgerEntry, Reproducibility, TestEvidence, TestSuggestion, UnknownItem,
};

/// Which state of the repository was analyzed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    /// Uncommitted working-tree changes (default).
    #[default]
    Worktree,
    /// Changes staged in the index.
    Staged,
    /// A specific commit.
    Revision,
}

impl std::fmt::Display for ScopeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ScopeMode::Worktree => "worktree",
            ScopeMode::Staged => "staged",
            ScopeMode::Revision => "revision",
        };
        f.write_str(s)
    }
}

/// What was analyzed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeScope {
    pub mode: ScopeMode,
    /// The commit whose changes were analyzed (for `ScopeMode::Revision`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Sorted repository-relative changed file paths.
    pub changed_files: Vec<String>,
}

/// Precomputed counts so consumers do not re-derive them inconsistently.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub changed_files: usize,
    pub ledger_entries: usize,
    pub direct_affected: usize,
    pub transitive_affected: usize,
    pub unknowns: usize,
    pub blind_spots: usize,
    pub tests_identified: usize,
    pub tests_passed: usize,
    pub tests_failed: usize,
    pub tests_not_executed: usize,
    pub tests_unavailable: usize,
    pub suggestions: usize,
    /// Entries whose strongest impact classification is confirmed.
    pub confirmed_entries: usize,
    /// Entries whose strongest impact classification is only possible.
    pub possible_entries: usize,
    /// Entries with no located impact at all (resolution failed or none found).
    pub unknown_entries: usize,
}

/// A complete analysis result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: String,
    pub reproducibility: Reproducibility,
    pub scope: ChangeScope,
    pub ledger: Vec<LedgerEntry>,
    /// Union of per-entry test evidence, deduplicated by (id, file).
    pub tests: Vec<TestEvidence>,
    pub unknowns: Vec<UnknownItem>,
    pub blind_spots: Vec<BlindSpot>,
    /// Present only when history analysis was requested and available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryReport>,
    pub summary: Summary,
}

impl Report {
    /// Sort all collections deterministically and recompute the summary.
    pub fn finalize(&mut self) {
        self.scope.changed_files.sort();
        self.scope.changed_files.dedup();
        self.ledger.sort_by(|a, b| a.id.cmp(&b.id));
        for entry in &mut self.ledger {
            sort_affected(&mut entry.direct_affected);
            sort_affected(&mut entry.transitive_affected);
            entry.methods.sort();
            entry
                .tests
                .sort_by(|a, b| (&a.file, &a.id).cmp(&(&b.file, &b.id)));
            entry.suggestions.sort_by(|a, b| a.id.cmp(&b.id));
            entry.changed_lines.sort_unstable();
            entry.changed_lines.dedup();
        }
        self.unknowns.sort_by(|a, b| a.id.cmp(&b.id));
        self.blind_spots.sort_by(|a, b| a.id.cmp(&b.id));
        if let Some(h) = &mut self.history {
            h.co_changes
                .sort_by(|a, b| (&a.file, &a.files).cmp(&(&b.file, &b.files)));
            for c in &mut h.co_changes {
                c.files.sort();
            }
            h.relevant_commits.sort_by(|a, b| a.id.cmp(&b.id));
        }

        // Deduplicated union of tests across entries.
        self.tests.clear();
        for entry in &self.ledger {
            for t in &entry.tests {
                if !self.tests.iter().any(|x| x.id == t.id && x.file == t.file) {
                    self.tests.push(t.clone());
                }
            }
        }
        self.tests
            .sort_by(|a, b| (&a.file, &a.id).cmp(&(&b.file, &b.id)));

        self.summary = self.compute_summary();
    }

    fn compute_summary(&self) -> Summary {
        let mut s = Summary {
            changed_files: self.scope.changed_files.len(),
            ledger_entries: self.ledger.len(),
            unknowns: self.unknowns.len(),
            blind_spots: self.blind_spots.len(),
            tests_identified: self.tests.len(),
            suggestions: self.ledger.iter().map(|e| e.suggestions.len()).sum(),
            ..Summary::default()
        };
        for entry in &self.ledger {
            s.direct_affected += entry.direct_affected.len();
            s.transitive_affected += entry.transitive_affected.len();
            match entry.confidence {
                Confidence::ConfirmedRelationship | Confidence::StronglySupportedImpact => {
                    s.confirmed_entries += 1
                }
                Confidence::PossibleImpact => s.possible_entries += 1,
                _ => s.unknown_entries += 1,
            }
        }
        for t in &self.tests {
            match t.status {
                crate::evidence::TestStatus::Passed => s.tests_passed += 1,
                crate::evidence::TestStatus::Failed => s.tests_failed += 1,
                crate::evidence::TestStatus::NotExecuted => s.tests_not_executed += 1,
                crate::evidence::TestStatus::Unavailable => s.tests_unavailable += 1,
                crate::evidence::TestStatus::Skipped => {}
            }
        }
        s
    }

    /// Copy with the timestamp removed, for reproducibility comparisons.
    pub fn normalized(&self) -> Report {
        let mut r = self.clone();
        r.reproducibility.generated_at = String::new();
        r
    }

    /// Pretty-printed, stable JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| {
            // Serialization of these types cannot fail; fall back loudly but safely.
            format!("{{\"error\":\"serialization failed: {}\"}}", e)
        })
    }

    /// Compact JSON (single line), used where size matters.
    fn to_json_compact(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Human-facing terminal view (the default for `aftermath inspect`).
    ///
    /// Deterministic: the same report always renders the same text, with or
    /// without color. Color is decoration only; it never carries meaning that
    /// the words do not.
    pub fn to_terminal(&self, color: bool) -> String {
        let t = Theme::new(color);
        let mut out = String::new();
        let r = &self.reproducibility;

        out.push_str(&t.head("Aftermath — change consequence ledger"));
        out.push('\n');
        out.push_str(&format!(
            "  {} {} · {} change · {} file(s) · {}\n",
            t.dim("aftermath"),
            r.aftermath_version,
            self.scope.mode,
            self.scope.changed_files.len(),
            r.generated_at
        ));
        match &r.repository_revision {
            Some(rev) => out.push_str(&format!(
                "  {} {} {}\n",
                t.dim("revision"),
                &rev[..rev.len().min(12)],
                if r.repository_dirty {
                    t.dim("(uncommitted changes present)")
                } else {
                    String::new()
                }
            )),
            None => out.push_str(&format!(
                "  {} unavailable (no commits yet)\n",
                t.dim("revision")
            )),
        }
        out.push('\n');

        // Summary line.
        let s = &self.summary;
        out.push_str(&t.subhead("Summary"));
        out.push('\n');
        out.push_str(&format!(
            "  {} file · {} symbol(s) · {} affected · {} unknown · {} blind spot(s) · {} test(s)\n",
            s.changed_files,
            s.ledger_entries,
            s.direct_affected + s.transitive_affected,
            s.unknowns,
            s.blind_spots,
            s.tests_identified,
        ));
        out.push('\n');

        // Ledger.
        out.push_str(&t.subhead("What changed and what it touches"));
        out.push('\n');
        if self.ledger.is_empty() {
            out.push_str(&format!(
                "  {}\n",
                t.dim("No changed declaration was located in the analyzed scope.")
            ));
        }
        for entry in &self.ledger {
            out.push_str(&format!(
                "  {} {}\n",
                t.bold(&entry.id),
                t.confidence(entry.confidence)
            ));
            let sym_desc = match &entry.changed_symbol {
                Some(sym) => format!("{} ({})", sym.name, sym.kind),
                None => "no declaration located in this file".to_string(),
            };
            out.push_str(&format!(
                "    change: {} — {}\n",
                entry.change_category, sym_desc
            ));
            if !entry.changed_lines.is_empty() {
                out.push_str(&format!(
                    "    changed lines: {}\n",
                    join_ranges(&entry.changed_lines)
                ));
            }
            for a in &entry.direct_affected {
                out.push_str(&format!(
                    "    → {} {} {}\n",
                    t.cyan(&format!("{}", a.relationship)),
                    a.symbol.id(),
                    t.confidence(a.confidence)
                ));
            }
            for a in &entry.transitive_affected {
                let chain = a
                    .chain
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>()
                    .join(" → ");
                out.push_str(&format!(
                    "    ⇢ {} {} (depth {}, {}) {}\n",
                    t.dim(&format!("{}", a.relationship)),
                    a.symbol.id(),
                    a.depth,
                    a.confidence.label(),
                    t.dim(&format!("[{}]", chain))
                ));
            }
            if entry.direct_affected.is_empty() && entry.transitive_affected.is_empty() {
                out.push_str(&format!(
                    "    {}\n",
                    t.dim("no affected symbols identified by the available analysis")
                ));
            }
            out.push('\n');
        }

        // Unknowns.
        out.push_str(&t.subhead("What we don't know"));
        out.push('\n');
        if self.unknowns.is_empty() {
            out.push_str(&format!(
                "  {}\n",
                t.dim("No unknowns were recorded for this scope.")
            ));
        } else {
            for u in &self.unknowns {
                out.push_str(&format!("  • {}\n", u.summary));
                out.push_str(&format!("    {} {}\n", t.dim("next →"), u.next_step));
            }
        }
        out.push('\n');

        // Blind spots.
        if !self.blind_spots.is_empty() {
            out.push_str(&t.subhead("Analysis blind spots"));
            out.push('\n');
            for b in &self.blind_spots {
                out.push_str(&format!("  • {} — {}\n", b.kind, b.reason));
            }
            out.push('\n');
        }

        // Tests.
        out.push_str(&t.subhead("Test evidence"));
        out.push('\n');
        if self.tests.is_empty() {
            out.push_str(&format!(
                "  {}\n",
                t.dim("No relevant test was identified by the available analysis.")
            ));
        } else {
            for te in &self.tests {
                out.push_str(&format!(
                    "  · {} {} {}\n",
                    te.id,
                    t.dim(&format!("({})", te.file)),
                    t.status(te.status)
                ));
            }
        }
        if !r.test_runs.is_empty() {
            out.push_str(&format!(
                "\n  {} executed run(s) recorded.\n",
                r.test_runs.len()
            ));
        }
        out.push('\n');

        // Suggestions.
        let sugg: Vec<&TestSuggestion> = self
            .ledger
            .iter()
            .flat_map(|e| e.suggestions.iter())
            .collect();
        out.push_str(&t.subhead("Suggested next tests (proposals, not executed)"));
        out.push('\n');
        if sugg.is_empty() {
            out.push_str(&format!(
                "  {}\n",
                t.dim("No structural suggestion matched this change.")
            ));
        } else {
            for s in sugg {
                out.push_str(&format!("  • [{}] {}\n", s.rule, s.title));
                out.push_str(&format!("      {}\n", t.dim(&s.rationale)));
            }
        }
        out.push('\n');

        if let Some(h) = &self.history {
            out.push_str(&t.subhead("Repository archaeology (correlation, not proof)"));
            out.push('\n');
            out.push_str(&format!("  {} commit(s) examined.\n", h.commits_examined));
            for c in &h.co_changes {
                out.push_str(&format!(
                    "  · `{}` with {} ({} shared commits)\n",
                    c.file,
                    c.files
                        .iter()
                        .map(|f| format!("`{}`", f))
                        .collect::<Vec<_>>()
                        .join(", "),
                    c.shared_commits
                ));
            }
            out.push_str(&format!("  {}\n\n", t.dim(&h.disclaimer)));
        }

        out
    }

    /// Deterministic Markdown report (Feature G's detailed view).
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let r = &self.reproducibility;
        out.push_str("# Aftermath analysis report\n\n");
        out.push_str(&format!("- **Schema:** {}\n", self.schema_version));
        out.push_str(&format!("- **Aftermath:** {}\n", r.aftermath_version));
        match &r.repository_revision {
            Some(rev) => out.push_str(&format!(
                "- **Revision:** `{}`{}\n",
                &rev[..rev.len().min(12)],
                if r.repository_dirty {
                    " (uncommitted changes present)"
                } else {
                    ""
                }
            )),
            None => out.push_str("- **Revision:** unavailable (no commits yet)\n"),
        }
        out.push_str(&format!(
            "- **Scope:** {} change, {} file(s)\n",
            self.scope.mode,
            self.scope.changed_files.len()
        ));
        out.push_str(&format!("- **Generated:** {}\n", r.generated_at));
        out.push_str(&format!("- **Tests executed:** {}\n\n", r.test_runs.len()));

        out.push_str("## Summary\n\n");
        out.push_str("| Measure | Count |\n|---|---:|\n");
        out.push_str(&format!(
            "| Changed files | {} |\n",
            self.summary.changed_files
        ));
        out.push_str(&format!(
            "| Ledger entries | {} |\n",
            self.summary.ledger_entries
        ));
        out.push_str(&format!(
            "| Directly affected symbols | {} |\n",
            self.summary.direct_affected
        ));
        out.push_str(&format!(
            "| Transitively affected symbols | {} |\n",
            self.summary.transitive_affected
        ));
        out.push_str(&format!(
            "| Unknowns reported | {} |\n",
            self.summary.unknowns
        ));
        out.push_str(&format!(
            "| Analysis blind spots | {} |\n",
            self.summary.blind_spots
        ));
        out.push_str(&format!(
            "| Tests identified | {} |\n",
            self.summary.tests_identified
        ));
        out.push_str(&format!(
            "| Suggestions (not executed) | {} |\n\n",
            self.summary.suggestions
        ));

        out.push_str("## What we know\n\n");
        if self.ledger.is_empty() {
            out.push_str("No changed declaration was located in the analyzed scope.\n\n");
        }
        for entry in &self.ledger {
            out.push_str(&format!("### `{}`\n\n", entry.id));
            out.push_str(&format!("- Change: **{}**", entry.change_category));
            if let Some(sym) = &entry.changed_symbol {
                out.push_str(&format!(", on `{}` ({})", sym.name, sym.kind));
            } else {
                out.push_str(", no declaration located in this file");
            }
            out.push('\n');
            if !entry.changed_lines.is_empty() {
                out.push_str(&format!(
                    "- Changed lines: {}\n",
                    join_ranges(&entry.changed_lines)
                ));
            }
            out.push_str(&format!("- Confidence: **{}**\n", entry.confidence.label()));
            out.push_str(&format!(
                "- Methods: {}\n",
                entry
                    .methods
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));

            if !entry.direct_affected.is_empty() {
                out.push_str("- Directly affected:\n");
                for a in &entry.direct_affected {
                    out.push_str(&format!(
                        "  - {} `{}` — {} ({})\n",
                        a.relationship,
                        a.symbol.id(),
                        a.confidence.label(),
                        a.evidence
                            .iter()
                            .map(|e| format!(
                                "{} at {}:{}",
                                e.kind, e.location.file, e.location.line
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
            }
            if !entry.transitive_affected.is_empty() {
                out.push_str("- Transitively affected:\n");
                for a in &entry.transitive_affected {
                    let chain = a
                        .chain
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                        .join(" → ");
                    out.push_str(&format!(
                        "  - {} `{}` depth {} ({})\n",
                        a.relationship,
                        a.symbol.id(),
                        a.depth,
                        a.confidence.label()
                    ));
                    if !a.chain.is_empty() {
                        out.push_str(&format!("    - chain: {}\n", chain));
                    }
                }
            }
            if entry.direct_affected.is_empty() && entry.transitive_affected.is_empty() {
                out.push_str(
                    "- Affected symbols: none identified by the available analysis \
                     (this is not proof that nothing depends on it).\n",
                );
            }
            out.push('\n');
        }

        out.push_str("## What we don't know\n\n");
        if self.unknowns.is_empty() {
            out.push_str("No unknowns were recorded for this scope.\n\n");
        } else {
            for u in &self.unknowns {
                out.push_str(&format!("- **{}** — {}\n", u.kind, u.summary));
                out.push_str(&format!("  - next: {}\n", u.next_step));
                if let Some(sym) = &u.affected_symbol {
                    out.push_str(&format!("  - symbol: `{}`\n", sym.id()));
                }
            }
            out.push('\n');
        }

        if !self.blind_spots.is_empty() {
            out.push_str("## Analysis blind spots\n\n");
            for b in &self.blind_spots {
                out.push_str(&format!("- **{}**", b.kind));
                if let Some(f) = &b.file {
                    out.push_str(&format!(" — `{}`", f));
                }
                out.push_str(&format!(
                    "\n  - why: {}\n  - impact: {}\n",
                    b.reason, b.impact
                ));
            }
            out.push('\n');
        }

        out.push_str("## Test evidence\n\n");
        if self.tests.is_empty() {
            out.push_str(
                "No relevant test was identified by the available analysis for the changed symbols.\n\n",
            );
        } else {
            out.push_str("| Test | File | Status |\n|---|---|---|\n");
            for t in &self.tests {
                out.push_str(&format!("| `{}` | `{}` | {} |\n", t.id, t.file, t.status));
            }
            out.push('\n');
        }
        if !r.test_runs.is_empty() {
            out.push_str("### Executed runs\n\n");
            for run in &r.test_runs {
                out.push_str(&format!(
                    "- `{}` — {}{}\n",
                    run.command.join(" "),
                    run.status,
                    run.exit_code
                        .map(|c| format!(" (exit {})", c))
                        .unwrap_or_default()
                ));
            }
            out.push('\n');
        }

        let any_suggestions: Vec<&TestSuggestion> = self
            .ledger
            .iter()
            .flat_map(|e| e.suggestions.iter())
            .collect();
        out.push_str("## Suggested next tests\n\n");
        if any_suggestions.is_empty() {
            out.push_str("No structural suggestion matched this change.\n\n");
        } else {
            for s in any_suggestions {
                out.push_str(&format!(
                    "- **[{}]** {} — {}\n",
                    s.rule, s.title, s.rationale
                ));
                out.push_str("  - status: not executed (proposal only)\n");
            }
            out.push('\n');
        }

        if let Some(h) = &self.history {
            out.push_str("## Repository archaeology\n\n");
            out.push_str(&format!("{} commits examined.\n\n", h.commits_examined));
            if !h.co_changes.is_empty() {
                out.push_str("Files that repeatedly changed together:\n\n");
                for c in &h.co_changes {
                    out.push_str(&format!(
                        "- `{}` with {} ({} shared commits)\n",
                        c.file,
                        c.files
                            .iter()
                            .map(|f| format!("`{}`", f))
                            .collect::<Vec<_>>()
                            .join(", "),
                        c.shared_commits
                    ));
                }
                out.push('\n');
            }
            out.push_str(&format!("> {}\n\n", h.disclaimer));
        }

        out.push_str("## Test runs\n\n");
        if r.test_runs.is_empty() {
            out.push_str("No tests were executed by Aftermath in this run.\n\n");
        }

        out.push_str("## Reproducibility\n\n");
        out.push_str("### Language support and limitations\n\n");
        for lang in &r.language_support {
            out.push_str(&format!(
                "- **{}** ({})\n",
                lang.language,
                lang.extensions.join(", ")
            ));
            out.push_str(&format!("  - supports: {}\n", lang.supported.join("; ")));
            out.push_str(&format!(
                "  - limitations: {}\n",
                lang.limitations.join("; ")
            ));
        }
        out.push('\n');
        out.push_str("### Configuration used\n\n```json\n");
        out.push_str(&serde_json::to_string_pretty(&r.config).unwrap_or_else(|_| "{}".into()));
        out.push_str("\n```\n\n");
        if !r.commands_executed.is_empty() {
            out.push_str("### Commands executed by Aftermath\n\n");
            for c in &r.commands_executed {
                out.push_str(&format!("- `{}`\n", c.join(" ")));
            }
            out.push('\n');
        }
        out
    }

    /// Compare two serialized reports (Feature G entry point).
    pub fn diff_against(&self, before: &Report) -> EvidenceDiff {
        crate::evidencediff::compute_diff(before, self)
    }
}

fn sort_affected(v: &mut [AffectedSymbol]) {
    v.sort_by(|a, b| {
        (a.depth, &a.symbol.file, &a.symbol.name).cmp(&(b.depth, &b.symbol.file, &b.symbol.name))
    });
}

/// Render sorted 1-based line numbers as compact ranges (`12-18, 30`).
pub fn join_ranges(lines: &[u32]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = lines[0];
    let mut prev = lines[0];
    for &l in &lines[1..] {
        if l == prev + 1 {
            prev = l;
            continue;
        }
        parts.push(if start == prev {
            start.to_string()
        } else {
            format!("{}-{}", start, prev)
        });
        start = l;
        prev = l;
    }
    parts.push(if start == prev {
        start.to_string()
    } else {
        format!("{}-{}", start, prev)
    });
    parts.join(", ")
}

/// ANSI decoration for the terminal view. Everything degrades to plain text
/// when color is disabled, so the rendered report is byte-identical either way.
struct Theme {
    color: bool,
}

impl Theme {
    fn new(color: bool) -> Self {
        Self { color }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    fn head(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1;4m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn subhead(&self, s: &str) -> String {
        self.bold(s)
    }
    fn confidence(&self, c: Confidence) -> String {
        // Color conveys the same information as the label, never more.
        let label = c.label();
        let code = match c {
            Confidence::ConfirmedRelationship => "32",   // green
            Confidence::StronglySupportedImpact => "32", // green
            Confidence::VerifiedByExecutedTest => "32",  // green
            Confidence::PossibleImpact => "33",          // yellow
            Confidence::Unknown => "34",                 // blue (unknown ≠ negative)
            Confidence::NotVerified => "33",             // yellow
        };
        self.wrap(code, &format!("[{label}]"))
    }
    fn status(&self, s: crate::evidence::TestStatus) -> String {
        use crate::evidence::TestStatus;
        let code = match s {
            TestStatus::Passed => "32",
            TestStatus::Failed => "31",
            TestStatus::NotExecuted => "33",
            TestStatus::Unavailable => "33",
            TestStatus::Skipped => "34",
        };
        self.wrap(code, &format!("({s})"))
    }
}

/// Whether two reports are identical apart from the timestamp.
pub fn reports_equivalent(a: &Report, b: &Report) -> bool {
    a.normalized().to_json_compact() == b.normalized().to_json_compact()
}

/// Shared fixture used by module tests elsewhere in the crate.
#[cfg(test)]
pub(crate) fn sample_report_for_tests() -> Report {
    use crate::evidence::*;
    Report {
        schema_version: crate::SCHEMA_VERSION.into(),
        reproducibility: Reproducibility {
            aftermath_version: crate::VERSION.into(),
            schema_version: crate::SCHEMA_VERSION.into(),
            repository_revision: Some("0123456789abcdef0123456789abcdef01234567".into()),
            repository_dirty: false,
            config: serde_json::json!({}),
            language_support: default_language_support(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            commands_executed: vec![],
            test_runs: vec![],
        },
        scope: ChangeScope {
            mode: ScopeMode::Worktree,
            revision: None,
            changed_files: vec!["src/a.ts".into()],
        },
        ledger: vec![LedgerEntry {
            id: "src/a.ts::parse".into(),
            changed_file: "src/a.ts".into(),
            changed_symbol: Some(SymbolRef::new("src/a.ts", "parse", SymbolKind::Function)),
            change_category: ChangeCategory::Modified,
            changed_lines: vec![2],
            direct_affected: vec![AffectedSymbol {
                symbol: SymbolRef::new("src/b.ts", "run", SymbolKind::Function),
                relationship: Relationship::DirectCaller,
                depth: 1,
                confidence: Confidence::ConfirmedRelationship,
                evidence: vec![Evidence::new(
                    EvidenceKind::CallSite,
                    EvidenceLocation::line("src/b.ts", 1),
                    "call to parse()",
                )],
                chain: vec![],
            }],
            transitive_affected: vec![],
            methods: vec![AnalysisMethod::SyntaxAnalysis],
            confidence: Confidence::ConfirmedRelationship,
            tests: vec![TestEvidence {
                id: "parses".into(),
                file: "tests/a.test.ts".into(),
                symbol: None,
                status: TestStatus::NotExecuted,
                evidence: vec![],
                note: None,
            }],
            unverified: vec![],
            suggested_next_steps: vec![],
            suggestions: vec![],
        }],
        tests: vec![],
        unknowns: vec![],
        blind_spots: vec![],
        history: None,
        summary: Summary::default(),
    }
}

/// Static language-support notes (single source of truth for Feature H).
pub fn default_language_support() -> Vec<LanguageSupportNote> {
    vec![LanguageSupportNote {
        language: "JavaScript / TypeScript".into(),
        extensions: vec![".js".into(), ".jsx".into(), ".mjs".into(), ".cjs".into(), ".ts".into(), ".tsx".into()],
        supported: vec![
            "function, method, class, interface, type and variable declarations".into(),
            "call expressions, including method calls".into(),
            "import/export relationships, including aliases and re-exports".into(),
            "extends/implements relationships".into(),
            "dynamic import() detection (recorded as an analysis blind spot)".into(),
            "test file discovery by naming convention".into(),
        ],
        limitations: vec![
            "no type checking: member calls are name-matched, never assumed to be the same symbol".into(),
            "runtime dispatch (eval, reflection, computed property access) blocks resolution and is reported as unknown".into(),
            "call resolution is per-module; similarly named functions in unrelated modules are only 'possible impact'".into(),
            "syntax-level analysis only; macro expansion, generated runtime wiring and framework inversion-of-control are not followed".into(),
        ],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::*;

    fn sample_report() -> Report {
        let mut entry = LedgerEntry {
            id: "src/a.ts::parse".into(),
            changed_file: "src/a.ts".into(),
            changed_symbol: Some(SymbolRef::new("src/a.ts", "parse", SymbolKind::Function)),
            change_category: ChangeCategory::Modified,
            changed_lines: vec![14, 15, 16, 20],
            direct_affected: vec![AffectedSymbol {
                symbol: SymbolRef::new("src/b.ts", "run", SymbolKind::Function),
                relationship: Relationship::DirectCaller,
                depth: 1,
                confidence: Confidence::ConfirmedRelationship,
                evidence: vec![Evidence::new(
                    EvidenceKind::CallSite,
                    EvidenceLocation::line("src/b.ts", 4),
                    "call to parse()",
                )],
                chain: vec![],
            }],
            transitive_affected: vec![],
            methods: vec![AnalysisMethod::CallSiteScan, AnalysisMethod::SyntaxAnalysis],
            confidence: Confidence::ConfirmedRelationship,
            tests: vec![TestEvidence {
                id: "parses input".into(),
                file: "tests/a.test.ts".into(),
                symbol: None,
                status: TestStatus::NotExecuted,
                evidence: vec![],
                note: None,
            }],
            unverified: vec!["error branch behavior".into()],
            suggested_next_steps: vec!["run the test suite".into()],
            suggestions: vec![],
        };
        entry.direct_affected.sort_by_key(|a| a.depth);
        Report {
            schema_version: crate::SCHEMA_VERSION.into(),
            reproducibility: Reproducibility {
                aftermath_version: crate::VERSION.into(),
                schema_version: crate::SCHEMA_VERSION.into(),
                repository_revision: Some("0123456789abcdef0123456789abcdef01234567".into()),
                repository_dirty: true,
                config: serde_json::json!({"depth": 3}),
                language_support: default_language_support(),
                generated_at: "2026-01-01T00:00:00Z".into(),
                commands_executed: vec![],
                test_runs: vec![],
            },
            scope: ChangeScope {
                mode: ScopeMode::Worktree,
                revision: None,
                changed_files: vec!["src/a.ts".into()],
            },
            ledger: vec![entry],
            tests: vec![],
            unknowns: vec![],
            blind_spots: vec![],
            history: None,
            summary: Summary::default(),
        }
    }

    #[test]
    fn finalize_computes_summary_and_sorts() {
        let mut r = sample_report();
        r.finalize();
        assert_eq!(r.summary.ledger_entries, 1);
        assert_eq!(r.summary.direct_affected, 1);
        assert_eq!(r.summary.confirmed_entries, 1);
        assert_eq!(r.summary.tests_identified, 1);
        assert_eq!(r.summary.tests_not_executed, 1);
        assert_eq!(r.ledger[0].changed_lines, vec![14, 15, 16, 20]);
    }

    #[test]
    fn json_output_is_deterministic() {
        let mut a = sample_report();
        a.finalize();
        let mut b = sample_report();
        b.finalize();
        assert_eq!(a.to_json(), b.to_json());
        assert!(reports_equivalent(&a, &b));
    }

    #[test]
    fn normalized_masks_only_timestamp() {
        let mut a = sample_report();
        a.finalize();
        let mut b = a.clone();
        b.reproducibility.generated_at = "2030-01-01T00:00:00Z".into();
        assert!(
            reports_equivalent(&a, &b),
            "timestamp must not affect comparison"
        );
    }

    #[test]
    fn markdown_contains_required_sections() {
        let mut r = sample_report();
        r.finalize();
        let md = r.to_markdown();
        assert!(md.contains("# Aftermath analysis report"));
        assert!(md.contains("## What we know"));
        assert!(md.contains("## What we don't know"));
        assert!(md.contains("## Test evidence"));
        assert!(md.contains("## Suggested next tests"));
        assert!(md.contains("No structural suggestion matched this change."));
        assert!(md.contains("src/a.ts::parse"));
    }

    #[test]
    fn line_ranges_render_compactly() {
        assert_eq!(join_ranges(&[1, 2, 3, 7, 9, 10]), "1-3, 7, 9-10");
        assert_eq!(join_ranges(&[5]), "5");
        assert_eq!(join_ranges(&[]), "");
    }
}
