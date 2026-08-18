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
