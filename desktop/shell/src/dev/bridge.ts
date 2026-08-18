/// A bridge for looking at the interface, and only for that.
///
/// The tests answer "does it say the right thing"; they cannot answer "does it
/// look like something a person would want to use", and that question has been
/// got wrong here before with every test green. This installs a `window.desk`
/// in the vite dev server so every surface can be opened in a browser with
/// content in it, in both themes.
///
/// Loaded only under `import.meta.env.DEV` and only for `?fake`, so it cannot
/// reach a packaged build. It is deliberately not the test fake: that one is
/// built on `vi.fn()` and pulling it in here would put vitest in the bundle.
/// The shapes are copied from it, which is the same source the tests use.
const RUN_WAITING = "01a0122b-217e-7e72-bec8-ad3273f16cd1";
const RUN_DONE = "01a0122a-18c8-7012-972a-d422fe9abde8";
const RUN_LIVE = "01a0122c-3a91-7c15-8e44-cd1234567890";
const RUN_UNJUDGED = "01a01230-7c1d-70f4-9a63-5f2e8b0d41aa";
const RUN_INPUT = "01a0122e-4c11-7b90-9d63-1f8ac4b57e20";
const RUN_PROCESS = "01a01519-9102-72e2-b80e-f0990dcbd799";
const SESSION = "01a01228-3d51-7f83-9c2b-8e4a1d5f6c70";
const BRANCH = "01a01228-3d51-7f83-9c2b-8e4a1d5f6c71";
const PROCESS_SESSION = "01a0151c-914a-7c31-8f0d-1b7c1a4e5d20";

let sequence = 0;
function ev(type: string, payload: Record<string, unknown>, runId = RUN_WAITING, minute = 0) {
  sequence += 1;
  return {
    event_id: `${runId.slice(0, 8)}-0000-4000-8000-${String(sequence).padStart(12, "0")}`,
    sequence,
    run_id: runId,
    timestamp:
      `2026-08-18T00:${String(minute).padStart(2, "0")}:${String(sequence % 60).padStart(2, "0")}.000Z`,
    type,
    payload,
    digest: "d".repeat(64),
  };
}

const output = (over: Record<string, unknown>) => ({
  session_id: PROCESS_SESSION, state: "running", pid: 66775, exit_code: null,
  termination_reason: null,
  stdout: "", stdout_start_cursor: 0, stdout_cursor: 0, stdout_truncated: false,
  stderr: "", stderr_start_cursor: 0, stderr_cursor: 0, stderr_truncated: false,
  ...over,
});
const call = (id: string, name: string, args: Record<string, unknown>, runId: string) =>
  ev("model.tool_call", { id, name, arguments: args }, runId, 20);
const result = (id: string, content: Record<string, unknown>, runId: string) =>
  ev(
    "tool.result",
    { tool_call_id: id, binding_digest: "b".repeat(64), content, is_error: false },
    runId,
    20,
  );

const LOGS: Record<string, { state: Record<string, unknown>; events: ReturnType<typeof ev>[] }> = {
  [RUN_WAITING]: {
    state: { state: "waiting_approval" },
    events: [
      ev("run.started", { status: "running" }),
      ev("model.output.delta", { text: "先看一眼目录里有什么，再决定改哪一个文件。" }),
      ev("model.usage", { input_tokens: 180, output_tokens: 24, cost_micros: 0 }),
      ev("model.tool_call", {
        name: "shell.exec", arguments: { command: "ls -la" }, id: "stub-call-1",
      }),
      ev("approval.required", {
        status: "waiting_approval",
        approval: {
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
            required_scopes: ["tool:shell.exec"], sandbox: "trusted_native",
            tool_name: "shell.exec",
          },
        },
      }),
    ],
  },
  [RUN_INPUT]: {
    state: { state: "suspended" },
    events: [
      ev("run.started", { status: "running" }, RUN_INPUT, 10),
      ev("mcp.input.required", {
        status: "suspended",
        input_version: 1,
        input: {
          schema_version: 1,
          input_id: "01a0122e-4c11-7b90-9d63-1f8ac4b57e21",
          server_id: "01a0122e-4c11-7b90-9d63-1f8ac4b57e22",
          server_name: "docs",
          tool_call_id: "stub-call-7",
          binding_digest: "7c9f".repeat(16),
          round: 1,
          request_state: "network-state-byte-exact",
          requests: {
            confirmation: {
              mode: "form",
              message: "确认这次检索",
              requested_schema: {
                type: "object",
                properties: {
                  confirmed: { type: "boolean" },
                  note: { type: "string", title: "备注", description: "要一起带过去的话" },
                },
                required: ["confirmed"],
              },
            },
            verification: {
              mode: "url",
              message: "在浏览器里完成验证",
              url: "https://docs.example.test/verify/9f2",
              elicitation_id: "elicit-9f2",
            },
          },
        },
      }, RUN_INPUT, 10),
    ],
  },
  [RUN_UNJUDGED]: {
    state: { state: "terminal", status: "indeterminate" },
    events: [
      ev("run.started", { status: "running" }, RUN_UNJUDGED, 15),
      ev("model.tool_call", {
        name: "shell.exec", arguments: { command: "make release" }, id: "c-9",
      }, RUN_UNJUDGED, 15),
      ev("run.terminated", {
        status: "indeterminate", reason: "duration_budget",
      }, RUN_UNJUDGED, 15),
    ],
  },
  [RUN_LIVE]: {
    state: { state: "running" },
    events: [
      ev("run.started", { status: "running" }, RUN_LIVE, 30),
      ev("model.output.delta", { text: "还在读第二个文件……" }, RUN_LIVE, 30),
    ],
  },
  [RUN_PROCESS]: {
    state: { state: "terminal", status: "succeeded" },
    events: [
      ev("run.started", { status: "running" }, RUN_PROCESS, 20),
      call("call-start", "process.start", {
        initial_stdin: "echo hello-from-session\n",
        tty: true, cols: 100, rows: 30, yield_time_ms: 2000,
      }, RUN_PROCESS),
      result("call-start", output({
        stdout: "echo hello-from-session\r\n", stdout_cursor: 25,
      }), RUN_PROCESS),
      call("call-write", "process.write", {
        session_id: PROCESS_SESSION, stdout_cursor: 25, stderr_cursor: 0,
        stdin: "printf 'line-two\\n'\n", yield_time_ms: 2000,
      }, RUN_PROCESS),
      result("call-write", output({
        stdout: "printf 'line-two\\n'\r\n", stdout_start_cursor: 25, stdout_cursor: 46,
      }), RUN_PROCESS),
      call("call-attach", "process.attach", {
        session_id: PROCESS_SESSION, max_bytes: 19,
      }, RUN_PROCESS),
      // A bounded tail read: the log reached 531 bytes while nothing was
      // reading it, so 46-512 exists only in the session's own file.
      result("call-attach", output({
        stdout: "[32mline-two[0m\r\n",
        stdout_start_cursor: 512, stdout_cursor: 531, stdout_truncated: true,
      }), RUN_PROCESS),
      call("call-close", "process.close", {
        session_id: PROCESS_SESSION, stdout_cursor: 531, stderr_cursor: 0,
      }, RUN_PROCESS),
      result("call-close", output({
        state: "terminated", pid: null, termination_reason: "closed",
        stdout_start_cursor: 531, stdout_cursor: 531,
      }), RUN_PROCESS),
      ev("run.succeeded", { status: "succeeded" }, RUN_PROCESS, 20),
    ],
  },
  [RUN_DONE]: {
    state: { state: "terminal", status: "succeeded" },
    events: [
      ev("run.started", { status: "running" }, RUN_DONE),
      ev("model.output.delta", {
        text: "改好了。注意 `notes.txt` 原来是空的，现在有一行。",
      }, RUN_DONE),
      ev("model.tool_call", {
        name: "workspace.write", arguments: { path: "notes.txt", contents: "x" }, id: "stub-call-2",
      }, RUN_DONE),
      ev("run.succeeded", { status: "succeeded" }, RUN_DONE),
    ],
  },
};

const TURNS: SessionTurn[] = [
  {
    turn_ordinal: 1, run_id: RUN_WAITING, digest: "a1b2".repeat(16),
    transcript: [
      { role: "user", content: [{ type: "text", text: "帮我看看这个目录" }] },
      { role: "assistant", content: [{ type: "text", text: "先看一眼目录里有什么。" }] },
    ],
  },
  {
    turn_ordinal: 2, run_id: RUN_DONE, digest: "c3d4".repeat(16),
    transcript: [
      { role: "user", content: [{ type: "text", text: "把结论写进 notes.txt" }] },
      { role: "assistant", content: [{ type: "text", text: "改好了。" }] },
    ],
  },
];

import type { Bridge, SessionTurn } from "../runtime";

const ok = <T,>(value: T) => ({ ok: true as const, value });

const HEAD = {
  session_id: SESSION, branch_id: BRANCH, generation: 1, turn_count: 2,
  active_run_id: null, history_digest: "a1b2c3d4".repeat(8),
};

const receipt = () => ({
  head: HEAD, run_id: RUN_DONE, owner_epoch: null,
  state: { state: "running" },
});

/// Installs the bridge. Returns nothing: the app reads `window.desk`.
export function installDevBridge() {
  const status = {
    transport: "local", stateRoot: "/tmp/dev-state",
    socketPath: "/tmp/dev-state/runtime-host.sock", connected: true, error: null,
  };
  const runtime: Bridge = {
    status: async () => ok(status),
    probe: async () => ok(status),
    list: async () => ok({
      runs: [
        { run_id: RUN_WAITING, input: "看看这个目录", state: { state: "waiting_approval" } },
        { run_id: RUN_INPUT, input: "查一下文档", state: { state: "suspended" } },
        { run_id: RUN_LIVE, input: "读两个文件", state: { state: "running" } },
        {
          run_id: RUN_UNJUDGED, input: "跑一次发布",
          state: { state: "finished", status: "indeterminate" },
        },
        {
          run_id: RUN_PROCESS, input: "开一个 shell 会话",
          state: { state: "finished", status: "succeeded" },
        },
        {
          run_id: RUN_DONE, input: "把结论写进 notes.txt",
          state: { state: "finished", status: "succeeded" },
        },
      ],
      nextAfterRunId: null,
    }),
    lifecycle: async () => ok({
      lifecycle: "ready", recovery: { completed_profiles: 1, total_profiles: 1 },
      active_runs: 1, queued_runs: 0, recovery_failures: 0, previous_shutdown: null,
    }),
    startRuntime: async () => ok(true),
    shutdown: async () => ok({}),
    events: async ({ runId }) => {
      const log = LOGS[runId];
      if (!log) {
        return ok({ ok: false as const, error: { code: "not_found", message: "no such run" } });
      }
      const last = log.events[log.events.length - 1]?.sequence ?? 0;
      return ok({
        ok: true as const,
        page: {
          run_id: runId,
          requested_after_sequence: 0,
          next_after_sequence: last,
          earliest_available_sequence: log.events[0]?.sequence ?? null,
          highest_committed_sequence: last,
          history_gap: false,
          has_more: false,
          state: log.state,
          events: log.events,
        },
      });
    },
    watch: async () => ok({ watching: true }),
    unwatch: async () => ok({}),
    onEvent: () => () => {},
    onWatchEnded: () => () => {},
    submit: async () => ok(RUN_DONE),
    control: async () => ok({}),
    steer: async () => ok({}),
    launch: async () => ok({ started: true, owned: true }),
    providers: async () => ok([{
      id: "local-stub", protocol: "openai_compatible",
      endpoint: "http://127.0.0.1:8081/v1", model: "stub-1", hasSecret: true,
      secretSetAt: "2026-08-18T09:00:00.000Z",
    }]),
    saveProvider: async () => ok({ id: "local-stub" }),
    forgetProvider: async () => ok({ id: "local-stub" }),
    mcpServers: async () => ok({
      servers: [{
        name: "filesystem", command: "/opt/homebrew/bin/npx",
        args: ["-y", "@modelcontextprotocol/server-filesystem"], cwd: null,
        toolNames: ["read_file"], required: false, scope: "tool:mcp:filesystem",
        digest: "9f2c41ab7d0e5613", addedAt: "2026-08-18T09:00:00.000Z",
      }],
      applied: [{ name: "filesystem", digest: "9f2c41ab7d0e5613" }],
    }),
    saveMcpServer: async () => ok({ name: "filesystem" }),
    forgetMcpServer: async () => ok({ name: "filesystem" }),
    resolveMcpInput: async () => ok({}),
    workspace: async () => ok({ root: "/tmp/dev-state/workspace", configured: true }),
    listFiles: async (relative: string) => ok({
      path: relative,
      truncated: false,
      entries: [
        { name: "notes.txt", kind: "file" as const, size: 14, modified: "2026-08-18T09:00:00.000Z" },
        { name: "runtime", kind: "folder" as const, size: null, modified: null },
      ],
    }),
    readFile: async () => ok({
      path: "notes.txt", binary: false, size: 14, truncated: false, text: "扫描每个 run 目录",
    }),
    sessionList: async () => ok({ heads: [HEAD], nextAfter: null }),
    sessionRead: async () => ok(HEAD),
    sessionHistory: async ({ limit = null }) =>
      ok({ turns: limit === 1 ? TURNS.slice(0, 1) : TURNS, nextAfterTurnOrdinal: null }),
    sessionStart: async () => ok(receipt()),
    sessionContinue: async () => ok(receipt()),
    sessionFork: async () => ok({ ...HEAD, branch_id: `${BRANCH}f`, turn_count: 1 }),
    sessionRollback: async () => ok({ ...HEAD, generation: 2, turn_count: 1 }),
  };
  (window as unknown as { desk: unknown }).desk = {
    mounted: (count: number) => console.log(`shell mounted, ${count} surface(s)`),
    drew: (summary: unknown) => console.log("drew", JSON.stringify(summary)),
    waiting: () => {},
    onAttend: () => () => {},
    runtime,
  };
}
