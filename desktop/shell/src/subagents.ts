/// What a Run delegated, read out of its own durable log.
///
/// This is the parent's account of its children, and that is the honest limit
/// of it: the parent's log says what it asked for, what came back, and what it
/// spent. What a child actually did is in the child's own Run -- a separate log
/// with its own events -- which is why a delegation carries `childRunId` and
/// the screen links to it rather than pretending to summarise it.
///
/// Every field here comes from an event the kernel emits. Nothing is derived
/// from a name, and nothing is filled in when the log does not say.
import type { RunEvent } from "./runtime";

export type SubagentState =
  /// Asked for, and the kernel has not yet recorded it as spawned. A synchronous
  /// delegation stays here for its whole life: only async spawns emit
  /// `subagent.spawned`.
  | { kind: "requested" }
  | { kind: "running" }
  | { kind: "finished"; status: string; error: boolean }
  /// The parent closed it. An irreversible edge, and the only one a person can
  /// cause from this client.
  | { kind: "closed" };

export type SubagentView = {
  /// `delegation_id`. Stable across every event about this delegation, which is
  /// what lets a fork name its source.
  id: string;
  role: string;
  /// What it was asked to do, from the spawn request. Absent when the log was
  /// retired before the request but after later events.
  asked: string | null;
  state: SubagentState;
  /// The child's own Run. Null until the kernel observes a terminal for it --
  /// the parent's log does not carry it before then.
  childRunId: string | null;
  tokens: number;
  costMicros: number;
  /// The cap this child was given, from `RunBudget` on the spawn request or the
  /// fork. Null when the log does not carry one -- not zero, which is a real cap
  /// that permits nothing and would report a child as over its limit at birth.
  budget: { maxTokens: number; maxCostCents: number; maxDurationSeconds: number } | null;
  /// Set on a forked delegation: the delegation it was forked from, and the
  /// generation it was taken at.
  forkedFrom: { id: string; generation: number; throughOrdinal: number } | null;
  /// Bumped by a rollback. A generation past 1 is why an earlier activation is
  /// no longer in the child's history.
  generation: number;
  /// Queued input the parent accepted for this child but has not activated.
  queued: number;
  at: string | null;
};

function payloadOf(event: RunEvent): Record<string, unknown> {
  return event.payload ?? {};
}

/// `SubagentBudgetUsage`, which is two numbers and not a pair of token counts.
///
/// This read `input_tokens + output_tokens` -- the shape of `model.usage`,
/// which is a different event, about the parent. Neither field has ever been on
/// a subagent event, so every child on screen reported 0 tokens no matter what
/// it spent, and the test that covered it asserted the sum of a fixture nobody
/// had checked against the runtime.
function budgetOf(source: Record<string, unknown>): SubagentView["budget"] {
  const budget = source.budget;
  if (!budget || typeof budget !== "object") return null;
  const fields = budget as Record<string, unknown>;
  return {
    maxTokens: Number(fields.max_tokens ?? 0),
    maxCostCents: Number(fields.max_cost_cents ?? 0),
    maxDurationSeconds: Number(fields.max_duration_seconds ?? 0),
  };
}

function usageOf(payload: Record<string, unknown>): { tokens: number; costMicros: number } {
  const usage = (payload.usage ?? {}) as Record<string, unknown>;
  return {
    tokens: Number(usage.tokens ?? 0),
    costMicros: Number(usage.cost_micros ?? 0),
  };
}

/// Folds one Run's events into what it delegated.
///
/// Order matters and is the log's, not this function's: a terminal seen after a
/// close is still a terminal, and the last event about a delegation is the one
/// that describes it now.
export function subagentsOf(events: RunEvent[]): SubagentView[] {
  const found = new Map<string, SubagentView>();

  const at = (id: string, timestamp: string | null): SubagentView => {
    const existing = found.get(id);
    if (existing) return existing;
    const created: SubagentView = {
      id,
      role: "",
      asked: null,
      state: { kind: "requested" },
      childRunId: null,
      tokens: 0,
      costMicros: 0,
      budget: null,
      forkedFrom: null,
      generation: 1,
      queued: 0,
      at: timestamp,
    };
    found.set(id, created);
    return created;
  };

  for (const event of events) {
    const payload = payloadOf(event);
    switch (event.type) {
      case "subagent.spawn.requested": {
        const request = (payload.request ?? {}) as Record<string, unknown>;
        const id = String(request.delegation_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        view.role = String(request.role ?? view.role);
        view.asked = typeof request.input === "string" ? request.input : view.asked;
        view.budget = budgetOf(request) ?? view.budget;
        break;
      }
      case "subagent.spawned": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        view.role = String(payload.role ?? view.role);
        view.state = { kind: "running" };
        break;
      }
      case "subagent.forked": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        view.role = String(payload.role ?? view.role);
        view.generation = Number(payload.generation ?? view.generation);
        // A fork carries its own budget, which is the fork's and not the
        // source's: the two are separate children with separate caps.
        view.budget = budgetOf(payload) ?? view.budget;
        view.forkedFrom = {
          id: String(payload.source_agent_id ?? ""),
          generation: Number(payload.source_generation ?? 0),
          throughOrdinal: Number(payload.through_activation_ordinal ?? 0),
        };
        break;
      }
      case "subagent.rolled_back": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        at(id, event.timestamp).generation = Number(payload.generation ?? 1);
        break;
      }
      case "subagent.input.accepted": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        at(id, event.timestamp).queued += 1;
        break;
      }
      case "subagent.input.activated": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        // Activated input is no longer queued. Counting it in both places would
        // show a child as waiting on something it is already working on.
        view.queued = Math.max(0, view.queued - 1);
        break;
      }
      case "subagent.terminal.observed":
      case "subagent.result.received": {
        const id = String(payload.agent_id ?? payload.delegation_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        view.childRunId = String(payload.child_run_id ?? view.childRunId ?? "") || null;
        const usage = usageOf(payload);
        // Both events carry the same usage for one delegation, so the larger is
        // taken rather than the sum: adding them would double every child.
        view.tokens = Math.max(view.tokens, usage.tokens);
        view.costMicros = Math.max(view.costMicros, usage.costMicros);
        view.state = {
          kind: "finished",
          status: String(payload.terminal_status ?? ""),
          error: payload.is_error === true,
        };
        break;
      }
      case "subagent.closed": {
        const id = String(payload.agent_id ?? "");
        if (!id) break;
        const view = at(id, event.timestamp);
        // A close after a terminal changes nothing: the child had already
        // finished, and saying "closed" would report work as discarded that was
        // in fact delivered.
        if (view.state.kind !== "finished") view.state = { kind: "closed" };
        break;
      }
      default:
        break;
    }
  }

  return [...found.values()];
}

/// Forked children under the delegation they came from, everything else at the
/// top. One level, because that is all the parent's log describes -- a child's
/// own delegations are events in the child's Run.
export function lineage(views: SubagentView[]): { view: SubagentView; depth: number }[] {
  const byId = new Map(views.map((view) => [view.id, view]));
  const ordered: { view: SubagentView; depth: number }[] = [];
  for (const view of views) {
    if (view.forkedFrom && byId.has(view.forkedFrom.id)) continue;
    ordered.push({ view, depth: 0 });
    for (const candidate of views) {
      if (candidate.forkedFrom?.id === view.id) ordered.push({ view: candidate, depth: 1 });
    }
  }
  return ordered;
}
