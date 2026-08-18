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
