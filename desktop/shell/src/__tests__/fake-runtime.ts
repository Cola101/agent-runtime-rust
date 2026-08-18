/// A bridge that answers like the host, for tests.
///
/// It returns the same shapes `localRuntime.cjs` returns, including the page
/// ceiling, so a test cannot pass against a friendlier runtime than the real
/// one. Run ids and payload shapes are copied from a real dev-runtime session.
import { vi } from "vitest";

export const RUN_WAITING = "01a0122b-217e-7e72-bec8-ad3273f16cd1";
export const RUN_DONE = "01a0122a-18c8-7012-972a-d422fe9abde8";
export const RUN_LIVE = "01a01231-9f40-7d31-8c22-6b1a0e55c704";

const APPROVAL = {
  approval_id: "01a0122b-217e-7e72-bec8-ad3273f16cd2",
  execution: {
    binding_digest: "3be24149daa5170d4f45345772146ab599c5044abfae3e1daf546f03bb1591b9",
    call: { arguments: { command: "ls -la" }, id: "stub-call-1", name: "shell.exec" },
    effect: "non_idempotent",
    sandbox: "trusted_native",
  },
  policy_digest: "210ca211f3b9a04823034901842751bf6f28720a6d4e1eb8bdc904446ef342c2",
  policy_snapshot: {
    approval: "ask", auto_approval: "never", effect: "non_idempotent",
    required_scopes: ["tool:shell.exec"], sandbox: "trusted_native", tool_name: "shell.exec",
  },
};

function event(
  sequence: number, type: string, payload: Record<string, unknown>, minute = 0,
) {
  return {
    event_id: `00000000-0000-4000-8000-${String(sequence).padStart(12, "0")}`,
    sequence, run_id: RUN_WAITING,
    timestamp: `2026-08-18T00:${String(minute).padStart(2, "0")}:0${sequence}.000Z`,
    type, payload, digest: "d".repeat(64),
  };
}

const LOGS: Record<string, { state: Record<string, unknown>; events: ReturnType<typeof event>[] }> = {
  [RUN_WAITING]: {
    state: { state: "waiting_approval" },
    events: [
      event(1, "run.started", { status: "running" }),
      event(2, "model.output.delta", { text: "I need to run a command." }),
      event(3, "model.usage", { input_tokens: 180, output_tokens: 24, cost_micros: 0 }),
      event(4, "model.tool_call", { call: { name: "shell.exec", arguments: { command: "ls -la" } } }),
      event(5, "approval.required", { approval: APPROVAL, status: "waiting_approval" }),
    ],
  },
  [RUN_LIVE]: {
    state: { state: "running" },
    events: [
      event(1, "run.started", { status: "running" }, 30),
      event(2, "model.output.delta", { text: "still going" }, 30),
    ],
  },
  [RUN_DONE]: {
    state: { state: "terminal", status: "succeeded" },
    events: [
      event(1, "run.started", { status: "running" }),
      event(2, "model.output.delta", { text: "done" }),
      event(3, "run.succeeded", { status: "succeeded" }),
    ],
  },
};

export function installFakeRuntime() {
  const control = vi.fn(async () => ({ ok: true as const, value: {} }));
  const submit = vi.fn(async () => ({ ok: true as const, value: RUN_DONE }));
  const runtime = {
    status: async () => ({ ok: true as const, value: status() }),
    probe: async () => ({ ok: true as const, value: status() }),
    list: async () => ({ ok: true as const, value: [RUN_WAITING, RUN_LIVE, RUN_DONE] }),
    events: async ({ runId, limit = 256 }: { runId: string; limit?: number }) => {
      // The daemon rejects an oversized page rather than clamping it. A test
      // that clamped here would hide exactly the bug this caught in practice.
      if (limit > 256) {
        return { ok: true as const, value: { ok: false as const, error: { code: "invalid_request" } } };
      }
      const log = LOGS[runId];
      if (!log) return { ok: true as const, value: { ok: false as const, error: { code: "not_found" } } };
      return {
        ok: true as const,
        value: {
          ok: true as const,
          page: {
            run_id: runId, requested_after_sequence: 0,
            next_after_sequence: log.events.length,
            earliest_available_sequence: 1,
            highest_committed_sequence: log.events.length,
            history_gap: false, has_more: false, state: log.state, events: log.events,
          },
        },
      };
    },
    submit,
    control,
  };
  const status = () => ({
    transport: "local", stateRoot: "/tmp/state", socketPath: "/tmp/state/runtime-host.sock",
    connected: true, error: null,
  });
  const desk = { mounted: vi.fn(), drew: vi.fn(), runtime };
  (window as unknown as { desk: typeof desk }).desk = desk;
  return { control, submit, desk };
}
