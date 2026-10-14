# Contributing to Aftermath

Thanks for your interest in Aftermath. It is a tool built on a specific principle —
**evidence before speculation, honesty over theater** — and contributions are expected
to uphold it. This document explains how to build, test, and extend it.

## Ground rules

1. **Never fabricate certainty.** No invented confidence percentages, no claiming an
   absence of found relationships equals safety. If analysis cannot prove something, it
   must be labeled `unknown` or reported as a blind spot.
2. **Read-only + consent.** Core never modifies user source and never runs a test
   without explicit consent. New features must preserve this.
3. **No shell.** Anything that executes a process spawns an argv array through the
   `Executor` trait — never a shell string.
4. **Offline.** No network calls in the tool itself.
5. **Determinism.** Reports must be byte-identical across runs except for the timestamp.

## Setup

```sh
git clone <your-fork>
cd aftermath
cargo build
cargo test --workspace
```

You do **not** need `git` on your PATH (Aftermath uses libgit2), nor `node` (only needed
to run the JS integration tests during `verify --run-tests`).

## Workflow

- `cargo build --workspace`
- `cargo test --workspace` — unit + integration tests
- `cargo clippy --workspace --all-targets` — must be warning-free
- `cargo fmt --all` — then `cargo fmt --all -- --check`

Please keep clippy and rustfmt clean before opening a PR.

## Where things live

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full map. The short version:

- Analysis logic goes in `aftermath-core`, which has no CLI dependencies. If you need to
  print something, return a model and let the CLI render it.
- The evidence vocabulary (`evidence.rs`) is the contract. Add a variant there first if
  you need a new kind of finding, then wire it through.
- Rendering goes in `report.rs` (terminal/markdown) or the CLI.

## Adding a language

The language layer is the `LanguageAdapter` trait in
[`crates/aftermath-core/src/lang`](crates/aftermath-core/src/lang). A new adapter:

1. Implements `language_name`, `extensions`, and `parse(path, source) -> ParsedFile`.
2. Extracts declarations (`Decl`), references (`Ref`), exports (`ExportBinding`), and
   tests (`TestDecl`) from a tree-sitter grammar.
3. Records what it cannot understand (parse errors, dynamic dispatch) so the workspace
   can turn those into blind spots.

Then register it in `lang::adapter_for`. The impact engine, evidence model, and report
do not change — that separation is the point.

## Adding a feature that makes a claim

If your change makes Aftermath assert anything about user code, it must:

- attach the **strongest** confidence the evidence actually supports,
- cite the evidence (file, line),
- add the reasoning to [docs/EVIDENCE_MODEL.md](docs/EVIDENCE_MODEL.md) and the
  [schema](../crates/aftermath-cli/schema/aftermath-report.schema.json).

If it cannot cite evidence, it belongs in `unknowns` or `blind_spots`, not the ledger.

## Tests

- Prefer stubbing the `Executor` over spawning processes.
- Use `gitrepo::testkit` to build throwaway committed repositories for integration
  tests; never point test code at a real repository.
- Pin `AnalysisOptions::generated_at` in any test that inspects a whole report so
  snapshots are stable.

## Reporting bugs

Use the GitHub issue templates. Include Aftermath's output, your platform, and whether
`aftermath doctor` passes. See [SECURITY.md](SECURITY.md) for security issues.

## License

By contributing, you agree that your contributions are dual-licensed under the
project's [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses.
