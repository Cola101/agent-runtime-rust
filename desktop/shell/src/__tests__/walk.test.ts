// @vitest-environment node
/// What this file is for.
///
/// The `@` completion walks the workspace, and a workspace can be a checkout
/// with a hundred thousand files under it. The bounds are the point rather
/// than a detail -- an unbounded walk spends a person's first keystroke
/// reading their disk -- and a walk that stopped at one quietly would read as
/// "that file is not there" rather than "I did not look that far".
import { describe, expect, it } from "vitest";
import { walkWorkspace } from "../mentions";

type Entry = { name: string; kind: string };

function tree(folders: Record<string, Entry[]>) {
  return async (path: string) => {
    const entries = folders[path];
    return entries
      ? { ok: true as const, value: { entries } }
      : { ok: false as const, error: `no such path: ${path}` };
  };
}

const file = (name: string): Entry => ({ name, kind: "file" });
const folder = (name: string): Entry => ({ name, kind: "folder" });

describe("walking the workspace for names", () => {
  it("returns paths from the root, not bare names", () => {
    // A bare `main.rs` is not something the runtime can resolve, and two files
    // of that name in different folders would be one entry.
    return walkWorkspace(tree({
      "": [folder("src"), file("notes.txt")],
      src: [file("main.rs"), folder("bin")],
      "src/bin": [file("main.rs")],
    })).then((walked) => {
      expect(walked.files.sort()).toEqual(["notes.txt", "src/bin/main.rs", "src/main.rs"]);
      expect(walked.complete).toBe(true);
    });
  });

  it("says it is incomplete when the depth bound cuts it", async () => {
    const walked = await walkWorkspace(tree({
      "": [folder("a")],
      a: [folder("b")],
      "a/b": [file("deep.txt")],
    }), { maxDepth: 1 });
    expect(walked.files).toEqual([]);
    expect(walked.complete).toBe(false);
  });

  it("says it is incomplete when the file bound cuts it", async () => {
    const walked = await walkWorkspace(tree({
      "": [file("one"), file("two"), file("three")],
    }), { maxFiles: 2 });
    expect(walked.files).toEqual(["one", "two"]);
    expect(walked.complete).toBe(false);
  });

  it("says it is incomplete when the folder bound cuts it", async () => {
    const walked = await walkWorkspace(tree({
      "": [folder("a"), folder("b")],
      a: [file("in-a")],
      b: [file("in-b")],
    }), { maxFolders: 2 });
    expect(walked.complete).toBe(false);
  });

  /// A folder that will not list is not a reason to abandon the rest of the
  /// workspace, and it is also not something to claim completeness over.
  it("keeps going past a folder it cannot read, and does not claim to be whole", async () => {
    const walked = await walkWorkspace(tree({
      "": [folder("locked"), folder("open")],
      open: [file("readable.txt")],
    }));
    expect(walked.files).toEqual(["open/readable.txt"]);
    expect(walked.complete).toBe(false);
  });
});
