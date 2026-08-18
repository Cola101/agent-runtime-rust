// Works out which packages the host process actually needs, and copies them in.
//
// The first version of this copied the shell package's direct dependencies and
// stopped there. The bundle passed every file check and then failed to load
// `@grpc/grpc-js`, which needs `@js-sdsl/ordered-map` -- a package nothing in
// this repository names. Transitive dependencies are not an optimisation to
// skip; they are most of the tree.
//
// Copied flat rather than nested. Node resolves by walking up, so one directory
// of packages is enough, and a conflict between two versions of one package
// would be caught by the load check the caller runs afterwards rather than
// hidden by a nesting that silently satisfies both.
import { createRequire } from "node:module";
import { cpSync, mkdirSync, readFileSync } from "node:fs";
import path from "node:path";

const [shell, target] = process.argv.slice(2);
if (!shell || !target) {
  console.error("usage: bundle-deps.mjs <shell package dir> <target node_modules dir>");
  process.exit(1);
}

const read = (file) => JSON.parse(readFileSync(file, "utf8"));

/// Resolved from the package that declares it, not from the root: a package's
/// dependency is whatever *it* resolves, which under pnpm is a specific version
/// in the store rather than whatever happens to be hoisted.
function locate(name, from) {
  const require = createRequire(path.join(from, "package.json"));
  try {
    return path.dirname(require.resolve(`${name}/package.json`));
  } catch {
    // Some packages have no `exports` entry for their manifest. Falling back to
    // the main entry and walking up to the manifest handles those without
    // guessing at a layout.
    let dir = path.dirname(require.resolve(name));
    for (let up = 0; up < 8; up += 1) {
      try {
        read(path.join(dir, "package.json"));
        return dir;
      } catch {
        dir = path.dirname(dir);
      }
    }
    throw new Error(`cannot locate ${name} from ${from}`);
  }
}

const closure = new Map();
const queue = Object.keys(read(path.join(shell, "package.json")).dependencies ?? {})
  .map((name) => ({ name, from: shell }));

while (queue.length > 0) {
  const { name, from } = queue.shift();
  if (closure.has(name)) continue;
  const dir = locate(name, from);
  closure.set(name, dir);
  // Only `dependencies`. `devDependencies` are the package's own tooling and
  // `optionalDependencies` are, by their own declaration, allowed to be absent.
  for (const next of Object.keys(read(path.join(dir, "package.json")).dependencies ?? {})) {
    if (!closure.has(next)) queue.push({ name: next, from: dir });
  }
}

mkdirSync(target, { recursive: true });
for (const [name, dir] of closure) {
  const into = path.join(target, name);
  mkdirSync(path.dirname(into), { recursive: true });
  // Dereferenced: pnpm's tree is symlinks into a store that will not exist on
  // the machine this bundle is copied to.
  cpSync(dir, into, { recursive: true, dereference: true });
}

console.log(`bundled ${closure.size} package(s)`);
