// @vitest-environment node
/// A merge that was never finished, caught before it ships.
///
/// This exists because it already happened twice. Two merges committed conflict
/// markers into `app.css`, and both gates were green: `tsc` does not read CSS,
/// and no test parses it. The browser drops the rule the marker lands in and
/// every rule after it until it recovers at the next `}`, so what shipped was a
/// stylesheet with holes in it -- silently, in the one file this project has no
/// other check on.
///
/// The `.tsx`/`.ts` case is covered by `tsc` (TS1185), which is why this guard
/// is not about types. It is about every other file that ships.
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const ROOT = new URL("../..", import.meta.url).pathname;

/// Split so this file does not match itself.
const OPEN = "<<<<" + "<<<";
const MID = "====" + "===";
const CLOSE = ">>>>" + ">>>";

function walk(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir)) {
    if (entry === "node_modules" || entry === "dist" || entry.startsWith(".")) continue;
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) found.push(...walk(path));
    else found.push(path);
  }
  return found;
}

describe("nothing half-merged ships", () => {
  it("finds no conflict marker in any file the app is built from", () => {
    const left: string[] = [];
    for (const path of walk(ROOT)) {
      if (!/\.(ts|tsx|css|cjs|mjs|js|json|html)$/.test(path)) continue;
      if (path === new URL(import.meta.url).pathname) continue;
      const text = readFileSync(path, "utf8");
      for (const line of text.split("\n")) {
        if (line.startsWith(OPEN) || line === MID || line.startsWith(CLOSE)) {
          left.push(`${path.slice(ROOT.length)}: ${line.slice(0, 40)}`);
        }
      }
    }
    expect(left).toEqual([]);
  });
});
