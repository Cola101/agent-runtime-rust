/// What this file is for.
///
/// The reference page is only worth having if it is generated from the same
/// declarations the shell dispatches. A hand-written cheatsheet would pass a
/// test that reads the page and finds "j 下一个" in it, and would still be
/// wrong the day someone rebinds j.
///
/// So these tests never ask whether something plausible is on the page. They
/// compare the rows against the declarations as lists — in order, key and hint
/// together — so an invented row fails as loudly as a missing one; they press
/// the chord that is printed; and they drive the runtime state a row claims to
/// be reporting. Containment in one direction is what let this page grow a
/// row nobody declared, which is the drift the registry exists to prevent.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { all, commands } from "../surfaces/registry";
import { DRAWER_KEY, SHELL_KEYS, printedKey } from "../shell-keys";
import { installFakeRuntime, RUN_LIVE } from "./fake-runtime";

async function open(surface: string, options?: Parameters<typeof installFakeRuntime>[0]) {
  const user = userEvent.setup();
  const bridge = installFakeRuntime(options);
  render(<App />);
  await waitFor(() => expect(screen.getByRole("button", { name: /对话/ })).toBeTruthy());
  await waitFor(() => expect(bridge.desk.runtime.list).toBeDefined());
  await rail(user, surface);
  return { user, bridge };
}

async function rail(user: ReturnType<typeof userEvent.setup>, surface: string) {
  await user.click(
    screen.getAllByRole("button", { name: new RegExp(`^${surface}`) })
      .find((node) => node.classList.contains("r"))!,
  );
}

/// The block under one heading. Scoping every assertion to it is deliberate:
/// a surface's label and a key's hint both appear elsewhere on the screen, and
/// a test that matched them anywhere would pass with the section missing.
function section(heading: string): HTMLElement {
  const found = [...document.querySelectorAll<HTMLElement>(".keyref section")]
    .find((node) => node.querySelector("h3")?.textContent === heading);
  if (!found) throw new Error(`the reference has no "${heading}" section`);
  return found;
}

function group(label: string): HTMLElement {
  const found = [...document.querySelectorAll<HTMLElement>(".keyref .kface")]
    .find((node) => node.querySelector("h4")?.textContent === label);
  if (!found) throw new Error(`the reference has no group for "${label}"`);
  return found;
}

function row(scope: HTMLElement, what: string): HTMLElement {
  const found = [...scope.querySelectorAll<HTMLElement>(".krow, .crow")]
    .find((node) => node.querySelector(".what")?.textContent === what);
  if (!found) throw new Error(`no row for "${what}"`);
  return found;
}

/// Every key row in a block, in the order it is printed, as the two things the
/// row claims together: the chord, and what it says that chord does. Compared
/// as a whole list against the declarations, so a wrong key, a wrong hint, a
/// row in the wrong place, a missing row and an invented row all fail.
function printed(scope: HTMLElement): string[] {
  return [...scope.querySelectorAll<HTMLElement>(".krow")].map((node) => {
    const chord = node.querySelector("kbd")?.textContent;
    const what = node.querySelector(".what")?.textContent;
    // Neither: the line a surface with nothing to declare prints instead.
    return chord == null || what == null ? node.textContent ?? "" : `${chord}\t${what}`;
  });
}

/// What the row says about itself right now, in the words on screen.
function marked(node: HTMLElement): boolean {
  return node.querySelector(".no")?.textContent === "现在不生效";
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("the reference is generated, not written", () => {
  it("prints exactly the keys a surface declares, under that surface", async () => {
    await open("键位");
    for (const surface of all()) {
      const keys = surface.keys ?? [];
      expect(printed(group(surface.label)), surface.id).toEqual(
        keys.length > 0
          ? keys.map((key) => `${printedKey(key.key)}\t${key.hint}`)
          // A surface with nothing to declare says so, rather than being left
          // out and read as an oversight.
          : ["这个面没有声明键位"],
      );
    }
  });

  it("prints exactly the keys the shell claims, with the chord that dispatches each", async () => {
    await open("键位");
    expect(printed(section("外壳")))
      .toEqual(SHELL_KEYS.map((key) => `${key.chord}\t${key.hint}`));
  });

  it("names every surface ⌘I works on and no surface it does not, and the key agrees", async () => {
    const { user } = await open("键位");
    const said = row(section("外壳"), DRAWER_KEY.hint).querySelector(".where")?.textContent ?? "";

    // The line is a list, so it is read as one. `includes` on the whole string
    // is what this guard did first, and it reported the conversation surface as
    // having a drawer for the whole time the process surface existed: `会话` is
    // inside `进程会话`. A label that is a prefix of another label is ordinary,
    // and a substring test cannot tell the two apart.
    const named = new Set(
      said.replace("只有这些面有：", "").split("、").map((item) => item.split("（")[0]),
    );

    // Checked in both directions from the registry. A line that named every
    // surface contains every label a correct line contains, so looking only
    // for the surfaces that do have a drawer would pass on it — which is how
    // the scope line could over-claim with every guard green.
    expect(all().some((surface) => surface.drawer)).toBe(true);
    expect(all().some((surface) => !surface.drawer)).toBe(true);
    for (const surface of all()) {
      expect(named.has(surface.label), `${surface.id} in "${said}"`)
        .toBe(Boolean(surface.drawer));
    }

    // And the line is a claim about the key, not about the page: press ⌘I on
    // every surface and a drawer appears exactly where the line said it would.
    for (const surface of all()) {
      await rail(user, surface.label);
      await user.keyboard("{Meta>}i{/Meta}");
      expect(Boolean(document.querySelector("aside.drawer")), `⌘I on ${surface.id}`)
        .toBe(named.has(surface.label));
      // The drawer is window state, not surface state; leave it as found.
      await user.keyboard("{Meta>}i{/Meta}");
    }
  });

  it("prints exactly the commands the palette can run", async () => {
    await open("键位");
    const list = section("命令");
    expect([...list.querySelectorAll<HTMLElement>(".crow")]
      .map((node) => node.querySelector(".what")?.textContent))
      .toEqual(commands().map((command) => command.title));
    for (const command of commands()) {
      expect(row(list, command.title).querySelector(".s")?.textContent)
        .toBe(command.surfaceLabel);
    }
  });

  it("has no binding whose key prints as nothing", async () => {
    // The status line and this page print through the same `printedKey`, so a
    // key with no visible character of its own — space — reads the same in both
    // rather than as an empty <kbd> in either. That one table is all that
    // stands between a binding and an unreadable hint, so what is checked here
    // is the printed form, not the raw `KeyboardEvent.key` behind it.
    for (const surface of all()) {
      for (const key of surface.keys ?? []) {
        const shown = printedKey(key.key);
        expect(shown, `${surface.id} 的键位印不出来`).toBe(shown.trim());
        expect(shown, `${surface.id} 声明了一个印不出来的键位`).not.toBe("");
      }
    }
  });
});

describe("what the reference says about a key right now", () => {
  it("marks a binding whose own condition does not hold, and unmarks it when it does", async () => {
    const { user } = await open("键位");
    // Nothing is selected yet, so the digits on the queue have no run to act
    // on and the row says so.
    expect(marked(row(group("待决定"), "执行"))).toBe(true);
    expect(marked(row(group("Run"), "看转录"))).toBe(true);
    // j/k on the run list only need runs, and there are three.
    expect(marked(row(group("Run"), "下一个"))).toBe(false);

    await rail(user, "待决定");
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await rail(user, "键位");

    expect(marked(row(group("待决定"), "执行"))).toBe(false);
    expect(marked(row(group("Run"), "看转录"))).toBe(false);
  });

  it("marks a command whose own condition does not hold, and leaves an unconditional one alone", async () => {
    // The open conversation's own Run is running, which is what makes 停止
    // something you can do right now. Said explicitly rather than left to the
    // fixture's default: the command asks about the Run on screen, and the
    // default conversation has none running -- in which case the reference is
    // right to mark it, and this test would be asserting the opposite.
    const { user } = await open("键位", { activeRunId: RUN_LIVE });
    expect(marked(row(section("命令"), "停止当前 Run"))).toBe(false);
    // A command that declares no condition is never in doubt.
    expect(marked(row(section("命令"), "回到对话"))).toBe(false);

    await rail(user, "待决定");
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await rail(user, "键位");

    // The cursor is now on a run parked on an approval. Nothing is running,
    // the palette drops the command entirely, and the reference has to say so
    // rather than printing it as available.
    expect(marked(row(section("命令"), "停止当前 Run"))).toBe(true);
    expect(marked(row(section("命令"), "回到对话"))).toBe(false);
  });

  it("says a surface key also needs the caret out of a text box, and it does", async () => {
    // The condition is two things, not one. The shell drops a bare key while
    // something is being typed, and the one surface people spend their time on
    // is the surface with a composer — so a page that said only "while that
    // surface is showing" would be wrong exactly where it is read.
    const { user, bridge } = await open("键位");
    expect(section("各个面").querySelector(".sub")?.textContent).toContain("输入框");

    // Park the cursor on the run that is asking, so the binding's own `when`
    // holds and the caret is the only thing standing between the key and a
    // decision the runtime would carry out.
    await rail(user, "待决定");
    await user.click(screen.getAllByText(/shell\.exec/)[0]);
    await rail(user, "对话");
    const key = (all().find((surface) => surface.id === "chat")?.keys ?? [])[0];
    expect(key, "对话 declares no keys for this to be about").toBeTruthy();
    await rail(user, "键位");
    expect(marked(row(group("对话"), key.hint))).toBe(false);

    await rail(user, "对话");
    // The composer specifically. The gate drawn in the transcript has a text
    // box of its own now -- the one a refusal is explained in -- and "the only
    // text box on this surface" stopped being true the moment it landed.
    const box = await screen.findByPlaceholderText(/接着说|说一句话/);
    await user.click(box);
    await user.keyboard(key.key);
    expect(bridge.control).not.toHaveBeenCalled();
    expect((box as HTMLTextAreaElement).value).toBe(key.key);
  });
});

describe("the key that opens it", () => {
  // Why the chord is ⌘ and not the usual bare `?`: with the caret in the
  // composer a bare key belongs to the text box, and the one surface people
  // spend their time on is the surface with a composer.
  it("answers with the caret in the composer, and swallows the key", async () => {
    const { user } = await open("对话");
    const box = await screen.findByRole("textbox");
    await user.click(box);

    // Dispatched by hand rather than through user-event because the assertion
    // is about the event object: the key has to be swallowed, or in a browser
    // the character reaches the text box as well. Asserting the textarea
    // stayed empty would prove nothing — jsdom inserts nothing while a
    // modifier is held either way.
    const press = new KeyboardEvent("keydown", {
      key: "/", metaKey: true, bubbles: true, cancelable: true,
    });
    box.dispatchEvent(press);
    expect(press.defaultPrevented).toBe(true);
    await waitFor(() => expect(document.querySelector(".keyref")).toBeTruthy());
  });

  it("puts you back where you were when pressed again", async () => {
    const { user } = await open("Run");
    await user.keyboard("{Meta>}/{/Meta}");
    await waitFor(() => expect(document.querySelector(".keyref")).toBeTruthy());
    await user.keyboard("{Meta>}/{/Meta}");
    await waitFor(() => expect(document.querySelector(".keyref")).toBeNull());
    // The run list, not the surface the window opened on. Asserted by the
    // column only that surface draws: counting rows made this a test about how
    // many Runs the fixture happens to hold, and it broke the moment another
    // branch added one.
    await waitFor(() =>
      expect(screen.getByRole("columnheader", { name: "问的是" })).toBeTruthy());
  });

  it("is reachable from the palette as well", async () => {
    const { user } = await open("对话");
    await user.keyboard("{Meta>}k{/Meta}");
    const input = await screen.findByPlaceholderText("输入命令");
    await user.type(input, "键位");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(document.querySelector(".keyref")).toBeTruthy());
  });
});
