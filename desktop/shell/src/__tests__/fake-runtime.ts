/// A bridge that answers like the host, for tests.
///
/// It returns the same shapes `localRuntime.cjs` returns, including the page
/// ceiling, so a test cannot pass against a friendlier runtime than the real
/// one. Run ids and payload shapes are copied from a real dev-runtime session.
import { vi } from "vitest";

export const RUN_WAITING = "01a0122b-217e-7e72-bec8-ad3273f16cd1";
export const RUN_DONE = "01a0122a-18c8-7012-972a-d422fe9abde8";
export const RUN_LIVE = "01a01231-9f40-7d31-8c22-6b1a0e55c704";
export const RUN_PROCESS = "01a01519-9102-72e2-b80e-f0990dcbd799";

/// The durable process session the `process.*` events below belong to. Taken,
/// with its cursors, from a runtime-host run recorded against a real PTY.
export const PROCESS_SESSION = "01a0151c-914a-7c31-8f0d-1b7c1a4e5d20";

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
  sequence: number, type: string, payload: Record<string, unknown>,
  minute = 0, runId = RUN_WAITING,
) {
  return {
    event_id: `${runId.slice(0, 8)}-0000-4000-8000-${String(sequence).padStart(12, "0")}`,
    sequence, run_id: runId,
    timestamp:
      `2026-08-18T00:${String(minute).padStart(2, "0")}:${String(sequence % 60).padStart(2, "0")}.000Z`,
    type, payload, digest: "d".repeat(64),
  };
}

/// One `tool.result` carrying a `ProcessSessionOutput`.
///
/// Field names and the byte-cursor semantics are the runtime's: `stdout` is the
/// range `[stdout_start_cursor, stdout_cursor)` of the session's own log, and
/// `stdout_truncated` means a tail read started past bytes the log never got.
function output(over: Record<string, unknown>) {
  return {
    session_id: PROCESS_SESSION,
    state: "running",
    pid: 66775,
    exit_code: null,
    termination_reason: null,
    stdout: "", stdout_start_cursor: 0, stdout_cursor: 0, stdout_truncated: false,
    stderr: "", stderr_start_cursor: 0, stderr_cursor: 0, stderr_truncated: false,
    ...over,
  };
}

function processCall(
  sequence: number, id: string, name: string, args: Record<string, unknown>, runId: string,
) {
  return event(sequence, "model.tool_call", { id, name, arguments: args }, 20, runId);
}

function processResult(
  sequence: number, id: string, content: Record<string, unknown>, runId: string,
) {
  return event(
    sequence, "tool.result",
    { tool_call_id: id, binding_digest: "b".repeat(64), content, is_error: false },
    20, runId,
  );
}

const LOGS: Record<string, { state: Record<string, unknown>; events: ReturnType<typeof event>[] }> = {
  [RUN_WAITING]: {
    state: { state: "waiting_approval" },
    events: [
      event(1, "run.started", { status: "running" }),
      event(2, "model.output.delta", { text: "I need to run a command." }),
      event(3, "model.usage", { input_tokens: 180, output_tokens: 24, cost_micros: 0 }),
      // Flat, which is what the runtime actually writes -- copied from a real
      // dev-runtime log. It nests the call inside `approval.required` and not
      // here, and a fake that nested both would let this client pass against a
      // shape the runtime does not emit.
      event(4, "model.tool_call", { name: "shell.exec", arguments: { command: "ls -la" }, id: "stub-call-1" }),
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
      // A tool call that names a path, which is what the workspace surface
      // reads to say what the agent was asked to touch.
      event(3, "model.tool_call", {
        name: "workspace.write", arguments: { path: "notes.txt", contents: "x" }, id: "stub-call-2",
      }),
      // A second run with a session, so "move to the next run that has one" is
      // a key with somewhere to go.
      processCall(4, "done-start", "process.start", { initial_stdin: "uname\n" }, RUN_DONE),
      processResult(5, "done-start", output({
        stdout: "Darwin\r\n", stdout_cursor: 8,
      }), RUN_DONE),
      event(6, "run.succeeded", { status: "succeeded" }),
    ],
  },
  /// A whole PTY session as the durable log holds one.
  ///
  /// Every shape here was recorded from a runtime-host run against a real
  /// `/bin/sh` on a PTY: the flat `model.tool_call` payload, the
  /// `ProcessSessionOutput` inside `tool.result`, and the byte cursors that
  /// make each read locatable. What is constructed rather than recorded is the
  /// pair of bounded `process.attach` reads — a tail read whose `max_bytes` is
  /// smaller than the log is how bytes the agent never read become a hole the
  /// client has to say out loud.
  [RUN_PROCESS]: {
    state: { state: "terminal", status: "succeeded" },
    events: [
      event(1, "run.started", { status: "running" }, 20, RUN_PROCESS),
      processCall(2, "call-start", "process.start", {
        initial_stdin: "echo hello-from-session\n",
        tty: true, cols: 100, rows: 30, yield_time_ms: 2000,
      }, RUN_PROCESS),
      // The PTY echoes what was typed before the shell has answered anything.
      processResult(3, "call-start", output({
        stdout: "echo hello-from-session\r\n", stdout_cursor: 25,
      }), RUN_PROCESS),
      processCall(4, "call-write", "process.write", {
        session_id: PROCESS_SESSION, stdout_cursor: 25, stderr_cursor: 0,
        stdin: "printf 'line-two\\n'\n", yield_time_ms: 2000,
      }, RUN_PROCESS),
      processResult(5, "call-write", output({
        stdout: "printf 'line-two\\n'\r\n",
        stdout_start_cursor: 25, stdout_cursor: 46,
      }), RUN_PROCESS),
      processCall(6, "call-poll", "process.poll", {
        session_id: PROCESS_SESSION, stdout_cursor: 46, stderr_cursor: 0,
      }, RUN_PROCESS),
      processResult(7, "call-poll", output({
        stdout_start_cursor: 46, stdout_cursor: 46,
      }), RUN_PROCESS),
      processCall(8, "call-attach", "process.attach", {
        session_id: PROCESS_SESSION, max_bytes: 19,
      }, RUN_PROCESS),
      // The log reached 531 bytes while nothing was reading it. A 19-byte tail
      // read starts at 512, and 46–512 exists only in the session's own file.
      processResult(9, "call-attach", output({
        stdout: "\u001b[32mline-two\u001b[0m\r\n",
        stdout_start_cursor: 512, stdout_cursor: 531, stdout_truncated: true,
      }), RUN_PROCESS),
      processCall(10, "call-again", "process.attach", {
        session_id: PROCESS_SESSION, max_bytes: 19,
      }, RUN_PROCESS),
      processResult(11, "call-again", output({
        stdout: "\u001b[32mline-two\u001b[0m\r\n",
        stdout_start_cursor: 512, stdout_cursor: 531, stdout_truncated: true,
      }), RUN_PROCESS),
      processCall(12, "call-close", "process.close", {
        session_id: PROCESS_SESSION, stdout_cursor: 531, stderr_cursor: 0,
      }, RUN_PROCESS),
      processResult(13, "call-close", output({
        state: "terminated", pid: null, termination_reason: "closed",
        stdout_start_cursor: 531, stdout_cursor: 531,
      }), RUN_PROCESS),
      event(14, "run.succeeded", { status: "succeeded" }, 20, RUN_PROCESS),
    ],
  },
};

/// A Session with two committed Turns and nothing in flight.
///
/// Shapes copied from a real `session_history` reply: roles, content parts and
/// a per-Turn digest. A fake that flattened a Turn to a string would let the
/// renderer pass against a transcript the runtime does not have.
export const SESSION = "01a01430-0000-7000-8000-000000000001";
export const SESSION_BRANCH = "01a01430-0000-7000-8000-000000000002";
/// An older conversation. Its id sorts *below* SESSION, which is the whole
/// point: this client mints v7 ids and the runtime returns heads in id order,
/// so "older" is a property of the id rather than a label the test asserts.
export const OLDER_SESSION = "01a0142f-0000-7000-8000-000000000001";
export const OLDER_BRANCH = "01a0142f-0000-7000-8000-000000000002";

const OLDER_TURNS = [
  {
    turn_ordinal: 1,
    run_id: RUN_DONE,
    transcript: [
      { role: "user", content: [{ type: "text", text: "上礼拜那段对话" }] },
      { role: "assistant", content: [{ type: "text", text: "还在这儿。" }] },
    ],
    digest: "e".repeat(64),
  },
];

const TURNS = [
  {
    turn_ordinal: 1,
    run_id: RUN_DONE,
    transcript: [
      { role: "user", content: [{ type: "text", text: "我叫小林，请记住" }] },
      { role: "assistant", content: [{ type: "text", text: "记住了。" }] },
    ],
    digest: "a".repeat(64),
  },
  {
    turn_ordinal: 2,
    run_id: RUN_LIVE,
    transcript: [
      { role: "user", content: [{ type: "text", text: "我刚才说我叫什么？" }] },
      { role: "assistant", content: [{ type: "text", text: "小林。" }] },
    ],
    digest: "b".repeat(64),
  },
];

export function installFakeRuntime({ activeRunId = null }: { activeRunId?: string | null } = {}) {
  const control = vi.fn(async () => ({ ok: true as const, value: {} }));
  const submit = vi.fn(async () => ({ ok: true as const, value: RUN_DONE }));
  const head = () => ({
    session_id: SESSION,
    branch_id: SESSION_BRANCH,
    generation: 1,
    turn_count: TURNS.length,
    history_digest: "c".repeat(64),
    active_run_id: activeRunId,
  });
  const olderHead = () => ({
    session_id: OLDER_SESSION,
    branch_id: OLDER_BRANCH,
    generation: 1,
    turn_count: OLDER_TURNS.length,
    history_digest: "f".repeat(64),
    active_run_id: null,
  });
  const turnsFor = (sessionId: string) => (sessionId === OLDER_SESSION ? OLDER_TURNS : TURNS);
  const sessionStart = vi.fn(async (_request: {
    sessionId: string; branchId: string; runId: string; input: string;
  }) => ({
    ok: true as const, value: { head: head(), run_id: RUN_LIVE, owner_epoch: 1, state: { state: "running" } },
  }));
  const sessionContinue = vi.fn(async (_request: {
    sessionId: string; branchId: string; generation: number; runId: string; input: string;
  }) => ({
    ok: true as const, value: { head: head(), run_id: RUN_LIVE, owner_epoch: 1, state: { state: "running" } },
  }));
  /// The stream, as the host delivers it: a subscription plus a way to push.
  /// Tests drive `emit` to make an event arrive between polls, which is the
  /// only way to tell a streamed transcript from a polled one.
  const listeners = new Set<(payload: { runId: string; event: unknown }) => void>();
  const watch = vi.fn(async (_request: { runId: string; afterSequence?: number }) => ({
    ok: true as const, value: { watching: true },
  }));
  const unwatch = vi.fn(async (_runId: string) => ({ ok: true as const, value: {} }));
  /// A small workspace, shaped like `workspace.cjs` answers. The escape is not
  /// simulated here -- containment is the host's, and it is tested against a
  /// real filesystem in `workspace.test.ts`.
  const FILES: Record<string, { entries: unknown[] }> = {
    "": {
      entries: [
        { name: "src", kind: "folder", size: null, modified: "2026-08-18T09:00:00.000Z" },
        { name: "notes.txt", kind: "file", size: 56, modified: "2026-08-18T09:30:00.000Z" },
      ],
    },
    src: {
      entries: [{ name: "main.rs", kind: "file", size: 30, modified: "2026-08-18T09:10:00.000Z" }],
    },
  };
  const steer = vi.fn(async (_request: { runId: string; steeringId: string; input: string }) => ({
    ok: true as const, value: {},
  }));
  const launch = vi.fn(async () => ({ ok: true as const, value: { started: true, owned: true } }));
  const saveProvider = vi.fn(async (_request: {
    id: string; protocol: string; endpoint: string; model: string; secret?: string | null;
  }) => ({ ok: true as const, value: { id: "local-stub" } }));
  const forgetProvider = vi.fn(async (_id: string) => ({ ok: true as const, value: { id: "local-stub" } }));
  const sessionRead = vi.fn(async (request: { sessionId: string; branchId: string }) => ({
    ok: true as const,
    value: request.sessionId === OLDER_SESSION ? olderHead() : head(),
  }));
  const runtime = {
    status: async () => ({ ok: true as const, value: status() }),
    probe: async () => ({ ok: true as const, value: status() }),
    // The owner surface's shape: what each Run was asked to do and where it
    // got to, not a list of ids. A fake that still answered ids would let the
    // renderer pass against a contract the runtime no longer has.
    list: async () => ({
      ok: true as const,
      value: {
        runs: [
          { run_id: RUN_WAITING, input: "run a shell command", state: { state: "waiting_approval" } },
          { run_id: RUN_LIVE, input: "something still going", state: { state: "running" } },
          { run_id: RUN_DONE, input: "something finished", state: { state: "finished", status: "succeeded" } },
          {
            run_id: RUN_PROCESS, input: "open a shell session",
            state: { state: "finished", status: "succeeded" },
          },
        ],
        nextAfterRunId: null,
      },
    }),
    lifecycle: async () => ({
      ok: true as const,
      value: {
        lifecycle: "ready",
        recovery: { completed_profiles: 1, total_profiles: 1 },
        active_runs: 1,
        queued_runs: 0,
        recovery_failures: 0,
        previous_shutdown: null,
      },
    }),
    startRuntime: async () => ({ ok: true as const, value: true }),
    shutdown: async () => ({ ok: true as const, value: {} }),
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
    steer,
    sessionStart,
    sessionContinue,
    sessionRead,
    // No secret, because the host has no call that returns one.
    providers: async () => ({
      ok: true as const,
      value: [{
        id: "local-stub",
        protocol: "openai_compatible",
        endpoint: "http://127.0.0.1:51234/v1/chat/completions",
        model: "stub",
        hasSecret: true,
        secretSetAt: "2026-08-18T09:00:00.000Z",
      }],
    }),
    watch,
    unwatch,
    onEvent: (handler: (payload: { runId: string; event: unknown }) => void) => {
      listeners.add(handler);
      return () => listeners.delete(handler);
    },
    onWatchEnded: () => () => {},
    launch,
    workspace: async () => ({ ok: true as const, value: { root: "/tmp/workspace", configured: true } }),
    listFiles: async (relative: string) => (
      FILES[relative]
        ? { ok: true as const, value: { path: relative, entries: FILES[relative].entries, truncated: false } }
        : { ok: false as const, error: "no such path in the workspace" }
    ),
    readFile: async (relative: string) => (
      relative === "notes.txt"
        ? {
          ok: true as const,
          value: { path: relative, binary: false, size: 56, truncated: false, text: "扫描每个 run 目录" },
        }
        : { ok: false as const, error: "that path is outside the workspace" }
    ),
    saveProvider,
    forgetProvider,
    // Ascending by id, the order `list_session_heads` returns.
    sessionList: async () => ({
      ok: true as const, value: { heads: [olderHead(), head()], nextAfter: null },
    }),
    // The daemon pages history and answers `limit: 1` with exactly one Turn,
    // which is what the list rows ask for and all they need.
    sessionHistory: async (
      { sessionId, limit = null }: { sessionId: string; limit?: number | null },
    ) => {
      const turns = turnsFor(sessionId);
      return {
        ok: true as const,
        value: { turns: limit === 1 ? turns.slice(0, 1) : turns, nextAfterTurnOrdinal: null },
      };
    },
  };
  const status = () => ({
    transport: "local", stateRoot: "/tmp/state", socketPath: "/tmp/state/runtime-host.sock",
    connected: true, error: null,
  });
  const desk = { mounted: vi.fn(), drew: vi.fn(), runtime };
  (window as unknown as { desk: typeof desk }).desk = desk;
  const emit = (runId: string, event: Record<string, unknown>) => {
    for (const listener of listeners) listener({ runId, event });
  };
  return {
    control, submit, sessionStart, sessionContinue, sessionRead,
    saveProvider, forgetProvider, watch, unwatch, emit, event, launch, steer, desk,
  };
}
