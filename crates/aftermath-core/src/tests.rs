//! Feature D — test evidence: discovery, consent-gated execution, and
//! "which tests are near my change?" mapping.
//!
//! Two distinct things are called "tests" here, and they must not be
//! conflated:
//!
//! 1. **Discovered unit tests** — `test(...)`/`it(...)`/`function testX()`
//!    declarations found *inside* source files by the language adapter. These
//!    are mapped to changed files for the evidence ledger, but Aftermath
//!    *never executes them implicitly*: they live inside the language
//!    toolchain (cargo test, jest, vitest, ...), which is outside Aftermath's
//!    offline, read-only contract. Execution state is recorded as
//!    `Unavailable`, with the reason spelled out.
//! 2. **Runnable integration tests** — standalone scripts under a recognized
//!    test directory (for JavaScript projects: `tests/**\/*.test.js`). These
//!    *can* be executed with explicit consent, via a fixed, non-interpolated
//!    argv (never through a shell).
//!
//! Both kinds record distinct statuses: `Passed`, `Failed`, `Unavailable`,
//! `NotExecuted`. A tool that did not run tests must never report them as
//! passing.

use crate::evidence::{
    Consent, Evidence, EvidenceKind, EvidenceLocation, TestEvidence, TestRunRecord, TestStatus,
};
use crate::lang::ParsedFile;
use std::path::{Path, PathBuf};

/// A test that was discovered in the repository, independent of the report
/// model so it can be inspected before any status is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTest {
    /// Stable identifier: repository-relative path, plus `::name` for unit
    /// tests inside a file.
    pub id: String,
    /// Repository-relative file, forward slashes.
    pub file: String,
    /// 1-based line where the test is declared (0 for whole-file records).
    pub line: u32,
    pub kind: TestKind,
    /// Literal test title when the framework supplied one.
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    /// Declared inside a source file; not runnable without the project
    /// toolchain, therefore never executed by Aftermath.
    Unit,
    /// A standalone script under a recognized test directory; runnable with
    /// consent.
    Integration,
}

/// Configuration for what counts as a runnable test file and what Aftermath
/// knows how to execute.
#[derive(Debug, Clone)]
pub struct TestRunnerConfig {
    /// Glob-ish infix a runnable JS test file must match, e.g.
    /// `tests/` prefix and `.test.js` suffix.
    pub runnable_dir: &'static str,
    pub runnable_suffix: &'static str,
    /// Honest note attached to unit tests that were not executed.
    pub unit_not_executed_note: &'static str,
}

impl Default for TestRunnerConfig {
    fn default() -> Self {
        Self {
            runnable_dir: "tests/",
            runnable_suffix: ".test.js",
            unit_not_executed_note: "unit tests inside source files require the \
project's own toolchain (jest/vitest/cargo test) and are never executed by \
Aftermath",
        }
    }
}

/// Discover tests in the already-parsed files.
///
/// `files` are repository-relative paths of *changed* files when doing a
/// scoped discovery, or every indexed file for a full inventory. Unit tests
/// come from the adapter; a file under the runnable directory additionally
/// counts as one integration test.
pub fn discover(parsed: &[(&str, &ParsedFile)], cfg: &TestRunnerConfig) -> Vec<DiscoveredTest> {
    let mut out = Vec::new();
    for (rel, pf) in parsed {
        let rel_slash = rel.replace('\\', "/");
        for t in &pf.tests {
            out.push(DiscoveredTest {
                // Include the line so two `test(...)` calls in one file get
                // distinct, stable ids.
                id: format!("{rel_slash}::{}@{}", t.name, t.start_line),
                file: rel_slash.clone(),
                line: t.start_line,
                kind: TestKind::Unit,
                title: t.title.clone(),
            });
        }
        if is_runnable(rel_slash.as_str(), cfg) {
            out.push(DiscoveredTest {
                id: rel_slash.clone(),
                file: rel_slash.clone(),
                line: 0,
                kind: TestKind::Integration,
                title: None,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Whether a repository-relative path is a runnable integration test file.
pub fn is_runnable(rel: &str, cfg: &TestRunnerConfig) -> bool {
    rel.starts_with(cfg.runnable_dir)
        && rel.ends_with(cfg.runnable_suffix)
        && rel.len() > cfg.runnable_dir.len() + cfg.runnable_suffix.len()
}

/// Build [`TestEvidence`] for every discovered test, honouring consent.
///
/// * Unit tests → `Unavailable` (never executed by Aftermath).
/// * Integration tests → `NotExecuted` without consent; with consent, run via
///   `exec` and record `Passed`/`Failed`, or `Unavailable` if the runner
///   could not be spawned (e.g. `node` missing).
///
/// Returns the per-test evidence plus a [`TestRunRecord`] for each test that
/// was actually handed to the executor (Feature H reproducibility).
pub fn collect(
    tests: &[DiscoveredTest],
    consent: Consent,
    cfg: &TestRunnerConfig,
    exec: &mut dyn crate::executor::Executor,
    now_rfc3339: &str,
    repo_root: &Path,
) -> (Vec<TestEvidence>, Vec<TestRunRecord>) {
    let mut evidence = Vec::new();
    let mut runs = Vec::new();
    for t in tests {
        match t.kind {
            TestKind::Unit => {
                evidence.push(unit_evidence(t, cfg));
            }
            TestKind::Integration => {
                let (ev, run) = integration_evidence(t, consent, exec, now_rfc3339, repo_root);
                evidence.push(ev);
                if let Some(r) = run {
                    runs.push(r);
                }
            }
        }
    }
    (evidence, runs)
}

fn unit_evidence(t: &DiscoveredTest, cfg: &TestRunnerConfig) -> TestEvidence {
    let title = t.title.as_deref().unwrap_or("(untitled)");
    TestEvidence {
        id: t.id.clone(),
        file: t.file.clone(),
        symbol: None,
        status: TestStatus::Unavailable,
        evidence: vec![Evidence::new(
            EvidenceKind::TestReference,
            EvidenceLocation::line(t.file.clone(), t.line.max(1)),
            format!("test `{title}` declared here"),
        )],
        note: Some(cfg.unit_not_executed_note.to_string()),
    }
}

fn integration_evidence(
    t: &DiscoveredTest,
    consent: Consent,
    exec: &mut dyn crate::executor::Executor,
    now_rfc3339: &str,
    repo_root: &Path,
) -> (TestEvidence, Option<TestRunRecord>) {
    let argv = ["node", "--test", t.file.as_str()];
    let loc = EvidenceLocation::line(t.file.clone(), 1);

    if consent == Consent::NotGranted {
        return (
            TestEvidence {
                id: t.id.clone(),
                file: t.file.clone(),
                symbol: None,
                status: TestStatus::NotExecuted,
                evidence: vec![Evidence::new(
                    EvidenceKind::TestReference,
                    loc,
                    "integration test identified; not run (no consent)",
                )],
                note: Some(
                    "test execution requires explicit consent \
(--run-tests); rerun to execute"
                        .to_string(),
                ),
            },
            None,
        );
    }

    // Consent granted: run through the executor (no shell, argv array).
    let file_arg = t.file.clone();
    match exec.run(repo_root, "node", &["--test", file_arg.as_str()]) {
        Ok(true) => (
            TestEvidence {
                id: t.id.clone(),
                file: t.file.clone(),
                symbol: None,
                status: TestStatus::Passed,
                evidence: vec![Evidence::new(
                    EvidenceKind::TestExecution,
                    loc,
                    "integration test executed and passed",
                )],
                note: None,
            },
            Some(runs_record(
                t,
                &argv,
                now_rfc3339,
                TestStatus::Passed,
                0,
                repo_root,
            )),
        ),
        Ok(false) => (
            TestEvidence {
                id: t.id.clone(),
                file: t.file.clone(),
                symbol: None,
                status: TestStatus::Failed,
                evidence: vec![Evidence::new(
                    EvidenceKind::TestExecution,
                    loc,
                    "integration test executed and failed (non-zero exit)",
                )],
                note: Some("test runner reported failure".to_string()),
            },
            Some(runs_record(
                t,
                &argv,
                now_rfc3339,
                TestStatus::Failed,
                1,
                repo_root,
            )),
        ),
        Err(e) => (
            TestEvidence {
                id: t.id.clone(),
                file: t.file.clone(),
                symbol: None,
                status: TestStatus::Unavailable,
                evidence: vec![Evidence::new(
                    EvidenceKind::TestExecution,
                    loc,
                    "integration test could not be executed (runner missing)",
                )],
                note: Some(format!("could not spawn `node` (no shell involved): {e:#}")),
            },
            Some(TestRunRecord {
                command: argv.iter().map(|s| s.to_string()).collect(),
                cwd: rel_str(repo_root),
                status: TestStatus::Unavailable,
                exit_code: None,
                executed_at: now_rfc3339.to_string(),
                duration_ms: None,
                summary: None,
                consent,
            }),
        ),
    }
}

fn runs_record(
    t: &DiscoveredTest,
    argv: &[&str],
    now: &str,
    status: TestStatus,
    exit_code: i32,
    repo_root: &Path,
) -> TestRunRecord {
    TestRunRecord {
        command: argv.iter().map(|s| s.to_string()).collect(),
        cwd: rel_str(repo_root),
        status,
        exit_code: Some(exit_code),
        executed_at: now.to_string(),
        duration_ms: None,
        summary: Some(format!("{} — {}", t.id, status)),
        consent: Consent::Explicit,
    }
}

fn rel_str(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.trim_end_matches('/').to_string()
}

/// Conservative "tests near the change" mapping: tests declared in the same
/// files the diff touched. We do **not** claim to prove which individual
/// assertions a diff affects — that would require deeper symbolic execution
/// and is speculation. We report same-file tests and say so.
pub fn tests_in_changed_files(tests: &[DiscoveredTest], changed: &[PathBuf]) -> Vec<String> {
    let changed: Vec<String> = changed
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let mut ids: Vec<String> = tests
        .iter()
        .filter(|t| changed.iter().any(|c| c == &t.file))
        .map(|t| t.id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod unit {
    use super::*;
    use crate::lang::{adapter_for, LanguageAdapter};

    fn parse(rel: &str, src: &str) -> ParsedFile {
        let adapter: Box<dyn LanguageAdapter> = adapter_for(rel).unwrap();
        adapter.parse(rel, src)
    }

    const UNIT_SRC: &str = "test('adds numbers', () => {});\n\n\
                            it('adds negatives', () => {});\n\n\
                            describe('outer', () => {\n  test('inner', () => {});\n});\n";

    fn cfg() -> TestRunnerConfig {
        TestRunnerConfig::default()
    }

    #[test]
    fn discovers_unit_tests_from_parsed_file() {
        let pf = parse("src/math.test.js", UNIT_SRC);
        let files = [("src/math.test.js", &pf)];
        let found = discover(&files, &cfg());
        // test/it/describe calls: three `test`/`it` + one `describe` = 4 unit
        // tests (the nested `test('inner')` is also found by the walker).
        let unit: Vec<_> = found.iter().filter(|t| t.kind == TestKind::Unit).collect();
        assert!(unit.len() >= 4, "expected >=4 unit tests, got: {found:#?}");
        assert!(unit
            .iter()
            .any(|t| t.title.as_deref() == Some("adds numbers")));
    }

    #[test]
    fn runnable_test_file_is_integration() {
        let pf = parse("tests/api.test.js", "test('x', () => {});\n");
        let files = [("tests/api.test.js", &pf)];
        let found = discover(&files, &cfg());
        let int: Vec<_> = found
            .iter()
            .filter(|t| t.kind == TestKind::Integration)
            .collect();
        assert_eq!(int.len(), 1, "{found:#?}");
        assert_eq!(int[0].file, "tests/api.test.js");
    }

    #[test]
    fn source_test_file_outside_tests_dir_is_not_integration() {
        let pf = parse("src/math.test.js", UNIT_SRC);
        let files = [("src/math.test.js", &pf)];
        let found = discover(&files, &cfg());
        assert!(found.iter().all(|t| t.kind == TestKind::Unit));
    }

    struct StubExec {
        ok: bool,
        spawn_fails: bool,
    }
    impl crate::executor::Executor for StubExec {
        fn run(&mut self, _cwd: &Path, _program: &str, _args: &[&str]) -> anyhow::Result<bool> {
            if self.spawn_fails {
                anyhow::bail!("no such program: node")
            }
            Ok(self.ok)
        }
    }

    fn integration_test() -> DiscoveredTest {
        DiscoveredTest {
            id: "tests/api.test.js".into(),
            file: "tests/api.test.js".into(),
            line: 0,
            kind: TestKind::Integration,
            title: None,
        }
    }

    #[test]
    fn without_consent_integration_is_not_executed() {
        let tests = vec![integration_test()];
        let mut exec = StubExec {
            ok: true,
            spawn_fails: false,
        };
        let (ev, runs) = collect(
            &tests,
            Consent::NotGranted,
            &cfg(),
            &mut exec,
            "2026-01-01T00:00:00Z",
            Path::new("."),
        );
        assert_eq!(ev[0].status, TestStatus::NotExecuted);
        assert!(ev[0].note.as_deref().unwrap().contains("consent"));
        assert!(runs.is_empty(), "no run record without consent");
    }

    #[test]
    fn unit_tests_always_unavailable_with_reason() {
        let pf = parse("src/math.test.js", UNIT_SRC);
        let files = [("src/math.test.js", &pf)];
        let found = discover(&files, &cfg());
        let mut exec = StubExec {
            ok: true,
            spawn_fails: false,
        };
        let (ev, runs) = collect(
            &found,
            Consent::Explicit,
            &cfg(),
            &mut exec,
            "2026-01-01T00:00:00Z",
            Path::new("."),
        );
        assert!(ev.iter().all(|e| e.status == TestStatus::Unavailable));
        assert!(ev[0].note.as_deref().unwrap().contains("toolchain"));
        assert!(runs.is_empty(), "unit tests never produce run records");
    }

    #[test]
    fn consented_success_records_passed_and_run() {
        let tests = vec![integration_test()];
        let mut exec = StubExec {
            ok: true,
            spawn_fails: false,
        };
        let (ev, runs) = collect(
            &tests,
            Consent::Explicit,
            &cfg(),
            &mut exec,
            "2026-01-01T00:00:00Z",
            Path::new("."),
        );
        assert_eq!(ev[0].status, TestStatus::Passed);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].command, vec!["node", "--test", "tests/api.test.js"]);
        assert_eq!(runs[0].status, TestStatus::Passed);
        assert_eq!(runs[0].exit_code, Some(0));
    }

    #[test]
    fn consented_failure_records_failed() {
        let tests = vec![integration_test()];
        let mut exec = StubExec {
            ok: false,
            spawn_fails: false,
        };
        let (ev, runs) = collect(
            &tests,
            Consent::Explicit,
            &cfg(),
            &mut exec,
            "2026-01-01T00:00:00Z",
            Path::new("."),
        );
        assert_eq!(ev[0].status, TestStatus::Failed);
        assert_eq!(runs[0].status, TestStatus::Failed);
    }

    #[test]
    fn missing_runner_records_unavailable_not_passed() {
        let tests = vec![integration_test()];
        let mut exec = StubExec {
            ok: true,
            spawn_fails: true,
        };
        let (ev, runs) = collect(
            &tests,
            Consent::Explicit,
            &cfg(),
            &mut exec,
            "2026-01-01T00:00:00Z",
            Path::new("."),
        );
        assert_eq!(
            ev[0].status,
            TestStatus::Unavailable,
            "a missing runner is never a pass"
        );
        assert_eq!(runs[0].status, TestStatus::Unavailable);
        assert!(ev[0].note.as_deref().unwrap().contains("node"));
    }

    #[test]
    fn tests_in_changed_files_maps_by_path() {
        let tests = vec![
            DiscoveredTest {
                id: "a.test.js::one".into(),
                file: "a.test.js".into(),
                line: 1,
                kind: TestKind::Unit,
                title: None,
            },
            DiscoveredTest {
                id: "b.test.js::two".into(),
                file: "b.test.js".into(),
                line: 1,
                kind: TestKind::Unit,
                title: None,
            },
        ];
        let changed = vec![PathBuf::from("a.test.js")];
        assert_eq!(
            tests_in_changed_files(&tests, &changed),
            vec!["a.test.js::one"]
        );
    }
}
