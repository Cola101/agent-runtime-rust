/// What this file is for.
///
/// Claude Code's status line reads `17m 15s · 34.4k tokens · almost done
/// thinking…`. Two of those three are facts and the third is not: no event a
/// runtime writes supports "almost done", and a screen that says it is guessing
/// at a model's interior and presenting the guess as progress.
///
/// So this holds the two that are facts -- a clock counted from the Run's first
/// event, and an activity taken from the last event it wrote -- and holds the
/// line against the third by keeping every phrase traceable to an event type.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";
import { doing, elapsed } from "../surfaces/model";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the clock", () => {
  it("counts from the first event rather than describing the last", () => {
    const start = "2026-08-18T10:00:00.000Z";
    // A finished Run is measured end to end. "35 分钟前" answers a different
    // question from "it took 35 minutes", and this is the second one.
    expect(elapsed(start, "2026-08-18T10:17:15.000Z")).toBe("17m 15s");
    expect(elapsed(start, "2026-08-18T10:00:09.000Z")).toBe("9s");
    expect(elapsed(start, "2026-08-18T12:34:00.000Z")).toBe("2h 34m");
  });

  it("says nothing when there is nothing to count from", () => {
    expect(elapsed(null, null)).toBe("");
    expect(elapsed("not a date", null)).toBe("");
  });

  /// The clock advances without any event arriving, which is what makes it a
  /// clock rather than a timestamp. Nothing in the app has a timer for it: the
  /// store's own re-read every 1.2s is what re-renders it. A dedicated
  /// `setInterval` was written first and removed -- stopping it changed nothing
  /// a test could see.
  it("advances while a Run is moving, with no event arriving", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      installFakeRuntime({ activeRunId: RUN_LIVE });
      render(<App />);
      await waitFor(() => expect(screen.getByText("运行中")).toBeTruthy());
      const clock = () => screen.getByText(/^\d+[smh]/).textContent;
      const first = clock();
      // Five minutes, not an hour. The cost of this line is one `load()` per
      // POLL_MS of simulated time -- an hour was 3000 polls, and on a loaded
      // machine that ran past the 5s budget more often than not. Five minutes
      // is 250, and still crosses a unit the clock prints, which is all the
      // assertion below is asking about.
      await vi.advanceTimersByTimeAsync(5 * 60 * 1000);
      expect(clock()).not.toBe(first);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("what it says the Run is doing", () => {
  it("maps each phrase to an event the runtime actually writes", () => {
    expect(doing("run.started")).toBe("在想");
    expect(doing("model.output.delta")).toBe("在回答");
    expect(doing("model.tool_call")).toBe("在用工具");
    expect(doing("approval.required")).toBe("等你决定");
    expect(doing("run.steer.applied")).toBe("刚改了向");
    expect(doing("subagent.spawned")).toBe("在派子代理");
  });

  it("estimates nothing", () => {
    // There is no event for "almost done", so there is no phrase for it. The
    // absence is the point: an unrecognised event yields null, and the screen
    // shows the type instead of inventing a description.
    expect(doing("model.private_state.omitted")).toBeNull();
    expect(doing("something.this.build.has.never.seen")).toBeNull();
    expect(doing(null)).toBeNull();
  });

  it("shows the raw type when it does not recognise the last event", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(40, "model.private_state.omitted", {}, 30));
    // Named rather than smoothed over: an event this build does not know is
    // worth seeing, and the type is what makes it lookupable.
    await waitFor(() =>
      expect(screen.getByText("model.private_state.omitted")).toBeTruthy());
  });

  it("says nothing about activity once the Run is over", async () => {
    const user = userEvent.setup();
    installFakeRuntime();
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
    // The open conversation has no Turn in flight, so the surface is about no
    // Run until one is chosen. Choosing the finished one from the Run list is
    // what makes this about a Run that is over.
    await user.click(
      screen.getAllByRole("button", { name: /^Run/ }).find((n) => n.classList.contains("r"))!,
    );
    await user.click(await screen.findByText("something finished"));
    await user.click(
      screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
    );
    await waitFor(() => expect(screen.getByText(/用了/)).toBeTruthy());
    // A finished Run is not doing anything, and a status line still saying
    // "在回答" about one would be describing a process that has stopped.
    expect(screen.queryByText("在回答")).toBeNull();
  });
});
