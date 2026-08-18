// @vitest-environment node
/// What this file is for.
///
/// This is the only piece of the client that computes something instead of
/// reading it, and it computes it for a screen where someone is deciding
/// whether to let a file be overwritten. A diff that is subtly wrong there is
/// worse than none: it would be wrong with authority.
import { describe, expect, it } from "vitest";
import { changes, hunks, MAX_LINES } from "../diff";

const kinds = (before: string, after: string) =>
  changes(before, after)!.map((change) => `${change.kind[0]}:${change.text}`);

describe("what one write would change", () => {
  it("keeps what is the same and names what moves in and out", () => {
    expect(kinds("one\ntwo\nthree", "one\ntwo point five\nthree")).toEqual([
      "s:one", "d:two", "a:two point five", "s:three",
    ]);
  });

  it("reads an append as an append rather than as a rewrite", () => {
    expect(kinds("one\ntwo", "one\ntwo\nthree")).toEqual(["s:one", "s:two", "a:three"]);
  });

  it("reads a deletion as a deletion", () => {
    expect(kinds("one\ntwo\nthree", "one\nthree")).toEqual(["s:one", "d:two", "s:three"]);
  });

  it("says nothing changed when nothing did", () => {
    expect(changes("same\n", "same\n")!.every((change) => change.kind === "same")).toBe(true);
  });

  /// Refused rather than truncated. Half a diff is one you cannot trust, and
  /// the person is deciding about a whole file.
  it("refuses a comparison too large to be worth trusting", () => {
    const huge = new Array(MAX_LINES + 1).fill("x").join("\n");
    expect(changes(huge, "x")).toBeNull();
    expect(changes("x", huge)).toBeNull();
  });
});

describe("showing only what is near a change", () => {
  it("drops a long unchanged run and says how much it dropped", () => {
    const before = ["a", ...new Array(20).fill("same"), "b"].join("\n");
    const after = ["A", ...new Array(20).fill("same"), "b"].join("\n");
    const shown = hunks(changes(before, after)!);
    const gap = shown.find((hunk) => hunk.skipped > 0);
    expect(gap).toBeTruthy();
    expect(gap!.skipped).toBeGreaterThan(0);
    // The changed line survives, which is the point of dropping the rest.
    const kept = shown.flatMap((hunk) => hunk.changes).map((change) => change.text);
    expect(kept).toContain("A");
    expect(kept).toContain("a");
  });

  it("keeps a short file whole rather than gapping it", () => {
    const shown = hunks(changes("one\ntwo", "one\nTWO")!);
    expect(shown.every((hunk) => hunk.skipped === 0)).toBe(true);
  });
});
