# Architecture

Aftermath is a two-crate Rust workspace:

```
aftermath/
├── crates/
│   ├── aftermath-core/   # the engine: pure, UI-agnostic, testable
│   └── aftermath-cli/    # the interface: arg parsing + rendering
├── docs/
├── Cargo.toml            # workspace
└── README.md
```

The split is deliberate: `aftermath-core` knows nothing about terminals, colors, or
argument parsing, and `aftermath-cli` re-implements no analysis. If you can call it
from the CLI, you can call it from a test — which is why the core has no `main()` and
no stdout.

## The pipeline

`aftermath inspect` runs `analyze::analyze`, which is a straight-line pipeline:

```
GitRepo::changes ──► filter (.aftermath/, excludes)
        │
        ▼
Workspace::build ─── walk + parse (new AND old side of every changed file)
        │
        ▼
resolve_modules ──── import/export bindings, edges, blind spots
        │
        ▼
ImpactEngine ──────── resolve references to symbols (confidence per link)
        │
        ▼
per-change ledger ─── locate enclosing declarations, collect affected symbols
        │
        ▼
tests::discover / collect ── evidence + (optional, consensual) execution
        │
        ▼
unknowns + suggestions + blind spots
        │
        ▼
Report::finalize ──── sort, redact, summarize ──► render (json / markdown / terminal)
```

Everything upstream of `Report::finalize` is deterministic. The finalizer sorts every
collection by a stable key, normalizes paths to `/`-separated repository-relative form,
and recomputes the summary — so the only field that varies between runs is the
timestamp, which `Report::normalized()` strips for comparisons.

## `aftermath-core` modules

| Module | Responsibility |
|---|---|
| `evidence.rs` | The evidence model: `SymbolRef`, `Confidence`, `Evidence`, `LedgerEntry`, `TestSuggestion`, `UnknownItem`, `BlindSpot`, `HistoryReport`, `Reproducibility`, … The shared vocabulary every other module speaks. |
| `report.rs` | The `Report` envelope, `finalize`/`normalized`, and the three deterministic renderers (JSON, Markdown, colored terminal). Also `default_language_support`. |
| `analyze.rs` | Orchestration (`analyze`, `history_report`). The only module that wires the others together. |
| `gitrepo.rs` | Git access via `git2` (libgit2). Reads changes for worktree/staged/revision scopes with exact hunk lines. Contains `testkit` (the only repository-writing code in the crate), used to build throwaway fixtures. |
| `workspace.rs` | Repository indexing: walk with excludes, parse, resolve module specifiers, build the binding and export tables, and record every skip as a blind spot. |
| `impact.rs` | The impact engine. Indexes every reference by name, then resolves each against the workspace to attach a relationship + confidence. |
| `lang/` | The `LanguageAdapter` trait and the JavaScript/TypeScript adapter (tree-sitter). |
| `tests.rs` | Test discovery and consent-gated execution. Distinguishes unit tests (inside source files, never executed) from runnable integration tests. |
| `executor.rs` | `Executor` trait + `LocalExecutor`. Spawns a program with an argv array — **no shell** — and is stubbed in tests. |
| `cache.rs` | Content-hash parse cache at `.aftermath/cache.json`. Disposable; deleting it is always safe. |
| `config.rs` | `aftermath.toml` parsing (`deny_unknown_fields`) plus the `**` glob matcher. |
| `redact.rs` | Secret- and path-redaction for anything that reaches a report. |
| `time.rs` | RFC 3339 timestamps without a heavyweight date library. |
| `evidencediff.rs` | Feature G: diff two reports into an `EvidenceDiff` and render it. |

## `aftermath-cli`

- `args.rs` — the clap definitions (`inspect`, `why`, `gaps`, `verify`, `report`,
  `history`, `watch`, `doctor`, `schema`, `init`).
- `main.rs` — resolves input (repo, config), calls the core analyzer, and renders. It
  also owns the `aftermath watch` hook script and the `schema` output (embedded via
  `include_str!`).

## Key design decisions

**Two crates, not seven.** The suggested seven-crate split added indirection without
separation here: `aftermath-core`'s modules are already cohesive and its public
surface is enforced by `pub` boundaries. A second crate would be sufficient only if we
later embed the engine in other front-ends.

**`git2` over shelling out to `git`.** No network, no version skew, no argument
injection. libgit2 is vendored, so the build needs no `git` binary on `PATH`.

**tree-sitter for JS/TS.** Syntax-level analysis that recovers from errors rather than
failing. The `LanguageAdapter` trait keeps the evidence model language-agnostic:
adding a language is a new adapter, not a change to `impact.rs`.

**Content-hash cache, no database.** A JSON cache keyed by the SHA-256 of file content.
Simpler than SQLite, disposable, and honest: it only avoids re-parsing unchanged files,
and it is never consulted when `cache.enabled = false`.

**Module resolution is per-file and bounded.** `resolve_specifier` handles relative,
`.js`→`.ts`, and `index.*` conventions with a candidate list; re-export chains are
followed to a bounded depth. Anything unresolved becomes a blind spot, not a guess.

**Confidence is computed, not scored.** See [EVIDENCE_MODEL.md](EVIDENCE_MODEL.md).
The impact engine's resolution table is documented at the top of `impact.rs` and is the
single source of truth for how a reference becomes a confidence class.

## Testing

- **Unit tests** live beside the code they cover (`#[cfg(test)] mod …`) and stub the
  `Executor` so no test ever spawns a process or waits on the environment.
- **Integration tests** (`crates/aftermath-cli/tests/cli.rs`) build real committed
  fixtures with `gitrepo::testkit`, then drive the *built binary* via
  `env!("CARGO_BIN_EXE_aftermath")` and assert on output and exit codes.
- The core has no clock or environment dependency in its logic; `analyze` takes an
  optional pinned `generated_at` so report tests are deterministic.

Run everything from the workspace root:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```
