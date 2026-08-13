# Complete workload identity evidence — 2026-08-13

## Proven

- Claims schema 4 preserves and verifies application, workload principal,
  Session, Workspace and AgentVersion in addition to the existing tenant/Run/
  attempt/Worker/ModelPolicy binding.
- ModelInvocation schema 5 rejects a changed Workspace before provider egress.
- MCP request schema 2 accepts the exact signed server snapshot and rejects an
  endpoint substitution before reaching the substituted endpoint.
- Worker v20 admission rejects a changed Workspace, an MCP snapshot mismatch,
  and an MCP-enabled token without `mcp.federate`.
- Checkpoint binding schema 2 rejects cross-Workspace access over the real
  in-process mTLS gRPC test surface.
- Restricted Tool input carries the complete invocation identity.
- A daemon constructed for invocation B skips invocation A's durable running
  record and leaves it unchanged.
- Legacy model/MCP requests and identity renewal remain compatible only after
  optional identity fields are normalized to nil; they cannot smuggle those
  fields as authority.

## RED to GREEN tests

- `token_contract::v4_token_preserves_the_complete_runtime_identity`
- `token_contract::v4_token_cannot_authorize_another_application`
- `mcp_server_authorization::digest_binds_the_exact_wire_server_snapshot`
- `grpc::tests::schema_five_binds_the_complete_runtime_invocation_identity`
- `mcp_grpc_identity::a_v4_token_authorizes_only_the_exact_mcp_server_snapshot`
- `assignment::v20_worker_admission_rejects_a_workspace_not_bound_by_the_workload_token`
- `assignment::v20_worker_admission_requires_federation_authority_when_mcp_is_configured`
- `grpc_contract::v2_checkpoint_binding_rejects_a_different_workspace`
- `daemon_recovery::a_daemon_does_not_recover_a_foreign_invocation_record`

## Validation status

- Identity/MCP/Model/Checkpoint/daemon targeted suites pass.
- Worker MCP end-to-end: 14/14 pass after preserving strict legacy/v20 wire
  separation.
- Worker model-gateway transport: 13/13 pass with the same separation.
- Worker assignment: 72/72 pass, including legacy identity renewal.
- Tool persistent process session: 17/17 passes. The full gate first exposed a
  real two-exchange race: an idle PTY supervisor could retire after the
  handshake and its authenticated replacement could successfully start the
  process, but the client compared the response only with the stale generation
  and manufactured `indeterminate`. The client now accepts a changed generation
  only after a fresh handshake proves that responder is the live supervisor for
  the same state root.
- Final default-parallel `cargo test --workspace --quiet`: 615 passed, 0 failed,
  6 explicitly ignored external-live tests (621 listed tests total).
- `cargo check --workspace --all-targets`, Clippy workspace/all-targets with
  `-D warnings`, Rust format and `git diff --check` pass.

All validation is Mac-native. No Java service, database, NATS, Docker, VM or
Kubernetes workload is started.

Final cleanup removed the Rust `target` tree (24.6 GiB), Graphify query/cache
output, 32 failed-stress temporary state roots, 307 stale PTY sockets and eight
test supervisor processes. Post-cleanup checks found no Runtime process,
Runtime temporary marker, PTY socket root or `runtime/target` directory.

## Remaining boundary

This milestone proves complete signed workload identity inside the Rust
data-plane and local daemon. It does not prove external caller authentication,
Java v20 producer parity, active revocation/key rotation, distributed
Workspace ownership, edge-node enrollment or signed offline task execution.
