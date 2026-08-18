/// What this file is for.
///
/// The runtime has had Fork and Rollback since before this client existed, and
/// no line of the client called either. These tests hold what makes them usable
/// rather than merely reachable: a Fork is cut through the Turn the person
/// pointed at and at the generation the branch is at now, the branch it
/// produced is the one that opens and a refused one opens nothing, a Rollback
/// takes two presses and does nothing on the first, and the second press
/// destroys what the first press named -- or nothing, because the arming names
/// a head, and heads move.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE, SESSION, SESSION_BRANCH } from "./fake-runtime";
import { all } from "../surfaces/registry";

async function openChat(options?: Parameters<typeof installFakeRuntime>[0]) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(options);
  render(<App />);
  // The third Turn, so the whole committed conversation is on screen before
  // anything is clicked on it.
  await waitFor(() => expect(screen.getByText("记下了。")).toBeTruthy());
  return { user, bridge };
}

/// The branching row under one Turn, found by the sentence in that Turn.
///
/// By position rather than by label: every Turn offers the same two words, so a
/// query by name alone would answer with whichever row is first and the test
/// would pass while the button acted on a different Turn.
function actsUnder(said: string): HTMLElement {
  const turn = screen.getByText(said).closest(".turn");
  if (!turn) throw new Error(`no Turn holds ${said}`);
  const acts = turn.querySelector<HTMLElement>(".branch");
  if (!acts) throw new Error(`the Turn holding ${said} offers nothing`);
  return acts;
}

const forkAt = (said: string) =>
  screen.getByText(said).closest(".turn")!.querySelector<HTMLButtonElement>(".branch .flat")!;
const rollbackAt = (said: string) =>
  actsUnder(said).querySelector<HTMLButtonElement>(".back")!;

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("forking a conversation", () => {
  it("cuts through the Turn that was pointed at, at the generation the runtime reports", async () => {
    const { user, bridge } = await openChat();
    await user.click(forkAt("我刚才说我叫什么？"));

    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());
    const sent = bridge.sessionFork.mock.calls[0][0];
    expect(sent.sessionId).toBe(SESSION);
    expect(sent.sourceBranchId).toBe(SESSION_BRANCH);
    expect(sent.throughTurnOrdinal).toBe(2);
    // Read from the head immediately before the write, exactly as continuing
    // does: a Fork carries a generation fence, and a remembered one is a Fork
    // off history the person may already have rolled back.
    expect(bridge.sessionRead).toHaveBeenCalled();
    expect(sent.sourceGeneration).toBe(1);
    // A new branch, named by this client. The same id as the source would be
    // refused, and the daemon uses this one to recognise a retry.
    expect(sent.targetBranchId).not.toBe(SESSION_BRANCH);
  });

  it("opens the branch it just made, not the one it was cut from", async () => {
    const { user, bridge } = await openChat();
    await user.click(forkAt("我刚才说我叫什么？"));
    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());
    const cut = bridge.sessionFork.mock.calls[0][0].targetBranchId;

    // The Fork carries Turns 1 and 2 and not the third, so the transcript is
    // the evidence that the new branch is what is on screen.
    await waitFor(() => expect(screen.queryByText("记下了。")).toBeNull());
    expect(screen.getByText("小林。")).toBeTruthy();

    // And the next sentence continues the branch that was cut, which is the
    // whole point of cutting it. Keyed by Session alone -- which is how this
    // client identified a conversation before Fork existed -- this lands on the
    // source branch instead, and the person's new strand is never written to.
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "在新分支上说");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.sessionContinue).toHaveBeenCalled());
    expect(bridge.sessionContinue.mock.calls[0][0].branchId).toBe(cut);
  });

  it("cuts from the generation the branch is at now, not the one it opened at", async () => {
    const { user, bridge } = await openChat();
    // A Rollback first, which is the thing that moves a generation.
    await user.click(rollbackAt("我刚才说我叫什么？"));
    await user.click(rollbackAt("我刚才说我叫什么？"));
    await waitFor(() => expect(screen.queryByText("记下了。")).toBeNull());

    await user.click(forkAt("我叫小林，请记住"));
    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());
    // Read here, not remembered from when the conversation was opened: a Fork
    // carrying the generation this branch has left is refused, and the person
    // gets a no for a fence they moved themselves.
    expect(bridge.sessionFork.mock.calls[0][0].sourceGeneration).toBe(2);
    expect(document.querySelector(".err")).toBeNull();
  });

  it("stays where it is when the Fork is refused, and says so", async () => {
    // A Session already at its branch ceiling. The daemon takes that ceiling as
    // a policy rather than a constant, so this is a state a Session reaches.
    const { user, bridge } = await openChat({ maxBranches: 1 });
    await user.click(forkAt("我刚才说我叫什么？"));
    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());

    // Nothing was cut, so nothing opens. The conversation on screen is still
    // the three-Turn one, the refusal is drawn rather than swallowed, and the
    // next sentence goes where it was already going.
    await waitFor(() => expect(screen.getByText(/ceiling of 1/)).toBeTruthy());
    expect(screen.getByText("记下了。")).toBeTruthy();
    const box = await screen.findByRole("textbox");
    await user.click(box);
    await user.type(box, "接着说");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(bridge.sessionContinue).toHaveBeenCalled());
    expect(bridge.sessionContinue.mock.calls[0][0].branchId).toBe(SESSION_BRANCH);
  });

  it("names the strand on rows a Session now has two of", async () => {
    const { user, bridge } = await openChat();
    await user.click(forkAt("我刚才说我叫什么？"));
    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());
    const cut = bridge.sessionFork.mock.calls[0][0].targetBranchId;

    await user.click(
      screen.getAllByRole("button", { name: /^会话/ }).find((n) => n.classList.contains("r"))!,
    );
    // Two rows for the forked Session, each naming its branch; the other
    // conversation has one branch and says nothing about it, because there its
    // branch id answers a question nobody asked.
    await waitFor(() => expect(screen.getAllByText(/^分支 /)).toHaveLength(2));
    expect(screen.getByText(`分支 ${cut.slice(0, 8)}`)).toBeTruthy();
    expect(screen.getByText(`分支 ${SESSION_BRANCH.slice(0, 8)}`)).toBeTruthy();
  });
});

describe("rolling a branch back", () => {
  it("does nothing on the first press and says so", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我叫小林，请记住"));

    expect(bridge.sessionRollback).not.toHaveBeenCalled();
    expect(actsUnder("我叫小林，请记住").textContent).toContain("再按一次确认");
    // The Turns it would drop are still here, which is the fact the assertion
    // above is standing in for.
    expect(screen.getByText("小林。")).toBeTruthy();
    expect(screen.getByText("记下了。")).toBeTruthy();
  });

  it("drops the Turns after the one it was armed on, on the second press", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我叫小林，请记住"));
    await user.click(rollbackAt("我叫小林，请记住"));

    await waitFor(() => expect(bridge.sessionRollback).toHaveBeenCalled());
    const sent = bridge.sessionRollback.mock.calls[0][0];
    expect(sent.sessionId).toBe(SESSION);
    expect(sent.branchId).toBe(SESSION_BRANCH);
    expect(sent.throughTurnOrdinal).toBe(1);
    expect(sent.generation).toBe(1);
    // The conversation on screen is the shorter one afterwards. Two Turns went.
    await waitFor(() => expect(screen.queryByText("小林。")).toBeNull());
    expect(screen.queryByText("记下了。")).toBeNull();
    expect(screen.getByText("我叫小林，请记住")).toBeTruthy();
  });

  it("re-arms rather than fires when a different Turn is pointed at", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我叫小林，请记住"));
    // A different Turn's Rollback is a different act, so this arms that one
    // instead of firing the one already armed.
    await user.click(rollbackAt("我刚才说我叫什么？"));
    expect(bridge.sessionRollback).not.toHaveBeenCalled();
    expect(actsUnder("我叫小林，请记住").textContent).not.toContain("再按一次确认");
    expect(actsUnder("我刚才说我叫什么？").textContent).toContain("再按一次确认");

    await user.click(rollbackAt("我刚才说我叫什么？"));
    await waitFor(() => expect(bridge.sessionRollback).toHaveBeenCalled());
    // The Turn the second press was aimed at, not the one armed first.
    expect(bridge.sessionRollback.mock.calls[0][0].throughTurnOrdinal).toBe(2);
  });

  it("rolls back at the generation the branch is at now, not the one it opened at", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我刚才说我叫什么？"));
    await user.click(rollbackAt("我刚才说我叫什么？"));
    await waitFor(() => expect(screen.queryByText("记下了。")).toBeNull());
    expect(bridge.sessionRollback.mock.calls[0][0].generation).toBe(1);

    // A second Rollback has to carry the generation the first one produced,
    // and the only place that number exists is the head -- read again, here,
    // immediately before the write. Remembered, or never read at all, this is
    // fenced off by the branch and the Turns stay.
    await user.click(rollbackAt("我叫小林，请记住"));
    await user.click(rollbackAt("我叫小林，请记住"));
    await waitFor(() => expect(bridge.sessionRollback).toHaveBeenCalledTimes(2));
    expect(bridge.sessionRollback.mock.calls[1][0].generation).toBe(2);
    const wrote = bridge.sessionRollback.mock.invocationCallOrder;
    expect(bridge.sessionRead.mock.invocationCallOrder
      .some((read) => read > wrote[0] && read < wrote[1])).toBe(true);
    // And it landed, which is the fact the generation is standing in for.
    await waitFor(() => expect(screen.queryByText("小林。")).toBeNull());
    expect(screen.getByText("记住了。")).toBeTruthy();
    expect(document.querySelector(".err")).toBeNull();
  });

  it("disarms when a Turn lands under it, rather than dropping more than was confirmed", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我叫小林，请记住"));
    // Two Turns is what the person was shown and what they confirmed.
    expect(actsUnder("我叫小林，请记住").textContent).toContain("撤掉后面 2 轮");
    expect(actsUnder("我叫小林，请记住").textContent).toContain("再按一次确认");

    // A fourth Turn lands on this branch from somewhere else, and the poll
    // brings it in. Nobody confirmed anything about a conversation this long.
    bridge.elsewhere.commits("再帮我记一件事", "也记下了。");
    await waitFor(
      () => expect(screen.getByText("再帮我记一件事")).toBeTruthy(), { timeout: 4_000 },
    );

    // The arming is gone on its own: the head it named is not the head here.
    expect(actsUnder("我叫小林，请记住").textContent).toContain("撤掉后面 3 轮");
    expect(actsUnder("我叫小林，请记住").textContent).not.toContain("再按一次确认");
    // And the press that would have been the second one arms the new target
    // instead of firing at it. Three Turns do not go on a press aimed at two.
    await user.click(rollbackAt("我叫小林，请记住"));
    expect(bridge.sessionRollback).not.toHaveBeenCalled();
    expect(actsUnder("我叫小林，请记住").textContent).toContain("再按一次确认");
  });

  it("disarms when the branch moves to another generation under it", async () => {
    const { user, bridge } = await openChat();
    await user.click(rollbackAt("我叫小林，请记住"));

    // Elsewhere the branch is taken back to Turn 2 and a different third Turn
    // lands. Three Turns again, and 回到这里 still says two -- but they are not
    // the two the person was looking at, which is why the count alone is not
    // the target.
    bridge.elsewhere.rollsBackTo(2);
    bridge.elsewhere.commits("换个话题", "好。");
    await waitFor(() => expect(screen.getByText("换个话题")).toBeTruthy(), { timeout: 4_000 });
    expect(screen.queryByText("记下了。")).toBeNull();

    expect(actsUnder("我叫小林，请记住").textContent).toContain("撤掉后面 2 轮");
    expect(actsUnder("我叫小林，请记住").textContent).not.toContain("再按一次确认");
    await user.click(rollbackAt("我叫小林，请记住"));
    expect(bridge.sessionRollback).not.toHaveBeenCalled();
  });

  it("disarms when the person reaches for a Fork instead", async () => {
    // Refused, so the conversation stays on screen to be asked about after --
    // a Fork that succeeded would open the branch it cut and take this
    // component with it.
    const { user, bridge } = await openChat({ maxBranches: 1 });
    await user.click(rollbackAt("我叫小林，请记住"));
    expect(actsUnder("我叫小林，请记住").textContent).toContain("再按一次确认");

    await user.click(forkAt("我刚才说我叫什么？"));
    await waitFor(() => expect(bridge.sessionFork).toHaveBeenCalled());
    expect(actsUnder("我叫小林，请记住").textContent).not.toContain("再按一次确认");
    await user.click(rollbackAt("我叫小林，请记住"));
    expect(bridge.sessionRollback).not.toHaveBeenCalled();
  });

  it("is not offered on the last Turn, where the runtime would refuse it", async () => {
    await openChat();
    // Every Turn can be forked from; only the ones with something after them
    // can be rolled back to.
    expect(actsUnder("帮我记一下今天的日期").textContent).toContain("从这里分叉");
    expect(actsUnder("帮我记一下今天的日期").querySelector(".back")).toBeNull();
    expect(actsUnder("我刚才说我叫什么？").textContent).toContain("撤掉后面 1 轮");
    expect(actsUnder("我叫小林，请记住").textContent).toContain("撤掉后面 2 轮");
  });

  it("is offered on no Turn while one is in flight, and neither is a Fork", async () => {
    await openChat({ activeRunId: RUN_LIVE });
    // The conversation is all there.
    expect(screen.getByText("小林。")).toBeTruthy();
    expect(screen.getByText("记下了。")).toBeTruthy();
    // And carries no branching row on any Turn: the branch refuses both while
    // a Turn is running, and a control certain to be refused is the same
    // mistake as a key hint for a key that does nothing. What this moment does
    // offer is in the box, which says so.
    expect(document.querySelectorAll(".turn .branch")).toHaveLength(0);
    const box = await screen.findByRole("textbox");
    expect((box as HTMLTextAreaElement).placeholder).toContain("改向");
  });

  it("is on no bare key the conversation surface claims", async () => {
    const { user, bridge } = await openChat();
    const chat = all().find((surface) => surface.id === "chat");
    expect(chat?.keys?.length).toBeGreaterThan(0);
    for (const key of chat!.keys!) await user.keyboard(key.key);
    // Every key this surface claims has now been pressed. None of them may be
    // the one that destroys Turns -- the rule the approval queue follows for
    // ending a Run, which is the other irreversible thing this client can do.
    expect(bridge.sessionRollback).not.toHaveBeenCalled();
    expect(bridge.sessionFork).not.toHaveBeenCalled();
  });
});
