/// What this file is for.
///
/// The MCP section is one screen holding three facts that look like one: what
/// this app has configured, what the runtime behind the window was actually
/// started with, and whether any of it came up. Only the first two are knowable
/// here. The third is `McpServerDiscoveryStatus` inside the runtime process,
/// which the local socket does not expose at all -- so the failure this file
/// guards against is a row that reads as "running" because it is configured.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime } from "./fake-runtime";

async function openMcp(options: Parameters<typeof installFakeRuntime>[0] = {}) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(options);
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /设置/ })).toBeTruthy());
  await user.click(
    screen.getAllByRole("button", { name: /^设置/ }).find((node) => node.classList.contains("r"))!,
  );
  await user.click(screen.getByRole("button", { name: /MCP 服务/ }));
  return { user, bridge };
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the configured servers", () => {
  it("lists what is configured, with the authority it is granted", async () => {
    await openMcp();
    // Scoped to its own cell: the server name also appears in the scope string
    // and in the note below, and matching either would let this pass with no
    // row rendered at all.
    await waitFor(() =>
      expect(screen.getByText("filesystem", { selector: "td.p.mono" })).toBeTruthy());
    // The scope is not decoration. It is what `valid_mcp_servers` requires the
    // Run to carry, and it is the whole authority the server gets.
    expect(screen.getByText("tool:mcp:filesystem")).toBeTruthy();
    expect(screen.getByText("read_file")).toBeTruthy();
    // The runtime was started with exactly this configuration, so the row is
    // allowed to say so. This half and the next test are a pair: one server,
    // one name, two answers, and only the digest can produce both.
    expect(screen.getByText("Runtime 启动时拿到了")).toBeTruthy();
  });

  it("says a server is only saved until the runtime has actually been given it", async () => {
    // Same server, different arguments from the ones the runtime was started
    // with -- which is what editing one looks like. The name is unchanged, so a
    // client comparing names would call this live while the runtime is still
    // running the old command.
    await openMcp({ mcpApplied: [{ name: "filesystem", digest: "0000000000000000" }] });
    await waitFor(() => expect(screen.getByText("还没生效")).toBeTruthy());
  });

  it("refuses to say anything about a runtime this app did not start", async () => {
    await openMcp({ mcpApplied: null });
    await waitFor(() => expect(screen.getByText("不知道有没有生效")).toBeTruthy());
    expect(screen.getByText(/它读的是哪一份 MCP 配置，这个应用无从知道/)).toBeTruthy();
  });

  it("still names a deleted server the runtime is running", async () => {
    await openMcp({
      mcpApplied: [
        { name: "filesystem", digest: "9f2c41ab7d0e5613" },
        { name: "notes", digest: "1111111111111111" },
      ],
    });
    // Configured no longer, running still. Dropping it from the screen would
    // say the removal had taken effect.
    await waitFor(() => expect(screen.getByText(/Runtime 启动时还带着已经删掉的 notes/)).toBeTruthy());
  });
});

describe("whether a server came up", () => {
  /// This page used to say the runtime keeps the answer to itself, which was
  /// true and is not any more: every Run writes what discovery found into its
  /// own log. What the page must not do is imply it has a live answer -- one
  /// Run is one discovery, and a page-level "up / down" would be a claim about
  /// a moment nobody asked about.
  it("points at where the answer is, per Run, rather than implying a live one", async () => {
    await openMcp();
    await waitFor(() =>
      expect(screen.getByText(/起没起来是每个 Run 各问一次的事/)).toBeTruthy());
    expect(screen.getByText("mcp.discovery.completed")).toBeTruthy();
    // No wording left claiming nothing can read it.
    expect(screen.queryByText(/本地 socket 没有任何调用能读到它/)).toBeNull();
  });

  /// The page told people to quit and reopen the app, which stopped being true
  /// when 重启 Runtime landed -- and quitting is a much bigger thing to ask.
  it("offers a Runtime restart where the change needs one", async () => {
    await openMcp();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /重启 Runtime/ })).toBeTruthy());
    expect(screen.queryByText(/退出应用再打开/)).toBeNull();
  });

  it("names a required server a Run was actually refused for", async () => {
    // The one report that survives outside the runtime process: `run.failed`
    // with `kind: "required_mcp_unavailable"`, in that Run's durable log.
    await openMcp({ failed: "required_mcp_unavailable" });
    await waitFor(() =>
      expect(screen.getByText("filesystem", { selector: "td.p.mono.warn" })).toBeTruthy());
    expect(screen.getByText("这些是 Run 日志里真的报过起不来的服务")).toBeTruthy();
  });

  it("does not read a Run that failed for another reason as a server failure", async () => {
    // A budget that ran out is the same event type with a different `kind`, and
    // it carries no servers. A client keying off `run.failed` alone would put
    // this Run's failure under the MCP servers, where the person would read it
    // as one of them being down.
    await openMcp({ failed: "budget_exhausted" });
    await waitFor(() =>
      expect(screen.getByText("filesystem", { selector: "td.p.mono" })).toBeTruthy());
    // The configured row is a `td.p.mono`; a reported failure is a
    // `td.p.mono.warn`. Waiting for the first is what stops this absence from
    // passing on a page that rendered nothing.
    expect(document.querySelector("td.p.mono.warn")).toBeNull();
    expect(screen.queryByText("这些是 Run 日志里真的报过起不来的服务")).toBeNull();
  });
});

describe("adding one", () => {
  it("sends the runtime's own shape to the host, not the text that was typed", async () => {
    const { user, bridge } = await openMcp();
    await user.type(screen.getByPlaceholderText("filesystem"), "notes");
    await user.type(screen.getByPlaceholderText("/opt/homebrew/bin/npx"), "/bin/sh");
    // One argument per line, because an MCP command's arguments are paths and
    // one of them can have a space in it.
    await user.type(screen.getByRole("textbox", { name: /参数/ }), "-c{Enter}/tmp/my server.sh");
    await user.type(screen.getByPlaceholderText("read_file write_file"), "search, fetch");
    await user.click(screen.getByRole("checkbox"));
    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(bridge.saveMcpServer).toHaveBeenCalledWith({
      name: "notes",
      command: "/bin/sh",
      args: ["-c", "/tmp/my server.sh"],
      cwd: null,
      toolNames: ["search", "fetch"],
      required: true,
    }));
  });

  it("shows what the host refused rather than clearing the form", async () => {
    const { user, bridge } = await openMcp();
    bridge.saveMcpServer.mockResolvedValueOnce({ ok: false, error: "命令必须是绝对路径" });
    await user.type(screen.getByPlaceholderText("filesystem"), "notes");
    await user.type(screen.getByPlaceholderText("/opt/homebrew/bin/npx"), "npx");
    await user.type(screen.getByPlaceholderText("read_file write_file"), "search");
    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(screen.getByText("命令必须是绝对路径")).toBeTruthy());
    // The rejected values stay put: retyping a command because the app cleared
    // it is how a person gives up on a form.
    expect(screen.getByPlaceholderText("filesystem")).toHaveProperty("value", "notes");
  });
});
