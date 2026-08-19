/// What this file is for.
///
/// A failover is four questions, and a window that answers three of them has
/// told a story with the acting part missing: which Provider took over, why the
/// first one was dropped, which attempt this is, and -- while the Run is parked
/// on a backoff -- how much of the wait is left.
///
/// The four are not the client's to invent. Every one of them is a field the
/// kernel already writes (`runtime/crates/kernel/src/lib.rs`, the three
/// `record_model_provider_*` methods), and the ones that are not there are
/// named as absent rather than guessed at: the
/// Provider's own sentence reaches `run.failed` and nothing else, so a Run that
/// failed over successfully has no words from the Provider anywhere, and this
/// file does not pretend otherwise.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";
import { retryWait } from "../surfaces/model";

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

describe("why a Provider was dropped", () => {
  /// One HTTP status, two kinds, and only the kind was on screen.
  ///
  /// A 429 that means "slow down" and a 429 that means "the account is out of
  /// quota" are now told apart by the runtime (`RateLimited` vs `Billing`), and
  /// they call for opposite things from a person: wait, versus go and pay. The
  /// status the Provider answered with is what ties the category back to
  /// something checkable -- a Provider's own dashboard, its rate-limit docs --
  /// and it is on the payload (`kernel/src/lib.rs`,
  /// `record_model_provider_failure`) and was dropped.
  it("names the HTTP status the Provider answered with", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.failed", {
      provider_id: "openai", kind: "billing", retryable: false, status: 429,
    }, 30));
    await waitFor(() => expect(screen.getByText(/账上不让再调了/)).toBeTruthy());
    expect(screen.getByText(/HTTP 429/)).toBeTruthy();
  });

  /// The kernel writes `Option<u16>` there, so a failure the transport never
  /// got a status for says nothing rather than inventing one.
  it("says no status when the failure never had one", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.failed", {
      provider_id: "openai", kind: "unavailable", retryable: true, status: null,
    }, 30));
    await waitFor(() => expect(screen.getByText(/连不上/)).toBeTruthy());
    expect(screen.queryByText(/HTTP/)).toBeNull();
  });
});

describe("which attempt a retry is", () => {
  /// `provider_attempt` counts attempts on *this* Provider
  /// (`runtime-host/src/lib.rs`: it is `same_provider_attempts + 1` where the
  /// retry is pushed onto the route journal). Printed as a bare "第 2 次" next
  /// to a failover story it reads as the second Provider, which is a different
  /// event and a different thing to do about it.
  it("says the count is of tries at this Provider, not of Providers", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(20, "model.provider.retry_scheduled", {
      provider_id: "local-stub", provider_attempt: 2, delay_ms: 1500,
      kind: "rate_limited", status: 429,
    }, 30));
    await waitFor(() => expect(screen.getByText(/1\.5 秒后再试/)).toBeTruthy());
    expect(screen.getByText(/第 2 次试这个 Provider/)).toBeTruthy();
  });
});

describe("how much of a retry wait is left", () => {
  const scheduled = (payload: Record<string, unknown>, timestamp = "2026-08-18T00:00:10.000Z") => ({
    type: "model.provider.retry_scheduled", timestamp, payload,
  });

  /// The transcript line says what was scheduled; it is a log entry and it is
  /// right to stay frozen. The status line is the live one, and while the Run
  /// sits on a 30-second backoff it is the only thing on screen that can say
  /// the Run is waiting rather than stuck.
  it("counts down from the event's own timestamp", () => {
    const now = Date.parse("2026-08-18T00:00:20.000Z");
    expect(retryWait(scheduled({ delay_ms: 30_000 }), now)).toBe("在等重试・还要 20 秒");
  });

  /// Past the deadline the wait is over and the retry is in flight. Counting
  /// into negative numbers, or holding the last figure, would both be the
  /// screen saying something no event supports.
  it("stops counting once the wait is spent", () => {
    const now = Date.parse("2026-08-18T00:01:00.000Z");
    expect(retryWait(scheduled({ delay_ms: 1_500 }), now)).toBe("在等重试");
  });

  /// A retry with no delay on it is still a retry being waited for. The phrase
  /// survives; only the figure goes.
  it("still says it is waiting when the event carries no delay", () => {
    const now = Date.parse("2026-08-18T00:00:20.000Z");
    expect(retryWait(scheduled({}), now)).toBe("在等重试");
    expect(retryWait(scheduled({ delay_ms: 30_000 }, "not a date"), now)).toBe("在等重试");
  });

  it("is about this one event type and nothing else", () => {
    expect(retryWait(null, 0)).toBeNull();
    expect(retryWait({
      type: "model.provider.failed", timestamp: "2026-08-18T00:00:10.000Z", payload: {},
    }, 0)).toBeNull();
  });

  /// The status line had no phrase for this event at all, so a Run parked on a
  /// backoff printed `model.provider.retry_scheduled` in dim mono under a
  /// tooltip apologising for having no words for it -- while the one question
  /// being asked was how long this goes on.
  it("says the Run is waiting instead of printing the raw event type", async () => {
    const bridge = await watching();
    bridge.emit(RUN_LIVE, bridge.event(40, "model.provider.retry_scheduled", {
      provider_id: "local-stub", provider_attempt: 2, delay_ms: 1500,
      kind: "rate_limited", status: 429,
    }, 30));
    await waitFor(() => expect(screen.getByText("在等重试")).toBeTruthy());
    // Asserted through the title rather than the type string, for the reason
    // `timing.test.tsx` already writes down: the transcript names the same type
    // on the line it drew for the event, so "this string is nowhere on the
    // page" is not a statement about the status line. This title is the status
    // line's alone -- it is the apology it prints when it has no phrase.
    expect(screen.queryByTitle("这个版本没有给这个事件写说法")).toBeNull();
  });
});
