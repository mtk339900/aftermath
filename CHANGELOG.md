# Changelog

All notable changes to Aftermath are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
Semantic Versioning.

## [0.1.0] — Unreleased

Initial release.

### Added

**Analysis engine (`aftermath-core`)**

- **Change Consequence Ledger** — resolve what a change touches, with structural
  evidence and a classified confidence per link (`confirmed_relationship`,
  `strongly_supported_impact`, `possible_impact`, `unknown`, …).
- **Unknowns report** — first-class output for behavior the analysis could not prove;
  absence of discovered coverage is never presented as safety.
- **Counterfactual test suggestions** — structural, never executed (`boundary values`,
  `branch coverage`, `error path`, `signature change`, `multiple callers`,
  `malformed input`, `public API consumer`).
- **Test evidence with consent** — discover unit and integration tests; execute only
  with explicit consent, via an argv array (no shell). Four honest states: `passed`,
  `failed`, `not_executed`, `unavailable`.
- **Blind-spot detection** — unsupported languages, unresolved imports, dynamic
  dispatch, generated/binary/oversized files, ignored directories.
- **Repository archaeology** — file co-change across commits, always labeled
  correlation, never proof.
- **Evidence diff** — compare two runs (`inspect --baseline`) into a structured diff.
- **Reproducibility** — versioned, redacted, deterministic reports; the only varying
  field is the timestamp, which is normalized away for comparison.

**Interface (`aftermath-cli`)**

- Commands: `init`, `inspect`, `why`, `gaps`, `verify`, `report`, `history`, `watch`,
  `doctor`, `schema`.
- Shared flags: `--json`, `--no-color`, `--quiet`, `--verbose`, `--offline`,
  `--repo/-C`, `--config`.
- Stable exit codes (0/1/2/3/4), colored-but-optional terminal rendering, JSON Schema
  emission.
- TypeScript/JavaScript support via tree-sitter; a `LanguageAdapter` trait for adding
  languages.

**Docs and project hygiene**

- `README.md`, `docs/ARCHITECTURE.md`, `docs/EVIDENCE_MODEL.md`, `docs/LIMITATIONS.md`,
  `CONTRIBUTING.md`, `SECURITY.md`, JSON Schema, issue/PR templates, CI workflow.
- MIT OR Apache-2.0 dual licensing.
- Unit tests beside the code plus end-to-end CLI integration tests that drive the built
  binary against real Git fixtures.

[0.1.0]: https://github.com/mtk339900/aftermath/releases
