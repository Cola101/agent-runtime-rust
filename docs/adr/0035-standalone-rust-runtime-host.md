# ADR-0035: Standalone Rust runtime-host for local and edge execution

## Status

Accepted

## Context

Every Run today needs the Java control plane: the scheduler owns dispatch, PostgreSQL owns
authoritative state, NATS carries commands and events, and the Worker only executes what it is
told. That is correct for a multi-tenant PaaS, but it makes three things impossible.

A desktop client cannot run an Agent without operating a database, a message broker, and a JVM. An
edge device cannot keep working while its uplink is down. And a local developer cannot execute a
single Run without the whole native stack.

Codex runs a Thread entirely in one process and persists rollout files locally. OpenClaw's
`node-host` executes on the device that owns the resources and reports back. Neither carries a
tenant authority, because neither has one. This platform does, and the risk is that a local mode
quietly becomes a second execution semantics with weaker rules — a Skill that widens Tools locally,
a Checkpoint that does not bind what it bound in the cloud, an approval that can be skipped because
"it is just local".

`apps/edge-node` is currently a seven-line placeholder, so there is no existing local execution path
to preserve.

## Decision

1. **`runtime-host` is a Rust binary that executes Runs with no Java, no PostgreSQL, no NATS, and no
   gRPC.** It links the existing execution core as libraries: the Worker execution core for the
   model/Tool loop and its security invariants, `agent-model-gateway` for provider protocol
   adapters, `agent-tool-runtime` for trusted native Tools, and `agent-kernel` for the Run state
   machine and event sequence.

2. **Local mode reuses the Worker execution core unchanged; it does not reimplement the loop.**
   The intersection `Skill declared ∩ trusted Tool registry ∩ delegated scopes`, the refusal to
   expose unactivated Tools, the fail-closed rejection of unavailable Tools, the approval gate, and
   the Checkpoint bindings are the same code in both modes. A local Run may be *smaller* in
   authority than a cloud Run; it is never permitted to be larger.

3. **`runtime-host` synthesizes the execution command instead of receiving it.** In cloud mode the
   Java scheduler produces `RunExecutionCommand`; locally `runtime-host` constructs the same
   contract. Fields that exist to arbitrate between competing Workers — owner epoch, fencing token,
   worker incarnation — take fixed local values. They are coordination inputs, not a trust boundary,
   and single-writer local execution has nothing to arbitrate.

4. **Local mode has no workload identity, because it has no boundary to cross.** In cloud mode the
   short-lived token exists so a Worker can call the Model Gateway without holding provider
   credentials. Locally the provider adapter runs in the same process as the kernel, so there is no
   inter-process hop to authenticate. `runtime-host` therefore holds the provider credential
   directly and must never expose a network listener that reaches it.

5. **The filesystem is the authoritative local store.** Runs, events, and Checkpoints are written
   under a local state root as append-only JSONL plus Checkpoint payload files. There is no local
   PostgreSQL. Cloud mode's rule that PostgreSQL is the source of truth is unchanged; local mode
   simply has a different, smaller authority for its own Runs.

6. **A local Run is not a tenant-scoped Run.** `runtime-host` does not issue tenant identity, does
   not enforce RLS, and does not produce cloud audit records. Local Runs are not promotable to cloud
   Runs by copying files. Migration, if it is ever wanted, is a separate export contract.

7. **The client is separable from the Runtime.** `runtime-host` owns Run lifetime. A CLI today and a
   desktop GUI later are clients that attach and detach; killing the client must not terminate a
   Run, and reattaching must reconstruct state from the local store rather than from client memory.

8. **Skill signature verification stays mandatory, and the local host never signs.** Local mode does
   not get an unsigned Skill path. A `runtime-host` configured with no Skill verifier refuses every
   Skill-carrying command, exactly as the Worker does.

   Local Skills are **control-plane signed and carried offline**: the control plane remains the sole
   signer, the local host is issued only the verifying key, and a signed artifact travels with the
   device. A host that signed its own Skills would be both signer and verifier, which is the same as
   having no signature at all. Key distribution and artifact export are a separate contract and are
   not implemented yet; until then a local Run carries no Skill snapshot.

## Consequences

### Positive

- Desktop and edge execution become possible without operating a database or a broker.
- The security model has one implementation, so a local regression is caught by the same tests that
  guard cloud execution.
- Provider adapters, trusted Tools, and Checkpoint semantics are shared rather than forked.
- The GUI can crash or restart without killing work.

### Negative

- `runtime-host` depends on the Worker crate, so the Worker's public surface becomes a real internal
  API and cannot be reshaped casually.
- Local Runs carry cloud-shaped fields that mean nothing locally, which is honest but not tidy.
- Two authoritative stores now exist — PostgreSQL for cloud, filesystem for local — and they must
  never be presented as one.

### Neutral

- The local store format is versioned from the start and is expected to change before any GUI ships.

## Failure Modes

- Missing or malformed provider configuration: refuse to start rather than run without a model.
- Skill verifier absent while a Skill snapshot is present: refuse the Run.
- Checkpoint present but its bound instructions, Tool catalog, or Skill identity do not match the
  recomputed effective state: refuse to resume. Identical to cloud recovery.
- Local state root not writable: refuse to start, because a Run that cannot checkpoint cannot be
  resumed and would silently lose work on exit.

## Alternatives Considered

- **Run the Java control plane locally in a reduced mode:** rejected. It reintroduces the JVM,
  PostgreSQL, and NATS, which is exactly the cost a desktop client cannot pay.
- **Write a second, simpler execution loop for local mode:** rejected. It would fork the security
  semantics, and the fork would be discovered by a user, not by a test.
- **Embed the Model Gateway as a child process and keep gRPC:** rejected for local mode. It adds
  mTLS material, a port, and a supervision problem to solve a boundary that does not exist locally.
  Cloud mode keeps the separate Gateway because there the boundary is the point.
- **Let the GUI own the Runtime:** rejected. Run lifetime would follow window lifetime, and closing
  a window would lose work.

## References

- ADR-0024 native macOS development runtime
- ADR-0029 signed SkillVersion and trusted Tool activation
- Codex `codex-rs/core/src/thread_manager.rs`
- OpenClaw `src/node-host/runtime.ts`
