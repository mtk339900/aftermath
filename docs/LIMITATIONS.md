# Limitations

Aftermath is deliberately honest about its scope. This document states exactly what it
does **not** do, so a report is never mistaken for more than it is. When Aftermath
reaches one of these edges, it reports a **blind spot** or an **unknown** rather than
guessing.

## Language support

- **TypeScript/JavaScript only.** `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`. Files in
  other languages (Python, Go, Rust, …) are reported as an `unsupported_language` blind
  spot; their symbols are invisible to the analysis.
- **Syntax-level, not semantic, analysis.** There is no type checker, no constant
  propagation, and no data-flow analysis. This has concrete consequences:
  - A member call `obj.method()` is attributed only when `obj` is bound to a known
    import or `this` inside the declaring class. Otherwise it is `possible_impact` (a
    name match), because without types Aftermath cannot know `obj`'s shape.
  - Similarly named functions in unrelated modules are `possible_impact`, never
    confirmed, unless an import connects them.

## What blocks resolution entirely

These produce an `unknown`, not an answer:

- **Dynamic dispatch** — `eval`, `new Function`, reflection, framework DI, event-bus
  registration. Targets reached only at runtime cannot be resolved statically.
- **Computed member access** — `obj[key]` where `key` is not a string literal.
- **Non-literal dynamic import** — `import(variable)`, `require(variable)`.
- **Unresolved imports** — a specifier that matches no file in the repository (bare
  package specifiers are external by definition).

## Framework and metaprogramming blind spots

Aftermath follows code structure, not framework conventions. It does not understand:

- decorators / annotations that register behavior (Angular-style DI, NestJS, class-validator),
- React/Vue/Svelte component contracts, hooks, or JSX event wiring,
- dependency-injection containers and inversion-of-control wiring,
- code generated at build time from templates or macros.

Where these dominate, treat the ledger as a lower bound on impact, not an upper bound.

## Test execution

- Aftermath executes only standalone integration test files under a recognized test
  directory (for JS: `tests/**/*.test.js`, run with `node --test`), and **only with
  explicit consent** (`aftermath verify --run-tests`).
- Unit tests declared *inside* source files (`test(...)`, `it(...)`, `function testX()`)
  are discovered and reported but **never executed** — they belong to the project's own
  toolchain (jest/vitest/cargo test), which is outside Aftermath's offline, read-only
  contract. Their status is `unavailable`, with the reason recorded.
- If `node` is not installed, integration tests are `unavailable`, never `passed`.

## Redaction is best-effort

`redact.rs` removes common credential shapes (key/value secrets, AWS/GitHub/Slack/OpenAI
token patterns, JWTs, PEM blocks) and replaces absolute machine paths with portable
placeholders. It is a safety net for shared reports, **not** a data-loss-prevention
system. Always review a report before sharing it externally.

## History is correlation, never proof

`aftermath history` reports files that changed together in the past. Co-occurrence is
labeled correlation everywhere it appears; it is never presented as evidence of a
technical dependency.

## Scale and bounds

- Transitive traversal is depth-bounded (`analysis.depth`, default 3).
- The ledger is capped (`MAX_LEDGER_ENTRIES`) and suggestions are capped per entry;
  affected-symbol lists are capped too. On very large changes or repos, results are a
  bounded sample, not exhaustive.
- Files larger than `analysis.max_file_bytes` (default 1 MiB) and binary files are
  skipped and reported as blind spots.

## Repository scope

- Aftermath reads a Git repository and requires a working directory (a bare repository
  has nothing to analyze).
- Rename detection is best-effort; heavily rewritten renames may appear as separate
  delete + add changes.
- The only thing Aftermath writes is its own cache under `.aftermath/`. It never
  modifies tracked files. (Its `watch` hook *reads* the staged index; it does not commit.)

## Determinism

Two runs over the same change produce identical reports except for the
`reproducibility.generated_at` timestamp, which `Report::normalized()` removes so runs
can be compared. There is no other nondeterminism: collections are sorted by stable
keys and paths are normalized to repository-relative, `/`-separated form.

## Not what Aftermath is

- It is not a linter, formatter, or a replacement for your test suite or CI.
- It is not a security scanner (though it redacts secrets and flags them).
- It is not a type checker or a full program analysis framework.
- It does not connect to any service, and it has no daemon or background process.
