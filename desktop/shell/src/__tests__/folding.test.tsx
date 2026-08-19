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

describe("what the runtime said in words", () => {
  /// `model.reasoning` carries `summary` as a list -- the kernel emits
  /// `Vec<String>` and `json!` writes an array. The client read it as a string
  /// and, finding one absent, drew the hairline and dropped every word of it.
  /// A reasoning summary reduced to its own type name is the same loss as not
  /// drawing the event at all, which is the reason this path exists.
  it("draws a reasoning summary the kernel sends as a list", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.reasoning", {
      summary: ["先看一眼目录", "再决定改哪一个文件"],
      has_private_state: true,
    }, 30));

    await waitFor(() => expect(screen.getByText(/先看一眼目录/)).toBeTruthy());
    // Both parts, and each on its own: the runtime sent two, and joining them
    // into one paragraph would report one thought where it reported two.
    expect(screen.getByText(/再决定改哪一个文件/)).toBeTruthy();
  });

  /// The finder counts the marks standing in the column, and its own rule is
  /// that everything the transcript draws which a person could search goes
  /// through `Mark`. This prose did not: it was the one part of the column
  /// ⌘F could never reach, and a summary the model wrote is exactly the kind
  /// of thing someone comes back looking for.
  it("lets the finder reach the words inside a reasoning summary", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.reasoning", {
      summary: ["先看一眼目录"], has_private_state: false,
    }, 30));
    await waitFor(() => expect(screen.getByText(/先看一眼目录/)).toBeTruthy());

    await user.keyboard("{Meta>}f{/Meta}");
    await user.keyboard("目录");
    await waitFor(() => expect(document.querySelectorAll("mark").length).toBeGreaterThan(0));
  });

  /// `run.failed` is one event type covering several endings, and the client
  /// drew only its name. A Run that stopped because it reached the cap shown in
  /// its own status line said nothing about which cap, and read as an agent
  /// that broke.
  it("says which limit a Run ran into, not only that it failed", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "run.failed", {
      status: "failed", kind: "budget_exhausted", dimension: "tokens", retryable: false,
    }, 30));
    await waitFor(() => expect(screen.getByText(/token 预算用完了/)).toBeTruthy());
  });

  it("names the servers when a required MCP server is what stopped it", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "run.failed", {
      status: "failed", kind: "required_mcp_unavailable", servers: ["docs"], retryable: false,
    }, 30));
    await waitFor(() => expect(screen.getByText(/docs/)).toBeTruthy());
  });

  /// A kind this build has never heard of is printed rather than swallowed.
  /// The runtime may add one, and "failed for a reason this client cannot name"
  /// is a true sentence where silence is not.
  it("prints a failure kind it does not recognise", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "run.failed", {
      status: "failed", kind: "something_new", retryable: false,
    }, 30));
    await waitFor(() => expect(screen.getByText(/something_new/)).toBeTruthy());
  });

  /// A person who pressed 不执行 stopped the call. The runtime records that as
  /// a `tool.result` carrying `is_error` and `approval_denied`, the same shape
  /// a tool that broke produces -- so a client that read only `is_error` tells
  /// someone their own decision was a malfunction.
  it("says a call was declined, not that the tool broke", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "shell.exec", arguments: { command: "rm -rf build" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: true,
      content: {
        error: { code: "approval_denied", message: "tool execution was denied by a reviewer" },
      },
    }, 30));
    await waitFor(() => expect(screen.getByText(/你没让它执行/)).toBeTruthy());
    // And not the wording for a tool that failed on its own.
    expect(screen.queryByText(/工具报错/)).toBeNull();
  });

  it("still calls a genuine tool failure a failure", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "shell.exec", arguments: { command: "ls" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: true,
      content: { error: { code: "exit_status", message: "exit 1" } },
    }, 30));
    await waitFor(() => expect(screen.getByText(/工具报错/)).toBeTruthy());
  });

  /// You approve `ls -la`, it runs, and the listing is the thing you approved
  /// it for. Folding a successful result away made the fold work and left the
  /// output reachable only from the raw-event drawer -- so the transcript
  /// showed that a command had been run and never what it said.
  it("shows what a command printed, under the call that printed it", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    const call = (sequence: number, id: string, command: string) =>
      bridge.emit(RUN_LIVE, bridge.event(sequence, "model.tool_call", {
        name: "shell.exec", arguments: { command }, id,
      }, 30));
    const answer = (sequence: number, id: string, stdout: string) =>
      bridge.emit(RUN_LIVE, bridge.event(sequence, "tool.result", {
        tool_call_id: id, binding_digest: "b".repeat(64), is_error: false,
        content: { exit_code: 0, stdout, stdout_truncated: false, stderr: "", stderr_truncated: false },
      }, 30));
    call(20, "c0", "ls -la");
    answer(21, "c0", "total 8\ndrwxr-xr-x  notes.txt\n");
    call(22, "c1", "wc -l notes.txt");
    answer(23, "c1", "3 notes.txt\n");

    // Still one fold: the results belong to the calls, not between them.
    const row = await screen.findByText("2 个工具调用");
    expect(screen.queryByText(/drwxr-xr-x/)).toBeNull();
    await user.click(row);
    // Opened, each call carries what it printed.
    await waitFor(() => expect(screen.getByText(/drwxr-xr-x/)).toBeTruthy());
    expect(screen.getByText(/3 notes\.txt/)).toBeTruthy();
  });

  it("says a command failed by its exit code, and shows what it wrote", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "shell.exec", arguments: { command: "cat missing" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: false,
      content: {
        exit_code: 1, stdout: "", stdout_truncated: false,
        stderr: "cat: missing: No such file\n", stderr_truncated: false,
      },
    }, 30));
    // A single call is not folded, so this is on screen without opening
    // anything -- and a non-zero exit is not an error event, it is a command
    // that ran and said no.
    await waitFor(() => expect(screen.getByText(/No such file/)).toBeTruthy());
    expect(screen.getByText(/退出码 1/)).toBeTruthy();
  });

  /// Reading a file is the other commonest thing an agent does, and the trusted
  /// workspace tool answers it with `{path, text, bytes}` rather than the shell
  /// shape. Showing one and not the other would mean the transcript carries
  /// what a command printed and not what a file said.
  it("shows what a file said, and how much of it there was", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "workspace.read_text", arguments: { path: "notes.txt" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: false,
      content: { path: "notes.txt", text: "扫描每个 run 目录\n", bytes: 26 },
    }, 30));
    await waitFor(() => expect(screen.getByText(/扫描每个 run 目录/)).toBeTruthy());
    // The byte count, because a file the agent read is a file it spent context
    // on, and 26 bytes and 26 kilobytes read very differently.
    expect(screen.getByText(/26 B/)).toBeTruthy();
  });

  /// An MCP tool answers in the shape the MCP protocol specifies: a `content`
  /// array of parts. That is a third shape, and unlike a tool nobody has heard
  /// of it is written down -- so reading it is not guessing. MCP servers are
  /// configurable from this app, so a person can install one and then watch it
  /// answer with nothing on screen.
  it("shows what an MCP tool answered, from the array the protocol specifies", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "mcp:docs/search", arguments: { query: "retention" }, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: false,
      content: [
        { type: "text", text: "第一条结果" },
        { type: "text", text: "第二条结果" },
      ],
    }, 30));
    await waitFor(() => expect(screen.getByText(/第一条结果/)).toBeTruthy());
    expect(screen.getByText(/第二条结果/)).toBeTruthy();
  });

  /// A part this build cannot render is named rather than dropped. An MCP
  /// server may answer with an image or a resource link, and a transcript that
  /// showed only the text parts would be quietly hiding half an answer.
  it("names an MCP part it cannot draw instead of dropping it", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.tool_call", {
      name: "mcp:docs/shot", arguments: {}, id: "c0",
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "tool.result", {
      tool_call_id: "c0", binding_digest: "b".repeat(64), is_error: false,
      content: [{ type: "image", data: "…", mimeType: "image/png" }],
    }, 30));
    await waitFor(() => expect(screen.getByText(/image/)).toBeTruthy());
  });

  /// Eight failure kinds, and what a person should do about them is different
  /// for each: authentication means go and fix the key, rate limiting means
  /// wait, context overflow means say it shorter. "Provider 失败" means none of
  /// those, and it was all the transcript said.
  it("says why a provider failed and which one", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.failed", {
      provider_id: "local-stub", kind: "authentication", retryable: false, status: "running",
    }, 30));
    await waitFor(() => expect(screen.getByText(/密钥不对/)).toBeTruthy());
    expect(screen.getByText(/local-stub/)).toBeTruthy();
  });

  it("says how long a retry waits and which attempt it is", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.retry_scheduled", {
      provider_id: "local-stub", provider_attempt: 2, delay_ms: 1500,
      kind: "rate_limited", status: "running",
    }, 30));
    // A wait a person can see is a wait they can decide to sit through.
    await waitFor(() => expect(screen.getByText(/1\.5 秒后/)).toBeTruthy());
    expect(screen.getByText(/第 2 次/)).toBeTruthy();
  });

  /// A kind this build has never seen is printed as it arrived, for the same
  /// reason an unknown `run.failed` kind is.
  it("prints a provider failure kind it does not recognise", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.failed", {
      provider_id: "p1", kind: "moon_phase", retryable: true, status: "running",
    }, 30));
    await waitFor(() => expect(screen.getByText(/moon_phase/)).toBeTruthy());
  });

  /// Which Provider answered is bookkeeping right up until it is not the one
  /// that was asked. `failed_provider_ids` says a failover happened, and a Run
  /// answered by a fallback is a Run answered by a different model, at a
  /// different price, behaving differently -- and it looked identical.
  it("says when a Run was answered by a Provider that was not the first choice", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.selected", {
      provider_id: "backup", failed_provider_ids: ["local-stub"],
    }, 30));
    await waitFor(() => expect(screen.getByText(/backup/)).toBeTruthy());
    expect(screen.getByText(/local-stub/)).toBeTruthy();
  });

  /// And stays quiet when the first choice answered, which is every ordinary
  /// turn. A line per turn saying which Provider answered would be a machine
  /// log printed through a conversation.
  it("says nothing when the Provider asked is the Provider that answered", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.selected", {
      provider_id: "local-stub", failed_provider_ids: [],
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "model.output.delta", { text: "答完了" }, 30));
    await waitFor(() => expect(screen.getByText(/答完了/)).toBeTruthy());
    expect(screen.queryByText(/选定 Provider/)).toBeNull();
  });

  /// `Stop` and `ToolCalls` are bookkeeping -- every turn ends one of those two
  /// ways. `Length` and `ContentFilter` are not: the reply on screen is
  /// unfinished, and nothing said so. Someone reads a cut-off answer and takes
  /// it for the whole one.
  it("says a reply was cut off by length rather than finished", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.output.delta", { text: "开头是这样的" }, 30));
    // The shape the kernel actually emits. This used to send
    // `model.turn.completed { reason: "length" }`, which the runtime has never
    // produced -- a length-capped reply *ends* the Run, so it arrives on a
    // terminal event. The guard was green against a fiction.
    bridge.emit(RUN_LIVE, bridge.event(21, "run.succeeded", {
      status: "succeeded", reason: "length",
    }, 30));
    await waitFor(() => expect(screen.getByText(/没说完/)).toBeTruthy());
  });

  it("says a reply was stopped by a content filter", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    // Likewise: a content filter arrives as
    // `run.failed { kind: "protocol", reason: "content_filter" }`
    // (`crates/kernel/src/lib.rs`), never as a completed turn. Against the real
    // shape this said "回复的格式不对" until `failureReason` learned to prefer
    // the reason over the kind.
    bridge.emit(RUN_LIVE, bridge.event(20, "run.failed", {
      status: "failed", kind: "protocol", reason: "content_filter",
    }, 30));
    await waitFor(() => expect(screen.getByText(/内容过滤/)).toBeTruthy());
  });

  /// The two ordinary endings stay silent. A line per turn saying a turn ended
  /// is a line under every paragraph of every conversation.
  it("says nothing when a turn ended the ordinary way", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.output.delta", { text: "答完了" }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "model.turn.completed", { reason: "stop" }, 30));
    await waitFor(() => expect(screen.getByText(/答完了/)).toBeTruthy());
    expect(screen.queryByText(/这一轮结束/)).toBeNull();
    expect(screen.queryByText(/没说完/)).toBeNull();
  });

  it("still draws a refusal, which arrives as a plain string", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(20, "model.refusal", { text: "这个我不能做" }, 30));
    await waitFor(() => expect(screen.getByText(/这个我不能做/)).toBeTruthy());
  });
});
