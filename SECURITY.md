# Security Policy

## Aftermath's security posture

Aftermath is a local, offline, read-only analysis tool. Its threat surface is
correspondingly small, and its safety properties are enforced in code, not by policy
you have to trust:

- **Read-only on your repository.** Aftermath writes only its own disposable cache
  under `.aftermath/`. It never modifies tracked files. (The `watch` hook reads the
  staged index and prints a report; it does not commit.)
- **No shell, no injection.** Anything Aftermath executes is spawned as a program with
  an argv array (`Executor` → `std::process::Command`), never interpolated into a shell
  string. A file named `; rm -rf / ` is just a filename.
- **No execution without consent.** Tests run only when you pass `--run-tests`. There is
  no background activity, no daemon, and no telemetry.
- **Offline by default.** The tool makes no network calls. `--offline` merely asserts
  this.
- **Secret and path redaction.** Before text reaches a report, credential-shaped
  strings and absolute machine paths are redacted (best-effort — see
  [docs/LIMITATIONS.md](docs/LIMITATIONS.md)).

## What Aftermath is *not*

- It is **not** a data-loss-prevention system. Redaction is a safety net, not a
  guarantee. Review any report before sharing it externally.
- It is **not** a security scanner. It flags and redacts secret shapes it happens to
  see in a diff; that is not a substitute for a dedicated secrets/SAST tool.
- It does **not** vouch for the correctness of your code. It reports structure and
  evidence.

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Instead, report privately via GitHub Security Advisories ("Report a vulnerability" on
the Security tab), or email the maintainers if you have a private contact. Include:

- a description and the affected version (`aftermath --version`),
- steps to reproduce,
- the impact you believe it has.

We aim to acknowledge reports within a reasonable window and will coordinate disclosure
with you. Given Aftermath's read-only, offline design, most reports are expected to be
low severity, but we treat every report seriously.

## Supported versions

The latest released version receives security fixes. Until 1.0, the project makes no
stability guarantee; fix forward on `main`.
