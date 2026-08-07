# ADR-0036: Seatbelt-contained trusted Tools and the first Workspace write

## Status

Accepted. Amends ADR-0025 decision 5.

## Context

ADR-0025 shipped exactly one trusted native Tool, `workspace.read_text`, and made
its safety rest on the binary itself: registered by digest, revalidated before
every spawn, no shell, no argument concatenation, model input only through
bounded JSON on stdin. That is a strong supply-chain boundary and no runtime
boundary at all. Nothing stopped a defect in a trusted binary, or a crafted
argument it mishandled, from writing anywhere the user could write or opening a
socket.

That was tolerable while the only Tool could not write. It stops being tolerable
the moment a Tool can, and an Agent that can only read is not useful enough to
build a product on.

Codex solves the same problem on macOS with Seatbelt (`codex-rs/sandboxing`).
Two of its decisions turned out to be load-bearing, and one of them is the
opposite of what we assumed.

## Decision

1. **Every trusted native Tool launch on macOS is wrapped in Seatbelt.** The
   executor spawns `/usr/bin/sandbox-exec` with a generated profile and the
   registered executable after `--`. Registration, digest pinning, and
   revalidation from ADR-0025 are unchanged; containment is added beneath them,
   not instead of them.

2. **`/usr/bin/sandbox-exec` is referenced by absolute path.** Resolving it
   through `PATH` would let anyone able to write a `PATH` entry replace the
   sandbox with a no-op and lose containment silently.

3. **The Workspace path is passed as a Seatbelt `-D` parameter, never
   interpolated into the profile text.** A Workspace path containing profile
   syntax would otherwise be able to rewrite the policy that is supposed to
   contain it.

4. **Reads are not restricted.**
   **Superseded by ADR-0037.** The measurement below is correct — a read
   *allowlist* does abort the process — but the conclusion drawn from it was
   too broad. Codex does restrict reads, using allow-everything-then-carve-out
   rather than an allowlist, and ADR-0037 adopts that shape for credential
   directories. The claim here that Codex "allows `file-read*` outright" is
   wrong; it holds only when no unreadable roots are configured. Read the rest
   of this decision as the original reasoning, not as current behaviour.

   This is deliberate and is the decision that
   most needs writing down. An allowlist tight enough to be meaningful prevents
   dyld from loading and the process aborts before it runs; measured here as
   SIGABRT on every variant tried. Codex reaches the same conclusion and allows
   `file-read*` outright. Therefore:

   | Property | Contained |
   | --- | --- |
   | Writing outside the Workspace | yes |
   | Outbound network | yes |
   | Reading outside the Workspace | **no** |

   Containment protects **integrity and exfiltration, not confidentiality**. A
   contained Tool still reads whatever its user can read. No document, log, or
   interface may describe these Tools as "sandboxed" without that qualification.

5. **No network rule is ever emitted**, so `(deny default)` keeps every socket
   operation closed. Tools that need network access will need their own ADR and
   their own explicit policy; they do not inherit one.

6. **`WorkspaceAccess::ReadWrite` is added, and grants writes only beneath the
   canonical Workspace root.** `ReadOnly` Tools get no write rule at all beyond
   the standard character devices.

7. **`workspace.write_text` is the first write-capable Tool**, amending ADR-0025
   decision 5's "no write capability". It declares `non_idempotent`, `ask`, and
   `tool:workspace.write`, so it is approval-gated and is never auto-retried
   after an ambiguous failure.

8. **The Tool keeps its own path discipline.** Absolute paths, any non-normal
   component, and a symlink at any existing ancestor are refused; a missing
   parent directory is refused rather than created; a target that exists and is
   not a regular file is refused. Containment is the outer boundary and this is
   the inner one, so a gap in either alone does not let a write escape.

## Consequences

### Positive

- A defect in a trusted binary can no longer write outside the Workspace or
  reach the network.
- The Agent can produce work, not only read it.
- The same containment applies to every future trusted Tool without per-Tool
  work.

### Negative

- macOS only. Linux Workers get no containment from this ADR; `landlock` is the
  equivalent and is not implemented.
- `sandbox-exec` is formally deprecated by Apple while remaining the only
  practical interface to Seatbelt.
- Confidentiality is explicitly not protected, which must be repeated wherever
  containment is described.

### Neutral

- Launch shape changed: the registered executable now appears in argv after
  `--` instead of as the program. Tests assert the executable that actually runs
  rather than the shape.

## Failure Modes

- `/usr/bin/sandbox-exec` missing or non-executable: the launch fails; it does
  not silently run uncontained.
- Profile rejected by the kernel: the process aborts before running the Tool.
- Write refused by containment: surfaces as a normal bounded Tool error, so the
  model sees a refusal rather than a hang.

## Alternatives Considered

- **Restrict reads as well:** rejected on evidence. Every allowlist tried
  aborted the process before it ran, and Codex does not attempt it either.
- **Trust the binary and skip containment:** rejected. That is the status quo
  ADR-0025 established, and it does not survive adding a write capability.
- **Run Tools in containers locally:** rejected by ADR-0024; the native
  development runtime excludes Docker.
- **Write our own sandbox policy from scratch:** rejected. A hand-rolled policy
  would repeat the read-restriction mistake that cost this round several
  iterations to discover.

## Attribution

The Seatbelt approach here is adapted from OpenAI Codex, `codex-rs/sandboxing`
(Apache-2.0): the absolute `sandbox-exec` path and its rationale, passing paths
as profile parameters, and the finding that reads cannot practically be
restricted. No Codex source is copied; the profile and executor integration are
written for this platform. Recorded in `NOTICE`.

## References

- ADR-0024 native macOS development runtime
- ADR-0025 trusted native development Tools
- Codex `codex-rs/sandboxing/src/seatbelt.rs`
- Codex `codex-rs/sandboxing/src/seatbelt_base_policy.sbpl`
