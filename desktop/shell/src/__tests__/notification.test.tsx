/// What this file is for.
///
/// The window is the only side that can read the log, and the host is the only
/// side that can raise a notification or know whether anyone is looking. These
/// tests hold the seam between them: what the window reports, and that the one
/// thing a banner promises — press it and the decision is in front of you —
/// is actually wired to something.
///
/// Two kinds of waiting cross this seam and only one of them is an approval. A
/// Run that ended `indeterminate` blocks a person just as hard and has no
/// approval to name it by, so it is reported here by name in its own right —
/// a suite that only ever saw approvals would stay green with that whole
/// branch replaced.
///
/// The host's own rules live in `attention.test.js` and `banner.test.js`.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { App } from "../App";
import {
  APPROVAL_ID, installFakeRuntime, MCP_INPUT_ID, MCP_INPUT_VERSION,
  RUN_DONE, RUN_INPUT, RUN_LIVE, RUN_UNJUDGED, RUN_WAITING,
} from "./fake-runtime";

/// The three things the fixture's runtime is blocked on, in the order the queue
/// puts them: newest-touched first, which is the order the surface shows and
/// therefore the order the host is told.
const UNJUDGED = { kind: "indeterminate", key: `run:${RUN_UNJUDGED}`, runId: RUN_UNJUDGED };
const ASKED = {
  kind: "approval", key: `approval:${APPROVAL_ID}`, runId: RUN_WAITING, toolName: "shell.exec",
};
/// The third way a Run stops on a person, and the one a merge nearly lost: a
/// suspended Run carrying an MCP server's question. Keyed by the round rather
/// than the Run, because a server asking again about the same round is asking
/// something new.
const ASKED_BY_MCP = {
  kind: "mcp-input",
  key: `mcp:${MCP_INPUT_ID}:${MCP_INPUT_VERSION}`,
  runId: RUN_INPUT,
  serverName: "docs",
};

/// The window, after its first load has landed.
async function mounted() {
  const bridge = installFakeRuntime();
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  return bridge;
}

/// Every report that had something in it. The empty ones before the first page
/// lands are the window saying it has not looked yet, not that nothing waits.
function reports(bridge: Awaited<ReturnType<typeof mounted>>) {
  return bridge.waiting.mock.calls
    .map((call) => call[0] as { kind: string; key: string; runId: string; toolName?: string }[])
    .filter((items) => items.length > 0);
}

/// The first settled report, once one has arrived.
async function firstReport(bridge: Awaited<ReturnType<typeof mounted>>) {
  await waitFor(() => expect(reports(bridge).length).toBeGreaterThan(0));
  return reports(bridge)[0];
}

function rail(label: RegExp) {
  return screen
    .getAllByRole("button", { name: label })
    .find((node) => node.classList.contains("r"));
}

/// The card the cursor is on, on the queue.
function underCursor() {
  return document.querySelector('.gate[data-on="true"]');
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("what the window tells the host is waiting", () => {
  it("reports the whole queue, not the first thing in it", async () => {
    const bridge = await mounted();
    // Both, in the queue's order. A host told about one of two waiting Runs
    // stays silent about the other one forever: it is never "new" again.
    expect(await firstReport(bridge)).toEqual([UNJUDGED, ASKED_BY_MCP, ASKED]);
  });

  it("reports the Run nobody can judge, which has no approval to name it by", async () => {
    const bridge = await mounted();
    const unjudged = (await firstReport(bridge)).filter((item) => item.runId === RUN_UNJUDGED);
    // Named by the Run itself — a Run crosses a terminal boundary once, so its
    // own id names that question exactly — and carrying no tool, because there
    // is no call here to name. Not an approval with the name left blank.
    expect(unjudged).toEqual([
      { kind: "indeterminate", key: `run:${RUN_UNJUDGED}`, runId: RUN_UNJUDGED },
    ]);
  });

  it("leaves out the Runs that are running or finished", async () => {
    const bridge = await mounted();
    const runIds = (await firstReport(bridge)).map((item) => item.runId);
    expect(runIds).not.toContain(RUN_LIVE);
    expect(runIds).not.toContain(RUN_DONE);
  });

  it("names them the same things on every poll, so the host can tell they are the same things",
    async () => {
      const bridge = await mounted();
      // The store polls; the same three are read again and reported again.
      await waitFor(() => expect(reports(bridge).length).toBeGreaterThan(1), { timeout: 4_000 });
      const named = new Set(
        reports(bridge).map((items) => items.map((item) => item.key).join(" ")),
      );
      // One entry in the set: every poll named the same things the same way.
      // The keys are written out rather than derived from the queue, because
      // deriving them from the same code that produces them would pass however
      // that code changed -- which is the whole property being tested.
      expect([...named]).toEqual([
        `run:${RUN_UNJUDGED} mcp:${MCP_INPUT_ID}:${MCP_INPUT_VERSION} approval:${APPROVAL_ID}`,
      ]);
    });
});

describe("clicking the notification", () => {
  it("lands on the queue, on the Run the banner named", async () => {
    const bridge = await mounted();
    await firstReport(bridge);
    // Where a person is before they click: the transcript, nothing chosen.
    expect(rail(/^待决定/)?.getAttribute("aria-current")).toBe("false");
    expect(underCursor()).toBeNull();

    act(() => bridge.attend(RUN_WAITING));

    expect(rail(/^待决定/)?.getAttribute("aria-current")).toBe("true");
    // The queue is showing, and the cursor is on the Run the host named —
    // not merely the surface it happens to be on.
    await waitFor(() => {
      const on = underCursor();
      expect(on, "the queue is showing, but no Run is under the cursor").toBeTruthy();
      expect(on?.textContent).toContain("shell.exec");
    });
  });

  it("lands on the unjudged Run too, and says that is what it is", async () => {
    const bridge = await mounted();
    await firstReport(bridge);

    act(() => bridge.attend(RUN_UNJUDGED));

    expect(rail(/^待决定/)?.getAttribute("aria-current")).toBe("true");
    await waitFor(() => {
      const on = underCursor();
      expect(on, "the queue is showing, but no Run is under the cursor").toBeTruthy();
      // The card for the other Run: no decision buttons, and the runtime's own
      // word for why. Landing on it and reading "等你决定" would be the app
      // promising an approval that is not there.
      expect(on?.textContent).toContain("结果无法判定");
      expect(on?.querySelector(".picks")).toBeNull();
    });
  });
});
