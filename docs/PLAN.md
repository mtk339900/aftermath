# Aftermath — Implementation Plan

Environment observed on this machine:

- Windows 10/11, `x86_64-pc-windows-msvc`, rustc/cargo 1.99.0 stable, rustfmt + clippy available.
- **No `git` binary on PATH** (checked common install locations). Git access must therefore go
  through a library, not subprocesses. Fixture repositories must be created programmatically.
- No `cmake`; native dependencies must build through the `cc` crate with MSVC cl.exe.
- Node.js v24 + npm present (usable as a *test runner* for fixtures, never auto-executed).
- crates.io reachable (build-time dependency fetch only; the tool itself is offline).

Dependency probes (actually compiled and executed in `scratch/` before use):

| Crate | Version resolved | Probe result |
|---|---|---|
| `git2` (default-features off) | 0.20.4 | builds; libgit2 vendored via `cc` works |
| `tree-sitter` + `tree-sitter-javascript` | 0.25.x / 0.25.0 | parses JS |
| `tree-sitter-typescript` | 0.23.2 | parses TS |
| `clap` 4, `serde`, `serde_json`, `anyhow`, `thiserror`, `similar` | current | build |

Decisions:

1. **Two-crate workspace** (`aftermath-core`, `aftermath-cli`). The core owns the evidence model,
   analysis, and JSON/Markdown output; the CLI owns argument parsing, terminal rendering, and exit
   codes. Splitting further would add ceremony without benefit at this size.
2. **`git2`, not subprocess git.** No git binary exists here, and libgit2 gives diff hunks with
   line ranges (needed to map changed lines to symbols), history walking, and index/status access.
3. **tree-sitter for JS/TS**, one `LanguageAdapter` interface so more languages can be added later.
4. **No database.** A content-hash parse cache in `.aftermath/cache.json` provides incremental
   analysis; a database would add setup cost with no benefit at this scale.
5. **Test execution uses direct process spawning** (no shell), requires explicit consent, and
   records `passed / failed / unavailable / not-executed` as distinct states.
6. Node's built-in test runner (`node --test`) is used by fixtures so integration tests exercise
   real test execution offline with zero package installs.

Phases (mapping to the master prompt §12):

1. ✅ Inspect workspace/toolchain/platform — done, recorded above.
2. ✅ This plan.
3. Evidence data model + CLI contract (types, schema, config, exit codes).
4. Vertical slice: open repo → read change → changed symbol → direct relationships → traceable report.
5. Test discovery + Unknowns report.
6. Deterministic JSON + Markdown output (+ JSON Schema).
7. Security controls (redaction, path guards, size limits), regression fixtures, perf measurement.
8. README + docs written from real program output.
9. Full test run, fmt, clippy, release build, release-readiness review.
10. Completion report.
