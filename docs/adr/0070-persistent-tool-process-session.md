# ADR-0070: Persistent protocol-neutral Tool process sessions

## Status

Accepted and implemented for the standalone Rust Runtime on macOS. Real child
processes, a real owner-process crash, replacement Host recovery, model-visible
Tool turns and process-tree cleanup form the acceptance boundary.

## Context

One-shot Tool execution cannot support a compiler, REPL, local service or other
long-running process without either restarting it on every model turn or hiding
its lifecycle in one Host's memory. The first choice loses state; the second
makes a Host crash turn a live process into an unreachable orphan.

Codex keeps bounded process sessions in `ProcessStore` and exposes polling,
stdin and interruption through `exec_command` / `write_stdin`. OpenClaw has a
process supervisor plus PTY stdin, resize and pause/resume. The inspected paths
are mature, but their live registry remains process-local. This Runtime also
needs tenant/Workspace binding and an explicit cross-Host recovery result.

## Decision

1. `process.start`, `process.write`, `process.poll`, `process.interrupt` and
   `process.close` form one protocol-neutral Tool family. Installation is
   explicit through a trusted executable; no shell is enabled implicitly.
2. `start` returns a stable UUID handle. A digest-protected manifest binds the
   handle to tenant, canonical Workspace, source Run/attempt/Tool call, Tool
   binding digest and executable implementation digest.
3. State is written atomically before process launch (`starting`) and again
   after PID/process-group publication (`running`). stdout/stderr are append-only
   spools read through caller-supplied byte cursors, with at most 1 MiB returned
   per interaction. stdin is capped at 64 KiB per write.
4. The child leads a dedicated process group and inherits a locked identity
   file. Recovery accepts a live session only when both the exact process group
   and inherited identity are present. It returns `reattached`, `terminated` or
   `indeterminate`; an ambiguous PID is never signalled.
5. Write, interrupt and close intents are persisted before the external action.
   They remain `NonIdempotent`, so the existing Worker ambiguity boundary stops
   automatic replay if a Host dies before the bound Tool Result is durable.
   Poll is `Pure` and uses explicit cursors.
6. Close and natural leader exit both apply TERM, a 500 ms grace period and KILL
   to the whole process group. Terminal state is not published before inherited
   identity release has been checked.
7. A cross-process capacity lock serializes live-session reservation. The cap is
   64 active sessions; terminal history does not consume live capacity, while
   malformed state fails closed and does.
8. All five Tools require `tool:process.session`, use the normal approval gate
   and are exposed in the signed local Skill snapshot. State does not require
   Java, PostgreSQL, NATS, Docker or an external daemon.

## Consequences

### Positive

- A replacement Host can continue interacting with the original process rather
  than silently starting another one.
- Tenant, Workspace, implementation and output-position boundaries are durable
  and independently testable.
- Process cleanup covers descendants on explicit close, interruption and natural
  leader exit.
- The same Tool protocol can later sit behind a CLI, desktop UI or control plane
  without moving lifecycle authority into that wrapper.

### Negative and incomplete

- This phase uses pipes/FIFO rather than a PTY. Terminal resize, echo modes,
  pause/resume and full-screen applications remain unsupported.
- This first phase had only a global live cap. ADR-0071 subsequently added
  tenant/Workspace quotas, idle/deadline governance, resource ceilings and a
  replacement-Host sweeper; ADR-0070 remains the lifecycle foundation.
- stdout/stderr are returned as lossy UTF-8; binary-safe output frames are not
  part of schema 1.
- macOS Seatbelt is the verified containment boundary. Linux has no equivalent
  Landlock/cgroup proof here, so this must be described as an explicitly trusted
  native executable, not as strong sandboxing.
- Real vendor models, remote nodes and the optional NATS Worker were not used.

## References

- Codex `codex-rs/core/src/unified_exec/{mod.rs,process_manager.rs,process_state.rs}`
- Codex `codex-rs/core/src/tools/handlers/unified_exec/write_stdin.rs`
- OpenClaw `src/process/supervisor/supervisor.ts`
- OpenClaw `src/process/terminal-pty.ts`
- OpenClaw `src/node-host/pty-command.ts`
- `runtime/crates/tool-runtime/src/process_session.rs`
- `runtime/crates/tool-runtime/tests/persistent_process_session.rs`
- `runtime/apps/runtime-host/tests/process_session_loop.rs`
