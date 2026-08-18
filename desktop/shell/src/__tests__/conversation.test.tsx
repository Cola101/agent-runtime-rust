/// What this file is for.
///
/// The composer used to call `submit`, which starts a bare Run: no history, so
/// every sentence was the first sentence of its own conversation and the model
/// was never told what came before. It looked like a chat and was not one.
///
/// These tests hold the three things that make it one: the second sentence is a
/// Turn on the same branch, the conversation is drawn from the Session's frozen
/// transcript rather than from an event log, and the box is shut while a Turn
/// is in flight -- because the branch refuses a second one and a refusal the
/// person could not see coming is a bug with an error message.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";
import { all } from "../surfaces/registry";
import { uuidv7 } from "../ids";

async function openChat(options?: { activeRunId?: string | null }) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(options);
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  return { user, bridge };
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("a conversation", () => {
  it("shows the committed Turns, not a rendering of the event log", async () => {
    await openChat();
    await waitFor(() => expect(screen.getByText("我叫小林，请记住")).toBeTruthy());
    expect(screen.getByText("记住了。")).toBeTruthy();
    expect(screen.getByText("我刚才说我叫什么？")).toBeTruthy();
    expect(screen.getByText("小林。")).toBeTruthy();
  });

  it("continues the open branch at the generation the runtime reports", async () => {
    const { user, bridge } = await openChat();
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "第二轮");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.sessionContinue).toHaveBeenCalled());
    const sent = bridge.sessionContinue.mock.calls[0][0];
    expect(sent.input).toBe("第二轮");
    // Read from the head immediately before the write, never remembered: a
    // rollback moves the generation, and a client continuing at a stale one
    // lands a Turn on history the person already retired.
    expect(bridge.sessionRead).toHaveBeenCalled();
    expect(sent.generation).toBe(1);
    expect(bridge.sessionStart).not.toHaveBeenCalled();
  });

  /// A refused send must not eat what was typed.
  ///
  /// The box is cleared before the write, so any refusal -- the branch busy
  /// finishing the Turn a Runtime restart interrupted, a socket that went away
  /// -- leaves the person looking at an error and an empty box, with the
  /// sentence they wrote reachable only if they know to press ↑.
  it("puts the message back in the box when the send was refused", async () => {
    const { user, bridge } = await openChat();
    bridge.sessionContinue.mockResolvedValueOnce({
      ok: false as const, error: "local execution was refused: root Session branch already has an active Turn",
    });
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "重启之后的第一句");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.sessionContinue).toHaveBeenCalled());
    await waitFor(() =>
      expect((box as HTMLTextAreaElement).value).toBe("重启之后的第一句"));
  });

  /// And what it says is a sentence, not the runtime's own words.
  it("says a branch still finishing a Turn in words a person can act on", async () => {
    const { user, bridge } = await openChat();
    bridge.sessionContinue.mockResolvedValueOnce({
      ok: false as const, error: "local execution was refused: root Session branch already has an active Turn",
    });
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "重启之后的第一句");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(screen.getByText(/这轮还没结束/)).toBeTruthy());
    expect(screen.queryByText(/root Session branch/)).toBeNull();
  });

  it("starts a new branch after 新对话, instead of continuing the old one", async () => {
    const { user, bridge } = await openChat();
    await waitFor(() => expect(screen.getByText("小林。")).toBeTruthy());
    // Through the declared key rather than a button in the composer: the shell
    // renders `n 新对话` from the key registry, and a second button beside the
    // input was the same affordance twice.
    await user.click(screen.getByText("小林。"));
    await user.keyboard("n");
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "另开一段");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.sessionStart).toHaveBeenCalled());
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
    const sent = bridge.sessionStart.mock.calls[0][0];
    expect(sent.input).toBe("另开一段");
    // Ids are the caller's to choose, and `runId` is the idempotency key. All
    // three have to be distinct or a Turn would be its own Session.
    expect(new Set([sent.sessionId, sent.branchId, sent.runId]).size).toBe(3);
  });

  /// This used to assert the opposite -- the box was shut while a Turn ran,
  /// because the Runtime had no way to redirect one and the only honest thing
  /// was to refuse. It has one now, so the same box does a different thing
  /// depending on what the Run is doing, and the person is told which.
  it("redirects the Turn in flight instead of queueing a next one", async () => {
    const { user, bridge } = await openChat({ activeRunId: RUN_LIVE });
    const box = await screen.findByRole("textbox");
    await waitFor(() => expect((box as HTMLTextAreaElement).disabled).toBe(false));
    expect((box as HTMLTextAreaElement).placeholder).toContain("改向");

    await user.click(box);
    await user.type(box, "换个方向");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.steer).toHaveBeenCalled());
    const sent = bridge.steer.mock.calls[0][0];
    expect(sent.input).toBe("换个方向");
    expect(sent.runId).toBe(RUN_LIVE);
    // A steer is not a Turn. Sending one as a continuation would queue work
    // the person meant to replace.
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
  });

  it("never claims a steer was applied", async () => {
    await openChat({ activeRunId: RUN_LIVE });
    const box = await screen.findByRole("textbox");
    // The box says what it will do with what you type, and the button says the
    // same word. Neither says it worked -- the Runtime refuses to redirect
    // while a tool call or approval is unresolved, and the only evidence is
    // `run.steer.applied` appearing in the transcript.
    await waitFor(() =>
      expect((box as HTMLTextAreaElement).placeholder).toContain("会拿去改向"));
    expect(screen.getByRole("button", { name: "改向" })).toBeTruthy();
    for (const claim of [/已改向/, /改向成功/, /已生效/]) {
      expect(screen.queryByText(claim)).toBeNull();
    }
  });
});

describe("the conversation list", () => {
  async function openList(user: ReturnType<typeof userEvent.setup>) {
    await user.click(
      screen.getAllByRole("button", { name: /^会话/ }).find((n) => n.classList.contains("r"))!,
    );
  }

  it("opens the newest conversation on launch, not an arbitrary one", async () => {
    await openChat();
    // Both exist; the newest is the one on screen. With v4 ids the runtime's
    // order would be arbitrary and this would pass or fail by luck.
    await waitFor(() => expect(screen.getByText("我叫小林，请记住")).toBeTruthy());
    expect(screen.queryByText("上礼拜那段对话")).toBeNull();
  });

  it("switches back to an older conversation", async () => {
    const { user } = await openChat();
    await waitFor(() => expect(screen.getByText("小林。")).toBeTruthy());
    await openList(user);
    await waitFor(() => expect(screen.getByText("上礼拜那段对话")).toBeTruthy());
    await user.click(screen.getByText("上礼拜那段对话"));
    await user.click(
      screen.getAllByRole("button", { name: /^对话/ }).find((n) => n.classList.contains("r"))!,
    );
    // The transcript follows the list. Before the list existed there was no
    // way back here at all.
    await waitFor(() => expect(screen.getByText("还在这儿。")).toBeTruthy());
    expect(screen.queryByText("小林。")).toBeNull();
  });

  it("puts nothing irreversible on a bare key", () => {
    const surface = all().find((candidate) => candidate.id === "conversations");
    expect(surface).toBeTruthy();
    for (const key of surface!.keys ?? []) {
      expect(["j", "k", "Enter", "n"]).toContain(key.key);
    }
  });
});

describe("the ids this client mints", () => {
  /// The runtime returns conversations sorted by id. With random v4 ids that
  /// order is arbitrary, and "最近的对话" drawn over it would be invented.
  it("sorts in the order they were minted", () => {
    let clock = 1_760_000_000_000;
    const ids = Array.from({ length: 200 }, () => uuidv7(() => clock));
    const later = uuidv7(() => (clock += 1));
    expect([...ids].sort()).toEqual(ids);
    expect(later > ids[ids.length - 1]).toBe(true);
  });

  it("is a valid version 7 uuid", () => {
    expect(uuidv7()).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  });
});
