# ADR-0038: The Shell Tool

## Status

Accepted. Amends ADR-0025 decision 5 again (after ADR-0036 added writes).

## Context

Every Tool shipped so far does one narrow thing to one file. An Agent that can
only read and write single text files cannot build, test, search, or inspect
anything, which is most of the work.

A Shell Tool is different in kind, not degree: it hands arbitrary command
execution to the model. ADR-0025 built its safety on a rule that a Shell Tool
appears to break outright — *no shell, no argument concatenation, model input
only through bounded JSON on stdin*.

It is worth being precise about why that rule existed, because the reason does
not transfer. It exists so a crafted argument cannot alter a command **we**
composed. With a Shell Tool there is no command of ours to alter: the model
authors the whole thing by design. Argument parsing was never the boundary; it
was a way of keeping a boundary that lives elsewhere.

The boundary that does the work is containment, and it only just became strong
enough. ADR-0037 said so explicitly:

> A Shell Tool over a container that can read `~/.ssh` is not something to ship
> and fix afterwards.

That precondition is now met: credential directories are denied, the denial
survives an unset `$HOME`, and links do not bypass it — each measured against a
real `sandbox-exec`, each with a test that has been shown to fail when the
mechanism is removed.

## Decision

1. **The registered binary stays ours.** `shell.exec` is a third operation on
   `agent-trusted-workspace-tool`, not a registration of `/bin/sh`. The digest
   pinning, revalidation-before-spawn and bounded-JSON-on-stdin contract from
   ADR-0025 are unchanged; the model's command arrives as JSON, never as argv.

2. **The command is a shell string executed by `/bin/sh -c`.** Codex's shell tool
   takes the same shape (`core/src/tools/handlers/shell_spec.rs`: "Shell command
   to execute"). `/bin/sh` is referenced by absolute path for the reason
   ADR-0036 gives for `sandbox-exec`.

3. **The command environment is fixed, never inherited.** The executor already
   calls `env_clear()`, so the Tool starts with nothing; the Tool then sets
   exactly two variables:

   - `PATH=/usr/bin:/bin:/usr/sbin:/sbin` — system only. The Workspace is
     deliberately absent, so a binary the model writes next to its command is
     not reachable by bare name.
   - `HOME=<workspace>/.agent-home`.

   This is stricter than Codex, which inherits a core set minus `*KEY*`,
   `*SECRET*`, `*TOKEN*` patterns (`protocol/src/shell_environment.rs:82-84`).
   Pattern-based exclusion is a denylist over an inherited set; ours starts
   empty. The Worker process holds provider credentials, database passwords and
   NATS credentials, and none of it should depend on a name matching a pattern.

4. **`HOME` points at a dot-directory inside the Workspace, not at the Workspace
   root.** Measured: with `HOME` at the root, macOS frameworks create
   `Library/Caches/...` there on the first `git` or `python3` invocation, so the
   model's own directory fills with junk. `.agent-home` keeps `~` contained and
   the visible Workspace clean.

5. **A non-zero exit is a result, not a Tool error.** `exit_code`, `stdout` and
   `stderr` are returned with `is_error: false`. Only the Tool being unable to
   run the command at all is an error. Collapsing the two would make "the
   command failed" indistinguishable from "the side effects are unknown", and
   fail-closed applies to the second.

6. **Output is truncated per stream at 64 KiB**, with `stdout_truncated` and
   `stderr_truncated` reported, so a command cannot exhaust Worker memory
   through its result. Truncation lands on a character boundary, so the result
   is always valid UTF-8.

7. **Commands are bounded at 16 KiB**, and an empty command or one containing a
   NUL byte is refused before anything spawns.

8. **`shell.exec` requires its own scope, `tool:shell.exec`.** Granting shell is
   a different decision from granting file writes and must not ride along with
   one. It declares `non_idempotent` and `ask`, so every invocation is
   approval-gated and is never auto-retried after an ambiguous failure.

## Consequences

### Positive

- The Agent can build, test, search and inspect — the work an Agent is for.
- Nothing from the Worker's environment can reach a model-authored command,
  by construction rather than by pattern matching.
- The capability is separately grantable, separately approvable, and separately
  revocable from file access.

### Negative

- **This is the widest capability the platform has granted.** Everything the
  container permits, a command can now do: read any non-credential file the user
  can read, write anywhere in the Workspace, spawn any system binary.
- `/bin/sh` cannot be digest-pinned the way our own binaries are. It is a system
  file that changes with OS updates; we reference it by absolute path and rely on
  system integrity protection for the rest.
- Approving every command individually is heavy for real work. Codex mitigates
  this with command allow-lists; we do not have one, on purpose (see below).

### Neutral

- The Tool's `arguments` field is now untyped at the boundary and parsed per
  operation, because `shell.exec` no longer shares a shape with the file
  operations.

## Failure Modes

- Command spawn fails: reported as a Tool error, so fail-closed applies and the
  Run does not retry it.
- Command writes outside the Workspace: refused by Seatbelt, surfacing as an
  ordinary non-zero exit the model can read.
- Command reads a credential directory: refused by ADR-0037's denial.
- Command opens a socket: refused; no network rule is ever emitted.
- Command runs forever: bounded by the execution context's timeout, which
  already applies to every trusted native Tool.

## Limitations

- **No command allow-list.** Every command needs an approval. This is the
  conservative default for a first version; a list of commands that can run
  without approval is a real product need and a separate decision, because
  getting it wrong grants unattended execution.
- **No interactive or long-running sessions.** One command, one result. Codex
  has `unified_exec` with session ids for this; we do not.
- **No per-command review affordance beyond the existing approval payload.**
  The approver sees the command string, which is the right thing, but there is
  no diffing, no dry run, and no explanation.
- **The timeout is the generic Tool timeout**, not a shell-specific budget.
- **Untested against a command that forks and detaches.** A background process
  outliving the Tool is contained by Seatbelt but is not tracked or reaped.
- macOS only, like all containment here. On Linux a Shell Tool would currently
  run with no containment at all, which is why the Worker's Linux path must not
  register it before `landlock` exists.

## Alternatives Considered

- **An argv vector instead of a shell string:** rejected. It removes pipes,
  redirection and globbing — most of what makes a shell useful — while adding no
  security, since the model would simply pass `sh` as argv[0].
- **Registering `/bin/sh` as the trusted binary directly:** rejected. It
  discards ADR-0025's digest pinning for no gain, and puts a system binary
  inside our supply-chain boundary.
- **Inheriting a filtered environment like Codex:** rejected. Starting empty and
  adding two variables is simpler to audit than a denylist over whatever the
  Worker happened to be started with.
- **Waiting for a command allow-list before shipping shell:** rejected as the
  wrong order. An allow-list is an optimisation over an approval flow that has
  to exist and be trusted first.

## References

- ADR-0025 trusted native development Tools
- ADR-0036 Seatbelt-contained trusted Tools and the first Workspace write
- ADR-0037 sensitive-path read containment
- Codex `codex-rs/core/src/tools/handlers/shell_spec.rs`
- Codex `codex-rs/protocol/src/shell_environment.rs`
