// @vitest-environment jsdom
/// What this file is for.
///
/// The first thing a person sees after installing this is a window with no
/// runtime behind it, because a fresh install has no provider and the app will
/// not start a runtime without one. The main process knows that and says so on
/// its own console; the window said "连不上 Runtime" and printed a socket path.
///
/// That is the difference between an app that looks broken and an app that
/// tells you the one thing you have to do. It is also the only screen every
/// new person is guaranteed to see.
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

/// The status a host reports when it declined to start a runtime for want of a
/// provider: a state root it knows, a socket nothing is on, and the reason.
function withoutAProvider() {
  const bridge = installFakeRuntime();
  const idle = {
    transport: "local",
    stateRoot: "/tmp/state",
    socketPath: "/tmp/state/runtime-host.sock",
    connected: false,
    error: "no runtime is listening",
    reason: "no-provider",
  };
  bridge.desk.runtime.status = async () => ({ ok: true as const, value: idle });
  bridge.desk.runtime.probe = async () => ({ ok: true as const, value: idle });
  return bridge;
}

describe("the first launch", () => {
  it("says a provider is what is missing, not that a socket is silent", async () => {
    withoutAProvider();
    render(<App />);
    await waitFor(() => expect(screen.getByText(/还没配 Provider/)).toBeTruthy());
    // The socket path is not the person's problem and naming it reads as a
    // fault in the app rather than a step they have not taken.
    expect(screen.queryByText(/runtime-host\.sock/)).toBeNull();
  });

  /// The identity in the corner is the first thing a person reads about the
  /// state of the app, and on a fresh install it said 无宿主 -- the host is
  /// there, it just has no Provider yet. Three of the seven link states fell
  /// through to that word, including the one every fresh install starts in.
  it("does not call a host that is present and unconfigured 无宿主", async () => {
    withoutAProvider();
    render(<App />);
    await waitFor(() => expect(screen.getByText(/还没配 Provider/)).toBeTruthy());
    expect(screen.queryByText("无宿主")).toBeNull();
  });

  /// The connection row on the settings page said nothing at all for the same
  /// three states -- a blank where the one fact the row exists for belongs.
  it("says what the connection is, on the page about the connection", async () => {
    const user = userEvent.setup();
    withoutAProvider();
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /设置/ })).toBeTruthy());
    await user.click(
      screen.getAllByRole("button", { name: /^设置/ }).find((node) => node.classList.contains("r"))!,
    );
    // The 状态 row itself. It named four of the seven states and drew a blank
    // for this one -- on the page whose whole subject is the connection.
    await waitFor(() =>
      expect(screen.getByText(/没有 Provider，所以没启动 Runtime/)).toBeTruthy());
  });

  it("says where to go, in the words the rail uses for that surface", async () => {
    withoutAProvider();
    render(<App />);
    // 设置 is what the rail calls it. A banner that said "preferences" would be
    // sending someone to a place this window does not have.
    await waitFor(() => expect(screen.getByText(/设置/)).toBeTruthy());
  });

  /// A runtime that refuses to start says why on stderr and is then gone.
  /// `RuntimeProcess` keeps those lines for exactly that reason -- its own
  /// comment says so -- and they went to the main process's console and
  /// nowhere else, while the window showed a silent socket. The person who has
  /// to act on it is the one at the window.
  it("shows what the runtime said before it died, rather than a silent socket", async () => {
    const bridge = installFakeRuntime();
    const died = {
      transport: "local",
      stateRoot: "/tmp/state",
      socketPath: "/tmp/state/runtime-host.sock",
      connected: false,
      error: "no runtime is listening",
      reason: "start-failed",
      said: 'Error: "AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT is required"',
    };
    bridge.desk.runtime.status = async () => ({ ok: true as const, value: died });
    bridge.desk.runtime.probe = async () => ({ ok: true as const, value: died });
    render(<App />);
    await waitFor(() =>
      expect(screen.getByText(/AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT/)).toBeTruthy());
  });

  /// A bundle without its runtime binary is a broken download, not a
  /// misconfiguration, and there is nothing the person can do in this window
  /// about it. Saying which of the two it is is the whole value of the
  /// message: one sends them to the settings page and the other to the
  /// download.
  it("says the app is missing its runtime, rather than blaming a socket", async () => {
    const bridge = installFakeRuntime();
    const absent = {
      transport: "local",
      stateRoot: "/tmp/state",
      socketPath: "/tmp/state/runtime-host.sock",
      connected: false,
      error: "no runtime is listening",
      reason: "no-binary",
      said: null,
    };
    bridge.desk.runtime.status = async () => ({ ok: true as const, value: absent });
    bridge.desk.runtime.probe = async () => ({ ok: true as const, value: absent });
    render(<App />);
    await waitFor(() => expect(screen.getByText(/这个应用里没有 Runtime/)).toBeTruthy());
    expect(screen.queryByText(/runtime-host\.sock/)).toBeNull();
  });

  /// A runtime that is genuinely unreachable is a different fact and keeps its
  /// own words: something was there and is not answering.
  it("still names the socket when a runtime should have been there", async () => {
    const bridge = installFakeRuntime();
    const dead = {
      transport: "local",
      stateRoot: "/tmp/state",
      socketPath: "/tmp/state/runtime-host.sock",
      connected: false,
      error: "connection refused",
      reason: null,
    };
    bridge.desk.runtime.status = async () => ({ ok: true as const, value: dead });
    bridge.desk.runtime.probe = async () => ({ ok: true as const, value: dead });
    render(<App />);
    await waitFor(() => expect(screen.getByText(/runtime-host\.sock/)).toBeTruthy());
  });
});
