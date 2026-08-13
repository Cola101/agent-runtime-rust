# Root Session immutable branch evidence — 2026-08-10

## Protocol and Worker proof

- RunExecution schema 16 separates authoritative `session_branch` from
  lower-authority `history_import` and delegated `subagent_history`.
- A branch binds stable `session_id`, independent `branch_id`, positive
  generation, ordered immutable Turn digests and one aggregate history digest.
- Worker model preparation includes the exact historical user/assistant/Tool
  messages, while `plan_next_tool_call` reports no pending work for a
  historical Tool Call.
- Checkpoint schema 23 stores the exact branch snapshot. A replacement command
  with the same history but a different generation is rejected.

## Real Fork and Rollback loop

A deterministic HTTP model and real Streamable HTTP MCP server execute:

1. root Turn 1 calls `mcp:local/search` and receives a real Tool result;
2. the source branch appends Turn 2;
3. Fork creates a generation-1 sibling from Turn 1 and appends independently;
4. source Rollback archives generation 1, creates generation 2 at Turn 1 and
   appends a different Turn 2;
5. archived generation 1 and current generation 2 remain independently
   readable;
6. all source/Fork/Rollback model requests retain the historical typed Tool
   pair, while the MCP server observes exactly one `tools/call`.

The stale generation-1 continuation is rejected before model egress. Fork and
Rollback are also rejected while a branch has an active Turn. Removing an
archived generation makes the Session record invalid instead of silently
erasing an old head.

## Recovery proof

- Provider HTTP 503 after the next root Turn is checkpointed leaves the exact
  active Session binding durable. A replacement Host restores with a higher
  owner epoch, completes the Turn and does not execute the historical MCP Tool.
- A separate test recreates the narrow crash after terminal Checkpoint/event
  durability but before Session head persistence. The replacement commits the
  terminal transcript directly; the provider sees no fourth request and the
  Tool call count remains one.
- Session head advancement validates Run, branch, generation, history digest,
  input digest, terminal Checkpoint digest and terminal event status together.

## Reference comparison

- Codex `ff352fab6209` remains ahead on marker-based rollout reconstruction,
  reference-context/token-usage recomputation, richer response items and the
  mature Thread command surface. This Runtime now matches the central stable
  root identity, active-Turn refusal and non-destructive effective-history move,
  with an explicit multi-tenant-oriented generation/history fence.
- OpenClaw `58b4b9430457` remains ahead on reset cascade, active/queue cleanup,
  archive hooks, channel integration and lifecycle ownership. This Runtime has
  independent sibling branches and exact Checkpoint binding, but deliberately
  does not claim OpenClaw product-level Session reset parity.

## Validation

- Protocol execution contract: 35 passed, 0 failed.
- Worker assignment: 60 passed, 0 failed after compatibility fixes.
- Standalone Host: 25 passed, 0 failed.
- Full Rust workspace: 439 passed, 0 failed, 5 external live tests explicitly
  ignored; 444 total.
- `cargo check --workspace --all-targets`, Clippy over
  workspace/all-targets/all-features with `-D warnings`, Rust formatting, JSON,
  diff and residue gates passed.

No Docker, Java, PostgreSQL, NATS, external daemon or API key was used.
