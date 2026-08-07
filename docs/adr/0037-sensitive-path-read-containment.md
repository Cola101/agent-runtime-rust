# ADR-0037: Sensitive-path read containment

## Status

Accepted. **Corrects ADR-0036 decision 4.**

## Context

ADR-0036 recorded, as a measured finding, that reads cannot practically be
restricted under Seatbelt, and concluded that containment protects integrity and
exfiltration but not confidentiality. It stated that Codex "allows `file-read*`
outright".

That conclusion was reached from one experiment — every read *allowlist* tried
aborted the process before it ran (SIGABRT, dyld) — and then generalised too far.
Re-reading Codex's `codex-rs/sandboxing/src/seatbelt.rs` shows it restricts reads
in production, by a shape the earlier experiment never tried:

- when the policy has full disk read access **and** unreadable roots exist, Codex
  builds a readable root of `/` with `(require-not …)` carve-outs
  (`seatbelt.rs:687-695`);
- separately it emits `(deny file-read* (regex …))` per unreadable glob
  (`seatbelt.rs:467`), appended **after** the read policy in the assembled
  sections (`seatbelt.rs:741-747`), relying on SBPL resolving by last match.

Both are allow-everything-then-carve-out. Neither is an allowlist. The ADR-0036
finding about allowlists is correct; the conclusion drawn from it was not.

This matters now rather than later: ADR-0036's own text says the unprotected-read
row is tolerable only while Tools read and write Workspace text, and a Shell Tool
is the next capability on the roadmap. A Shell Tool over a container that can
read `~/.ssh` is not something to ship and fix afterwards.

## Decision

1. **Credential directories are denied to every contained Tool.** The set is
   `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`.

2. **Denials are emitted as `(deny file-read* …)` after the blanket
   `(allow file-read*)`**, matching Codex's glob-denial shape. SBPL resolves by
   last match, so ordering is load-bearing and is asserted by test.

3. **Each denial covers `subpath` and `literal`.** `subpath` alone leaves the
   directory node itself reachable, so the directory could still be stat'd and
   enumerated. Codex makes the same pairing for its excluded subpaths and records
   the same reason (`seatbelt.rs:381-389`).

4. **Writes to the denied paths are denied too.** Outside a Workspace this is
   already implied by `(deny default)`, but a denied directory sitting *inside* a
   writable Workspace would otherwise stay reachable through create and unlink.

5. **Paths are passed as numbered `-D` parameters**, never interpolated into the
   profile text — the same rule ADR-0036 decision 3 set for the Workspace root.

6. **Paths are normalized before entering the profile.** Seatbelt evaluates the
   path the kernel resolves. On macOS `/var` and `/tmp` are symlinks into
   `/private`, so an unresolved prefix yields a rule that silently never matches.
   Normalization resolves the longest existing ancestor and re-attaches the
   remainder, because a protected directory need not exist yet.

7. **`sensitive_read_denials` takes `home` as an argument** rather than reading
   `$HOME` internally, so tests exercise real containment against a temporary
   directory. No test reads, writes, or names the developer's real credential
   directories.

8. **The home directory is resolved from `$HOME` with a passwd-database
   fallback, and an unresolvable home refuses the launch.** `std::env::home_dir`
   provides the fallback on Unix, so a Worker started without `HOME` exported is
   still contained rather than silently unprotected. If no home can be resolved
   at all, `prepare` returns `ToolExecutionError::ContainmentUnavailable` and the
   Tool does not run. The Worker maps it to its own error code,
   `tool_containment_unavailable`, kept distinct from `tool_execution_failed`
   because it is a containment-posture failure rather than a misbehaving Tool.

   *(Added after the original decision; it closes the gap the first version of
   this ADR recorded under Limitations.)*

## Consequences

### Positive

- Credential theft through a defective or subverted trusted Tool is now blocked
  by the kernel rather than by the Tool's own good behaviour.
- The precondition ADR-0036 named for a Shell Tool is met.
- The normalization requirement is now explicit, so the next path-based rule
  cannot repeat a silently-inert policy.

### Negative

- Confidentiality is protected **only for the enumerated directories**. Every
  other readable file remains readable. This is narrower than "reads are
  contained" and must not be described as the latter.
- The list is fixed in code. Adding a directory needs a code change; there is no
  tenant-level configuration.
- macOS only. Linux Workers still get no containment; `landlock` is the
  equivalent and remains unimplemented.

### Neutral

- Profile length grows by two rules per denied path.

## Failure Modes

- A denied path that does not exist: the rule is still emitted against the
  normalized path, and matches once the directory appears.
- A Workspace that *is* the home directory: the denials still apply, so a Tool
  cannot reach credentials by having its Workspace pointed at `$HOME`.
- `$HOME` unset: the home directory is resolved from the passwd database
  instead, so containment holds. Verified by
  `credential_denials_survive_an_unset_home_variable`, which removes `HOME` in
  its own process and asserts the denials are still emitted.
- No home resolvable at all: the launch is refused with
  `ContainmentUnavailable`. The model receives a bound Tool error carrying
  `tool_containment_unavailable`; nothing runs uncontained.

## Limitations

- Not verified against a real Shell Tool, because none exists yet.
- The refusal path itself has never fired on a real host: every machine tested
  resolves a home directory. It is proven by unit test, not by observation.
- Links were measured and do not bypass the denial
  (`docs/evidence/2026-08-07-link-bypass-measurement.md`): the kernel applies
  the rule to a symlink's resolved path, and hard links cannot be created inside
  the container at all — though that second protection comes from `file-link`
  never being granted, **not** from this ADR's denial. Still untested:
  `/dev/fd`-style paths, inherited file descriptors, mount points and firmlinks,
  and case variants on a case-insensitive filesystem.
- No test asserts the denial survives a hard link or a symlink *into* a denied
  directory created from outside the sandbox.

## Alternatives Considered

- **Read allowlist:** rejected on the ADR-0036 evidence, which stands. Every
  allowlist tried aborted the process before it ran.
- **Keep ADR-0036's position and gate only the Shell Tool:** rejected. The
  weakness is in the container, so every current and future Tool carries it.
- **Codex's `require-not` under a `/` readable root:** equivalent in effect and
  also viable. The append-deny form was chosen because our protected paths are
  fixed directories, not globs, so it needs no regex construction and no
  escaping.

## Attribution

The allow-then-deny shape, the `subpath` + `literal` pairing, and the need to
normalize paths before they enter the profile are all adapted from OpenAI Codex,
`codex-rs/sandboxing/src/seatbelt.rs` (Apache-2.0). No Codex source is copied;
the profile construction here is written for this platform. Recorded in `NOTICE`.

## References

- ADR-0025 trusted native development Tools
- ADR-0036 Seatbelt-contained trusted Tools and the first Workspace write
- Codex `codex-rs/sandboxing/src/seatbelt.rs`
- Codex `codex-rs/sandboxing/src/seatbelt_base_policy.sbpl`
