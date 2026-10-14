# The Evidence Model

This document is the contract for how Aftermath reasons about your code. Every claim
in a report traces back to one of the categories defined here. The model is
language-agnostic; the [language adapters](../crates/aftermath-core/src/lang) feed it.

Two rules govern everything:

1. **Evidence before speculation.** A relationship is only ever *confirmed* when a
   structural link exists. Everything weaker is labeled *possible* or *unknown*.
2. **Absence is not safety.** "I found no affected symbols" is reported as an
   *unknown*, never as "this change is safe."

## Confidence

Confidence is a **classification, never a percentage.** A number would imply a
probability Aftermath cannot compute. The classes, strongest first:

| Class | Meaning |
|---|---|
| `confirmed_relationship` | A resolved, structural link exists: a call to a locally declared or import-resolved symbol; a diff hunk covering the declaration; `this.method()` inside the declaring class; `ns.method()` resolving through a namespace import; an `extends`/type reference resolved through imports. |
| `strongly_supported_impact` | Multiple independent signals converge, or an all-confirmed chain was reached through bounded transitive traversal. |
| `possible_impact` | Plausible but unproven. A name match with no structural link, or a member call that cannot be attributed without type information. |
| `unknown` | Resolution was *blocked* (dynamic dispatch, unresolved import, parse error, computed access). This is a statement about the analysis, **not** a statement that no relationship exists. |
| `verified_by_executed_test` | An executed test exercises the code and its recorded result is *pass*. (Describes verification, not relationship strength, so it sorts lowest for impact ranking.) |
| `not_verified` | No relevant test was identified, or identified tests were not executed. |

Each ledger entry carries the strongest *impact* classification found among its
affected symbols, unless an executed passing test upgrades it to
`verified_by_executed_test`.

## Transitive confidence propagation

When Aftermath walks further than one hop, confidence is the **weakest link** on
the path: a chain of exclusively confirmed links becomes `strongly_supported_impact`;
a chain containing a single name-only match stays `possible_impact`. Traversal is
bounded (`analysis.depth`, default 3) so a large graph cannot explode.

## The four test states

A tool that runs no tests must never imply they passed. Aftermath records exactly one of:

| State | Meaning |
|---|---|
| `passed` | Executed; observed to pass (exit 0). |
| `failed` | Executed; observed to fail (non-zero exit, or the runner could not be spawned — a missing runner is `unavailable`, never `passed`). |
| `not_executed` | Identified, but not executed during this analysis (no consent given). |
| `unavailable` | Could not be executed — for example, a unit test that lives inside a source file and needs the project's own toolchain, or a missing `node`. |
| `skipped` | Explicitly skipped by the runner. |

Test execution requires explicit consent (`aftermath verify --run-tests`). Aftermath
never runs a shell: it spawns `node --test <file>` with an argv array, so a filename
is never interpreted as a command.

## Relationships

How an affected symbol relates to the changed one:

| Relationship | Trigger |
|---|---|
| `direct_caller` | A call resolves to the changed symbol. |
| `imports_symbol` | An import binding (alias-resolved) points at the symbol. |
| `inherits_from` | `extends` / `implements` resolved to the symbol. |
| `type_consumer` | A type annotation references the symbol. |
| `transitive` | Reached through further resolved links. |
| `test_consumer` | A test file references the symbol. |
| `uses_symbol` | Reads/uses the symbol outside a call. |

## Evidence

Each finding is backed by `Evidence` items, each with a kind, a location
(repository-relative file + line + a redacted, length-capped snippet) and a detail
line. Kinds:

- `git_diff_hunk`, `syntax_tree`, `call_site`, `import_edge`, `inheritance_edge`,
  `type_reference`, `test_reference`, `test_execution`, `history_correlation`,
  `name_match`, `config_declaration`.

The distinction `import_edge` / `call_site` / … vs `name_match` is what separates a
confirmed link from a possible one: a `name_match` is the weakest evidence and can
never by itself yield `confirmed_relationship`.

## Analysis methods

Every ledger entry records *how* its conclusions were reached, so a reader can audit:
`syntax_analysis`, `import_resolution`, `call_site_scan`,
`bounded_transitive_traversal`, `test_discovery`, `test_execution`,
`history_correlation` (correlation only), `name_heuristic` (the fallback used where
resolution failed).

## Blind spots and unknowns

These are the two ways Aftermath says "here is what I could not do."

- **Blind spots** describe *inputs that were not analyzed*: an unsupported language,
  an unresolved import, dynamic dispatch (`eval`/computed access), generated code, a
  binary or oversized file, an ignored directory, an import of a name the target file
  does not export. An analysis is never called complete while relevant inputs were
  skipped.
- **Unknowns** describe *behavior that was not verified*: a changed public symbol with
  no identified test, an unresolvable call site, tests that were not executed, a
  changed interface with unknown consumers.

Wording matters: an unknown says "no test referencing this symbol was identified," not
"no test exists." That distinction is deliberate and load-bearing.

## Reproducibility

Two runs over the same change produce the same report, byte for byte, except
`reproducibility.generated_at`. `Report::normalized()` removes that field so reports
can be compared across runs (this is what `aftermath inspect --baseline` uses to
render an evidence diff). The report also records the Aftermath version, the schema
version, the analyzed revision, the effective (redacted) configuration, the language
support notes, and every command Aftermath itself executed.

## Schema

The report schema is [published as JSON Schema](../crates/aftermath-cli/schema/aftermath-report.schema.json)
and is emitted by `aftermath schema`. `schema_version` is `1.0.0` and is bumped on any
breaking change.
