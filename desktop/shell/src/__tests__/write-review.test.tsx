// @vitest-environment jsdom
/// What this file is for.
///
/// Approving `workspace.write_text` is approving an overwrite. The card showed
/// the whole new text and nothing about what is there now, so the one question
/// a person is actually being asked -- what changes -- had to be answered by
/// going to another surface, finding the file, reading it, and holding it in
/// their head while they came back.
///
/// The comparison is drawn from the file the runtime would write to, read
/// through the same bridge call the workspace surface uses. If it cannot be
/// read the card says so rather than showing a diff against nothing, because a
/// diff against an assumption is worse than no diff.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../App";
import { installFakeRuntime, RUN_WAITING } from "./fake-runtime";

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

/// A Run parked on a write, with whatever the file currently holds.
///
/// The approval is in the fixture's log rather than streamed, because
/// `withStreamed` keeps `run.approval` exactly as the cursor reported it -- on
/// purpose, since whether a Run is parked on a person is the cursor's to say.
function pendingWrite(existing: string | null, text: string, path = "notes.txt") {
  const bridge = installFakeRuntime({
    activeRunId: RUN_WAITING,
    pending: { id: "w-1", name: "workspace.write_text", arguments: { path, text } },
  });
  bridge.desk.runtime.readFile = async (relative: string) => (
    existing === null
      ? { ok: false as const, error: "no such file in the workspace" }
      : {
        ok: true as const,
        value: {
          path: relative, binary: false, size: existing.length,
          truncated: false, text: existing,
        },
      }
  );
  return bridge;
}

describe("approving a write", () => {
  it("shows the lines it would add and the ones it would take away", async () => {
    pendingWrite("one\ntwo\nthree\n", "one\ntwo point five\nthree\n");
    render(<App />);
    await waitFor(() => expect(screen.getByText(/\+two point five/)).toBeTruthy());
    expect(screen.getByText(/-two/)).toBeTruthy();
    // Unchanged lines are not the question and are not repeated as changes.
    expect(screen.queryByText(/\+one/)).toBeNull();
  });

  /// The same decision is asked on two surfaces. Evidence on one of them and
  /// not the other means the queue -- which is where someone goes *to* decide
  /// -- is the one asking blind.
  it("shows the same comparison in the queue, not only in the transcript", async () => {
    const user = userEvent.setup();
    pendingWrite("one\ntwo\nthree\n", "one\ntwo point five\nthree\n");
    render(<App />);
    await waitFor(() => expect(screen.getByRole("button", { name: /待决定/ })).toBeTruthy());
    await user.click(screen.getByRole("button", { name: /^待决定/ }));
    await waitFor(() => expect(screen.getByText(/\+two point five/)).toBeTruthy());
  });

  /// The arguments line is `JSON.stringify` of the call, and for a write the
  /// arguments *are* the file -- so a two-thousand-line write puts two
  /// thousand lines in one `<code>` element above the comparison that was
  /// added to make it readable. The path is what identifies the call; the
  /// content is what the diff is for.
  it("names the file being written rather than inlining all of it", async () => {
    pendingWrite("one\n", "one\ntwo\n");
    render(<App />);
    const line = await screen.findByText(/workspace\.write_text\(notes\.txt\)/);
    const gate = line.closest(".gate")!;
    expect(gate.querySelector("code.cmd")!.textContent).toBe("workspace.write_text(notes.txt)");
    // And nowhere on the screen is the whole new content printed as an
    // argument -- the transcript's own call line names the file too, because
    // the result underneath it already draws what was written.
    expect(document.body.textContent).not.toContain('"text":');
  });

  it("says a file is new rather than diffing against nothing", async () => {
    pendingWrite(null, "hello\n", "new.txt");
    render(<App />);
    await waitFor(() => expect(screen.getByText(/这个文件现在还不存在/)).toBeTruthy());
  });
});
