// @vitest-environment node
/// What this file is for.
///
/// The app now owns a runtime-host process, and "owns" only means something if
/// quitting actually ends it. Two things here are the kind that silently never
/// work: the escalation from SIGTERM to SIGKILL, which only runs against a
/// process that ignores the first signal, and the refusal to stop a runtime
/// this app attached to rather than started -- which, if it broke, would take
/// down someone else's host and look like nothing at all from in here.
import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { RuntimeProcess } = require("../../electron/runtimeProcess.cjs");

const roots: string[] = [];
function root(): string {
  const made = mkdtempSync(path.join(tmpdir(), "runtime-process-"));
  roots.push(made);
  return made;
}

/// A stand-in runtime: this node, running a script, with no shell in between.
///
/// `stubborn` ignores SIGTERM, which is the only way to reach the escalation
/// path on purpose.
function fakeRuntime(dir: string, { stubborn = false } = {}): string[] {
  const file = path.join(dir, "fake-runtime.mjs");
  // The handler is installed before the line is printed, so the line is proof
  // it is installed. Signalling a process that has not finished starting is a
  // race in the product too -- which is why the supervisor waits for the
  // socket rather than for the spawn to return.
  writeFileSync(file, [
    stubborn ? 'process.on("SIGTERM", () => {});' : "",
    'process.stderr.write("runtime-host listening\\n");',
    "setInterval(() => {}, 1000);",
  ].join("\n"));
  return [file];
}

async function listening(runtime: { log: string[] }): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (runtime.log.some((line) => line.includes("listening"))) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`the fake runtime never reported listening: ${runtime.log.join(" / ")}`);
}

afterEach(() => {
  for (const dir of roots.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("the runtime this app owns", () => {
  it("stops on quit, and drains before it signals", async () => {
    const dir = root();
    const runtime = new RuntimeProcess();
    const pid = runtime.start({
      binary: process.execPath, args: fakeRuntime(dir), stateRoot: dir,
    });
    expect(pid).toBeGreaterThan(0);
    await listening(runtime);
    expect(runtime.running).toBe(true);

    const order: string[] = [];
    const outcome = await runtime.stop({
      drain: async () => { order.push("drained"); return { active_before_drain: 0 }; },
    });
    order.push("stopped");
    expect(outcome.stopped).toBe(true);
    // Drain first: a runtime asked to stop without being asked to drain leaves
    // work to be recovered from a Checkpoint that could have finished.
    expect(order).toEqual(["drained", "stopped"]);
    expect(outcome.escalated).toBe(false);
    expect(outcome.exit?.signal).toBe("SIGTERM");
  }, 20_000);

  it("escalates when the runtime ignores the first signal", async () => {
    const dir = root();
    const runtime = new RuntimeProcess();
    runtime.start({
      binary: process.execPath, args: fakeRuntime(dir, { stubborn: true }), stateRoot: dir,
    });
    await listening(runtime);
    const outcome = await runtime.stop();
    expect(outcome.stopped).toBe(true);
    expect(outcome.escalated).toBe(true);
    expect(outcome.exit?.signal).toBe("SIGKILL");
  }, 30_000);

  it("stops even when the drain itself fails", async () => {
    const dir = root();
    const runtime = new RuntimeProcess();
    runtime.start({ binary: process.execPath, args: fakeRuntime(dir), stateRoot: dir });
    await listening(runtime);
    const outcome = await runtime.stop({
      drain: async () => { throw new Error("socket closed"); },
    });
    expect(outcome.stopped).toBe(true);
    expect(runtime.log.some((line: string) => line.includes("socket closed"))).toBe(true);
  }, 20_000);

  it("refuses to stop a runtime it only attached to", async () => {
    const runtime = new RuntimeProcess();
    runtime.attach();
    const outcome = await runtime.stop({ drain: async () => ({}) });
    expect(outcome.stopped).toBe(false);
    expect(outcome.reason).toContain("not this app's runtime");
  });

  /// A socket file outlives the process that made it -- killing a runtime-host
  /// leaves one behind. Refusing to start on the file's existence would have
  /// meant one crash blocking every later launch, over debris the daemon
  /// removes itself once it has proved nothing answers on it.
  it("starts over the socket file a crashed runtime left behind", async () => {
    const dir = root();
    writeFileSync(path.join(dir, "runtime-host.sock"), "");
    const runtime = new RuntimeProcess();
    expect(() => runtime.start({
      binary: process.execPath, args: fakeRuntime(dir), stateRoot: dir,
    })).not.toThrow();
    await listening(runtime);
    await runtime.stop();
  }, 20_000);
});
