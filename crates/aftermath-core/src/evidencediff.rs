//! The Evidence Diff (Feature G): compare *evidence*, not text.
//!
//! Given two reports produced at different times (typically `before` and `after`
//! a change), compute which relationships, tests, unknowns and blind spots were
//! gained or lost. Removed evidence is never described as a regression.

use crate::evidence::{
    Confidence, EvidenceDiff, RelationshipSummary, TestStatusChange, TestSummary,
};
use crate::report::Report;

pub fn compute_diff(before: &Report, after: &Report) -> EvidenceDiff {
    let before_rel = relationships(before);
    let after_rel = relationships(after);

    let mut relationships_added = Vec::new();
    let mut relationships_removed = Vec::new();
    for (from, to, conf) in &after_rel {
        if !before_rel.iter().any(|(f, t, _)| f == from && t == to) {
            relationships_added.push(RelationshipSummary {
                from: from.clone(),
                to: to.clone(),
                confidence: *conf,
            });
        }
    }
    for (from, to, conf) in &before_rel {
        if !after_rel.iter().any(|(f, t, _)| f == from && t == to) {
            relationships_removed.push(RelationshipSummary {
                from: from.clone(),
                to: to.clone(),
                confidence: *conf,
            });
        }
    }

    let before_tests = tests(before);
    let after_tests = tests(after);
    let mut tests_added = Vec::new();
    let mut tests_removed = Vec::new();
    let mut tests_status_changed = Vec::new();
    for t in &after_tests {
        match before_tests
            .iter()
            .find(|b| b.id == t.id && b.file == t.file)
        {
            None => tests_added.push(t.clone()),
            Some(b) if b.status != t.status => tests_status_changed.push(TestStatusChange {
                id: t.id.clone(),
                file: t.file.clone(),
                before: b.status,
                after: t.status,
            }),
            Some(_) => {}
        }
    }
    for t in &before_tests {
        if !after_tests.iter().any(|a| a.id == t.id && a.file == t.file) {
            tests_removed.push(t.clone());
        }
    }

    let before_unknowns: Vec<String> = before.unknowns.iter().map(|u| u.id.clone()).collect();
    let after_unknowns: Vec<String> = after.unknowns.iter().map(|u| u.id.clone()).collect();
    let before_blind: Vec<String> = before.blind_spots.iter().map(|b| b.id.clone()).collect();
    let after_blind: Vec<String> = after.blind_spots.iter().map(|b| b.id.clone()).collect();

    EvidenceDiff {
        before_revision: before.reproducibility.repository_revision.clone(),
        after_revision: after.reproducibility.repository_revision.clone(),
        relationships_added: sorted(relationships_added),
        relationships_removed: sorted(relationships_removed),
        tests_added: sorted(tests_added),
        tests_removed: sorted(tests_removed),
        tests_status_changed: sorted(tests_status_changed),
        unknowns_added: set_diff(&after_unknowns, &before_unknowns),
        unknowns_removed: set_diff(&before_unknowns, &after_unknowns),
        blindspots_added: set_diff(&after_blind, &before_blind),
        blindspots_removed: set_diff(&before_blind, &after_blind),
        disclaimer: crate::evidence::EvidenceDiff::DISCLAIMER.to_string(),
    }
}

fn relationships(report: &Report) -> Vec<(String, String, Confidence)> {
    let mut out = Vec::new();
    for e in &report.ledger {
        for a in e.direct_affected.iter().chain(&e.transitive_affected) {
            out.push((e.id.clone(), a.symbol.id(), a.confidence));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn tests(report: &Report) -> Vec<TestSummary> {
    report
        .tests
        .iter()
        .map(|t| TestSummary {
            id: t.id.clone(),
            file: t.file.clone(),
            status: t.status,
        })
        .collect()
}

fn set_diff(a: &[String], b: &[String]) -> Vec<String> {
    let mut out: Vec<String> = a.iter().filter(|x| !b.contains(x)).cloned().collect();
    out.sort();
    out.dedup();
    out
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v.dedup();
    v
}

/// Concise terminal view of an evidence diff.
pub fn render_terminal(diff: &EvidenceDiff) -> String {
    let mut out = String::new();
    out.push_str("Evidence diff\n");
    out.push_str(&format!(
        "  before: {}\n  after:  {}\n\n",
        rev_label(&diff.before_revision),
        rev_label(&diff.after_revision)
    ));

    section(
        &mut out,
        "Relationships",
        diff.relationships_added.len(),
        diff.relationships_removed.len(),
    );
    for r in &diff.relationships_added {
        out.push_str(&format!(
            "  + {} -> {} ({})\n",
            r.from,
            r.to,
            r.confidence.label()
        ));
    }
    for r in &diff.relationships_removed {
        out.push_str(&format!(
            "  - {} -> {} ({})\n",
            r.from,
            r.to,
            r.confidence.label()
        ));
    }

    section(
        &mut out,
        "Tests",
        diff.tests_added.len(),
        diff.tests_removed.len(),
    );
    for t in &diff.tests_added {
        out.push_str(&format!("  + {} [{}]\n", t.id, t.status));
    }
    for t in &diff.tests_removed {
        out.push_str(&format!("  - {} [{}]\n", t.id, t.status));
    }
    for c in &diff.tests_status_changed {
        out.push_str(&format!("  ~ {} {} -> {}\n", c.id, c.before, c.after));
    }

    section(
        &mut out,
        "Unknowns",
        diff.unknowns_added.len(),
        diff.unknowns_removed.len(),
    );
    for u in &diff.unknowns_added {
        out.push_str(&format!("  + {}\n", u));
    }
    for u in &diff.unknowns_removed {
        out.push_str(&format!("  - {}\n", u));
    }

    section(
        &mut out,
        "Blind spots",
        diff.blindspots_added.len(),
        diff.blindspots_removed.len(),
    );
    for b in &diff.blindspots_added {
        out.push_str(&format!("  + {}\n", b));
    }
    for b in &diff.blindspots_removed {
        out.push_str(&format!("  - {}\n", b));
    }

    out.push_str(&format!("\n{}\n", diff.disclaimer));
    out
}

fn section(out: &mut String, name: &str, added: usize, removed: usize) {
    out.push_str(&format!("{} (+{} / -{})\n", name, added, removed));
}

fn rev_label(rev: &Option<String>) -> String {
    match rev {
        Some(r) => r[..r.len().min(12)].to_string(),
        None => "unavailable".to_string(),
    }
}

/// Detailed Markdown view of an evidence diff.
pub fn render_markdown(diff: &EvidenceDiff) -> String {
    let mut out = String::new();
    out.push_str("# Evidence diff\n\n");
    out.push_str(&format!(
        "- before: `{}`\n",
        rev_label(&diff.before_revision)
    ));
    out.push_str(&format!(
        "- after: `{}`\n\n",
        rev_label(&diff.after_revision)
    ));

    out.push_str("## Relationships\n\n");
    if diff.relationships_added.is_empty() && diff.relationships_removed.is_empty() {
        out.push_str("No relationship changes.\n\n");
    } else {
        for r in &diff.relationships_added {
            out.push_str(&format!(
                "- added: `{} -> {}` ({})\n",
                r.from,
                r.to,
                r.confidence.label()
            ));
        }
        for r in &diff.relationships_removed {
            out.push_str(&format!(
                "- removed: `{} -> {}` ({})\n",
                r.from,
                r.to,
                r.confidence.label()
            ));
        }
        out.push('\n');
    }

    out.push_str("## Tests\n\n");
    if diff.tests_added.is_empty()
        && diff.tests_removed.is_empty()
        && diff.tests_status_changed.is_empty()
    {
        out.push_str("No test evidence changes.\n\n");
    } else {
        for t in &diff.tests_added {
            out.push_str(&format!(
                "- added: `{}` in `{}` ({})\n",
                t.id, t.file, t.status
            ));
        }
        for t in &diff.tests_removed {
            out.push_str(&format!(
                "- removed: `{}` in `{}` ({})\n",
                t.id, t.file, t.status
            ));
        }
        for c in &diff.tests_status_changed {
            out.push_str(&format!(
                "- status: `{}` in `{}` {} → {}\n",
                c.id, c.file, c.before, c.after
            ));
        }
        out.push('\n');
    }

    out.push_str("## Unknowns\n\n");
    for u in &diff.unknowns_added {
        out.push_str(&format!("- newly unknown: `{}`\n", u));
    }
    for u in &diff.unknowns_removed {
        out.push_str(&format!("- no longer unknown: `{}`\n", u));
    }
    if diff.unknowns_added.is_empty() && diff.unknowns_removed.is_empty() {
        out.push_str("No change in reported unknowns.\n");
    }
    out.push('\n');

    out.push_str("## Analysis limitations\n\n");
    for b in &diff.blindspots_added {
        out.push_str(&format!("- new blind spot: `{}`\n", b));
    }
    for b in &diff.blindspots_removed {
        out.push_str(&format!("- blind spot cleared: `{}`\n", b));
    }
    if diff.blindspots_added.is_empty() && diff.blindspots_removed.is_empty() {
        out.push_str("No change in analysis blind spots.\n");
    }
    out.push_str(&format!("\n> {}\n", diff.disclaimer));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::*;

    fn base_report() -> Report {
        let mut r = crate::report::sample_report_for_tests();
        r.finalize();
        r
    }

    #[test]
    fn diff_detects_added_relationship_and_unknown() {
        let before = base_report();
        let mut after = before.clone();
        // Add a new relationship.
        if let Some(entry) = after.ledger.get_mut(0) {
            entry.direct_affected.push(AffectedSymbol {
                symbol: SymbolRef::new("src/c.ts", "extra", SymbolKind::Function),
                relationship: Relationship::Transitive,
                depth: 1,
                confidence: Confidence::PossibleImpact,
                evidence: vec![],
                chain: vec![],
            });
        }
        after.unknowns.push(UnknownItem {
            id: "u1".into(),
            kind: UnknownKind::DynamicImport,
            summary: "dynamic import blocks resolution".into(),
            affected_symbol: None,
            evidence: vec![],
            next_step: "inspect importer".into(),
        });
        after.finalize();

        let d = compute_diff(&before, &after);
        assert_eq!(d.relationships_added.len(), 1);
        assert_eq!(d.relationships_added[0].to, "src/c.ts::extra");
        assert!(d.relationships_removed.is_empty());
        assert_eq!(d.unknowns_added, vec!["u1".to_string()]);
        assert!(d.unknowns_removed.is_empty());
        assert!(!d.disclaimer.is_empty());
    }

    #[test]
    fn diff_detects_test_status_change() {
        let before = base_report();
        let mut after = before.clone();
        for t in &mut after.tests {
            t.status = TestStatus::Passed;
        }
        for e in &mut after.ledger {
            for t in &mut e.tests {
                t.status = TestStatus::Passed;
            }
        }
        after.finalize();
        let d = compute_diff(&before, &after);
        assert_eq!(d.tests_status_changed.len(), 1);
        assert_eq!(d.tests_status_changed[0].before, TestStatus::NotExecuted);
        assert_eq!(d.tests_status_changed[0].after, TestStatus::Passed);
    }

    #[test]
    fn renders_mention_disclaimers() {
        let before = base_report();
        let d = compute_diff(&before, &before);
        let term = render_terminal(&d);
        assert!(term.contains("not proof of a regression"));
        let md = render_markdown(&d);
        assert!(md.contains("# Evidence diff"));
        assert!(md.contains("not proof of a regression"));
    }
}
