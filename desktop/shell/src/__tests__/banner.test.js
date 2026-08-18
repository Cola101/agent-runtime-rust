/// What this file is for.
///
/// The banner is the only part of this feature a person reads. It arrives when
/// they are not looking at the app, it is three or four words long, and if
/// those words are wrong there is nothing else on screen to correct them —
/// which is exactly the kind of mistake a green suite hides. This file reads
/// them back.
///
/// The strongest check here is the last one: the banner's words are the
/// surface's words, taken from the same table the queue renders from. Two
/// wordings for one state is two names for one thing, and the drift starts the
/// day someone edits one of them.
///
/// Plain JavaScript on purpose — the module under test is the CommonJS one
/// main.cjs actually requires, not a TypeScript copy of it.
import { describe, expect, it } from "vitest";
import { bannerText } from "../../electron/banner.cjs";
import { lifecycleLabel } from "../surfaces/model";
import { RUN_UNJUDGED, RUN_WAITING } from "./fake-runtime";

const ASKED = {
  kind: "approval", key: `approval:x`, runId: RUN_WAITING, toolName: "shell.exec",
};
const UNJUDGED = { kind: "indeterminate", key: `run:${RUN_UNJUDGED}`, runId: RUN_UNJUDGED };

describe("what the banner says", () => {
  it("names the tool the runtime stopped on, and the Run it stopped in", () => {
    const words = bannerText(ASKED);
    expect(words.title).toBe("等你决定");
    // The tool name verbatim: it is the runtime's word, and a person deciding
    // whether to allow `shell.exec` is deciding about that name.
    expect(words.body).toContain("shell.exec");
    // Cut where the rest of this client cuts a Run id, so the banner and the
    // screen it lands on show the same eight characters.
    expect(words.body).toContain(RUN_WAITING.slice(0, 8));
  });

  it("says a Run nobody can judge is that, and names no tool", () => {
    const words = bannerText(UNJUDGED);
    expect(words.title).toBe("结果无法判定");
    expect(words.body).toContain(RUN_UNJUDGED.slice(0, 8));
    // There is no call here to name. A tool name on this banner would be the
    // host inventing one.
    expect(words.body).not.toContain("shell.exec");
  });

  it("still calls an approval an approval when the log carried no tool name", () => {
    const words = bannerText({ ...ASKED, toolName: "" });
    // The person is still the only way forward, so it is still 等你决定. The
    // old code decided the kind by whether this string was empty and told them
    // the result could not be judged — about a Run that was asking them a
    // question.
    expect(words.title).toBe("等你决定");
    expect(words.body).toContain(RUN_WAITING.slice(0, 8));
  });

  it("counts what it could not name one at a time", () => {
    const words = bannerText({ kind: "several", count: 6, runId: RUN_WAITING });
    expect(words.title).toBe("等你决定");
    expect(words.body).toContain("6");
  });

  it("says nothing at all about a kind this build does not know", () => {
    // A blank banner is still an interruption, and a guessed one is a lie.
    expect(bannerText({ kind: "something-newer", runId: RUN_WAITING })).toBeNull();
    expect(bannerText(undefined)).toBeNull();
  });

  it("uses the same words for a state as the screen it lands on", () => {
    // `lifecycleLabel` is what the queue writes over each card. The banner is
    // the same sentence arriving early, not a second vocabulary for it.
    expect(bannerText(ASKED).title).toBe(lifecycleLabel({ kind: "waiting_approval" }));
    expect(bannerText(UNJUDGED).title)
      .toBe(lifecycleLabel({ kind: "terminal", status: "indeterminate" }));
  });
});
