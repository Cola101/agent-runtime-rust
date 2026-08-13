# Contributing

Thank you for considering a contribution to Agent Runtime Rust. The project is in technical alpha,
so small, evidence-backed changes are preferred over broad rewrites.

## Before opening a change

1. Read `docs/project-goal.md`, `docs/implementation-status.md`, and the relevant ADRs.
2. Open an issue before changing a public contract, durable state schema, security boundary, or
   architecture decision.
3. Keep the Rust runtime protocol-neutral and independently runnable. Java, Docker, Kubernetes,
   PostgreSQL, and NATS must not become prerequisites for completing a local Run.
4. Do not commit credentials, `.local/` state, generated build output, or external service data.

## Development checks

Run the narrowest relevant tests while developing, then use the workspace gates before requesting
review:

```bash
cd runtime
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

If a change affects Java, Vue, contracts, or deployment files, also run the matching repository
targets documented in the root `Makefile`. Tests that require live external providers must remain
explicit and must never record API keys.

## Pull requests

- Explain the user-visible or runtime problem, the chosen boundary, and alternatives considered.
- Add a failing regression test before fixing a defect when practical.
- Update the implementation status and ADR/evidence record when a claim or durable contract changes.
- State what was verified locally and what remains unverified.
- Preserve third-party license headers and update `NOTICE` and `docs/third-party-sources.md` for any
  adapted or copied source.

By contributing, you agree that your contribution is licensed under Apache License 2.0.
