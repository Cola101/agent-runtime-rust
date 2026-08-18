// @vitest-environment jsdom
/// What this file is for.
///
/// Telling a coding agent which file to look at is the commonest thing anyone
/// types into it, and the only way to do it here was to write the path out and
/// hope it was right. `@` reads the workspace this app is already showing on
/// another surface, so the path that lands in the message is one the runtime
/// will accept rather than one that was remembered.
///
/// What the guards are about is the honesty of it. A completion that offered
/// paths from somewhere else, or inserted a name that is not what the runtime
/// would resolve, would be worse than typing: it would look authoritative.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

async function composer() {
  const user = userEvent.setup();
  const bridge = installFakeRuntime();
  render(<App />);
  await waitFor(() => expect(screen.getByPlaceholderText(/接着说|说一句话/)).toBeTruthy());
  const box = screen.getByPlaceholderText(/接着说|说一句话/) as HTMLTextAreaElement;
  return { user, bridge, box };
}

describe("naming a file with @", () => {
  it("offers what the workspace actually holds, not a remembered list", async () => {
    const { user, bridge, box } = await composer();
    await user.click(box);
    await user.keyboard("看一下 @");
    // The fixture's workspace, from the same call the workspace surface makes.
    await waitFor(() => expect(screen.getByText("notes.txt")).toBeTruthy());
    expect(bridge.desk.runtime.listFiles).toBeTruthy();
  });

  it("puts the path into the message, and closes", async () => {
    const { user, box } = await composer();
    await user.click(box);
    await user.keyboard("看一下 @");
    await user.click(await screen.findByText("notes.txt"));
    await waitFor(() => expect(box.value).toBe("看一下 @notes.txt "));
    // Closed: a list still standing over the box after it has answered is a
    // list you have to dismiss before you can carry on typing.
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("narrows as the name is typed", async () => {
    // The fixture's workspace root holds `src` and `notes.txt`, which is what
    // the workspace surface reads from the same call.
    const { user, box } = await composer();
    await user.click(box);
    await user.keyboard("@not");
    await waitFor(() => expect(screen.getByText("notes.txt")).toBeTruthy());
    // `src` does not contain "not", so it is gone rather than greyed.
    expect(screen.queryByText("src")).toBeNull();
  });

  /// A coding workspace keeps its files in folders. A completion that only knew
  /// the root would miss every one of them, which is most of what anyone wants
  /// to name -- and it would look like the file is not there rather than like
  /// the completion cannot see it.
  it("offers files inside folders, by the path the runtime would resolve", async () => {
    const { user, box } = await composer();
    await user.click(box);
    await user.keyboard("@main");
    // The fixture keeps `main.rs` under `src`, and what goes in the message is
    // the path from the workspace root rather than the bare name.
    await waitFor(() => expect(screen.getByText("src/main.rs")).toBeTruthy());
    await user.click(screen.getByText("src/main.rs"));
    await waitFor(() => expect(box.value).toBe("@src/main.rs "));
  });

  /// An `@` in the middle of a word is an email address, a decorator, a handle.
  /// Opening a file list over one of those would interrupt ordinary typing.
  it("does not open on an @ that is inside a word", async () => {
    const { user, box } = await composer();
    await user.click(box);
    await user.keyboard("写信给 a@b");
    expect(screen.queryByText("notes.txt")).toBeNull();
  });

  it("closes on Escape without touching what was typed", async () => {
    const { user, box } = await composer();
    await user.click(box);
    await user.keyboard("看一下 @");
    await waitFor(() => expect(screen.getByText("notes.txt")).toBeTruthy());
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByText("notes.txt")).toBeNull());
    expect(box.value).toBe("看一下 @");
  });

  /// Enter sends the message. While the list is open it must choose instead --
  /// otherwise the commonest key in the box sends a half-written mention.
  it("takes Enter for the highlighted file rather than sending", async () => {
    const { user, bridge, box } = await composer();
    await user.click(box);
    await user.keyboard("看一下 @not");
    await waitFor(() => expect(screen.getByText("notes.txt")).toBeTruthy());
    await user.keyboard("{Enter}");
    await waitFor(() => expect(box.value).toContain("notes.txt"));
    expect(bridge.sessionStart).not.toHaveBeenCalled();
  });
});
