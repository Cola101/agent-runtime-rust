/// What this file is for.
///
/// A Turn takes minutes and a person keeps thinking during them. Everything
/// typed while one was running used to be a steer -- a redirection of the Turn
/// in flight -- so the second sentence of a thought could not be said at all
/// without changing the first one's course. These tests hold the other thing
/// the box has to be able to do: keep what was typed, in order, and send it as
/// the Turn ends.
///
/// The queue is this window's, not the Runtime's. Nothing in it has been sent
/// to a model, which is what makes it a draft rather than state -- and what
/// makes the hand-back tests the important ones here: a Turn that was
/// cancelled, failed, or refused the sentence on its way out must put those
/// drafts back where they can be read and edited, not drop them and not fire
/// them at a conversation the person just stopped.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, OLDER_BRANCH, RUN_DONE, RUN_LIVE } from "./fake-runtime";

/// The window with a Turn already in flight, which is the only state this
/// whole file is about.
async function openRunningChat() {
  const user = userEvent.setup();
  const bridge = installFakeRuntime({ activeRunId: RUN_LIVE });
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  const box = (await screen.findByRole("textbox")) as HTMLTextAreaElement;
  // The box stays open while a Turn runs -- that is the premise. A disabled
  // one would make every assertion below pass for the wrong reason.
  await waitFor(() => expect(box.disabled).toBe(false));
  return { user, bridge, box };
}

async function say(
  user: ReturnType<typeof userEvent.setup>, box: HTMLTextAreaElement, text: string,
) {
  await user.click(box);
  await user.type(box, text);
  await user.keyboard("{Enter}");
}

const queue = () => screen.queryByRole("list", { name: /排队/ });
const queued = () => [...(queue()?.querySelectorAll("li") ?? [])].map((row) => row.textContent ?? "");
const said = () => document.querySelector(".err")?.textContent ?? "";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("typing while a Turn is running", () => {
  it("queues the sentence instead of steering with it, and starts no second Turn", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "顺便把测试也跑一下");

    await waitFor(() => expect(queued().join("")).toContain("顺便把测试也跑一下"));
    expect(box.value).toBe("");
    // Not a steer: a steer replaces what the Turn is doing, and this sentence
    // is not about the Turn in flight at all.
    expect(bridge.steer).not.toHaveBeenCalled();
    // And not a Turn: the branch refuses a second one, and a client that asked
    // anyway would be showing an error instead of a queue.
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
  });

  it("sends what is queued as the Turn ends, one at a time", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "第一句");
    await say(user, box, "第二句");
    expect(queued().length).toBe(2);

    bridge.elsewhere.ends("succeeded");
    await waitFor(
      () => expect(bridge.sessionContinue).toHaveBeenCalledTimes(1), { timeout: 4_000 },
    );
    expect(bridge.sessionContinue.mock.calls[0][0].input).toBe("第一句");
    // The second one waits for the Turn the first one just started. Sending
    // both here would be asking a branch that holds one Turn to hold two.
    expect(queued().join("")).toContain("第二句");

    bridge.elsewhere.starts();
    await waitFor(
      () => expect(box.placeholder).toMatch(/排队/), { timeout: 4_000 },
    );
    bridge.elsewhere.ends("succeeded");
    await waitFor(
      () => expect(bridge.sessionContinue).toHaveBeenCalledTimes(2), { timeout: 4_000 },
    );
    expect(bridge.sessionContinue.mock.calls[1][0].input).toBe("第二句");
    expect(queue()).toBeNull();
  });

  /// The one that matters.
  ///
  /// A queue that fires into a cancelled conversation is worse than no queue,
  /// and a queue that quietly drops what it held is worse than both. What was
  /// never sent goes back to the box, where it can be read, edited, or thrown
  /// away by the person who wrote it.
  it("puts what is queued back in the box when the Turn was cancelled", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "第一句");
    await say(user, box, "第二句");

    bridge.elsewhere.ends("cancelled");
    await waitFor(() => expect(box.value).toContain("第一句"), { timeout: 4_000 });
    expect(box.value).toContain("第二句");
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
    expect(queue()).toBeNull();
    // And it says why the box just filled itself, rather than leaving that to
    // be worked out from a status line somewhere else.
    expect(said()).toMatch(/放回|没有正常结束/);
  });

  it("puts them back when the Turn failed, too", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "跑完再说这个");

    bridge.elsewhere.ends("failed");
    await waitFor(() => expect(box.value).toContain("跑完再说这个"), { timeout: 4_000 });
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
  });

  /// The other way a queued sentence could disappear.
  ///
  /// Reading the head and writing to it are two calls, and the branch can pick
  /// up a Turn in between -- a Runtime restart resumes the Turn it interrupted,
  /// so this is not hypothetical. A drain that shrugged at a refused send would
  /// drop the sentence it had just taken out of the queue.
  it("gives back a sentence the runtime refused on the way out", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "第一句");
    await say(user, box, "第二句");
    bridge.sessionContinue.mockResolvedValueOnce({
      ok: false as const,
      error: "local execution was refused: root Session branch already has an active Turn",
    });

    bridge.elsewhere.ends("succeeded");
    await waitFor(() => expect(box.value).toContain("第一句"), { timeout: 4_000 });
    // Both of them, in the order they were typed: the one that was refused had
    // been taken out of the queue, and the one behind it is not going anywhere
    // now either.
    expect(box.value).toContain("第二句");
    expect(box.value.indexOf("第一句")).toBeLessThan(box.value.indexOf("第二句"));
    expect(queue()).toBeNull();
    // In the words that mean something to a person, not the runtime's own.
    expect(said()).toContain("这轮还没结束");
  });

  it("holds a stated number of sentences, and refuses the next one out loud", async () => {
    const { user, box } = await openRunningChat();
    await say(user, box, "第1句");
    // The ceiling is on screen rather than only in the code: a queue that
    // silently stops accepting is a box that stopped working.
    const head = document.querySelector(".queue-head")?.textContent ?? "";
    const cap = Number(/\/\s*(\d+)/.exec(head)?.[1] ?? NaN);
    expect(cap).toBeGreaterThan(1);

    for (let nth = 2; nth <= cap; nth += 1) await say(user, box, `第${nth}句`);
    expect(queued().length).toBe(cap);

    await say(user, box, "多出来的一句");
    // Refused, and the sentence is still where it was typed -- the same rule a
    // refused send follows, for the same reason: nothing happened to it.
    await waitFor(() => expect(box.value).toBe("多出来的一句"));
    expect(said()).toMatch(/满/);
    expect(queued().length).toBe(cap);
  });

  /// A queue belongs to the conversation it was typed into.
  ///
  /// "The open branch stopped naming an active Run" has two readings, and only
  /// one of them is a Turn ending: the other is the person having moved to a
  /// conversation that has nothing running. A client that could not tell them
  /// apart would send what was typed here into whatever is open there.
  it("gives them back when the person leaves the conversation they were typed into", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "第一句");
    await say(user, box, "第二句");

    // This conversation's Turn ends and the first sentence goes, as it should.
    // The second is waiting for the Turn the first one just started.
    bridge.elsewhere.ends("succeeded");
    await waitFor(
      () => expect(bridge.sessionContinue).toHaveBeenCalledTimes(1), { timeout: 4_000 },
    );

    // The person goes to read the other conversation, which has a Turn of its
    // own in flight, and comes back to find it.
    bridge.elsewhere.starts(RUN_DONE, OLDER_BRANCH);
    const surface = (name: RegExp) =>
      screen.getAllByRole("button", { name }).find((row) => row.classList.contains("r"))!;
    await user.click(surface(/^会话/));
    await user.click(await screen.findByText("上礼拜那段对话"));
    await user.click(surface(/^对话/));
    const back = (await screen.findByRole("textbox")) as HTMLTextAreaElement;
    await waitFor(() => expect(back.placeholder).toMatch(/排队/), { timeout: 4_000 });

    // And *that* Turn ends, cleanly. The end of a Turn over here is not the cue
    // for a sentence typed over there: the second sentence comes back to the
    // box instead of being said in a conversation it was never meant for.
    bridge.elsewhere.ends("succeeded", OLDER_BRANCH);
    await waitFor(() => expect(back.value).toContain("第二句"), { timeout: 4_000 });
    expect(bridge.sessionContinue).toHaveBeenCalledTimes(1);
    expect(queue()).toBeNull();
    // Longer than the default: this one waits on four polls in a row, and the
    // window reads the runtime every 1.2 seconds.
  }, 20_000);

  it("takes one back out of the queue and into the box", async () => {
    const { user, box } = await openRunningChat();
    await say(user, box, "第一句");
    await say(user, box, "第二句");

    await user.click(screen.getByRole("button", { name: /第一句/ }));
    await waitFor(() => expect(box.value).toContain("第一句"));
    expect(queued().join("")).not.toContain("第一句");
    expect(queued().join("")).toContain("第二句");
  });

  /// Steering did not go away, it stopped being what Enter does.
  it("still offers the steer, on a key and a control of its own", async () => {
    const { user, bridge, box } = await openRunningChat();
    await user.click(box);
    await user.type(box, "别改了，先看看日志");
    await user.keyboard("{Meta>}{Enter}{/Meta}");

    await waitFor(() => expect(bridge.steer).toHaveBeenCalled());
    expect(bridge.steer.mock.calls[0][0].input).toBe("别改了，先看看日志");
    expect(bridge.steer.mock.calls[0][0].runId).toBe(RUN_LIVE);
    expect(queue()).toBeNull();
  });
});

/// The window every ending passes through.
///
/// A poll reads the run list before it reads the session heads (`store.ts`,
/// `load()`), so `active_run_id` clearing is always seen *before* the Run's own
/// lifecycle is seen turning terminal. The drain predicate read the ending off
/// the older of those two samples and treated "not terminal yet" the same as
/// "did not end well" -- so on the ordinary happy path the queue handed itself
/// back and said the Turn had not finished properly.
///
/// The existing guards all used `ends()`, which clears the head and settles the
/// Run in one go. Nothing was wrong with those guards; the fake simply had no
/// way to express the interleaving that actually happens.
describe("the window between the head clearing and the Run settling", () => {
  it("waits for the ending instead of calling it a bad one", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "排队的那句话");
    await waitFor(() => expect(queued()).toHaveLength(1));

    // The head says the branch is free; the run list has not caught up.
    const runId = bridge.elsewhere.endsOnTheHeadFirst();
    // Long enough for the poll that sees the cleared head, and then some.
    await new Promise((settle) => setTimeout(settle, 2_600));

    expect(queued()).toHaveLength(1);
    expect(said()).not.toMatch(/没有正常结束/);
    expect(bridge.sessionContinue).not.toHaveBeenCalled();

    // And when it does catch up, the queue goes.
    bridge.elsewhere.settles(runId!, "succeeded");
    await waitFor(
      () => expect(bridge.sessionContinue).toHaveBeenCalledTimes(1), { timeout: 6_000 },
    );
    expect(queued()).toHaveLength(0);
  });

  it("still hands back once the ending it was waiting for turns out to be a bad one", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "排队的那句话");
    await waitFor(() => expect(queued()).toHaveLength(1));

    const runId = bridge.elsewhere.endsOnTheHeadFirst();
    bridge.elsewhere.settles(runId!, "cancelled");

    // Cancelled is the case the whole hand-back exists for: firing the next
    // sentence into a conversation somebody just stopped is worse than having
    // no queue at all.
    await waitFor(() => expect(said()).toMatch(/没有正常结束/), { timeout: 6_000 });
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toContain("排队的那句话");
  });
});

/// The button beside the box does what its label says.
///
/// This was the round's central split -- a queue and a steer are two
/// intentions, so they get two controls -- and nothing tested the button half
/// of it. Swapping its handler from `send(true)` to `send()`, which reverses
/// what it means, left all 362 tests green: the only assertion touching it
/// checked that *a button named 改向 exists*, which had been true since before
/// the button did anything.
describe("the 改向 button beside the box", () => {
  it("steers the running Turn and does not queue anything", async () => {
    const { user, bridge, box } = await openRunningChat();
    await user.click(box);
    await user.type(box, "换个方向");
    await user.click(screen.getByRole("button", { name: "改向" }));

    await waitFor(() => expect(bridge.steer).toHaveBeenCalledTimes(1));
    expect(bridge.steer.mock.calls[0][0].input).toBe("换个方向");
    // The two intentions stay apart: steering leaves nothing waiting to be
    // sent when the Turn ends.
    expect(queue()).toBeNull();
    expect(bridge.sessionContinue).not.toHaveBeenCalled();
  });

  it("is the only control that steers -- the primary one queues", async () => {
    const { user, bridge, box } = await openRunningChat();
    await say(user, box, "这句是排队的");
    await waitFor(() => expect(queued()).toHaveLength(1));
    expect(bridge.steer).not.toHaveBeenCalled();
  });
});

