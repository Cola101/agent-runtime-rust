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

  it("starts a new branch after 新对话, instead of continuing the old one", async () => {
    const { user, bridge } = await openChat();
    await waitFor(() => expect(screen.getByText("小林。")).toBeTruthy());
    await user.click(screen.getByRole("button", { name: "新对话" }));
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

  it("shuts the box while a Turn is in flight rather than letting it be refused", async () => {
    const { user } = await openChat({ activeRunId: RUN_LIVE });
    const box = await screen.findByRole("textbox");
    await waitFor(() => expect((box as HTMLTextAreaElement).disabled).toBe(true));
    expect((box as HTMLTextAreaElement).placeholder).toContain("这轮还在跑");
    await user.type(box, "插一句");
    expect((box as HTMLTextAreaElement).value).toBe("");
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
