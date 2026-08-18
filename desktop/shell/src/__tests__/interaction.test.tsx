/// What this file is for.
///
/// The screen used to show `1` `2` `3` beside three approval options, and
/// `j/k move · ↵ decide` in a toolbar, and a `⌘I` hint in the status line, and
/// none of those keys were bound to anything. They were characters. Every test
/// here presses a key that the interface advertises and asserts something
/// happened, so an advertised key and a working key cannot drift apart again.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { all } from "../surfaces/registry";
import { installFakeRuntime, RUN_LIVE, RUN_WAITING } from "./fake-runtime";

/// How many Runs the fixture holds, asked of the fixture.
///
/// Three branches have each added one, and every assertion that had written
/// the number down broke on the merge without telling anyone anything useful.
async function installedRuns() {
  const listed = await installFakeRuntime().desk.runtime.list();
  return listed.ok ? listed.value.runs : [];
}

async function open(surface: string) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime();
  render(<App />);
  // The store polls; the first page has to land before anything is on screen.
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  await waitFor(() => expect(bridge.desk.runtime.list).toBeDefined());
  await user.click(
    screen.getAllByRole("button", { name: new RegExp(`^${surface}`) })
      .find((node) => node.classList.contains("r"))!,
  );
  return { user, bridge };
}

/// Puts the cursor on the parked run and goes to its transcript.
///
/// Without this the transcript shows the most recently touched run, which is
/// the live one — correct behaviour, and not what these two tests are about.
async function openParkedTranscript() {
  const opened = await open("待决定");
  await opened.user.click(screen.getAllByText(/shell\.exec/)[0]);
  await opened.user.click(
    screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
  );
  return opened;
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the surface registry", () => {
  it("holds one surface per id", () => {
    const ids = all().map((surface) => surface.id);
    // A hot reload re-runs a module that already registered. Appending gave the
    // rail a second 对话 and rendered both.
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("every advertised key is bound", () => {
  it("declares a hint for each binding and a binding for each hint", () => {
    for (const surface of all()) {
      for (const key of surface.keys ?? []) {
        expect(key.hint, `${surface.id} ${key.key} has no hint`).toBeTruthy();
        expect(typeof key.run, `${surface.id} ${key.key} has no action`).toBe("function");
      }
    }
  });

  it("only offers ⌘I on a surface that declares a drawer", () => {
    for (const surface of all()) {
      if (surface.drawerLabel) expect(surface.drawer).toBeTruthy();
    }
    // At least one surface must actually have one, or the key is dead weight.
    expect(all().some((surface) => surface.drawer)).toBe(true);
  });
});

describe("approvals", () => {
  it("decides with the digit shown next to the option", async () => {
    const { user, bridge } = await open("待决定");
    await waitFor(() => expect(screen.getAllByText(/等你决定/).length).toBeGreaterThan(0));
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await user.keyboard("1");
    await waitFor(() =>
      expect(bridge.control).toHaveBeenCalledWith({ action: "approve", runId: RUN_WAITING }));
  });

  it("decides with the button too", async () => {
    const { user, bridge } = await open("待决定");
    await waitFor(() => expect(screen.getAllByText(/等你决定/).length).toBeGreaterThan(0));
    const deny = screen.getAllByRole("button", { name: /不执行/ })[0];
    await user.click(deny);
    await waitFor(() =>
      expect(bridge.control).toHaveBeenCalledWith({ action: "deny", runId: RUN_WAITING }));
  });

  it("does not fire a digit while a sentence is being typed", async () => {
    const { user, bridge } = await open("对话");
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.keyboard("1 2 3");
    expect(bridge.control).not.toHaveBeenCalled();
    expect((box as HTMLTextAreaElement).value).toBe("1 2 3");
  });
});

describe("ending a run", () => {
  it("arms before it acts, because it cannot be undone", async () => {
    const { user, bridge } = await open("待决定");
    await waitFor(() => expect(screen.getAllByText(/等你决定/).length).toBeGreaterThan(0));
    const end = screen.getAllByRole("button", { name: /结束这个 Run/ })[0];

    await user.click(end);
    expect(bridge.control).not.toHaveBeenCalled();
    expect(screen.getByText(/再按一次确认/)).toBeTruthy();

    await user.click(end);
    await waitFor(() =>
      expect(bridge.control).toHaveBeenCalledWith({ action: "cancel", runId: RUN_WAITING }));
  });

  it("is not on a bare key at all", () => {
    for (const surface of all()) {
      for (const key of surface.keys ?? []) {
        expect(
          key.hint,
          `${surface.id} put an irreversible action on the bare key ${key.key}`,
        ).not.toMatch(/结束这个 Run/);
      }
    }
  });

  it("disarms when another decision is chosen", async () => {
    const { user, bridge } = await open("待决定");
    await waitFor(() => expect(screen.getAllByText(/等你决定/).length).toBeGreaterThan(0));
    await user.click(screen.getAllByRole("button", { name: /结束这个 Run/ })[0]);
    expect(screen.getByText(/再按一次确认/)).toBeTruthy();

    // Choosing something else must not leave a destructive key armed behind it.
    await user.click(screen.getAllByRole("button", { name: /不执行/ })[0]);
    await waitFor(() =>
      expect(bridge.control).toHaveBeenCalledWith({ action: "deny", runId: RUN_WAITING }));
    expect(screen.queryByText(/再按一次确认/)).toBeNull();
  });
});

describe("the run list is operable from the keyboard", () => {
  it("moves the cursor with j and opens with Enter", async () => {
    const { user } = await open("Run");
    // As many rows as the fixture has Runs. Written from the fixture rather
    // than as a literal: three separate branches have added a Run to it, and a
    // number here just breaks each time without saying anything.
    const runs = (await installedRuns()).length;
    await waitFor(() => {
      const found = document.querySelectorAll("tbody tr");
      expect(found.length).toBe(runs);
      return found;
    });
    await user.keyboard("j");
    await waitFor(() =>
      expect(document.querySelector('tr[aria-selected="true"]')).toBeTruthy());
    await user.keyboard("{Enter}");
    // Enter on the list goes to the transcript, which is the only surface
    // with a composer.
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
  });

  it("gives every row a tab stop", async () => {
    await open("Run");
    const rows = await waitFor(() => {
      const found = document.querySelectorAll("tbody tr");
      expect(found.length).toBeGreaterThan(0);
      return found;
    });
    for (const row of rows) {
      expect(row.getAttribute("tabindex"), "a clickable row with no tab stop").toBe("0");
    }
  });
});

describe("the command palette", () => {
  it("opens focused, filters as you type, and runs on Enter", async () => {
    const { user, bridge } = await open("对话");
    await user.keyboard("{Meta>}k{/Meta}");
    const input = await screen.findByPlaceholderText("输入命令");
    expect(document.activeElement).toBe(input);

    const before = screen.getAllByRole("option").length;
    await user.type(input, "停止");
    // Filtering must actually reduce the list, or it is a label over a menu.
    const options = screen.getAllByRole("option");
    expect(options.length).toBeGreaterThan(0);
    expect(options.length).toBeLessThan(before);

    await user.keyboard("{Enter}");
    await waitFor(() => expect(screen.queryByPlaceholderText("输入命令")).toBeNull());
    await waitFor(() =>
      expect(bridge.control).toHaveBeenCalledWith({ action: "cancel", runId: RUN_LIVE }));
  });

  it("hides a command that cannot run right now", async () => {
    const { user } = await open("待决定");
    // The cursor is on a parked run, so "stop the current run" is not offered.
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await user.keyboard("{Meta>}k{/Meta}");
    const input = await screen.findByPlaceholderText("输入命令");
    await user.type(input, "停止");
    expect(screen.getByText("没有匹配的命令")).toBeTruthy();
  });

  it("closes on Escape and gives the focus back", async () => {
    const { user } = await open("对话");
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.keyboard("{Meta>}k{/Meta}");
    await screen.findByPlaceholderText("输入命令");
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByPlaceholderText("输入命令")).toBeNull());
    expect(document.activeElement).toBe(box);
  });

  it("says so rather than showing an empty list", async () => {
    const { user } = await open("对话");
    await user.keyboard("{Meta>}k{/Meta}");
    const input = await screen.findByPlaceholderText("输入命令");
    await user.type(input, "zzzzzz");
    expect(screen.getByText("没有匹配的命令")).toBeTruthy();
  });
});

describe("the drawer", () => {
  it("⌘I opens the raw log on a surface that has one", async () => {
    const { user } = await openParkedTranscript();
    await user.keyboard("{Meta>}i{/Meta}");
    const named = await screen.findAllByText("原始事件");
    // Named in the status line (what ⌘I opens) and on the panel itself.
    expect(named.length).toBe(2);
    expect(document.querySelector("aside.drawer")).toBeTruthy();
    // The raw log is the log, so the event types have to be there verbatim.
    await waitFor(() => expect(screen.getByText("approval.required")).toBeTruthy());
  });

  it("says the surface has none rather than offering a dead key", async () => {
    const { user } = await open("设置");
    await waitFor(() => expect(screen.getByText("这个面没有详情栏")).toBeTruthy());
    await user.keyboard("{Meta>}i{/Meta}");
    expect(screen.queryByText("原始事件")).toBeNull();
  });
});

describe("the composer", () => {
  it("sends on Enter and recalls the last line with ArrowUp", async () => {
    const { user, bridge } = await open("对话");
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "看看 notes.txt");
    await user.keyboard("{Enter}");
    // A Turn in the open conversation, not a bare Run. `submit` starts a Run
    // that carries no history, so a composer wired to it makes every sentence
    // the first sentence of its own conversation.
    await waitFor(() =>
      expect(bridge.sessionContinue).toHaveBeenCalledWith(
        expect.objectContaining({ input: "看看 notes.txt", generation: 1 }),
      ));
    expect(bridge.submit).not.toHaveBeenCalled();
    await waitFor(() => expect((box as HTMLTextAreaElement).value).toBe(""));
    await user.keyboard("{ArrowUp}");
    expect((box as HTMLTextAreaElement).value).toBe("看看 notes.txt");
  });

  it("keeps a newline on Shift+Enter", async () => {
    const { user, bridge } = await open("对话");
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "第一行");
    await user.keyboard("{Shift>}{Enter}{/Shift}");
    await user.type(box, "第二行");
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
    expect(bridge.sessionStart).not.toHaveBeenCalled();
    expect((box as HTMLTextAreaElement).value).toBe("第一行\n第二行");
  });
});

describe("the shell reports what it drew", () => {
  it("states the real counts, not that it mounted", async () => {
    const { bridge } = await open("对话");
    await waitFor(() => expect(bridge.desk.drew).toHaveBeenCalled());
    const summary = bridge.desk.drew.mock.calls[0][0];
    // These numbers are how a headless check tells a client from a shell. An
    // App rewrite dropped this once already.
    //
    // Three counts, three different questions. `waiting` is a decision about a
    // tool call; `input` is an MCP server asking a person for content; and the
    // Run nobody can judge is blocked on a person too but was never asked
    // anything by the runtime, so neither count claims it.
    expect(summary).toMatchObject({
      link: "live", runs: (await installedRuns()).length, waiting: 1, input: 1,
    });
    expect(summary.events).toBeGreaterThan(0);
  });
});

describe("the page ceiling", () => {
  it("never asks for more events than the cursor allows", async () => {
    const { bridge } = await openParkedTranscript();
    await waitFor(() => expect(bridge.desk.runtime.events).toBeDefined());
    // A limit above RUNTIME_EVENT_CURSOR_MAX_EVENTS is rejected outright, so
    // the transcript would be empty rather than truncated. It happened.
    await waitFor(() => expect(screen.getByText(/I need to run a command/)).toBeTruthy());
  });
});
