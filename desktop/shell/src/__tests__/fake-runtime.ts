/// A bridge that answers like the host, for tests.
///
/// It returns the same shapes `localRuntime.cjs` returns, including the page
/// ceiling, so a test cannot pass against a friendlier runtime than the real
/// one. Run ids and payload shapes are copied from a real dev-runtime session.
import { vi } from "vitest";
import { uuidv7 } from "../ids";
import type { Reply } from "../runtime";

export const RUN_WAITING = "01a0122b-217e-7e72-bec8-ad3273f16cd1";
export const RUN_DONE = "01a0122a-18c8-7012-972a-d422fe9abde8";
export const RUN_LIVE = "01a01231-9f40-7d31-8c22-6b1a0e55c704";
/// The third Turn's Run.
///
/// Its own, because one Turn is one Run: `valid_session_conversation_history`
/// inserts each Turn's run id into a set and refuses the history if the insert
/// finds one already there. A fake that gave two Turns of one branch the same
/// Run id would be standing for a conversation the runtime would not store.
export const RUN_NOTED = "01a01232-5b31-7a44-9c07-3f2e6b0d1a95";

export const RUN_INPUT = "01a0122e-4c11-7b90-9d63-1f8ac4b57e20";

/// One MCP input round, as `mcp.input.required` carries it.
///
/// The form request is the shape the runtime's own MCP round-trip test drives
/// (`runtime/apps/runtime-host/tests/grpc_invocation_mcp_input.rs`), with one
/// optional field added. The URL request is the protocol's other elicitation
/// mode. Two of them in one round because that is the part of the contract a
/// client gets wrong: a resolution must answer the exact pending key set, so
/// answering one of these is not answering the request.
/// The pending round's own id and version, which is how the host is told to
/// remember one question by. Exported for the same reason `APPROVAL_ID` is.
export const MCP_INPUT_ID = "01a0122e-4c11-7b90-9d63-1f8ac4b57e21";
export const MCP_INPUT_VERSION = 1;

const MCP_INPUT = {
  schema_version: 1,
  input_id: MCP_INPUT_ID,
  server_id: "01a0122e-4c11-7b90-9d63-1f8ac4b57e22",
  server_name: "docs",
  tool_call_id: "stub-call-7",
  binding_digest: "7c9f1f5b0f5d4a2e8b6c3d1a9e7f2b4c6d8e0a2c4e6f8a0b2d4f6a8c0e2f4b6d",
  round: 1,
  request_state: "network-state-byte-exact",
  requests: {
    confirmation: {
      mode: "form",
      message: "Confirm this search",
      requested_schema: {
        type: "object",
        properties: {
          confirmed: { type: "boolean" },
          note: { type: "string", title: "Note", description: "Anything to pass along" },
        },
        required: ["confirmed"],
      },
    },
    verification: {
      mode: "url",
      message: "Finish verification in your browser",
      url: "https://docs.example.test/verify/9f2",
      elicitation_id: "elicit-9f2",
    },
  },
};

/// A Run that reached a terminal boundary nobody can judge: the tool was cut
/// off mid-execution and the runtime will not guess whether the effect landed.
/// It blocks a person exactly as an approval does, and it is the one kind of
/// waiting that has no approval to name it by — which is the whole reason the
/// second branch of `waiting()` exists.
export const RUN_UNJUDGED = "01a01230-7c1d-70f4-9a63-5f2e8b0d41aa";
/// The approval's own id, as the runtime wrote it into the log. Exported
/// because it is what the host is told to remember one question by.
export const APPROVAL_ID = "01a0122b-217e-7e72-bec8-ad3273f16cd2";

/// A Run that failed. Which way it failed is the caller's to choose, and both
/// spellings are the Kernel's own: `run.failed` is one event type with a `kind`
/// that separates a missing MCP server from a budget that ran out, and a client
/// reading the type alone would report the second as the first.
export const RUN_FAILED = "01a01240-1c3a-7b90-9f01-5d5f1c0b7e22";

export const RUN_PROCESS = "01a01519-9102-72e2-b80e-f0990dcbd799";

/// The durable process session the `process.*` events below belong to. Taken,
/// with its cursors, from a runtime-host run recorded against a real PTY.
export const PROCESS_SESSION = "01a0151c-914a-7c31-8f0d-1b7c1a4e5d20";

const APPROVAL = {
  approval_id: APPROVAL_ID,
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

export function event(
  sequence: number, type: string, payload: Record<string, unknown>,
  minute = 0, runId = RUN_WAITING,
) {
  return {
    // Derived from the Run, because an id built from the sequence alone is the
    // same id in every Run's log.
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

type Log = { state: Record<string, unknown>; events: ReturnType<typeof event>[] };

/// The runs this host has started, built per install so a test can hand the
/// MCP round a request set of its own.
function logs(mcpRequests: Record<string, unknown>): Record<string, Log> {
  return {
    /// A whole PTY session as the durable log holds one.
    ///
    /// Every shape here was recorded from a runtime-host run against a real
    /// `/bin/sh` on a PTY: the flat `model.tool_call` payload, the
    /// `ProcessSessionOutput` inside `tool.result`, and the byte cursors that
    /// make each read locatable. What is constructed rather than recorded is the
    /// pair of bounded `process.attach` reads -- a tail read whose `max_bytes`
    /// is smaller than the log is how bytes the agent never read become a hole
    /// the client has to say out loud.
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
    // Suspended on an MCP server's input request. `suspended` is the boundary
    // the local adapter reports for exactly this, and `input_version` sits
    // beside the request rather than inside it -- both are what the client has
    // to read to answer.
    [RUN_INPUT]: {
      state: { state: "suspended" },
      events: [
        event(1, "run.started", { status: "running" }, 10, RUN_INPUT),
        event(2, "model.tool_call", {
          name: "mcp:docs/confirm_search", arguments: { query: "retention sweep" }, id: "stub-call-7",
        }, 10, RUN_INPUT),
        event(3, "mcp.input.required", {
          input: { ...MCP_INPUT, requests: mcpRequests }, input_version: 1, status: "suspended",
        }, 10, RUN_INPUT),
      ],
    },
    [RUN_UNJUDGED]: {
    state: { state: "terminal", status: "indeterminate" },
    events: [
      event(1, "run.started", { status: "running" }, 15, RUN_UNJUDGED),
      // Flat, like the parked Run's above: that is the shape the runtime
      // actually writes, and this fixture is the one place that claim is
      // held. Written nested first, which passed only because the reader
      // accepts both -- exactly the drift the note on RUN_WAITING warns of.
      event(2, "model.tool_call",
        { name: "shell.exec", arguments: { command: "rm -rf build" }, id: "stub-call-3" },
        15, RUN_UNJUDGED),
      event(3, "run.indeterminate", { status: "indeterminate" }, 15, RUN_UNJUDGED),
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
  };
}

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
  // A third Turn so that more than one Turn has something after it. With two,
  // only the first could be rolled back to, and "arming one and then pointing
  // at another" -- the case an armed irreversible control has to survive --
  // could not be written at all.
  {
    turn_ordinal: 3,
    run_id: RUN_NOTED,
    transcript: [
      { role: "user", content: [{ type: "text", text: "帮我记一下今天的日期" }] },
      { role: "assistant", content: [{ type: "text", text: "记下了。" }] },
    ],
    digest: "9".repeat(64),
  },
];

type FakeEvent = ReturnType<typeof event>;

type Turn = (typeof TURNS)[number];

/// The rule the runtime holds a branch history to, held here.
///
/// `valid_session_conversation_history` refuses a history whose ordinals are
/// not 1..n in order, and refuses one that repeats a Run id. Checked on every
/// head this fake answers, so a Turn added later cannot quietly make the fake
/// more permissive than the thing it stands for -- which is how a third Turn
/// carrying the first one's Run id survived here in the first place.
function committed(turns: Turn[]): Turn[] {
  const runs = new Set<string>();
  turns.forEach((turn, index) => {
    if (turn.turn_ordinal !== index + 1) {
      throw new Error(`Turn at index ${index} carries ordinal ${turn.turn_ordinal}`);
    }
    if (runs.has(turn.run_id)) throw new Error(`branch history repeats Run id ${turn.run_id}`);
    runs.add(turn.run_id);
  });
  return turns;
}

/// A digest of the history rather than a constant, because two branches of one
/// Session have two histories: a Fork carries a prefix of its source and must
/// not report the same digest for it.
function historyDigest(turns: Turn[]): string {
  return (turns.map((turn) => turn.digest.slice(0, 4)).join("") + "0".repeat(64)).slice(0, 64);
}

/// One configured MCP server, shaped exactly as `mcpServers.cjs` answers --
/// including `digest`, which is what makes "the runtime is running this" a
/// different fact from "a server with this name is configured".
const MCP_SERVER = {
  name: "filesystem",
  command: "/opt/homebrew/bin/npx",
  args: ["-y", "@modelcontextprotocol/server-filesystem"],
  cwd: null,
  toolNames: ["read_file"],
  required: false,
  scope: "tool:mcp:filesystem",
  digest: "9f2c41ab7d0e5613",
  addedAt: "2026-08-18T09:00:00.000Z",
};

export function installFakeRuntime(
  /// `later` appends to a run's durable log, so a test can set up a Run that had
  /// already written something before this client read it. `emit` cannot stand
  /// in for that: a streamed event is folded onto a Run whose boundary and
  /// pending approval came from the cursor, which is exactly the distinction
  /// the store draws on purpose.
  ///
  /// `gap` is the runtime saying the earlier events of a log are gone. It is a
  /// field on the cursor page, not something a client can work out, and what a
  /// transcript is allowed to claim about itself depends on it.
  ///
  /// `capped` is the other half of the same question and a different fact: the
  /// log is whole, and this client stopped paging before the end of it. Both
  /// mean "you are looking at part of a Run", and a client that only handled
  /// one of them would be silent in exactly the other case.
  ///
  /// `unreadable` is a run whose log the daemon will not return at all.
  ///
  /// `maxBranches` is the Session store's branch ceiling, so a test can reach
  /// it and watch a Fork be refused.
  {
    activeRunId = null, later = {}, gap = false, capped = false,
    unreadable = null, maxBranches = 32, mcpRequests = MCP_INPUT.requests,
    mcpApplied = [{ name: MCP_SERVER.name, digest: MCP_SERVER.digest }], failed = null,
  }: {
    activeRunId?: string | null;
    later?: Record<string, FakeEvent[]>;
    gap?: boolean;
    capped?: boolean;
    unreadable?: string | null;
    maxBranches?: number;
    /// Replaces the pending MCP round. It exists so a test can render a request
    /// this build does not understand: a newer runtime may add an elicitation
    /// mode, and the client must say so rather than draw a form for it.
    mcpRequests?: Record<string, unknown>;
    /// What the running runtime has actually loaded, which is a different
    /// fact from what is configured on disk. Null stands for a runtime that
    /// will not say.
    mcpApplied?: { name: string; digest: string }[] | null;
    /// How a fourth, failed Run ended, in the Kernel's own `kind` vocabulary.
    /// Null by default: a state root does not usually hold one, and every other
    /// test would otherwise be reading a run list with a failure in it.
    failed?: "required_mcp_unavailable" | "budget_exhausted" | null;
  } = {},
) {
  const control = vi.fn(async () => ({ ok: true as const, value: {} }));
  /// Answers as a runtime this app started and could restart. A test that wants
  /// the other case says so with `mockResolvedValueOnce` -- that reply is the
  /// one the client has to read rather than assume.
  const restart = vi.fn(async (): Promise<Reply<{
    restarted: boolean;
    reason: string | null;
    report: Record<string, unknown> | null;
    escalated?: boolean;
  }>> => ({
    ok: true,
    value: { restarted: true, reason: null, report: null, escalated: false },
  }));
  const submit = vi.fn(async () => ({ ok: true as const, value: RUN_DONE }));
  // Built per install, so a test can hand the MCP round a request set of its
  // own -- including one this build does not understand.
  const runLogs = logs(mcpRequests);
  // The two payloads the Kernel actually writes, copied field for field. Only
  // one of them carries `servers` -- which is why a client that read the event
  // type without the kind would attribute a budget to a missing server.
  //
  // Written into this install's own logs rather than a module-level map: a
  // failure left behind by one test would be read by every later one.
  if (failed) {
    runLogs[RUN_FAILED] = {
      state: { state: "terminal", status: "failed" },
      events: [
        // The Run's own id. Defaulting it would give this log an event that
        // names a different Run.
        event(1, "run.failed", failed === "required_mcp_unavailable"
          ? { status: "failed", kind: failed, servers: ["filesystem"], retryable: false }
          : { status: "failed", kind: failed, dimension: "tokens", retryable: false },
          40, RUN_FAILED),
      ],
    };
  }
  // Typed like the preload's own call, so a test can read back exactly what
  // the client decided to send.
  const resolveMcpInput = vi.fn(async (_request: {
    runId: string;
    inputId: string;
    inputVersion: number;
    bindingDigest: string;
    responses: Record<string, { action: string; content?: Record<string, unknown> }>;
  }) => ({ ok: true as const, value: {} }));
  /// Branches, the way the store root holds them: keyed by Session *and*
  /// branch, in the `(session_id, branch_id)` order `session_list` answers in.
  /// A Fork adds one here and a Rollback shortens one, so a test can read what
  /// the screen says afterwards instead of only what a mock was called with.
  const branches: { sessionId: string; branchId: string; generation: number; turns: Turn[] }[] = [
    { sessionId: OLDER_SESSION, branchId: OLDER_BRANCH, generation: 1, turns: [...OLDER_TURNS] },
    { sessionId: SESSION, branchId: SESSION_BRANCH, generation: 1, turns: [...TURNS] },
  ];
  const find = (sessionId: string, branchId: string) =>
    branches.find((branch) => branch.sessionId === sessionId && branch.branchId === branchId);
  /// `active_run_id` is the branch's, and only the Session under test has one:
  /// a Fork of a branch with a Turn in flight is refused by the daemon, so a
  /// fake that reported one everywhere would be describing an impossible state.
  const headOf = (branch: (typeof branches)[number]) => ({
    session_id: branch.sessionId,
    branch_id: branch.branchId,
    generation: branch.generation,
    turn_count: branch.turns.length,
    history_digest: historyDigest(committed(branch.turns)),
    active_run_id: branch.branchId === SESSION_BRANCH ? activeRunId : null,
  });
  const head = () => headOf(find(SESSION, SESSION_BRANCH)!);
  /// `through_turn_ordinal` is inclusive, and 0 means "carry nothing", exactly
  /// as `history_prefix` reads it in the runtime -- where an ordinal the history
  /// does not hold is an error rather than an empty prefix, which is what null
  /// is for here.
  const prefix = (turns: Turn[], throughTurnOrdinal: number): Turn[] | null => {
    if (throughTurnOrdinal === 0) return [];
    const index = turns.findIndex((turn) => turn.turn_ordinal === throughTurnOrdinal);
    return index === -1 ? null : turns.slice(0, index + 1);
  };
  /// What another window did to the Session under test while this one was
  /// looking at it. A head moves for exactly two reasons -- a Turn landed, or a
  /// Rollback took the branch to another generation -- and a client that is
  /// only ever shown a head that never moves is not being asked the question.
  /// Both go through the same branch list every reply is built from, so the
  /// next poll carries them.
  const elsewhere = {
    commits(said: string, back: string) {
      const branch = find(SESSION, SESSION_BRANCH)!;
      branch.turns = committed([...branch.turns, {
        turn_ordinal: branch.turns.length + 1,
        // Freshly minted: this Turn is a Run of its own, and a repeated id is a
        // history the runtime refuses.
        run_id: uuidv7(),
        transcript: [
          { role: "user", content: [{ type: "text", text: said }] },
          { role: "assistant", content: [{ type: "text", text: back }] },
        ],
        // Distinct from every digest written above, because the transcript is
        // keyed by it: two Turns sharing one digest would draw as one row.
        digest: `7e${(branch.turns.length + 1).toString(16).padStart(62, "0")}`,
      }]);
    },
    rollsBackTo(throughTurnOrdinal: number) {
      const branch = find(SESSION, SESSION_BRANCH)!;
      const kept = prefix(branch.turns, throughTurnOrdinal);
      if (!kept || kept.length >= branch.turns.length) {
        throw new Error(`nothing to roll back to at Turn ${throughTurnOrdinal}`);
      }
      branch.turns = kept;
      branch.generation += 1;
    },
  };
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
  /// Typed as the bridge's own reply rather than as the success shape, because
  /// the host refuses configuration the runtime would reject later and a test
  /// has to be able to make it do so.
  const saveMcpServer = vi.fn(async (_request: {
    name: string; command: string; args: string[]; cwd: string | null;
    toolNames: string[]; required: boolean;
  }): Promise<Reply<{ name: string }>> => ({ ok: true, value: { name: "filesystem" } }));
  const forgetMcpServer = vi.fn(async (_name: string) => ({
    ok: true as const, value: { name: "filesystem" },
  }));
  const sessionRead = vi.fn(async (request: { sessionId: string; branchId: string }) => {
    const branch = find(request.sessionId, request.branchId);
    return branch
      ? { ok: true as const, value: headOf(branch) }
      : { ok: false as const, error: "root Session branch does not exist" };
  });
  /// Cuts a branch, the way `fork_session_as` does: history through the named
  /// Turn, generation 1, nothing in flight. It refuses a stale generation for
  /// the same reason the daemon does -- a branch cut from a generation the
  /// source has left is a branch nobody asked for.
  const sessionFork = vi.fn(async (request: {
    sessionId: string; sourceBranchId: string; sourceGeneration: number;
    throughTurnOrdinal: number; targetBranchId: string;
  }) => {
    const source = find(request.sessionId, request.sourceBranchId);
    if (!source) return { ok: false as const, error: "root Session source branch does not exist" };
    if (source.generation !== request.sourceGeneration) {
      return { ok: false as const, error: "stale root Session generation" };
    }
    const history = prefix(source.turns, request.throughTurnOrdinal);
    if (!history) {
      return {
        ok: false as const,
        error: "root Session history does not contain the requested completed Turn",
      };
    }
    // The Session's branch ceiling, which the daemon takes as a policy rather
    // than a constant precisely so a test can stand at the boundary it
    // describes. A Fork refused here produced no branch, and a client that
    // opened one anyway would be showing a conversation nothing cut.
    const held = branches.filter((branch) => branch.sessionId === request.sessionId).length;
    if (held >= maxBranches) {
      return {
        ok: false as const,
        error: `Session already holds ${held} branches against a ceiling of ${maxBranches}`,
      };
    }
    const cut = {
      sessionId: request.sessionId,
      branchId: request.targetBranchId,
      generation: 1,
      turns: history,
    };
    branches.push(cut);
    branches.sort((a, b) => (a.sessionId + a.branchId).localeCompare(b.sessionId + b.branchId));
    return { ok: true as const, value: headOf(cut) };
  });
  /// Shortens a branch and moves it to the next generation, the way
  /// `rollback_session_at` does. An ordinal that removes nothing is refused
  /// there, so it is refused here.
  const sessionRollback = vi.fn(async (request: {
    sessionId: string; branchId: string; generation: number; throughTurnOrdinal: number;
  }) => {
    const branch = find(request.sessionId, request.branchId);
    if (!branch) return { ok: false as const, error: "root Session branch does not exist" };
    if (branch.generation !== request.generation) {
      return { ok: false as const, error: "stale root Session generation" };
    }
    const kept = prefix(branch.turns, request.throughTurnOrdinal);
    if (!kept) {
      return {
        ok: false as const,
        error: "root Session history does not contain the requested completed Turn",
      };
    }
    if (kept.length >= branch.turns.length) {
      return {
        ok: false as const,
        error: "root Session Rollback must move to an earlier completed Turn",
      };
    }
    branch.turns = kept;
    branch.generation += 1;
    return { ok: true as const, value: headOf(branch) };
  });
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
          { run_id: RUN_INPUT, input: "search the docs", state: { state: "suspended" } },
          {
            run_id: RUN_UNJUDGED,
            input: "delete the build directory",
            state: { state: "terminal", status: "indeterminate" },
          },
          { run_id: RUN_LIVE, input: "something still going", state: { state: "running" } },
          { run_id: RUN_DONE, input: "something finished", state: { state: "finished", status: "succeeded" } },
          {
            run_id: RUN_PROCESS, input: "open a shell session",
            state: { state: "finished", status: "succeeded" },
          },
          ...(failed
            ? [{
              run_id: RUN_FAILED,
              input: "search the notes",
              state: { state: "finished", status: "failed" },
            }]
            : []),
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
    restart,
    // The cap the app configured. Read by the surface so a token count is
    // shown against something rather than on its own.
    budget: async () => ({
      ok: true as const,
      value: { maxTokens: 400_000, maxCostCents: 500, maxDurationSeconds: 3_600 },
    }),
    events: async (
      { runId, afterSequence = 0, limit = 256 }:
      { runId: string; afterSequence?: number; limit?: number },
    ) => {
      // The daemon rejects an oversized page rather than clamping it. A test
      // that clamped here would hide exactly the bug this caught in practice.
      if (limit > 256) {
        return { ok: true as const, value: { ok: false as const, error: { code: "invalid_request" } } };
      }
      const log = runId === unreadable ? undefined : runLogs[runId];
      if (!log) return { ok: true as const, value: { ok: false as const, error: { code: "not_found" } } };
      // The durable log as this Run actually stands: what the fixture declared,
      // plus anything a test said had been written since. Every branch below
      // reads from this, so "the log grew" and "this page is a prefix of it"
      // stay two separate facts rather than one flag doing both jobs.
      const events = [...log.events, ...(later[runId] ?? [])];
      const highest = events[events.length - 1].sequence;
      if (capped) {
        // A log longer than this client will walk. Every page comes back full
        // and says there is more behind it, and the cursor keeps moving --
        // which is what the daemon does over a run that outran the client's
        // page budget. A page that claimed more and returned nothing would be
        // a different thing (a runtime bug), and a fake that served one would
        // send the reader down the loop's defensive break instead.
        const page = [];
        for (let sequence = afterSequence + 1; page.length < limit; sequence += 1) {
          page.push(
            sequence <= events.length
              ? events[sequence - 1]
              : event(sequence, "model.usage", {
                input_tokens: 0, output_tokens: 0, cost_micros: 0,
              }, 30),
          );
        }
        const next = afterSequence + page.length;
        return {
          ok: true as const,
          value: {
            ok: true as const,
            page: {
              run_id: runId, requested_after_sequence: afterSequence,
              next_after_sequence: next,
              earliest_available_sequence: 1,
              // Strictly ahead of this page: there is more log than the client
              // is going to read, which is the whole point of this branch.
              highest_committed_sequence: next + 1,
              history_gap: gap, has_more: true, state: log.state, events: page,
            },
          },
        };
      }
      return {
        ok: true as const,
        value: {
          ok: true as const,
          page: {
            run_id: runId, requested_after_sequence: 0,
            next_after_sequence: highest,
            earliest_available_sequence: 1,
            highest_committed_sequence: highest,
            // `gap` is the runtime's own claim that earlier events are gone; it
            // is not derivable from a page, so it comes from the fixture.
            history_gap: gap, has_more: false, state: log.state, events,
          },
        },
      };
    },
    submit,
    control,
    steer,
    resolveMcpInput,
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
    // `applied` is null for a runtime this app did not start, which is a
    // different answer from an empty list and is rendered as one.
    mcpServers: async () => ({
      ok: true as const, value: { servers: [MCP_SERVER], applied: mcpApplied },
    }),
    saveMcpServer,
    forgetMcpServer,
    // Ascending by `(session_id, branch_id)`, the order `list_session_heads`
    // returns -- which is what makes a Fork appear beside its source rather
    // than wherever the client happens to put it.
    sessionList: async () => ({
      ok: true as const, value: { heads: branches.map(headOf), nextAfter: null },
    }),
    // The daemon pages history and answers `limit: 1` with exactly one Turn,
    // which is what the list rows ask for and all they need.
    sessionHistory: async (
      { sessionId, branchId, limit = null }:
        { sessionId: string; branchId: string; limit?: number | null },
    ) => {
      const turns = find(sessionId, branchId)?.turns ?? [];
      return {
        ok: true as const,
        value: { turns: limit === 1 ? turns.slice(0, 1) : turns, nextAfterTurnOrdinal: null },
      };
    },
    sessionFork,
    sessionRollback,
  };
  const status = () => ({
    transport: "local", stateRoot: "/tmp/state", socketPath: "/tmp/state/runtime-host.sock",
    connected: true, error: null,
  });
  // The host side of the notification: what the window reports, and the way
  // back in when someone clicks one. `attend` stands in for that click.
  // Named apart from the event `listeners` above: that set is the runtime's
  // stream, this one is the host asking for a Run to be put in front of a
  // person, and the two arrive over different channels.
  const waiting = vi.fn();
  let attendee: ((runId: string) => void) | null = null;
  const onAttend = (handler: (runId: string) => void) => {
    attendee = handler;
    return () => { attendee = null; };
  };

  const desk = { mounted: vi.fn(), drew: vi.fn(), waiting, onAttend, runtime };
  (window as unknown as { desk: typeof desk }).desk = desk;
  const emit = (runId: string, event: Record<string, unknown>) => {
    for (const listener of listeners) listener({ runId, event });
  };
  return {
    control, restart, submit, sessionStart, sessionContinue, sessionRead, sessionFork, sessionRollback,
    resolveMcpInput, saveProvider, forgetProvider, saveMcpServer, forgetMcpServer,
    watch, unwatch, emit, event, launch, steer,
    desk, elsewhere, waiting, attend: (runId: string) => attendee?.(runId),
  };
}
