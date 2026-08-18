/// What this file is for.
///
/// The transcript drew an event only if this client had a phrase for it, and
/// the map of phrases had drifted a long way behind the kernel. Nine event
/// types the runtime writes today reached the column and were dropped without
/// a mark: a model refusing, a transcript being compacted into a summary, a
/// provider retry, an approval re-bound under a new digest. The most useful
/// thing a runtime can tell a client -- something new happened -- was the one
/// thing this client was guaranteed to hide.
///
/// Two properties are held here. The named events say what they mean. And an
/// event type nobody here has ever read still appears, because deciding an
/// unknown event is unimportant is a judgement made without the information
/// needed to make it.
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { event, installFakeRuntime, RUN_LIVE, RUN_WAITING } from "./fake-runtime";
import { knownEvent } from "../surfaces/model";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

/// A Run in flight, with its transcript on screen.
async function watching() {
  const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
  render(<App />);
  await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
  return { bridge, user: userEvent.setup() };
}

describe("events the runtime reports about the exchange", () => {
  it("shows a refusal, and the words the model refused with", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "model.refusal", { text: "这件事我不做。" }, 30));
    await waitFor(() => expect(screen.getByText("模型拒绝回答")).toBeTruthy());
    // The label alone would have replaced what the model said with six
    // characters, which is the same loss as drawing nothing.
    expect(screen.getByText("这件事我不做。")).toBeTruthy();
  });

  /// A configured MCP server that never came up.
  ///
  /// The Run carries on without those Tools, so the only observable difference
  /// was that the model never used them -- which reads exactly like the model
  /// choosing not to. Both halves are held here: the failure is said, and the
  /// ordinary case where every server answered stays out of the column.
  it("names an MCP server that did not come up, and stays quiet when they all did", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "mcp.discovery.completed", {
      servers: [
        { server_name: "docs", required: false, health: "ready", attempts: 1, capabilities: [] },
        {
          server_name: "notes", required: false, health: "unavailable", attempts: 3,
          capabilities: [], error: "server exited before initialize",
        },
      ],
    }, 30));
    await waitFor(() => expect(screen.getByText(/MCP 服务没起来/)).toBeTruthy());
    const said = screen.getByText(/MCP 服务没起来/).textContent ?? "";
    expect(said).toContain("notes");
    expect(said).toContain("试了 3 次");
    expect(said).toContain("server exited before initialize");
    // The one that answered is not named: a line listing what worked is a
    // machine log printed through a conversation.
    expect(said).not.toContain("docs");

    bridge.emit(RUN_LIVE, bridge.event(41, "mcp.discovery.completed", {
      servers: [
        { server_name: "docs", required: false, health: "ready", attempts: 1, capabilities: [] },
      ],
    }, 31));
    // Still exactly one line, from the first event.
    await waitFor(() => expect(screen.getAllByText(/MCP 服务没起来/).length).toBe(1));
  });

  it("says the transcript behind the model is a summary now", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "context.compacted", {
      binding_digest: "b".repeat(64), summary_digest: "s".repeat(64), source_message_count: 42,
    }, 30));
    await waitFor(() => expect(screen.getByText("上文压成了摘要")).toBeTruthy());
  });

  it("says a Provider failure is being retried rather than leaving a gap", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "model.provider.retry_scheduled", {
      provider_id: "local-stub", provider_attempt: 2, delay_ms: 800, kind: "transport",
    }, 30));
    await waitFor(() => expect(screen.getByText("已安排重试 Provider")).toBeTruthy());
  });

  it("says a tool whose outcome was never seen is being run again", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "tool.retry_requested", { status: "running" }, 30));
    await waitFor(() => expect(screen.getByText("结果未知，重试工具")).toBeTruthy());
  });
});

describe("an event type this build has never heard of", () => {
  it("appears where it happened, with its type and its sequence", async () => {
    const { bridge, user } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "run.checkpoint.pruned", { kept: 3 }, 30));
    // Found through the row rather than by the type string alone: the status
    // line names the last event too, and "this string is on the page" would
    // pass with the transcript still dropping it.
    const row = await screen.findByText("本版本不认识的事件");
    expect(row.closest("button")!.textContent).toContain("run.checkpoint.pruned");

    await user.click(row);
    // The payload is real and it is the only account of what happened, so it
    // is one click away rather than gone. Nothing is invented around it.
    await waitFor(() => expect(screen.getByText("第 40 条・run.checkpoint.pruned")).toBeTruthy());
    expect(screen.getByText(/"kept": 3/)).toBeTruthy();
  });

  it("folds a run of them into one row that still names the types", async () => {
    const { bridge } = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "run.checkpoint.pruned", {}, 30));
    bridge.emit(RUN_LIVE, bridge.event(41, "run.checkpoint.pruned", {}, 30));
    bridge.emit(RUN_LIVE, bridge.event(42, "policy.reloaded", {}, 30));
    // Three rules across the column would be the log dump this is avoiding; a
    // row saying only "3 events" would be the other failure.
    const row = await screen.findByText("3 条本版本不认识的事件");
    expect(row.closest("button")!.textContent).toContain("run.checkpoint.pruned・policy.reloaded");
  });

  it("does not call routine bookkeeping unknown", async () => {
    const { bridge } = await watching();
    for (const [sequence, type] of [
      [40, "tool.execution.requested"], [41, "tool.execution.auto_approved"],
      [42, "tool.execution.started"], [43, "tool.execution.progress"],
      [44, "mcp.input.continuation.started"], [45, "model.usage"],
    ] as [number, string][]) {
      bridge.emit(RUN_LIVE, bridge.event(sequence, type, {}, 30));
    }
    bridge.emit(RUN_LIVE, bridge.event(46, "policy.reloaded", {}, 30));
    // One row, naming one type.
    //
    // The first version of this asserted only that a row saying "本版本不认识
    // 的事件" existed, and it passed with all six of them drawn as unknown:
    // `model.usage` stayed routine, split the run in two, and left the tail
    // group holding `policy.reloaded` alone under the very label being looked
    // for. Anything leaking through now either brings a second row or writes
    // its name into this one.
    const rows = await screen.findAllByText(/本版本不认识的事件/);
    expect(rows).toHaveLength(1);
    expect(rows[0].closest("button")!.querySelector(".mono")!.textContent)
      .toBe("policy.reloaded");
  });
});

/// The kernel is the only place run event types are minted, and this is the
/// check that the client's account of them stays complete. It is what was
/// missing: nine types were added there over time and nothing here noticed.
///
/// It scans for dotted lowercase literals, which today are all event types --
/// `lib.rs` has no test module and no other string of that shape. If this ever
/// fails because a literal like that was added for something else, narrow the
/// scan; do not widen the client's map to swallow it.
describe("the client's account of the runtime's vocabulary", () => {
  /// Walked up from the working directory rather than resolved against
  /// `import.meta.url`, which under Vite is an http URL and not a path at all.
  function kernelSource(): string {
    const relative = "runtime/crates/kernel/src/lib.rs";
    for (let at = process.cwd(); at !== dirname(at); at = dirname(at)) {
      const candidate = resolve(at, relative);
      if (existsSync(candidate)) return candidate;
    }
    throw new Error(`no ${relative} above ${process.cwd()}`);
  }

  it("has a place for every event type the kernel emits", () => {
    const source = readFileSync(kernelSource(), "utf8");
    const types = [...new Set(source.match(/"[a-z][a-z_]*(?:\.[a-z][a-z_]*)+"/g) ?? [])]
      .map((quoted) => quoted.slice(1, -1));
    expect(types.length).toBeGreaterThan(30);
    expect(types.filter((type) => !knownEvent(type))).toEqual([]);
  });
});

describe("an approval re-bound after a recovery", () => {
  /// `approval.rebound` carries the whole request again under a fresh binding,
  /// and a decision taken against the digest it replaced does not bind. The
  /// gate went on offering the old one, so the buttons on screen could not do
  /// what they said.
  it("moves the gate onto the binding the runtime is now holding", async () => {
    const user = userEvent.setup();
    installFakeRuntime({
      later: {
        [RUN_WAITING]: [event(6, "approval.rebound", {
          status: "waiting_approval",
          approval: {
            approval_id: "01a0122b-217e-7e72-bec8-ad3273f16cd3",
            execution: {
              binding_digest: "9f".repeat(32),
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
        })],
      },
    });
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
    await user.click(
      screen.getAllByRole("button", { name: /^待决定/ }).find((n) => n.classList.contains("r"))!,
    );
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await user.click(
      screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
    );
    await waitFor(() => expect(screen.getByText(/绑定 9f9f9f9f9f9f9f9f/)).toBeTruthy());
    expect(screen.queryByText(/绑定 3be24149daa5170d/)).toBeNull();
  });
});
