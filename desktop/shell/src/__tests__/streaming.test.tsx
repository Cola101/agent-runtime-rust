/// What this file is for.
///
/// The transcript used to be a poll: every 1.2 seconds this client re-read the
/// whole log and re-rendered it. A reply appeared in 1.2-second steps no matter
/// how fast the runtime produced it, and the only way to know a run had moved
/// was to ask again.
///
/// Now the host holds a connection open per followed run and pushes each event
/// as it is committed. Two properties have to hold, and they pull in opposite
/// directions: an event that arrives between polls must reach the screen, and
/// the *boundary* -- running, waiting, terminal -- must still come from the
/// cursor, never from the last event that happened to arrive.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("a run being followed", () => {
  it("is followed while a Turn is in flight, and dropped when it is not", async () => {
    const idle = installFakeRuntime();
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
    // Nothing in flight: nothing to follow. A client that opened a connection
    // per run would hold one for every finished run in the directory.
    await waitFor(() => expect(idle.desk.runtime.list).toBeDefined());
    expect(idle.watch).not.toHaveBeenCalled();

    cleanup();
    const busy = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(busy.watch).toHaveBeenCalledWith(
      expect.objectContaining({ runId: RUN_LIVE }),
    ));
  });

  it("shows an event that arrives between polls", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    // Not in any page the poll would return: it exists only on the stream.
    bridge.emit(RUN_LIVE, bridge.event(9, "model.output.delta", { text: "边跑边出现的字" }, 30));
    await waitFor(() => expect(screen.getByText(/边跑边出现的字/)).toBeTruthy());
  });

  it("does not let a streamed terminal event decide the run is over", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    // The status line names the boundary, so that is where this is checked --
    // and by its exact label, because "not 成功" is also satisfied by the
    // client losing track of the state entirely.
    await waitFor(() => expect(screen.getByText("运行中")).toBeTruthy());
    bridge.emit(RUN_LIVE, bridge.event(10, "run.succeeded", { status: "succeeded" }, 30));
    // Give the event a chance to be wrong before asserting it was not.
    await new Promise((resolve) => setTimeout(resolve, 60));
    // The cursor still says running, so the screen still says running. A client
    // that concluded otherwise from an event would drop a live run the moment a
    // retired log replayed one.
    expect(screen.getByText("运行中")).toBeTruthy();
    expect(screen.queryByText("成功")).toBeNull();
    expect(screen.queryByText("本版本不认识的状态")).toBeNull();
  });

  it("merges by sequence, so a poll and the stream overlapping is harmless", async () => {
    const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
    render(<App />);
    await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
    // A sequence the polled page does not have, delivered twice -- which is
    // what a reconnecting stream does, since it replays from the cursor it was
    // given. Distinctive text, and asserted by content: consecutive deltas are
    // joined into one paragraph, so a duplicate leaves the element count at one
    // and doubles the words inside it.
    const twice = bridge.event(11, "model.output.delta", { text: "重复一次就错" }, 30);
    bridge.emit(RUN_LIVE, twice);
    bridge.emit(RUN_LIVE, twice);
    await waitFor(() => expect(screen.getByText(/重复一次就错/, { selector: "p" })).toBeTruthy());
    expect(screen.getByText(/重复一次就错/, { selector: "p" }).textContent)
      .toBe("still going重复一次就错");
  });
});
