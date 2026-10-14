<!--
Thanks for contributing to Aftermath. Please keep the tool's core principle in mind:
evidence before speculation, honesty over theater. A PR must not add fabricated
confidence or claim that "no impact found" equals "safe".
-->

**What this does**


**Why**


**Checklist**
- [ ] `cargo test --workspace` passes.
- [ ] `cargo clippy --workspace --all-targets` is warning-free.
- [ ] `cargo fmt --all -- --check` is clean.
- [ ] If this makes Aftermath assert something new about code: it cites evidence, uses
      the strongest confidence the evidence supports, and updates
      docs/EVIDENCE_MODEL.md + the JSON Schema.
- [ ] If this changes user-facing behavior: CHANGELOG.md is updated.
- [ ] No network calls; no shell; no execution without consent; reports stay
      deterministic (timestamp aside).
