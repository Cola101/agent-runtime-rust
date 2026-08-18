// @vitest-environment jsdom
/// What this file is for.
///
/// The workspace folder came only from an environment variable, and otherwise
/// defaulted to a directory under the app's own data. So a person who installs
/// this and opens it gets an agent working somewhere in Application Support
/// rather than in their project, with no way to change it from the window --
/// which is the single largest thing between this app and being used.
///
/// Choosing is a grant, not a preference: the folder is what every workspace
/// read and write is contained by after `realpath`, so this moves the boundary
/// itself. The guards here are about saying true things around that: what it
/// costs, when it cannot be done, and that nothing running has changed yet.
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

async function openWorkspace() {
  const user = userEvent.setup();
  const bridge = installFakeRuntime();
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /工作区/ })).toBeTruthy());
  await user.click(screen.getByRole("button", { name: /^工作区/ }));
  return { user, bridge };
}

describe("choosing the folder the agent works in", () => {
  it("opens the system picker rather than taking a typed path", async () => {
    // A text field would apply as it is typed, and half a path is a folder.
    const { user, bridge } = await openWorkspace();
    await user.click(await screen.findByRole("button", { name: /选择工作目录/ }));
    await waitFor(() => expect(bridge.chooseWorkspace).toHaveBeenCalled());
  });

  /// The root is read by `runtime-host` at startup. A window that showed the
  /// new folder as though it were in force would be describing the next
  /// runtime, not the one answering.
  it("says the choice reaches the next runtime, not the one running", async () => {
    const { user } = await openWorkspace();
    await user.click(await screen.findByRole("button", { name: /选择工作目录/ }));
    await waitFor(() => expect(screen.getByText(/重启 Runtime 之后才生效/)).toBeTruthy());
    expect(screen.getByText(/\/Users\/x\/code/)).toBeTruthy();
  });

  /// Cancelling is not a failure and must not read as one.
  it("says nothing at all when the picker was closed", async () => {
    const { user, bridge } = await openWorkspace();
    bridge.chooseWorkspace.mockResolvedValueOnce({
      ok: true as const, value: { chosen: null, reason: "cancelled" },
    });
    await user.click(await screen.findByRole("button", { name: /选择工作目录/ }));
    await waitFor(() => expect(bridge.chooseWorkspace).toHaveBeenCalled());
    expect(screen.queryByText(/重启 Runtime 之后才生效/)).toBeNull();
  });

  /// `RUNTIME_DESK_WORKSPACE` is how a checkout is used in development, and a
  /// stored choice overriding it would put a dev runtime in a folder nobody
  /// pointed it at. The control is absent and the reason is on screen, rather
  /// than a button that does nothing.
  it("does not offer the choice when a variable is holding the folder", async () => {
    const bridge = installFakeRuntime();
    bridge.desk.runtime.workspace = async () => ({
      ok: true as const,
      value: {
        root: "/checkout",
        configured: true,
        choosable: false,
        fixedBy: "environment",
      },
    });
    render(<App />);
    const user = userEvent.setup();
    await waitFor(() => expect(screen.getByRole("button", { name: /工作区/ })).toBeTruthy());
    await user.click(screen.getByRole("button", { name: /^工作区/ }));
    await waitFor(() => expect(screen.getByText(/环境变量/)).toBeTruthy());
    expect(screen.queryByRole("button", { name: /选择工作目录/ })).toBeNull();
  });
});
