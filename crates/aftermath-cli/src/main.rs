//! Aftermath command-line interface.
//!
//! Responsibilities kept deliberately thin here: resolve input (repo, config),
//! call the core analyzer, and render the [`Report`]. All analysis logic lives
//! in `aftermath-core`, so this crate never re-implements evidence rules.
//!
//! Exit codes (documented and stable):
//! - `0` success
//! - `1` runtime error (I/O, Git, config)
//! - `2` usage error (clap)
//! - `3` not a Git repository
//! - `4` not found (no such symbol / baseline file)

mod args;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use args::{Cli, Command, Format, Global};
use clap::Parser;

use aftermath_core::analyze::{self, AnalysisOptions};
use aftermath_core::cache::ParseCache;
use aftermath_core::config::Config;
use aftermath_core::evidencediff;
use aftermath_core::executor::LocalExecutor;
use aftermath_core::gitrepo::GitRepo;
use aftermath_core::report::{Report, ScopeMode};

/// Process exit codes.
mod code {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const NO_REPO: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
}
use code::*;

/// Shared, resolved CLI context.
struct Ctx {
    g: Global,
    color: bool,
    quiet: bool,
    verbose: bool,
}

impl Ctx {
    fn from(g: Global) -> Self {
        let no_color = g.no_color || std::env::var_os("NO_COLOR").is_some();
        let is_tty = std::io::stdout().is_terminal();
        Ctx {
            color: !no_color && is_tty,
            quiet: g.quiet,
            verbose: g.verbose,
            g,
        }
    }

    fn info(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }
    fn vinfo(&self, msg: &str) {
        if self.verbose && !self.quiet {
            eprintln!("{msg}");
        }
    }
    fn warn(&self, msg: &str) {
        eprintln!("warning: {msg}");
    }
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("aftermath: error: {e}");
            std::process::exit(e.code);
        }
    }
}

/// A typed error that can carry a specific exit code.
#[derive(Debug)]
struct CliError {
    msg: String,
    code: i32,
}
impl CliError {
    fn new(msg: impl Into<String>, code: i32) -> Self {
        Self {
            msg: msg.into(),
            code,
        }
    }
}
impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        CliError::new(e.to_string(), ERROR)
    }
}

fn run(cli: Cli) -> Result<i32, CliError> {
    let ctx = Ctx::from(cli.global);
    match cli.command {
        Command::Init { force, git } => cmd_init(&ctx, force, git),
        Command::Inspect {
            staged,
            revision,
            baseline,
            file,
        } => cmd_inspect(&ctx, staged, revision, baseline, &file),
        Command::Why {
            symbol,
            staged,
            revision,
        } => cmd_why(&ctx, &symbol, staged, revision),
        Command::Gaps { staged, revision } => cmd_gaps(&ctx, staged, revision),
        Command::Verify {
            staged,
            revision,
            run_tests,
        } => cmd_verify(&ctx, staged, revision, run_tests),
        Command::Report {
            format,
            out,
            staged,
            revision,
        } => cmd_report(&ctx, format, out, staged, revision),
        Command::History { file, limit } => cmd_history(&ctx, &file, limit),
        Command::Watch { remove } => cmd_watch(&ctx, remove),
        Command::Doctor => cmd_doctor(&ctx),
        Command::Schema { out } => cmd_schema(&ctx, out),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn find_repo(ctx: &Ctx) -> Result<GitRepo, CliError> {
    let start = match &ctx.g.repo {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(|e| CliError::new(e.to_string(), ERROR))?,
    };
    GitRepo::discover(&start).map_err(|_| {
        CliError::new(
            format!(
                "not a Git repository (searched from {})\n  \
                 Run `git init` or `aftermath init --git` to start one.",
                start.display()
            ),
            NO_REPO,
        )
    })
}

fn load_config(ctx: &Ctx, repo: &GitRepo) -> Result<Config, CliError> {
    let path = match &ctx.g.config {
        Some(p) => p.clone(),
        None => Config::find(repo.root()),
    };
    let cfg = Config::load(&path).map_err(|e| CliError::new(e.to_string(), ERROR))?;
    Ok(cfg)
}

fn scope(staged: bool, revision: Option<String>) -> (ScopeMode, Option<String>) {
    if let Some(rev) = revision {
        (ScopeMode::Revision, Some(rev))
    } else if staged {
        (ScopeMode::Staged, None)
    } else {
        (ScopeMode::Worktree, None)
    }
}

/// Run a full analysis and return the finalized report (+ the cache to save).
fn run_analysis(
    ctx: &Ctx,
    repo: &GitRepo,
    cfg: &Config,
    staged: bool,
    revision: Option<String>,
    run_tests: bool,
) -> Result<(Report, ParseCache), CliError> {
    let (mode, rev) = scope(staged, revision);
    let opts = AnalysisOptions {
        mode,
        revision: rev,
        run_tests,
        history: cfg.history.enabled,
        generated_at: None,
        home: home_dir(),
    };
    ctx.vinfo(&format!(
        "analyzing {} changes in {}",
        match opts.mode {
            ScopeMode::Worktree => "worktree",
            ScopeMode::Staged => "staged",
            ScopeMode::Revision => "revision",
        },
        repo.root().display()
    ));
    let mut cache = ParseCache::load(repo.root(), cfg.cache.enabled);
    let mut exec = LocalExecutor;
    let report = analyze::analyze(repo, cfg, &opts, &mut cache, &mut exec)
        .map_err(|e| CliError::new(e, ERROR))?;
    if cfg.cache.enabled {
        let _ = cache.save();
    }
    Ok((report, cache))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Resolve where output goes and write to it (file or stdout).
fn emit(ctx: &Ctx, out: Option<&Path>, content: &str) -> Result<i32, CliError> {
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            std::fs::write(path, content).map_err(|e| {
                CliError::new(format!("failed to write {}: {e}", path.display()), ERROR)
            })?;
            ctx.info(&format!("wrote {}", path.display()));
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = lock.write_all(content.as_bytes());
            let _ = lock.write_all(b"\n");
        }
    }
    Ok(OK)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_init(ctx: &Ctx, force: bool, git: bool) -> Result<i32, CliError> {
    let start = ctx
        .g
        .repo
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Optionally create a Git repository when none exists (explicit opt-in).
    let needs_git = GitRepo::discover(&start).is_err();
    if needs_git {
        if git {
            let repo = GitRepo::init(&start).map_err(|e| {
                CliError::new(format!("failed to initialize git repository: {e}"), ERROR)
            })?;
            ctx.info(&format!(
                "initialized git repository at {}",
                repo.root().display()
            ));
        } else {
            // A repository is not strictly required to write config, but warn.
            ctx.warn(
                "this directory is not a Git repository; `aftermath inspect` needs one. \
                 Re-run with --git to initialize it.",
            );
        }
    }

    let cfg_path = Config::find(&start);
    if cfg_path.exists() && !force {
        ctx.info(&format!(
            "{} already exists; leaving it unchanged (use --force to overwrite).",
            cfg_path.display()
        ));
    } else {
        let text = Config::default().to_toml();
        std::fs::write(&cfg_path, text).map_err(|e| {
            CliError::new(
                format!("failed to write {}: {e}", cfg_path.display()),
                ERROR,
            )
        })?;
        ctx.info(&format!("wrote {}", cfg_path.display()));
    }

    // Create the (gitignored) working directory for the disposable cache.
    let work_dir = start.join(".aftermath");
    let created = !work_dir.exists();
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| CliError::new(format!("failed to create .aftermath/: {e}"), ERROR))?;

    let gi = start.join(".gitignore");
    ensure_gitignore(&gi)?;
    if created {
        ctx.info("created .aftermath/ (safe to delete; regenerated on demand)");
    }
    ctx.info("Aftermath configured. Next: `aftermath inspect`.");
    Ok(OK)
}

fn ensure_gitignore(path: &Path) -> Result<(), CliError> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".aftermath/") {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n# Aftermath local analysis cache\n.aftermath/\n");
    std::fs::write(path, text)
        .map_err(|e| CliError::new(format!("failed to update .gitignore: {e}"), ERROR))
}

fn cmd_inspect(
    ctx: &Ctx,
    staged: bool,
    revision: Option<String>,
    baseline: Option<PathBuf>,
    files: &[String],
) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let cfg = load_config(ctx, &repo)?;
    let (report, _cache) = run_analysis(ctx, &repo, &cfg, staged, revision, false)?;

    let report = if files.is_empty() {
        report
    } else {
        filter_report_to_files(report, files)
    };

    // Baseline diff (Feature G): compare against a previously saved report.
    if let Some(base_path) = baseline {
        let base_json = std::fs::read_to_string(&base_path).map_err(|e| {
            CliError::new(
                format!("failed to read baseline {}: {e}", base_path.display()),
                NOT_FOUND,
            )
        })?;
        let before: Report = serde_json::from_str(&base_json)
            .map_err(|e| CliError::new(format!("baseline is not a valid report: {e}"), ERROR))?;
        let diff = evidencediff::compute_diff(&before, &report);
        if ctx.g.json {
            let json = serde_json::to_string_pretty(&diff)
                .map_err(|e| CliError::new(e.to_string(), ERROR))?;
            print!("{json}");
        } else {
            print!("{}", evidencediff::render_terminal(&diff));
        }
        return Ok(OK);
    }

    render_report(ctx, &report)
}

fn cmd_why(
    ctx: &Ctx,
    symbol: &str,
    staged: bool,
    revision: Option<String>,
) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let cfg = load_config(ctx, &repo)?;
    let (report, _cache) = run_analysis(ctx, &repo, &cfg, staged, revision, false)?;

    // Match `name` or `path::name`.
    let want_path: Option<&str> = symbol.split_once("::").map(|(p, _)| p);
    let want_name = symbol.rsplit("::").next().unwrap_or(symbol).to_string();

    let matches: Vec<&aftermath_core::evidence::LedgerEntry> = report
        .ledger
        .iter()
        .filter(|e| {
            e.changed_symbol
                .as_ref()
                .map(|s| {
                    (s.name == want_name || s.id() == symbol)
                        && want_path.map(|p| s.file == p).unwrap_or(true)
                })
                .unwrap_or(false)
        })
        .collect();

    if matches.is_empty() {
        return Err(CliError::new(
            format!(
                "no changed symbol named `{symbol}` in this scope. \
                 Try `aftermath inspect` to see what changed, or `aftermath why <path>::<name>`."
            ),
            NOT_FOUND,
        ));
    }

    if ctx.g.json {
        let json = serde_json::to_string_pretty(&matches)
            .map_err(|e| CliError::new(e.to_string(), ERROR))?;
        print!("{json}");
        return Ok(OK);
    }

    // Focused narrative for the requested symbol(s).
    for e in &matches {
        let sym = e.changed_symbol.as_ref().unwrap();
        println!("{} {}", style::bold("Why", ctx.color), sym.id());
        println!(
            "  change: {} (lines {})",
            e.change_category,
            join(&e.changed_lines)
        );
        println!(
            "  confidence: {}",
            style::bold(e.confidence.label(), ctx.color)
        );
        if e.direct_affected.is_empty() && e.transitive_affected.is_empty() {
            println!("  ↳ no affected symbols resolved by static analysis");
            println!("    (this is absence of found evidence, not proof of safety)");
        }
        for a in &e.direct_affected {
            println!(
                "  → {} {} {}",
                a.relationship,
                a.symbol.id(),
                style::conf(a.confidence, ctx.color)
            );
            for ev in &a.evidence {
                println!(
                    "      evidence: {} {}:{} — {}",
                    ev.kind, ev.location.file, ev.location.line, ev.detail
                );
            }
        }
        for a in &e.transitive_affected {
            let chain = a
                .chain
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(" → ");
            println!("  ⇢ {} via [{}]", a.symbol.id(), chain);
        }
        for s in &e.suggestions {
            println!("  ? next test to consider: {} — {}", s.title, s.rationale);
        }
        for u in e.unverified.iter().chain(e.suggested_next_steps.iter()) {
            println!("  ! {u}");
        }
        println!();
    }
    Ok(OK)
}

fn cmd_gaps(ctx: &Ctx, staged: bool, revision: Option<String>) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let cfg = load_config(ctx, &repo)?;
    let (report, _cache) = run_analysis(ctx, &repo, &cfg, staged, revision, false)?;

    if ctx.g.json {
        let payload = serde_json::json!({
            "unknowns": report.unknowns,
            "blind_spots": report.blind_spots,
        });
        print!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| CliError::new(e.to_string(), ERROR))?
        );
        return Ok(OK);
    }

    println!(
        "{}",
        style::bold("What the analysis could not prove", ctx.color)
    );
    if report.unknowns.is_empty() && report.blind_spots.is_empty() {
        println!("  (no unknowns or blind spots recorded)");
    }
    for u in &report.unknowns {
        println!("  • {}", u.summary);
        println!("      {} {}", style::dim("next:", ctx.color), u.next_step);
    }
    if !report.blind_spots.is_empty() {
        println!("\n{}", style::bold("Analysis blind spots", ctx.color));
    }
    for b in &report.blind_spots {
        println!(
            "  • {}{}",
            b.kind,
            b.file
                .as_ref()
                .map(|f| format!(" ({f})"))
                .unwrap_or_default()
        );
        println!("      {}", b.impact);
    }
    Ok(OK)
}

fn cmd_verify(
    ctx: &Ctx,
    staged: bool,
    revision: Option<String>,
    run_tests: bool,
) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let cfg = load_config(ctx, &repo)?;
    if run_tests {
        ctx.info("consent granted: executing discovered integration tests");
    }
    let (report, _cache) = run_analysis(ctx, &repo, &cfg, staged, revision, run_tests)?;

    let s = &report.summary;
    if ctx.g.json {
        let payload = serde_json::json!({
            "tests": report.tests,
            "summary": {
                "identified": s.tests_identified,
                "passed": s.tests_passed,
                "failed": s.tests_failed,
                "not_executed": s.tests_not_executed,
                "unavailable": s.tests_unavailable,
            },
            "runs": report.reproducibility.test_runs,
        });
        print!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| CliError::new(e.to_string(), ERROR))?
        );
        return Ok(OK);
    }

    println!("{}", style::bold("Verification state", ctx.color));
    println!(
        "  identified {} · passed {} · failed {} · not executed {} · unavailable {}",
        s.tests_identified,
        s.tests_passed,
        s.tests_failed,
        s.tests_not_executed,
        s.tests_unavailable
    );
    if s.tests_identified == 0 {
        println!(
            "  {}",
            style::dim(
                "(no tests referencing the changed symbols were identified)",
                ctx.color
            )
        );
    }
    for t in &report.tests {
        println!(
            "  · {} {} {}",
            t.id,
            style::dim(&format!("({})", t.file), ctx.color),
            style::status(t.status, ctx.color)
        );
        if let Some(note) = &t.note {
            println!("      {}", style::dim(note, ctx.color));
        }
    }
    if !run_tests {
        println!(
            "\n  {}",
            style::dim("tests were not executed. Re-run `aftermath verify --run-tests` to execute available integration tests (never without consent).", ctx.color)
        );
    }
    Ok(OK)
}

fn cmd_report(
    ctx: &Ctx,
    format: Format,
    out: Option<PathBuf>,
    staged: bool,
    revision: Option<String>,
) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let cfg = load_config(ctx, &repo)?;
    let (report, _cache) = run_analysis(ctx, &repo, &cfg, staged, revision, false)?;
    let content = match format {
        Format::Json => report.to_json(),
        Format::Markdown => report.to_markdown(),
        Format::Terminal => report.to_terminal(ctx.color),
    };
    emit(ctx, out.as_deref(), &content)
}

fn cmd_history(ctx: &Ctx, files: &[String], limit: Option<u32>) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let mut cfg = load_config(ctx, &repo)?;
    if let Some(l) = limit {
        cfg.history.limit = l;
    }
    // Focus defaults to the files currently changed.
    let focus: Vec<String> = if files.is_empty() {
        let changes = repo
            .changes(ScopeMode::Worktree, None)
            .map_err(|e| CliError::new(format!("failed to read changes: {e}"), ERROR))?;
        changes.into_iter().map(|c| c.path).collect()
    } else {
        files.to_vec()
    };

    let report =
        analyze::history_report(&repo, &cfg, &focus).map_err(|e| CliError::new(e, ERROR))?;

    if ctx.g.json {
        print!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|e| CliError::new(e.to_string(), ERROR))?
        );
        return Ok(OK);
    }

    println!("{}", style::bold("Repository archaeology", ctx.color));
    println!(
        "  {} commit(s) examined{}.",
        report.commits_examined,
        if report.truncated {
            " (truncated at limit)"
        } else {
            ""
        }
    );
    if report.co_changes.is_empty() {
        println!(
            "  {}",
            style::dim(
                "no repeated co-change above the configured threshold",
                ctx.color
            )
        );
    }
    for c in &report.co_changes {
        println!(
            "  · {} often changes with:",
            style::bold(&c.file, ctx.color)
        );
        for f in &c.files {
            println!("      {f}");
        }
    }
    println!("\n  {}", style::dim(&report.disclaimer, ctx.color));
    Ok(OK)
}

fn cmd_watch(ctx: &Ctx, remove: bool) -> Result<i32, CliError> {
    let repo = find_repo(ctx)?;
    let hooks = repo.root().join(".git").join("hooks");
    let hook = hooks.join("pre-commit");

    if remove {
        if hook.exists() {
            std::fs::remove_file(&hook)
                .map_err(|e| CliError::new(format!("failed to remove hook: {e}"), ERROR))?;
            ctx.info("removed the Aftermath change-witness hook.");
        } else {
            ctx.info("no Aftermath hook was installed.");
        }
        return Ok(OK);
    }

    std::fs::create_dir_all(&hooks)
        .map_err(|e| CliError::new(format!("failed to create hooks dir: {e}"), ERROR))?;
    if hook.exists() {
        let body = std::fs::read_to_string(&hook).unwrap_or_default();
        if body.contains("aftermath") {
            ctx.info("watch hook already installed (leaving it unchanged).");
            return Ok(OK);
        }
        // Preserve an existing hook by appending (best effort).
        let appended = format!(
            "{body}\n\n# --- Aftermath change witness (appended) ---\n{s}",
            s = script()
        );
        std::fs::write(&hook, appended)
            .map_err(|e| CliError::new(format!("failed to append hook: {e}"), ERROR))?;
        ctx.info("appended the Aftermath change witness to the existing pre-commit hook.");
        return Ok(OK);
    }

    std::fs::write(&hook, script())
        .map_err(|e| CliError::new(format!("failed to write hook: {e}"), ERROR))?;

    // Make it executable on Unix-like platforms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755));
    }

    ctx.info("installed the advisory pre-commit change witness.");
    ctx.info(
        "it runs `aftermath verify --staged` on each commit (advisory: never blocks by default).",
    );
    Ok(OK)
}

fn script() -> String {
    r#"#!/bin/sh
# Aftermath change witness (advisory by default).
# Prints the consequence ledger for staged changes on every commit.
if ! command -v aftermath >/dev/null 2>&1; then
    echo "aftermath: not installed; skipping change witness" >&2
    exit 0
fi

echo "aftermath: change witness — staged changes" >&2
aftermath inspect --staged
status=$?
if [ "$status" -ne 0 ] && [ "$status" -ne 1 ]; then
    echo "aftermath: failed to analyze staged changes" >&2
    exit 1
fi

# Optional blocking policy, honored only when explicitly configured.
policy=advisory
if [ -f aftermath.toml ]; then
    p=$(sed -n 's/^policy[[:space:]]*=[[:space:]]*"\([a-z]*\)".*/\1/p' aftermath.toml | head -n1)
    [ -n "$p" ] && policy=$p
fi
if [ "$policy" = "blocking" ]; then
    if ! aftermath verify --staged >/dev/null 2>&1; then
        exit 1
    fi
fi
exit 0
"#
    .to_string()
}

fn cmd_doctor(ctx: &Ctx) -> Result<i32, CliError> {
    let mut ok = true;

    // Git repository.
    match find_repo(ctx) {
        Ok(repo) => {
            report_check(
                ctx,
                true,
                &format!("Git repository at {}", repo.root().display()),
            );
            if let Some(rev) = repo.head_oid() {
                println!("      HEAD {}", &rev[..rev.len().min(12)]);
            }
            // Config validity (fall back to defaults if the file is missing).
            let cfg =
                Config::load(&Config::find(repo.root())).unwrap_or_else(|_| Config::default());
            report_check(ctx, true, "aftermath.toml is valid (or defaults in use)");
            let _ = cfg_writeable_check(ctx, &repo);

            // Cache sanity.
            let enabled = cfg.cache.enabled;
            let cache = ParseCache::load(repo.root(), enabled);
            report_check(
                ctx,
                true,
                &format!("parse cache readable ({} entries)", cache.len()),
            );
        }
        Err(_) => {
            report_check(
                ctx,
                false,
                "not a Git repository (run from a repo, or `git init`)",
            );
            ok = false;
        }
    }

    // Optional: node integration-test runner.
    let node = std::process::Command::new("node").arg("--version").output();
    match node {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            report_check(ctx, true, &format!("node available for integration tests (node {})", v.trim()));
        }
        _ => report_check(
            ctx,
            true,
            "node not found (unit tests inside source files still analyzed; integration tests runnable only in JS repos with node)",
        ),
    }

    // Versions.
    println!("      Aftermath {}", aftermath_core::VERSION);
    println!("      Report schema {}", aftermath_core::SCHEMA_VERSION);

    if ctx.g.offline {
        println!("      offline asserted (Aftermath is offline by default)");
    }

    Ok(if ok { OK } else { ERROR })
}

fn cfg_writeable_check(ctx: &Ctx, repo: &GitRepo) -> bool {
    let dir = repo.root().join(".aftermath");
    if std::fs::create_dir_all(&dir).is_ok() {
        true
    } else {
        report_check(
            ctx,
            false,
            ".aftermath/ is not writable; the cache will be disabled",
        );
        false
    }
}

fn report_check(ctx: &Ctx, ok: bool, msg: &str) {
    if ctx.g.json {
        return; // JSON checks are surfaced by the caller if ever needed
    }
    let mark = if ctx.color {
        if ok {
            "\x1b[32m✓\x1b[0m"
        } else {
            "\x1b[31m✗\x1b[0m"
        }
    } else if ok {
        "ok"
    } else {
        "X"
    };
    println!("  {mark} {msg}");
}

fn cmd_schema(ctx: &Ctx, out: Option<PathBuf>) -> Result<i32, CliError> {
    emit(ctx, out.as_deref(), schema_json())
}

fn schema_json() -> &'static str {
    include_str!("../schema/aftermath-report.schema.json")
}

// ---------------------------------------------------------------------------
// Report rendering + helpers
// ---------------------------------------------------------------------------

fn render_report(ctx: &Ctx, report: &Report) -> Result<i32, CliError> {
    if ctx.g.json {
        print!("{}", report.to_json());
        return Ok(OK);
    }
    print!("{}", report.to_terminal(ctx.color));
    Ok(OK)
}

/// Keep only ledger entries whose changed file is in `files` (for --file).
fn filter_report_to_files(mut report: Report, files: &[String]) -> Report {
    let set: std::collections::HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
    report
        .ledger
        .retain(|e| set.contains(e.changed_file.as_str()));
    report
        .scope
        .changed_files
        .retain(|f| set.contains(f.as_str()));
    report
}

fn join(lines: &[u32]) -> String {
    aftermath_core::report::join_ranges(lines)
}

/// Small ANSI helpers used outside the core renderer.
mod style {
    pub fn wrap(color: bool, code: &str, s: &str) -> String {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold(s: &str, color: bool) -> String {
        wrap(color, "1", s)
    }
    pub fn dim(s: &str, color: bool) -> String {
        wrap(color, "2", s)
    }
    pub fn conf(c: aftermath_core::Confidence, color: bool) -> String {
        wrap(color, "32", c.label())
    }
    pub fn status(s: aftermath_core::evidence::TestStatus, color: bool) -> String {
        wrap(color, "36", &format!("({s})"))
    }
}
