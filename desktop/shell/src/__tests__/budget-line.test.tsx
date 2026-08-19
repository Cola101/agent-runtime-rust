// @vitest-environment jsdom
/// What this file is for.
///
/// The status line shows a Run's tokens against the cap this app configured,
/// so `budget_exhausted` does not arrive out of nowhere. Cost and duration
/// have caps too — the same three numbers go into the child's environment
/// (`electron/childEnv.cjs`) — and they were drawn bare: a dollar figure and a
/// clock with nothing to measure them against.
///
/// That asymmetry is the bug. A Run that is one minute from its duration cap
/// looks exactly like one that has hours left, and the ending reads as the
/// agent giving up rather than as a limit somebody chose.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

async function watching() {
  const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
  render(<App />);
  await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
  return bridge;
}

describe("what a Run has spent, against what it may", () => {
  it("shows the cost cap beside the cost", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "model.usage", {
      input_tokens: 1_000, output_tokens: 200, cost_micros: 1_230_000,
    }, 30));
    // 500 cents is the fixture's cap, which is $5.00.
    await waitFor(() => expect(screen.getByText(/\$1\.23 \/ \$5\.00/)).toBeTruthy());
  });

  it("shows the duration cap beside the clock", async () => {
    await watching();
    // The fixture's cap is 3600 seconds.
    await waitFor(() => expect(screen.getByText(/\/ 1h 00m/)).toBeTruthy());
  });

  /// A cap beside a dash claims a measurement that is not being taken.
  ///
  /// This app writes `cost_per_million_tokens_micros: 0` for every Provider it
  /// configures (`electron/credentials.cjs:167`), so on a desktop build the
  /// cost is structurally zero and `costLabel` draws "—". Putting "/ $5.00"
  /// beside that dash would say a spend is being measured against a cap when
  /// nothing is measuring it.
  it("does not put a cap beside a cost nobody is measuring", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "model.usage", {
      input_tokens: 1_000, output_tokens: 200, cost_micros: 0,
    }, 30));
    await waitFor(() => expect(screen.getByText(/1,200 \/ 400,000 token/)).toBeTruthy());
    expect(screen.queryByText(/\$5\.00/)).toBeNull();
  });

  /// A Runtime this app did not start has a budget of its own and this window
  /// does not know it. That is a different answer from "no limit", and the
  /// token display already refuses to invent one.
  it("says nothing about caps when the host will not say", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    // The reachable path: the host refuses to say. `budget` staying null comes
    // from a reply that is not ok (or a preload too old to have the call at
    // all) -- never from a null value, which the declared shape does not allow.
    bridge.desk.runtime.budget = async () => ({
      ok: false as const, error: "这个 Runtime 不是这个应用启动的",
    });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    bridge.emit(RUN_LIVE, bridge.event(40, "model.usage", {
      input_tokens: 1_000, output_tokens: 200, cost_micros: 1_230_000,
    }, 30));
    await waitFor(() => expect(screen.getByText(/\$1\.23/)).toBeTruthy());
    expect(screen.queryByText(/\$1\.23 \//)).toBeNull();
  });
});
