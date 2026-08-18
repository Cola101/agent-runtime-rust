// @vitest-environment jsdom
/// What this file is for.
///
/// `runtime-host` reads the routing file, the MCP config and the delegated
/// scopes once, at startup. So changing a provider did nothing until the whole
/// app was quit and reopened -- and the app was asking for that while already
/// holding the two halves of a restart: it spawned the child, and it can drain
/// it over the owner socket.
///
/// The part worth guarding is not the happy path. It is that the app refuses to
/// claim a restart it cannot perform: `stop()` only ends its own child, so
/// "restarting" a runtime this app merely attached to would drain someone
/// else's and then start a second host over one state root.
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

async function openModels(options: Parameters<typeof installFakeRuntime>[0] = {}) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(options);
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /设置/ })).toBeTruthy());
  await user.click(
    screen.getAllByRole("button", { name: /^设置/ }).find((node) => node.classList.contains("r"))!,
  );
  await user.click(screen.getByRole("button", { name: /^模型/ }));
  return { user, bridge };
}

describe("making a provider change take effect", () => {
  it("restarts the runtime rather than asking a person to quit the app", async () => {
    const { user, bridge } = await openModels();
    await user.click(await screen.findByRole("button", { name: /重启 Runtime/ }));
    await waitFor(() => expect(bridge.restart).toHaveBeenCalled());
  });

  /// The refusal is the point of the reply shape: `restarted: false` with a
  /// reason. A client that read only `ok` would report a restart that did not
  /// happen, and the next Run would still be answered by the old provider.
  it("says it cannot when the runtime is not this app's, instead of claiming it did", async () => {
    const { user, bridge } = await openModels();
    bridge.restart.mockResolvedValueOnce({
      ok: true as const,
      value: { restarted: false, reason: "not this app's runtime", report: null },
    });
    await user.click(await screen.findByRole("button", { name: /重启 Runtime/ }));
    await waitFor(() => expect(screen.getByText(/这个 Runtime 不是这个应用启动的/)).toBeTruthy());
  });

  /// A drain is not a formality: the report says how much was still in flight
  /// when the runtime was asked to stop, and a person who just restarted to
  /// change a model should be told a Run was cut short by it.
  it("says what the drain found rather than only that it restarted", async () => {
    const { user, bridge } = await openModels();
    bridge.restart.mockResolvedValueOnce({
      ok: true as const,
      value: {
        restarted: true,
        reason: null,
        report: { active_runs: 2, queued_runs: 1 },
        escalated: false,
      },
    });
    await user.click(await screen.findByRole("button", { name: /重启 Runtime/ }));
    await waitFor(() => expect(screen.getByText(/2 个在跑/)).toBeTruthy());
  });
});
