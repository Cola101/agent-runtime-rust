/// What this file is for.
///
/// A turn that calls a tool eleven times used to be eleven blocks between two
/// sentences, and the sentences are what a person is reading. Codex and Claude
/// Code both fold these; this client flattened them.
///
/// The fold has to keep something the flat list had. A row that only said
/// "11 calls" would have traded a wall of detail for none, so the row names the
/// tools, and the detail is one click away rather than gone.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

async function withCalls(calls: { name: string; arguments: Record<string, unknown> }[]) {
  const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
  render(<App />);
  await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
  calls.forEach((call, index) => {
    bridge.emit(RUN_LIVE, bridge.event(20 + index, "model.tool_call", { ...call, id: `c${index}` }, 30));
  });
  return { user: userEvent.setup(), bridge };
}

describe("a run of tool calls", () => {
  it("folds into one row that still names the tools", async () => {
    await withCalls([
      { name: "shell.exec", arguments: { command: "ls" } },
      { name: "shell.exec", arguments: { command: "cat notes.txt" } },
      { name: "workspace.read_text", arguments: { path: "notes.txt" } },
    ]);
    await waitFor(() => expect(screen.getByText("3 个工具调用")).toBeTruthy());
    // Counted per tool, so the row says what happened rather than only how
    // much of it happened.
    expect(screen.getByText(/shell\.exec ×2/)).toBeTruthy();
    expect(screen.getByText(/workspace\.read_text/)).toBeTruthy();
    // Folded means the arguments are not on screen yet.
    expect(screen.queryByText(/cat notes\.txt/)).toBeNull();
  });

  it("opens to the calls it folded", async () => {
    const { user } = await withCalls([
      { name: "shell.exec", arguments: { command: "ls" } },
      { name: "shell.exec", arguments: { command: "cat notes.txt" } },
    ]);
    const row = await screen.findByText("2 个工具调用");
    expect(row.closest("button")?.getAttribute("aria-expanded")).toBe("false");
    await user.click(row);
    await waitFor(() => expect(screen.getByText(/cat notes\.txt/)).toBeTruthy());
    expect(row.closest("button")?.getAttribute("aria-expanded")).toBe("true");
  });

  it("leaves a single call alone", async () => {
    await withCalls([{ name: "shell.exec", arguments: { command: "ls" } }]);
    // Hiding one line behind a control that reveals it is not a saving.
    await waitFor(() => expect(screen.getAllByText("shell.exec").length).toBeGreaterThan(0));
    expect(screen.queryByText(/个工具调用/)).toBeNull();
  });

  it("folds a real run of calls, which arrive with their results between them", async () => {
    // Every test above emitted calls back to back, and no Run does that: the
    // runtime writes `tool.result` after each call, so consecutive calls are
    // never adjacent in a real log. Drawing a rule between every stage of every
    // call is the thing the fold exists to prevent, and a successful result was
    // drawing one -- which meant the fold had never once fired outside a test.
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    const done = (sequence: number, id: string) =>
      bridge.emit(RUN_LIVE, bridge.event(sequence, "tool.result", {
        tool_call_id: id, binding_digest: "b".repeat(64), content: {}, is_error: false,
      }, 30));
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "shell.exec", arguments: { command: "ls" }, id: "c0",
    }, 30));
    done(21, "c0");
    bridge.emit(RUN_LIVE, bridge.event(22, "model.tool_call", {
      name: "shell.exec", arguments: { command: "cat notes.txt" }, id: "c1",
    }, 30));
    done(23, "c1");
    bridge.emit(RUN_LIVE, bridge.event(24, "model.tool_call", {
      name: "workspace.read_text", arguments: { path: "notes.txt" }, id: "c2",
    }, 30));
    done(25, "c2");

    await waitFor(() => expect(screen.getByText("3 个工具调用")).toBeTruthy());
    expect(screen.getByText(/shell\.exec ×2/)).toBeTruthy();
    // And a result that succeeded says nothing of its own: the note carried no
    // outcome, so it was a line per call that added nothing and cost the fold.
    expect(screen.queryByText(/工具返回/)).toBeNull();
  });

  it("breaks the fold for a tool call that failed, and says it failed", async () => {
    // The other half of the rule. A failure is not routine, and folding it into
    // a tally would report a call that did not work as one of N that ran.
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "shell.exec", arguments: { command: "ls" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64),
      content: { text: "no such file" }, is_error: true,
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(22, "model.tool_call", {
      name: "shell.exec", arguments: { command: "cat notes.txt" }, id: "c1",
    }, 30));

    await waitFor(() => expect(screen.getByText(/工具报错/)).toBeTruthy());
    // Two calls either side of it, and neither folded into the other.
    expect(screen.queryByText(/个工具调用/)).toBeNull();
  });

  it("does not fold across what the model said in between", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", { name: "a.one", arguments: {} }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "model.tool_call", { name: "a.two", arguments: {} }, 30));
    bridge.emit(RUN_LIVE, bridge.event(22, "model.output.delta", { text: "中间说了一句" }, 30));
    bridge.emit(RUN_LIVE, bridge.event(23, "model.tool_call", { name: "b.one", arguments: {} }, 30));
    bridge.emit(RUN_LIVE, bridge.event(24, "model.tool_call", { name: "b.two", arguments: {} }, 30));

    // Two folds, not one of four: what the model says after using a tool is a
    // new part of the conversation, and folding across it would put the
    // sentence inside a group it does not belong to.
    await waitFor(() => expect(screen.getAllByText("2 个工具调用")).toHaveLength(2));
    expect(screen.getByText(/中间说了一句/)).toBeTruthy();
  });
});
