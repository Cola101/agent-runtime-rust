/// What this file is for.
///
/// The workspace surface makes two claims that are easy to blur into one: what
/// is on disk, and what the agent was asked to do. The second comes from the
/// durable log and includes calls that never ran because a person had not
/// decided yet. A screen that presented those as file changes would be
/// reporting work that has not happened.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime } from "./fake-runtime";

async function openWorkspace() {
  const user = userEvent.setup();
  const bridge = installFakeRuntime();
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  await user.click(
    screen.getAllByRole("button", { name: /^工作区/ }).find((node) => node.classList.contains("r"))!,
  );
  return { user, bridge };
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the workspace surface", () => {
  it("lists the folder and opens a file", async () => {
    const { user } = await openWorkspace();
    // Scoped to the listing: the same name also appears in the touched-paths
    // table below, and matching either would make this pass without the
    // listing rendering at all.
    const inListing = () => screen.getByText("notes.txt", { selector: "td.p:not(.mono)" });
    await waitFor(() => expect(inListing()).toBeTruthy());
    expect(screen.getByText(/目录 src/)).toBeTruthy();
    await user.click(inListing());
    await waitFor(() => expect(screen.getByText("扫描每个 run 目录")).toBeTruthy());
  });

  it("shows what the host refused instead of an empty folder", async () => {
    const { user } = await openWorkspace();
    await waitFor(() =>
      expect(screen.getByText("notes.txt", { selector: "td.p:not(.mono)" })).toBeTruthy());
    // The fake refuses everything but notes.txt, the way the host refuses a
    // path that resolves outside the workspace.
    await user.click(screen.getByText(/目录 src/));
    await user.click(await screen.findByText("main.rs"));
    await waitFor(() => expect(screen.getByText(/outside the workspace/)).toBeTruthy());
  });

  it("lists a path the agent was asked to touch, with the tool that asked", async () => {
    await openWorkspace();
    await waitFor(() => expect(screen.getByText("代理动过的路径")).toBeTruthy());
    expect(screen.getByText("workspace.write")).toBeTruthy();
    // Said as a request rather than as a change: a call parked on an approval
    // was asked and never ran, and both look identical in this list.
    expect(screen.getByText(/被要求过，但没有执行/)).toBeTruthy();
  });

  it("invents nothing from a tool call that names no path", async () => {
    await openWorkspace();
    await waitFor(() => expect(screen.getByText("代理动过的路径")).toBeTruthy());
    // The parked run asked for `shell.exec` with a command and no path. One row
    // is listed, from the call that named one -- not two.
    expect(screen.getByText("notes.txt", { selector: "td.p.mono" })).toBeTruthy();
    expect(screen.queryByText("shell.exec")).toBeNull();
  });
});

/// The first run of an installed app has no provider, so nothing is behind the
/// window until one is configured. Making the person quit and reopen to get
/// what they just configured is the difference between an app that works when
/// installed and one that needs instructions.
describe("configuring a provider on a fresh install", () => {
  it("brings a runtime up instead of asking for a restart", async () => {
    const user = userEvent.setup();
    const bridge = installFakeRuntime();
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /设置/ })).toBeTruthy());
    await user.click(
      screen.getAllByRole("button", { name: /^设置/ }).find((node) => node.classList.contains("r"))!,
    );
    await user.click(screen.getByRole("button", { name: /模型/ }));

    await user.type(screen.getByPlaceholderText("local-stub"), "local-stub");
    await user.type(
      screen.getByPlaceholderText(/^http:\/\/127/), "http://127.0.0.1:9/v1/chat/completions",
    );
    await user.type(screen.getByPlaceholderText("stub"), "stub");
    await user.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(bridge.saveProvider).toHaveBeenCalled());
    await waitFor(() => expect(bridge.launch).toHaveBeenCalled());
  });
});
