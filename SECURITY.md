# Security Policy

## Project status

Agent Runtime Rust is a technical alpha and is not yet suitable for production workloads or
sensitive regulated data. Security-sensitive boundaries and known gaps are documented in
`docs/implementation-status.md` and `docs/security/threat-model.md`.

## Reporting a vulnerability

Please do not disclose a suspected vulnerability in a public issue. Use GitHub's private
vulnerability reporting feature for this repository. Include the affected revision, impact,
reproduction steps, and any suggested mitigation. If private reporting is not available, open a
minimal public issue asking the maintainer to enable a private security channel, without including
exploit details.

The maintainer will acknowledge a complete report as soon as practical, assess severity, and keep
the reporter informed before coordinated disclosure. No fixed response SLA is promised during the
technical-alpha stage.

## Supported versions

Only the current `main` branch is evaluated for security fixes during technical alpha. There are no
supported release branches yet.

## Scope reminders

- Never include API keys, model credentials, workload signing keys, certificates, or tenant data in
  reports, tests, logs, or evidence files.
- A public repository, passing tests, or a documented design does not establish production-grade
  sandboxing or tenant isolation.
- Reports about Tool/MCP execution, path and symlink handling, process supervision, workload
  identity, approvals, checkpoints, model credential isolation, and cross-tenant access are
  especially valuable.
