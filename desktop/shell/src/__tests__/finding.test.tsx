/// What this file is for.
///
/// A transcript is the one surface here that grows without bound, and until now
/// the only way to find a sentence in one was to scroll. ⌘F searches what the
/// column draws -- what was said, what came back, and the tool calls in between
/// -- and every test here is about the difference between finding text and
/// claiming to have found it.
///
/// The one that matters most is the fold. A run of tool calls is drawn as a
/// single closed row, so a match inside one is a match nobody can see, and a
/// count that included it would be a number about a screen that does not exist.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

const marks = () => [...document.querySelectorAll("mark")];
const tally = () => document.querySelector(".find .tally")?.textContent ?? "";
const onAt = () => marks().findIndex((mark) => mark.classList.contains("on"));

/// A live Run on screen, so the transcript is the column being searched.
async function openTranscript(
  options?: { gap?: boolean; capped?: boolean; unreadable?: string | null },
) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime({ activeRunId: RUN_LIVE, ...options });
  render(<App />);
  await waitFor(() => expect(bridge.watch).toHaveBeenCalled());
  return { user, bridge };
}

/// The parked Run's transcript, which is the one with a decision drawn at the
/// foot of it. Selecting it first is what makes the transcript that Run's
/// rather than the live one's.
async function openParkedTranscript() {
  const user = userEvent.setup();
  const bridge = installFakeRuntime();
  render(<App />);
  const rail = (name: RegExp) =>
    screen.getAllByRole("button", { name }).find((node) => node.classList.contains("r"))!;
  await waitFor(() => expect(rail(/^对话/)).toBeTruthy());
  await user.click(rail(/^待决定/));
  await waitFor(() => expect(screen.getAllByText(/等你决定/).length).toBeGreaterThan(0));
  await user.click(screen.getAllByText(/shell\.exec/)[0]);
  await user.click(rail(/^对话/));
  await screen.findByText(/只对这一次调用有效/);
  return { user, bridge };
}

/// The three calls the fold tests search: two share a name, one is alone, and
/// the path only exists inside the arguments of the middle one.
function threeCalls(bridge: ReturnType<typeof installFakeRuntime>) {
  [
    { name: "shell.exec", arguments: { command: "ls" } },
    { name: "workspace.read_text", arguments: { path: "docs/adr/0145.md" } },
    { name: "shell.exec", arguments: { command: "cat notes.txt" } },
  ].forEach((call, index) => {
    bridge.emit(RUN_LIVE, bridge.event(20 + index, "model.tool_call", { ...call, id: `c${index}` }, 30));
  });
}

async function find(user: ReturnType<typeof userEvent.setup>, query: string) {
  await user.keyboard("{Meta>}f{/Meta}");
  const box = await screen.findByPlaceholderText("在这段对话里找");
  await user.type(box, query);
  return box;
}

describe("finding text in a transcript", () => {
  it("marks every match in the reply and counts them", async () => {
    const { user, bridge } = await openTranscript();
    bridge.emit(RUN_LIVE, bridge.event(20, "model.output.delta", {
      text: "先读 notes.txt，改完再写回 notes.txt",
    }, 30));
    await waitFor(() => expect(screen.getByText(/先读/)).toBeTruthy());

    await find(user, "notes");
    await waitFor(() => expect(marks().length).toBe(2));
    expect(marks().map((mark) => mark.textContent)).toEqual(["notes", "notes"]);
    // Exactly this, with nothing appended: this Run's log is whole, so there is
    // no caveat to put after the count.
    expect(tally()).toBe("1/2");
  });

  it("opens a fold that is hiding a match in a call's arguments", async () => {
    const { user, bridge } = await openTranscript();
    threeCalls(bridge);
    const row = await screen.findByText("3 个工具调用");
    expect(row.closest("button")?.getAttribute("aria-expanded")).toBe("false");
    // Folded, so the path being searched for is genuinely not on screen yet.
    expect(screen.queryByText(/0145/)).toBeNull();

    await find(user, "0145");
    // The fold opened because it holds the match, and the match is drawn inside
    // it. Either half alone would be a lie: an open fold with nothing marked,
    // or a count of one over a line still hidden behind a caret.
    await waitFor(() => expect(row.closest("button")?.getAttribute("aria-expanded")).toBe("true"));
    expect(marks().map((mark) => mark.textContent)).toEqual(["0145"]);
    expect(marks()[0].closest(".acts")).toBeTruthy();
    expect(tally()).toBe("1/1");
  });

  it("opens a fold that is hiding a match in a call's name", async () => {
    const { user, bridge } = await openTranscript();
    threeCalls(bridge);
    const row = (await screen.findByText("3 个工具调用")).closest("button")!;
    // The name is drawn twice over: once in the summary the fold shows while
    // it is shut, and once on the line inside it. The line is the one a person
    // can read the call from, and it is the one that has to be found -- a fold
    // that stayed shut over its own tool's name would answer "没有匹配" with
    // the word sitting on the row it drew.
    expect(row.textContent).toContain("workspace.read_text");
    expect(row.getAttribute("aria-expanded")).toBe("false");

    await find(user, "workspace.read_text");
    await waitFor(() => expect(row.getAttribute("aria-expanded")).toBe("true"));
    expect(marks().map((mark) => mark.textContent)).toEqual(["workspace.read_text"]);
    expect(marks()[0].closest(".act")).toBeTruthy();
    expect(tally()).toBe("1/1");
  });

  it("counts the calls in a fold, and not the summary that tallies them", async () => {
    const { user, bridge } = await openTranscript();
    threeCalls(bridge);
    const row = (await screen.findByText("3 个工具调用")).closest("button")!;
    // The summary says the name and how many, so the string being searched is
    // drawn there as well as on the two lines.
    expect(row.textContent).toContain("shell.exec ×2");

    await find(user, "shell.exec");
    await waitFor(() => expect(row.getAttribute("aria-expanded")).toBe("true"));
    // Two: one per call. Marking the summary too would make it three, and the
    // third would be a hit on a count of the other two rather than on anything
    // the Run did -- stepping through the matches would visit the same fold
    // twice for one call.
    expect(marks().length).toBe(2);
    expect(marks().every((mark) => mark.closest(".act"))).toBe(true);
    expect(row.querySelectorAll("mark").length).toBe(0);
    expect(tally()).toBe("1/2");
  });

  it("counts the decision the column is drawing under the transcript", async () => {
    const { user } = await openParkedTranscript();
    // The card is in the searched column, and the command it is asking about
    // is the most searchable thing on the screen. A count that quietly left it
    // out would be a "1/1" over two of the same word.
    await find(user, "shell.exec");
    await waitFor(() => expect(marks().length).toBe(2));
    expect(marks()[0].closest(".act")).toBeTruthy();
    expect(marks()[1].closest(".gate")).toBeTruthy();
    expect(document.querySelectorAll(".gate .cmd mark").length).toBe(1);
    expect(tally()).toBe("1/2");
  });

  it("finds the words in a note about the log, which are words on the screen", async () => {
    const { user } = await openTranscript({ gap: true, capped: true });
    // Both notes are up: one is this client's paging ceiling, the other is the
    // runtime saying the earlier events are gone. Neither is read off an
    // event's payload, and the finder has no way to tell -- they are notes in
    // the column like any other, and a note outside `Mark` is a note the count
    // denies.
    await screen.findByText(/更早的事件已被回收/);

    await find(user, "读到");
    await waitFor(() => expect(marks().length).toBe(2));
    expect(marks()[0].closest(".note")?.textContent).toContain("事件太多");
    expect(marks()[1].closest(".note")?.textContent).toContain("已被回收");
    expect(tally()).toBe("1/2・只在读到的这段里找");
  });

  it("says a count over a log it stopped paging is a count over what it read", async () => {
    const { user, bridge } = await openTranscript({ capped: true });
    // Nothing is missing from the middle of this one: the runtime reported no
    // gap. The log is simply longer than this client walked, which is the
    // other way a transcript is partial and the only one `truncated` carries.
    // The sequence is past the ceiling because that is where the stream is:
    // ahead of the page the reader stopped on.
    bridge.emit(RUN_LIVE, bridge.event(4000, "model.output.delta", { text: "还能看到这一句" }, 30));
    await screen.findByText(/还能看到这一句/);

    await find(user, "还能看到这一句");
    await waitFor(() => expect(tally()).toBe("1/1・只在读到的这段里找"));
  });

  it("finds what a delegation was asked, which the column draws too", async () => {
    const { user, bridge } = await openTranscript();
    const child = "01a01300-0000-7000-8000-00000000000a";
    bridge.emit(RUN_LIVE, bridge.event(20, "subagent.spawn.requested", {
      status: "running",
      request: {
        tool_call_id: "call-1", delegation_id: child,
        role: "reviewer", input: "把 retention 那段读一遍", mode: "async",
      },
    }, 30));
    bridge.emit(RUN_LIVE, bridge.event(21, "subagent.spawned", {
      agent_id: child, role: "reviewer", status: "running",
    }, 30));
    await screen.findByText("把 retention 那段读一遍");

    await find(user, "retention");
    await waitFor(() => expect(marks().length).toBe(1));
    expect(marks()[0].closest(".kid-ask")).toBeTruthy();
    expect(tally()).toBe("1/1");
  });

  it("finds the reason a log would not read", async () => {
    const { user } = await openTranscript({ unreadable: RUN_LIVE });
    await screen.findByText(/日志读不出来/);

    // The finder opens over this column like any other, and the code is the
    // one string a person searching a transcript that failed to load is
    // looking for.
    await find(user, "not_found");
    await waitFor(() => expect(marks().length).toBe(1));
    expect(marks()[0].closest(".offline")).toBeTruthy();
    expect(tally()).toBe("1/1");
  });

  it("steps through the matches with Enter, one at a time", async () => {
    const { user, bridge } = await openTranscript();
    bridge.emit(RUN_LIVE, bridge.event(20, "model.output.delta", {
      text: "先读 notes.txt，改完再写回 notes.txt",
    }, 30));
    await waitFor(() => expect(screen.getByText(/先读/)).toBeTruthy());

    await find(user, "notes");
    await waitFor(() => expect(onAt()).toBe(0));
    await user.keyboard("{Enter}");
    await waitFor(() => expect(onAt()).toBe(1));
    // One filled mark, or "which one am I on" has two answers.
    expect(document.querySelectorAll("mark.on").length).toBe(1);
    expect(tally()).toBe("2/2");
    // Past the end is the first one again, rather than a step that does
    // nothing and a person pressing Enter harder.
    await user.keyboard("{Enter}");
    await waitFor(() => expect(tally()).toBe("1/2"));
    await user.keyboard("{Shift>}{Enter}{/Shift}");
    await waitFor(() => expect(tally()).toBe("2/2"));
  });

  it("opens from the composer without typing an f into it", async () => {
    const { user } = await openTranscript();
    const composer = await screen.findByRole("textbox");
    await user.click(composer);
    await user.keyboard("{Meta>}f{/Meta}");

    const box = await screen.findByPlaceholderText("在这段对话里找");
    expect(document.activeElement).toBe(box);
    expect((composer as HTMLTextAreaElement).value).toBe("");

    // Esc gives the keyboard back to the box the person was writing in.
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByPlaceholderText("在这段对话里找")).toBeNull());
    expect(document.activeElement).toBe(composer);
  });

  it("finds what was said in a Turn that has already committed", async () => {
    const user = userEvent.setup();
    installFakeRuntime();
    render(<App />);
    // No Run in flight: the column is the Session's frozen transcript, which is
    // all that is left of a conversation once its logs are retired. A finder
    // that only searched the live Run would go blind exactly then.
    await waitFor(() => expect(screen.getByText("我叫小林，请记住")).toBeTruthy());

    await find(user, "小林");
    await waitFor(() => expect(marks().length).toBe(2));
    expect(marks()[0].closest(".ask")).toBeTruthy();
    expect(marks()[1].closest(".rep")).toBeTruthy();
    expect(tally()).toBe("1/2");
  });

  it("says a count over a cut log is a count over what it read", async () => {
    const { user, bridge } = await openTranscript({ gap: true });
    bridge.emit(RUN_LIVE, bridge.event(20, "model.output.delta", { text: "断在这一句" }, 30));
    await waitFor(() => expect(screen.getByText(/断在这一句/)).toBeTruthy());

    await find(user, "断在这一句");
    // The runtime said the earlier events are gone, so "1" is one match in the
    // part that survived. The number alone would imply the whole Run.
    await waitFor(() => expect(tally()).toBe("1/1・只在读到的这段里找"));

    await user.clear(await screen.findByPlaceholderText("在这段对话里找"));
    await user.type(await screen.findByPlaceholderText("在这段对话里找"), "写过什么");
    await waitFor(() => expect(tally()).toBe("没有匹配・只在读到的这段里找"));
  });

  it("draws the key it dispatches", async () => {
    const { user } = await openTranscript();
    const hint = await waitFor(() => {
      const found = [...document.querySelectorAll(".keys > span")]
        .find((span) => span.textContent?.includes("查找"));
      expect(found).toBeTruthy();
      return found!;
    });
    // The hint is rendered from the binding, so the modifier it draws is the
    // modifier the dispatcher requires. Pressing exactly what it says opens the
    // finder; a hint of "F" over a ⌘F binding would be a character.
    expect(hint.querySelector("kbd")?.textContent).toBe("⌘F");
    await user.keyboard("{Meta>}f{/Meta}");
    expect(await screen.findByPlaceholderText("在这段对话里找")).toBeTruthy();
  });
});
