---
name: Bug report
about: Aftermath produced output that is wrong, misleading, or crashed
title: "[bug] "
labels: bug
---

**Before you file:** if Aftermath reported something as an "unknown" or "blind spot",
that is expected behavior, not a bug — unless it was phrased as if it were certain.

**What happened**
A clear description of what Aftermath did.

**What you expected**
What you expected it to do instead, and why (per docs/EVIDENCE_MODEL.md or
docs/LIMITATIONS.md).

**Repro**
```
# commands, and the smallest repo/snippet that shows it
aftermath inspect
```

**Aftermath output** (redact any secrets — Aftermath helps, but double-check)
```
<paste>
```

**Environment**
- Aftermath version: (`aftermath --version`)
- `aftermath doctor` output:
```
<paste>
```
- OS, and whether a `git` binary is on PATH:

**Confidence/intent**
Did Aftermath state a *conclusion* it couldn't back with evidence (a confidence that
doesn't match the evidence)? If so, describe the mismatch — this is the kind of bug we
treat most seriously.
