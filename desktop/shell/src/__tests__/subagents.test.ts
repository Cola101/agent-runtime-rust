// @vitest-environment node
/// What this file is for.
///
/// Subagents are the deepest thing this runtime does -- async spawn, queued
/// input, fork, rollback, budgets, roles -- and the client rendered none of it.
/// Building that view means reading nine event types correctly, and the ways to
/// get it wrong all look plausible on screen: counting one delegation's usage
/// twice, showing a finished child as discarded because a close arrived after
/// it, or leaving activated input sitting in a queue it has already left.
///
/// Every payload here is the shape `crates/kernel` emits, taken from the
/// `json!` blocks that emit them.
import { describe, expect, it } from "vitest";
import { subagentsOf, lineage } from "../subagents";
import type { RunEvent } from "../runtime";

let sequence = 0;
function event(type: string, payload: Record<string, unknown>): RunEvent {
  sequence += 1;
  return {
    event_id: `00000000-0000-4000-8000-${String(sequence).padStart(12, "0")}`,
    sequence,
    run_id: "01a01231-9f40-7d31-8c22-6b1a0e55c704",
    timestamp: `2026-08-18T10:00:${String(sequence).padStart(2, "0")}.000Z`,
    type,
    payload,
    digest: "d".repeat(64),
  };
}

const A = "01a01300-0000-7000-8000-00000000000a";
const B = "01a01300-0000-7000-8000-00000000000b";

const requested = (id: string, role: string, input: string) =>
  event("subagent.spawn.requested", {
    status: "running",
    request: { tool_call_id: "call-1", delegation_id: id, role, input, mode: "async" },
  });

describe("what a Run delegated", () => {
  it("reads the ask from the request and the role from the spawn", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "把 retention 那段读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
    ]);
    expect(view.id).toBe(A);
    expect(view.role).toBe("reviewer");
    expect(view.asked).toBe("把 retention 那段读一遍");
    expect(view.state).toEqual({ kind: "running" });
    // The parent's log does not carry the child's Run id until it observes a
    // terminal, and inventing one would link to a Run that may not exist.
    expect(view.childRunId).toBeNull();
  });

  it("counts one delegation's usage once, not once per event that carries it", () => {
    // `SubagentBudgetUsage`, field for field: two numbers, and `tokens` is one
    // of them rather than a pair to add up. The fixture used to carry
    // `input_tokens`/`output_tokens` -- the shape of `model.usage`, which is a
    // different event about the parent -- so this file asserted a sum of two
    // fields no subagent event has ever had, and every child on screen showed
    // 0 tokens with the guard green.
    const usage = { tokens: 1020, cost_micros: 4200 };
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      // Both events carry the same usage for the same delegation.
      event("subagent.terminal.observed", {
        agent_id: A, child_run_id: "child-1", terminal_status: "succeeded", is_error: false, usage,
      }),
      event("subagent.result.received", {
        status: "running", remaining_subagents: 0, delegation_id: A, child_run_id: "child-1",
        terminal_status: "succeeded", is_error: false, usage,
      }),
    ]);
    expect(view.tokens).toBe(1020);
    expect(view.costMicros).toBe(4200);
    expect(view.childRunId).toBe("child-1");
    expect(view.state).toEqual({ kind: "finished", status: "succeeded", error: false });
  });

  it("keeps a failed child failed rather than only finished", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.terminal.observed", {
        agent_id: A, child_run_id: "child-1", terminal_status: "failed", is_error: true, usage: {},
      }),
    ]);
    expect(view.state).toEqual({ kind: "finished", status: "failed", error: true });
  });

  it("does not report a finished child as closed", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.terminal.observed", {
        agent_id: A, child_run_id: "child-1", terminal_status: "succeeded", is_error: false, usage: {},
      }),
      // A close after a terminal. The work was delivered; saying "closed" would
      // report it as discarded.
      event("subagent.closed", { agent_id: A, status: "closed" }),
    ]);
    expect(view.state).toEqual({ kind: "finished", status: "succeeded", error: false });
  });

  it("reports a child closed before it finished as closed", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      event("subagent.closed", { agent_id: A, status: "closed" }),
    ]);
    expect(view.state).toEqual({ kind: "closed" });
  });

  it("stops counting input as queued once it is activated", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      event("subagent.input.accepted", { agent_id: A, message_sequence: 1, child_run_id: A, status: "queued" }),
      event("subagent.input.accepted", { agent_id: A, message_sequence: 2, child_run_id: A, status: "queued" }),
      event("subagent.input.activated", { agent_id: A, message_sequence: 1, child_run_id: A, status: "running" }),
    ]);
    // One waiting, not two: a child shown as waiting on something it is already
    // working on is a child that looks stuck.
    expect(view.queued).toBe(1);
  });

  /// The runtime says which kind of acceptance it was, and only one of them
  /// ever queues.
  ///
  /// A message accepted as `active` went straight into the child's turn and is
  /// never followed by `input.activated` -- so counting every acceptance left
  /// it in the queue for the rest of the Run, and the card said a child was
  /// waiting on work it had already been handed.
  it("does not count a message that went straight in as waiting", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      event("subagent.input.accepted", { agent_id: A, message_sequence: 1, child_run_id: A, status: "active" }),
    ]);
    expect(view.queued).toBe(0);
  });

  /// A number of tokens spent means nothing without the number it was allowed.
  /// `RunBudget` rides on the spawn request the client already parses and on
  /// the fork event beside it, so this is the runtime's own cap rather than a
  /// figure worked out here.
  it("carries the cap the child was given, not only what it spent", () => {
    const [view] = subagentsOf([
      event("subagent.spawn.requested", {
        status: "running",
        request: {
          tool_call_id: "call-1", delegation_id: A, role: "reviewer",
          input: "读一遍", mode: "async",
          budget: { max_tokens: 20000, max_cost_cents: 50, max_duration_seconds: 600 },
        },
      }),
      event("subagent.terminal.observed", {
        agent_id: A, child_run_id: "child-1", terminal_status: "succeeded",
        is_error: false, usage: { tokens: 1020, cost_micros: 4200 },
      }),
    ]);
    expect(view.budget).toEqual({ maxTokens: 20000, maxCostCents: 50, maxDurationSeconds: 600 });
  });

  /// Null rather than zero. A budget of zero tokens is a real cap that permits
  /// nothing, and a client that wrote one where the log said nothing would be
  /// reporting a child as over its limit the moment it started.
  it("says nothing about a budget the log does not carry", () => {
    const [view] = subagentsOf([
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
    ]);
    expect(view.budget).toBeNull();
  });

  it("carries the generation a rollback moved to", () => {
    const [view] = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      event("subagent.rolled_back", {
        agent_id: A, from_generation: 1, generation: 2, through_activation_ordinal: 3,
      }),
    ]);
    expect(view.generation).toBe(2);
  });
});

describe("lineage", () => {
  it("puts a fork under the delegation it came from", () => {
    const views = subagentsOf([
      requested(A, "reviewer", "读一遍"),
      event("subagent.spawned", { agent_id: A, role: "reviewer", status: "running" }),
      event("subagent.forked", {
        source_agent_id: A, source_generation: 1, through_activation_ordinal: 2,
        source_history_digest: "a".repeat(64), agent_id: B, generation: 1, role: "reviewer",
        budget: { max_tokens: 1000, max_cost_cents: 10, max_duration_seconds: 60 },
      }),
    ]);
    const ordered = lineage(views);
    expect(ordered.map((entry) => [entry.view.id, entry.depth])).toEqual([[A, 0], [B, 1]]);
    expect(views.find((view) => view.id === B)?.forkedFrom).toEqual({
      id: A, generation: 1, throughOrdinal: 2,
    });
  });

  it("keeps a fork whose source is not in this log at the top", () => {
    // The source can be absent when the log was retired around it. Nesting
    // under a parent that is not there would drop the row entirely.
    const views = subagentsOf([
      event("subagent.forked", {
        source_agent_id: "01a01300-0000-7000-8000-0000000000ff", source_generation: 1,
        through_activation_ordinal: 2, agent_id: B, generation: 1, role: "reviewer", budget: {},
      }),
    ]);
    expect(lineage(views).map((entry) => entry.depth)).toEqual([0]);
  });
});
