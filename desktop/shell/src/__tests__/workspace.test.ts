// @vitest-environment node
/// What this file is for.
///
/// The renderer draws transcript content this app did not author. A workspace
/// call that took any path would therefore be a path chosen by whatever the
/// model last said, so the containment here is not a formality -- it is the
/// only thing between a rendered sentence and the rest of the disk.
///
/// The escapes are the tests. A containment check nobody has watched refuse is
/// a containment check nobody has tested.
import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync, rmSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Workspace, MAX_PREVIEW_BYTES } = require("../../electron/workspace.cjs");

const roots: string[] = [];

function workspace() {
  const base = mkdtempSync(path.join(tmpdir(), "ws-"));
  roots.push(base);
  const root = path.join(base, "workspace");
  mkdirSync(root);
  writeFileSync(path.join(root, "notes.txt"), "the retention sweep stats every run directory\n");
  mkdirSync(path.join(root, "src"));
  writeFileSync(path.join(root, "src", "main.rs"), "fn main() {}\n");
  writeFileSync(path.join(base, "outside.txt"), "not for the window");
  return { base, root, ws: new Workspace(root) };
}

afterEach(() => {
  for (const dir of roots.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("what the window may read", () => {
  it("lists the workspace, folders first", () => {
    const { ws } = workspace();
    const listed = ws.list("");
    expect(listed.entries.map((entry: { name: string }) => entry.name)).toEqual(["src", "notes.txt"]);
    expect(listed.entries[0].kind).toBe("folder");
    expect(listed.entries[1].size).toBeGreaterThan(0);
  });

  it("reads a file inside it", () => {
    const { ws } = workspace();
    const read = ws.read("src/main.rs");
    expect(read.text).toBe("fn main() {}\n");
    expect(read.binary).toBe(false);
  });
});

describe("what it may not", () => {
  it("refuses an absolute path", () => {
    const { ws } = workspace();
    expect(() => ws.read("/etc/hosts")).toThrow(/relative/);
  });

  it("refuses to climb out with ..", () => {
    const { ws } = workspace();
    expect(() => ws.read("../outside.txt")).toThrow(/outside the workspace/);
  });

  /// The one a string check misses. `link` is inside the workspace by every
  /// textual measure and resolves to a file that is not, which is why the check
  /// happens after `realpath` rather than before it.
  it("refuses a symlink that points out", () => {
    const { base, root, ws } = workspace();
    symlinkSync(path.join(base, "outside.txt"), path.join(root, "escape.txt"));
    expect(() => ws.read("escape.txt")).toThrow(/outside the workspace/);
  });

  it("refuses a symlinked folder that points out", () => {
    const { base, root, ws } = workspace();
    symlinkSync(base, path.join(root, "up"));
    expect(() => ws.list("up")).toThrow(/outside the workspace/);
    expect(() => ws.read("up/outside.txt")).toThrow(/outside the workspace/);
  });

  /// A sibling whose name merely starts with the root's. Without the trailing
  /// separator in the containment check this one passes.
  it("refuses a sibling directory with the root as a name prefix", () => {
    const { base, root } = workspace();
    const sibling = `${root}-evil`;
    mkdirSync(sibling);
    writeFileSync(path.join(sibling, "secret.txt"), "no");
    const ws = new Workspace(root);
    expect(() => ws.read(path.relative(root, path.join(sibling, "secret.txt")))).toThrow(
      /outside the workspace/,
    );
    expect(realpathSync(base)).toBeTruthy();
  });
});

describe("what it says about a file it will not print", () => {
  it("calls binary binary instead of rendering it", () => {
    const { root, ws } = workspace();
    writeFileSync(path.join(root, "blob.bin"), Buffer.from([0x00, 0x01, 0x02, 0x00]));
    const read = ws.read("blob.bin");
    expect(read.binary).toBe(true);
    expect(read.text).toBeUndefined();
  });

  it("says a long file was cut rather than pretending it ended", () => {
    const { root, ws } = workspace();
    writeFileSync(path.join(root, "long.txt"), "x".repeat(MAX_PREVIEW_BYTES + 10));
    const read = ws.read("long.txt");
    expect(read.truncated).toBe(true);
    expect(read.text).toHaveLength(MAX_PREVIEW_BYTES);
    expect(read.size).toBe(MAX_PREVIEW_BYTES + 10);
  });
});
