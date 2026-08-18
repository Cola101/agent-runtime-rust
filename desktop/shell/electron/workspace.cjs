// Reading the workspace, for the person rather than for the agent.
//
// The runtime contains the *agent* inside this folder. That containment is not
// this file's to reuse: it governs a Run, and what the window shows is the
// person's own folder on their own machine. But the renderer is contained
// anyway, and for a different reason -- it renders transcript content this app
// did not author, so a call that took any path would be a path chosen by
// whatever the model last said.
//
// Every path therefore arrives relative, is resolved against the real workspace
// root, and is checked after `realpath` rather than before. Checking the string
// first is what makes symlink escapes work: `a/b` can be inside and still
// resolve outside.
const fs = require("node:fs");
const path = require("node:path");

/// A directory listing stops here. Not a paging limit -- a refusal to walk a
/// folder that would take a second to draw and that nobody reads to the end of.
const MAX_ENTRIES = 500;
/// Previews are bounded because the point is to see what a file is, and a
/// window that stalls rendering a 200 MB log has answered a question nobody
/// asked.
const MAX_PREVIEW_BYTES = 256 * 1024;

class Workspace {
  constructor(root) {
    this.root = root ? fs.realpathSync.native(root) : null;
  }

  /// Resolves a caller's relative path, or refuses.
  ///
  /// Returns the real path only when it is the root or genuinely beneath it.
  /// The final separator matters: without it `/tmp/workspace-evil` passes a
  /// `startsWith("/tmp/workspace")` check.
  #resolve(relative) {
    if (!this.root) throw new Error("no workspace configured");
    if (typeof relative !== "string") throw new Error("path must be a string");
    if (path.isAbsolute(relative)) throw new Error("path must be relative to the workspace");
    const joined = path.resolve(this.root, relative);
    let real;
    try {
      real = fs.realpathSync.native(joined);
    } catch {
      throw new Error("no such path in the workspace");
    }
    if (real !== this.root && !real.startsWith(`${this.root}${path.sep}`)) {
      throw new Error("that path is outside the workspace");
    }
    return real;
  }

  status() {
    return { root: this.root, configured: this.root !== null };
  }

  list(relative = "") {
    const target = this.#resolve(relative);
    const stat = fs.statSync(target);
    if (!stat.isDirectory()) throw new Error("that path is not a folder");
    const entries = [];
    let truncated = false;
    for (const entry of fs.readdirSync(target, { withFileTypes: true })) {
      if (entries.length >= MAX_ENTRIES) { truncated = true; break; }
      const full = path.join(target, entry.name);
      let size = null;
      let modified = null;
      try {
        const info = fs.lstatSync(full);
        size = info.isFile() ? info.size : null;
        modified = new Date(info.mtimeMs).toISOString();
      } catch {
        // A file that vanished between readdir and lstat is reported as what it
        // is -- present in the listing, unknown in its details -- rather than
        // dropped, which would silently disagree with the folder.
      }
      entries.push({
        name: entry.name,
        kind: entry.isDirectory() ? "folder" : entry.isSymbolicLink() ? "link" : "file",
        size,
        modified,
      });
    }
    entries.sort((a, b) =>
      a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "folder" ? -1 : 1);
    return { path: path.relative(this.root, target), entries, truncated };
  }

  /// A bounded text preview. Binary is reported as binary rather than rendered
  /// as replacement characters, because "this is not text" is the useful
  /// answer and a screen of  is not.
  read(relative) {
    const target = this.#resolve(relative);
    const stat = fs.statSync(target);
    if (!stat.isFile()) throw new Error("that path is not a file");
    const handle = fs.openSync(target, "r");
    try {
      const buffer = Buffer.alloc(Math.min(stat.size, MAX_PREVIEW_BYTES));
      const read = fs.readSync(handle, buffer, 0, buffer.length, 0);
      const bytes = buffer.subarray(0, read);
      if (bytes.includes(0)) {
        return { path: path.relative(this.root, target), binary: true, size: stat.size };
      }
      return {
        path: path.relative(this.root, target),
        binary: false,
        size: stat.size,
        truncated: stat.size > read,
        text: bytes.toString("utf8"),
      };
    } finally {
      fs.closeSync(handle);
    }
  }
}

module.exports = { Workspace, MAX_ENTRIES, MAX_PREVIEW_BYTES };
