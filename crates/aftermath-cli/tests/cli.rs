//! End-to-end CLI tests: they build a real fixture repository, drive the built
//! `aftermath` binary, and assert on output and exit codes.
//!
//! The binary path comes from `env!("CARGO_BIN_EXE_aftermath")`, which Cargo
//! sets for integration tests, so these exercise the actual shipped interface.

use std::path::PathBuf;
use std::process::{Command, Output};

use aftermath_core::gitrepo::testkit::init_with;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_aftermath")
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A committed repo with `add()` in math.ts and `run()` calling it in app.ts.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            (
                "src/math.ts",
                "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
            ),
            (
                "src/app.ts",
                "import { add } from './math';\n\nexport function run(): number {\n  return add(2, 3);\n}\n",
            ),
        ];
        init_with(dir.path(), &files).unwrap();
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        // canonicalize so the path shown to the child matches what git reports
        self.dir.path().canonicalize().unwrap()
    }

    fn edit(&self, rel: &str, content: &str) {
        std::fs::write(self.dir.path().join(rel), content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .arg("--no-color")
            .arg("-C")
            .arg(self.path())
            .output()
            .expect("spawn aftermath")
    }

    fn stdout(out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).to_string()
    }
    fn stderr(out: &Output) -> String {
        String::from_utf8_lossy(&out.stderr).to_string()
    }
}

#[test]
fn inspect_detects_caller_of_changed_symbol() {
    let f = Fixture::new();
    // Change add()'s body only; run() calls it.
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a * b;\n}\n",
    );
    let out = f.run(&["inspect"]);
    assert!(
        out.status.success(),
        "inspect should exit 0: {}",
        Fixture::stderr(&out)
    );
    let text = Fixture::stdout(&out);
    assert!(
        text.contains("add"),
        "ledger should mention the changed symbol: {text}"
    );
    assert!(
        text.contains("run"),
        "affected caller should appear: {text}"
    );
    assert!(
        text.contains("consequence ledger") || text.contains("ledger"),
        "header: {text}"
    );
}

#[test]
fn inspect_json_is_a_valid_report() {
    let f = Fixture::new();
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a + b + 0;\n}\n",
    );
    let out = f.run(&["inspect", "--json"]);
    assert!(out.status.success());
    let text = Fixture::stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid JSON report");
    assert_eq!(v["schema_version"], "1.0.0");
    assert_eq!(v["scope"]["mode"], "worktree");
    assert!(v["ledger"].is_array());
    assert!(v["reproducibility"]["language_support"].is_array());
}

#[test]
fn why_matches_a_changed_symbol() {
    let f = Fixture::new();
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a - b;\n}\n",
    );
    let out = f.run(&["why", "add"]);
    assert!(
        out.status.success(),
        "why add should succeed: {}",
        Fixture::stderr(&out)
    );
    let text = Fixture::stdout(&out);
    assert!(
        text.contains("math.ts::add") || text.contains("add"),
        "why should explain add: {text}"
    );
    assert!(text.contains("run"), "why should show the caller: {text}");
}

#[test]
fn why_unknown_symbol_exits_not_found() {
    let f = Fixture::new();
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a + b;\n}\n\n// extra\n",
    );
    let out = f.run(&["why", "nonexistent_xyz"]);
    assert_eq!(out.status.code(), Some(4), "unknown symbol must exit 4");
}

#[test]
fn verify_without_consent_never_executes_tests() {
    let f = Fixture::new();
    f.edit("src/math.ts", "export function add(a: number, b: number): number {\n  return a + b;\n}\n\n/** changed */\n");
    let out = f.run(&["verify"]);
    assert!(out.status.success());
    let text = Fixture::stdout(&out);
    assert!(
        text.contains("not executed"),
        "verify must say tests weren't executed: {text}"
    );
    assert!(!text.contains("passed\n") || text.contains("not executed"));
}

#[test]
fn schema_prints_valid_json_schema() {
    let f = Fixture::new();
    let out = f.run(&["schema"]);
    assert!(out.status.success());
    let text = Fixture::stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).expect("schema must be valid JSON");
    assert_eq!(v["title"], "Aftermath analysis report");
    assert!(v["definitions"]["Confidence"].is_object());
}

#[test]
fn report_markdown_and_json_formats() {
    let f = Fixture::new();
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a + b * 2;\n}\n",
    );
    let md = f.run(&["report", "--format", "markdown"]);
    assert!(md.status.success());
    let md_text = Fixture::stdout(&md);
    assert!(md_text.contains("# Aftermath analysis report"));

    let js = f.run(&["report", "--format", "json"]);
    assert!(js.status.success());
    let v: serde_json::Value = serde_json::from_str(&Fixture::stdout(&js)).unwrap();
    assert!(v["ledger"].is_array());
}

#[test]
fn doctor_checks_environment() {
    let f = Fixture::new();
    let out = f.run(&["doctor"]);
    assert!(
        out.status.success(),
        "doctor should pass on a valid repo: {}",
        Fixture::stderr(&out)
    );
    let text = Fixture::stdout(&out);
    assert!(
        text.contains("Git repository"),
        "doctor should confirm git: {text}"
    );
    assert!(
        text.contains("Report schema"),
        "doctor should print schema version: {text}"
    );
}

#[test]
fn history_reports_correlation_with_disclaimer() {
    let f = Fixture::new();
    f.edit(
        "src/math.ts",
        "export function add(a: number, b: number): number {\n  return a + b + 2;\n}\n",
    );
    let out = f.run(&["history", "--limit", "50"]);
    assert!(
        out.status.success(),
        "history failed: {}",
        Fixture::stderr(&out)
    );
    let text = Fixture::stdout(&out);
    assert!(
        text.contains("correlation") || text.contains("not"),
        "history must carry disclaimer: {text}"
    );
}

#[test]
fn gaps_lists_unknowns_or_blind_spots() {
    let f = Fixture::new();
    f.edit("src/math.ts", "export function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport function mul() { return 2; }\n");
    let out = f.run(&["gaps", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&Fixture::stdout(&out)).unwrap();
    assert!(v["unknowns"].is_array());
    assert!(v["blind_spots"].is_array());
}

#[test]
fn outside_a_repo_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["inspect", "--no-color"])
        .arg("-C")
        .arg(dir.path())
        .output()
        .unwrap();
    // A bare temp dir (no parent repo) → clear "not a repository" exit.
    assert_eq!(out.status.code(), Some(3), "must exit 3 outside a repo");
}

#[test]
fn init_writes_config_and_aftermath_dir() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["init", "--git", "--no-color"])
        .arg("-C")
        .arg(dir.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dir.path().join("aftermath.toml").exists(),
        "init must write config"
    );
    assert!(
        dir.path().join(".aftermath").is_dir(),
        "init must create .aftermath/"
    );
    let cfg = std::fs::read_to_string(dir.path().join("aftermath.toml")).unwrap();
    assert!(cfg.contains("[analysis]"));
    assert!(cfg.contains("witness")); // policy documented
}
