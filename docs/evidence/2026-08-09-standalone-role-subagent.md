# Standalone role subagent evidence — 2026-08-09

## Scope

This evidence covers the serial parent-child-result vertical slice in ADR-0049
and the child-model cancellation and recovery closures in ADR-0050. It does not
claim durable cancellation intent, child Tool/MCP cancellation terminal mapping,
nested approval forwarding or eight-way child concurrency.

## Behavioural proof

`standalone_parent_executes_an_authorized_role_subagent_and_receives_its_result`
uses one real loopback OpenAI-compatible HTTP/SSE provider and the shipped Rust
Host execution path:

1. The parent request exposes `agent.spawn` and the configured `reviewer` role.
2. The model requests a 400 Token child task.
3. The child request contains only `Review evidence only.`, the delegated task
   and the 400 Token limit; it contains neither the parent instructions nor
   `agent.spawn` because the role lacks that scope.
4. The child reaches `run.succeeded` with its own event log and Checkpoint.
5. The parent receives the child's text, child Run identity and the original
   `call_review_1`, emits `subagent.result.received`, and completes its next turn.
6. The state root contains two independent Run Checkpoints.

`standalone_parent_can_run_two_role_subagents_sequentially` additionally proves
that two different Tool Calls can create two children in one parent attempt and
that both result identities remain in the final parent transcript. Its RED found
a real Worker defect: the result idempotency receipt was one global `Option`, so
the first completed child rejected every later child. Receipts are now keyed by
Tool Call identity.

`cancelling_a_parent_while_its_child_model_is_streaming_closes_the_child` uses a
real never-ending SSE child response. IPC cancellation closes the child TCP
request and durably emits `run.cancelled` for the child and suspended parent.

`restarting_the_host_resumes_the_same_child_without_spawning_again` aborts the
first Host while the child model request is live. The replacement restores the
parent pending-spawn Checkpoint and the child's existing Checkpoint, keeps two
Run identities, emits only one `subagent.spawn.requested`, then finishes.

`recovery_reuses_a_durable_child_result_without_calling_the_child_again` recreates
the narrower child-complete/parent-not-consumed crash window. It removes the
child terminal log so only the atomic result receipt can satisfy recovery. With
receipt loading disabled the test sends an unexpected second child request and
fails; with digest-bound loading restored it calls only the parent continuation.

The initial RED was the production error
`local host does not run subagents yet`; the test passed only after the Host
executed and rebound the child result.

## Source comparison used

- Codex: multi-agent spawn handlers, `agent/control/spawn.rs`, wait and close
  handlers at `ff352fab6209dc0f9d13fc0036ed3f9404682b2c`.
- OpenClaw: `sessions-spawn-tool.ts`, `spawn-plan.ts`,
  `subagent-capabilities.ts`, `spawn-pipeline.ts`, `subagent-control.ts` and
  `subagent-orphan-recovery.ts` at `58b4b9430457e91b44f0ccce73ad1b6c6bb11e28`.

## Validation

- `cargo test --workspace`: 384 passed, 0 failed, 5 explicitly ignored live
  integration cases; 389 test items listed after the cancellation-domain gate.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- Host slice: 46 passed, including the role-subagent and cancellation/recovery
  behavioural closures and the shipped binary role-file parser.
- Residue audit: no `agent-tool-runtime-*` temporary directory, Unix socket,
  partial file or runtime child process remained. Existing `runtime/target` was
  retained as reusable build cache and was not cleaned.
