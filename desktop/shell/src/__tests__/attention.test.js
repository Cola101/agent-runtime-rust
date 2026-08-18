/// What this file is for.
///
/// The window reports the same parked Run about once a second, for as long as
/// it stays parked. This is the rule that turns that stream into one banner,
/// and every case below is one a person would feel: an app that nags about a
/// decision they already saw, or one that stays quiet while a Run is stuck.
///
/// Plain JavaScript on purpose — the host process is plain CommonJS, and the
/// module under test is the one main.cjs actually requires rather than a
/// TypeScript copy of it.
import { beforeEach, describe, expect, it } from "vitest";
import { createAttention } from "../../electron/attention.cjs";

const RUN = "01a0122b-217e-7e72-bec8-ad3273f16cd1";
const ASKED = { kind: "approval", key: `approval:${RUN}-1`, runId: RUN, toolName: "shell.exec" };
const ASKED_AGAIN =
  { kind: "approval", key: `approval:${RUN}-2`, runId: RUN, toolName: "shell.exec" };
const UNJUDGED = { kind: "indeterminate", key: `run:${RUN}`, runId: RUN };

/// A queue of `count` distinct parked Runs, as a fresh process would first see
/// it: nothing known yet, everything new.
function queue(count) {
  return Array.from({ length: count }, (_, at) => ({
    kind: "approval",
    key: `approval:${RUN}-q${at}`,
    runId: `${at}`.padStart(8, "0"),
    toolName: "shell.exec",
  }));
}

const AWAY = false;
const WATCHING = true;

let attention;
beforeEach(() => {
  attention = createAttention();
});

describe("the host decides what is worth interrupting someone for", () => {
  it("raises a parked Run once, however many polls report it", () => {
    expect(attention.arrived([ASKED], AWAY)).toEqual([ASKED]);
    expect(attention.arrived([ASKED], AWAY)).toEqual([]);
    expect(attention.arrived([ASKED], AWAY)).toEqual([]);
  });

  it("keeps two questions on one Run apart", () => {
    expect(attention.arrived([ASKED], AWAY)).toEqual([ASKED]);
    expect(attention.arrived([ASKED, ASKED_AGAIN], AWAY)).toEqual([ASKED_AGAIN]);
  });

  it("tells a Run nobody can judge from the approval on the same Run", () => {
    expect(attention.arrived([ASKED], AWAY)).toEqual([ASKED]);
    expect(attention.arrived([UNJUDGED], AWAY)).toEqual([UNJUDGED]);
  });

  it("stays quiet about what appeared while the person was watching", () => {
    expect(attention.arrived([ASKED], WATCHING)).toEqual([]);
    // Still quiet once they walk away: the window already showed them this,
    // and a banner now would be about nothing that has changed.
    expect(attention.arrived([ASKED], AWAY)).toEqual([]);
  });

  it("does not raise again what one failed read dropped from a report", () => {
    expect(attention.arrived([ASKED], AWAY)).toEqual([ASKED]);
    // A Run whose log could not be read this pass is reported as waiting on
    // nobody, and is back in the next report unanswered.
    expect(attention.arrived([], AWAY)).toEqual([]);
    expect(attention.arrived([ASKED], AWAY)).toEqual([]);
  });

  it("says nothing about an item with no durable name", () => {
    // Nameless means undeduplicable, which would be one banner per poll.
    expect(attention.arrived([{ runId: RUN, toolName: "shell.exec" }], AWAY)).toEqual([]);
    expect(attention.arrived([{ key: "", runId: RUN }], AWAY)).toEqual([]);
  });
});

/// Opening the app onto a queue that piled up while it was closed.
///
/// This is not a rare case, it is the ordinary one: `known` is empty in a
/// fresh process, so the very first report is entirely new. Unbounded, a
/// person who left eight Runs parked overnight gets eight banners in the same
/// second — and on macOS the later ones shove the earlier ones out of the
/// corner before anyone has read them.
describe("a queue that piled up before the app opened", () => {
  it("does not hand a person one banner per parked Run", () => {
    const raised = attention.arrived(queue(8), AWAY);
    expect(raised.length).toBeLessThanOrEqual(3);
  });

  it("still says how many it did not name, rather than swallowing them", () => {
    const raised = attention.arrived(queue(8), AWAY);
    const several = raised[raised.length - 1];
    expect(several.kind).toBe("several");
    // Every one of the eight is accounted for: named, or counted.
    const named = raised.filter((banner) => banner.kind !== "several").length;
    expect(named + several.count).toBe(8);
    // And it lands somewhere: the first of the ones it stands for.
    expect(several.runId).toBe(queue(8)[named].runId);
  });

  it("names them all when there are few enough to name", () => {
    expect(attention.arrived(queue(3), AWAY)).toEqual(queue(3));
  });

  it("does not repeat the burst on the next poll", () => {
    expect(attention.arrived(queue(8), AWAY).length).toBeGreaterThan(0);
    // The same eight are still parked and still reported. They have been said.
    expect(attention.arrived(queue(8), AWAY)).toEqual([]);
  });
});
