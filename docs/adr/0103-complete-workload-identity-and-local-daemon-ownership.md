# ADR-0103: Complete workload identity and local daemon ownership

- Status: Accepted
- Date: 2026-08-13
- Scope: Rust workload token, Model/MCP/Checkpoint gRPC, Worker, Tool Runtime, standalone daemon

## Context

ADR-0102 introduced an explicit tenant/application/workload/Workspace/
AgentVersion/model-policy invocation profile, but the remote execution chain
still authorized only tenant, Run, attempt, Worker and model policy. A valid
workload token could therefore be replayed with a different application or
Workspace in request fields that were not signed. MCP authorization also named
a server by request body without binding the exact endpoint, sealed credential
envelope, protocol revision and client capabilities into the token.

The standalone daemon had a related local boundary: durable Run records were
addressed only by Run ID. A daemon opened under another invocation profile
could recover or decide a record from the same state root.

## Decision

1. Workload claims schema 4 binds tenant, application, non-secret workload
   principal, Run, Session, Workspace, AgentVersion, attempt, Worker
   incarnation, ModelPolicy and policy digest. Every non-legacy identity is
   non-nil and token lifetime remains bounded.
2. ModelInvocation schema 5 carries and verifies the complete identity before
   provider routing or network egress. Schemas 2-4 remain readable only with
   their legacy nil identity shape; fields from an older request cannot be
   promoted into authority.
3. MCP request schema 2 requires claims schema 4 and an exact per-server digest.
   The canonical digest covers server ID, name, endpoint, raw sealed credential
   envelope, protocol revision and sorted client capabilities. Worker
   admission requires `mcp.federate` whenever the command contains an MCP
   server and verifies the exact digest map before accepting the Run.
4. Checkpoint WorkloadBinding schema 2 binds the same complete identity for
   reads and writes. ToolExecutionContext carries the identity into restricted
   and trusted executors; provider credentials never enter the token or Tool
   context.
5. RunExecution v20 is the first cloud command eligible for complete identity
   admission. Worker token renewal preserves the v20 binding and exact MCP map.
   Legacy v2/v3 claims can renew only legacy commands and are normalized to nil
   optional identity fields on legacy gRPC requests.
6. A LocalRuntimeDaemon is constructed with one immutable
   RuntimeInvocationContext. New durable Run records store that owner identity;
   recovery, attach, approval, MCP input resolution and cancellation first read
   through the same ownership predicate. All-nil historical records remain
   accessible only to the built-in single-user local profile.

## Failure modes and invariants

- A valid token for another application, workload principal, Session,
  Workspace or AgentVersion is rejected before model, MCP or checkpoint egress.
- Changing any authority-bearing MCP server field changes its digest and is
  rejected before DNS resolution or credential opening.
- A token without `mcp.federate` cannot admit an MCP-enabled v20 Run.
- A legacy request cannot send non-empty complete-identity fields and acquire
  v20 authority through a compatibility path.
- A daemon bound to profile B neither recovers nor controls profile A's durable
  Run record even when both can see the same state root.
- Token verification does not authenticate the external caller. Embedding
  adapters remain responsible for authenticating users and selecting an
  already-authorized RuntimeInvocationContext.

## Compatibility and producer requirements

The Rust standalone/embedded path can issue and consume the complete contract.
Any external Java or other control plane that emits RunExecution v20 must
implement the same canonical MCP digest and claims schema 4 before enabling
v20; older producers remain on legacy schemas and do not receive the stronger
authority. This is an explicit migration boundary, not an implicit upgrade.

## Consequences

The Rust data-plane now preserves one signed resource identity through Worker,
Model Gateway, MCP Gateway, Checkpoint Gateway, Tool Runtime and local durable
control. This is stronger than process-local thread subscription or
device-pairing identity, but it does not yet provide node enrollment, device
attestation, token revocation, signing-key rotation, distributed owner epochs
or an offline task envelope. Those remain edge/control-plane work.
