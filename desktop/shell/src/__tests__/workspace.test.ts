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
import {
  mkdtempSync, mkdirSync, writeFileSync, symlinkSync, rmSync, realpathSync, readFileSync,
} from "node:fs";
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

/// The packaged app finds two files by convention: the runtime binary and the
/// proto, both dropped beside the bundle by `package-app.sh`. Nothing links the
/// two ends -- rename either and the app builds, launches, opens a window and
/// then reports it has no runtime. So the ends are checked against each other.
describe("what the packaged app expects to find beside it", () => {
  const root = path.join(import.meta.dirname, "..", "..");

  it("looks for exactly what the packaging script ships", () => {
    const main = readFileSync(path.join(root, "electron", "main.cjs"), "utf8");
    const script = readFileSync(
      path.join(root, "..", "scripts", "package-app.sh"), "utf8",
    );
    // The binary: resolved from `process.resourcesPath` in the app, copied in
    // as an extra resource by the script, and checked in the bundle by name.
    expect(main).toContain("process.resourcesPath");
    expect(main).toContain('"agent-runtime-host"');
    expect(script).toContain("Contents/Resources/agent-runtime-host");
    expect(script).toContain('cp "$runtime" "$app/Contents/Resources/agent-runtime-host"');

    const client = readFileSync(path.join(root, "electron", "runtime.cjs"), "utf8");
    expect(client).toContain('path.join(process.resourcesPath, "runtime.proto")');
    expect(script).toContain("Contents/Resources/runtime.proto");
    expect(script).toContain("contracts/proto/runtime.proto");
  });

  /// The app once started a runtime with only `tool:workspace.read` delegated,
  /// because the host falls back to that when the variable is unset and the
  /// launcher never set it. Everything else -- shell, writes, the approval
  /// machinery this window is built around -- was compiled, sandboxed, and
  /// never offered to the model. The acceptance run passed anyway: a turn ran,
  /// and the agent could not do anything.
  /// The scopes and the consent mode moved to `childEnv.cjs`, where
  /// `child-env.test.ts` reads the object the child is actually given rather
  /// than matching text. What stays here is what still lives in `main.cjs`:
  /// the roles file it derives and writes on every launch.
  ///
  /// Delegation is gated on two things, not one. The `agent.*` tool family is
  /// installed when the configured roles are non-empty *and* the parent holds
  /// `agent:spawn`; with roles alone the model is offered no way to use them.
  /// Checked by running a turn and reading the tool list the provider received:
  /// roles-only produced `shell.exec, workspace.read_text, workspace.write_text`
  /// and nothing else. This file owns the first half; the scope is the second.
  /// A restart must not attach. Attaching hands ownership away, and the quit
  /// path stops only what this app owns -- so an app that attached to whatever
  /// answered on the state root it had just cleared would leave the next
  /// runtime running when someone closed the window, which is the single thing
  /// that path exists to prevent.
  ///
  /// Read from the source, because `main.cjs` requires Electron at import time
  /// and this decision lives inside a handler rather than in a module a test
  /// can call. That is a weaker check than the ones in `child-env.test.ts`, and
  /// it is here because a weaker check on this is better than the nothing that
  /// covered it before.
  it("reopens after a restart without attaching to whatever answers", () => {
    const main = readFileSync(path.join(root, "electron", "main.cjs"), "utf8");
    const at = main.indexOf("runtime:restart");
    expect(at).toBeGreaterThan(-1);
    const handler = main.slice(at, at + 900);
    expect(handler, "the reopen after a restart must refuse to attach")
      .toContain("openRuntime({ mustOwn: true })");
    // And the flag has to actually stop the attach, not merely be accepted.
    const guard = main.indexOf("if (first.connected && mustOwn)");
    expect(guard).toBeGreaterThan(-1);
    expect(main.slice(guard, guard + 400)).toContain("return");
  });

  it("writes the roles file that is one half of the delegation gate", () => {
    const main = readFileSync(path.join(root, "electron", "main.cjs"), "utf8");
    // Roles narrower than the parent. A reviewer that can write the workspace
    // is not a reviewer, and a scope delegated cannot be taken back.
    expect(main).toMatch(/name:\s*"reader"/);
    expect(main).toContain("subagent-roles.json");
  });

  it("ships only what the host process needs, not the whole tree", () => {
    const script = readFileSync(
      path.join(root, "..", "scripts", "package-app.sh"), "utf8",
    );
    // Copying `node_modules` wholesale would put the test runner and the
    // TypeScript compiler in a shipped app, and would copy pnpm's symlinks into
    // a store the target machine does not have.
    expect(script).toContain("bundle-deps.mjs");
    expect(script).not.toMatch(/cp -R "\$shell\/node_modules"/);
    // The closure, not the direct dependencies: a bundle with only the latter
    // passes every file check and then cannot load `@grpc/grpc-js`.
    const bundler = readFileSync(
      path.join(root, "..", "scripts", "bundle-deps.mjs"), "utf8",
    );
    expect(bundler).toContain("queue.push");
    expect(bundler).toContain("dereference: true");
  });
});
