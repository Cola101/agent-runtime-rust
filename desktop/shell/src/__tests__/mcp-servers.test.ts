// @vitest-environment node
/// What this file is for.
///
/// Every mistake this store can make is invisible from the window. A config
/// file with one unexpected key is a runtime that exits during startup, in a
/// child process nobody is watching. A server whose scope was not granted is
/// *every* Run refused at admission, MCP or not. One Tool name too many is the
/// same, for a reason no error message on screen will mention.
///
/// So these run against the real filesystem, and the shape assertions are
/// checked against the runtime's own source rather than against a copy of it
/// kept here -- a copy would agree with itself while the runtime rejected the
/// file.
import { afterEach, describe, expect, it } from "vitest";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { McpServers, MAX_SKILL_TOOL_NAMES, NATIVE_TOOL_NAMES } =
  require("../../electron/mcpServers.cjs");

const dirs: string[] = [];

function store() {
  const dir = mkdtempSync(path.join(tmpdir(), "mcp-servers-"));
  dirs.push(dir);
  return new McpServers(dir);
}

/// A real executable, because the runtime refuses a command that is not an
/// existing absolute file and this store has to refuse it first.
function executable(name = "server.sh") {
  const dir = mkdtempSync(path.join(tmpdir(), "mcp-bin-"));
  dirs.push(dir);
  const file = path.join(dir, name);
  writeFileSync(file, "#!/bin/sh\nexit 0\n");
  chmodSync(file, 0o755);
  return file;
}

function runtimeSource(...parts: string[]) {
  return readFileSync(
    path.join(import.meta.dirname, "..", "..", "..", "..", "runtime", ...parts),
    "utf8",
  );
}

afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("the config file handed to the runtime", () => {
  it("carries only the fields LocalMcpServerConfig deserializes", () => {
    const servers = store();
    const command = executable();
    servers.save({
      name: "filesystem", command, args: ["-y", "pkg"], cwd: null,
      toolNames: ["read_file"], required: true,
    });

    const config = servers.config();
    const written = JSON.parse(readFileSync(config.file, "utf8"));
    expect(written).toHaveLength(1);

    // `LocalMcpServerConfig` and `LocalMcpTransportConfig` are both
    // `deny_unknown_fields`, so a key this app invents is not ignored -- it is
    // a parse error in a runtime that has already been spawned. The accepted
    // names are read out of the struct rather than listed here.
    const lib = runtimeSource("apps", "runtime-host", "src", "lib.rs");
    const struct = lib.slice(lib.indexOf("pub struct LocalMcpServerConfig {"));
    const accepted = new Set(
      [...struct.slice(0, struct.indexOf("\n}")).matchAll(/^\s{4}pub (\w+):/gm)]
        .map((match) => match[1]),
    );
    expect(accepted).toContain("server_id");
    for (const key of Object.keys(written[0])) expect(accepted).toContain(key);

    const stdio = lib.slice(lib.indexOf("    Stdio {"));
    const transportFields = new Set(
      [...stdio.slice(0, stdio.indexOf("\n    },")).matchAll(/^\s{8}(\w+):/gm)]
        .map((match) => match[1]),
    );
    expect(transportFields).toContain("command");
    for (const key of Object.keys(written[0].transport)) {
      if (key === "type") continue; // `#[serde(tag = "type")]`
      expect(transportFields).toContain(key);
    }
    // `rename_all = "snake_case"` on the transport enum.
    expect(written[0].transport.type).toBe("stdio");
  });

  it("writes no environment at all, which is where a secret would have gone", () => {
    const servers = store();
    servers.save({
      name: "filesystem", command: executable(), toolNames: ["read_file"],
    });
    const written = JSON.parse(readFileSync(servers.config().file, "utf8"));
    // An stdio server's environment is read only out of this file, and this
    // app has no way to put a value there that is not written down. So it
    // writes none -- asserted as an empty object rather than as an absent key,
    // because absence is what a broken writer also produces.
    expect(written[0].transport.env).toEqual({});
  });

  it("grants every configured server its scope", () => {
    const servers = store();
    const command = executable();
    servers.save({ name: "filesystem", command, toolNames: ["read_file"] });
    servers.save({ name: "notes", command, toolNames: ["search"] });
    // `valid_mcp_servers` requires `tool:mcp:<name>` for every server the Run
    // carries. Without it the command is invalid and the runtime refuses every
    // Run it is given, not only the ones that would have used MCP.
    expect(servers.config().scopes).toEqual(["tool:mcp:filesystem", "tool:mcp:notes"]);
  });

  it("is null when nothing is configured", () => {
    expect(store().config()).toBeNull();
  });

  it("keeps a server's id across an edit", () => {
    const servers = store();
    const command = executable();
    servers.save({ name: "filesystem", command, toolNames: ["read_file"] });
    const first = JSON.parse(readFileSync(servers.config().file, "utf8"))[0].server_id;
    servers.save({ name: "filesystem", command, args: ["--root", "/tmp"], toolNames: ["read_file"] });
    const second = JSON.parse(readFileSync(servers.config().file, "utf8"))[0].server_id;
    // The id and the canonical transport go into the Run's MCP binding digest
    // and into the Checkpoint. A new id per save would mean a restored Run
    // could no longer match the server it was admitted with.
    expect(second).toBe(first);
  });

  it("changes a server's digest when what the runtime would get changes", () => {
    const servers = store();
    const command = executable();
    servers.save({ name: "filesystem", command, toolNames: ["read_file"] });
    const before = servers.list()[0].digest;
    servers.save({ name: "filesystem", command, args: ["--root", "/tmp"], toolNames: ["read_file"] });
    // The name did not change, and the running runtime still has the old
    // arguments. Only the digest can say so.
    expect(servers.list()[0].digest).not.toBe(before);
  });
});

describe("configuration the runtime would refuse later", () => {
  it("refuses a name the Worker cannot namespace", () => {
    const servers = store();
    const command = executable();
    // `has_usable_namespace`. A name carrying `/` or `:` could make one
    // server's Tool resolve as another's; `1/b` is here because the first
    // version of that check read as "everything, or starts with a digit".
    for (const name of ["Filesystem", "a/b", "1/b", "x:y", "-lead", "a".repeat(65), ""]) {
      expect(() => servers.save({ name, command, toolNames: ["read_file"] }),
        `${name} must be refused`).toThrow();
    }
  });

  it("refuses a command that is not an absolute existing file", () => {
    const servers = store();
    expect(() => servers.save({ name: "a", command: "npx", toolNames: ["t"] }))
      .toThrow(/绝对路径/);
    expect(() => servers.save({ name: "a", command: "/nope/npx", toolNames: ["t"] }))
      .toThrow(/没有文件/);
    // A directory is not a command either -- `LocalRuntimeHost::start` checks
    // `is_file`, and a runtime that discovers this has already been spawned.
    expect(() => servers.save({ name: "a", command: tmpdir(), toolNames: ["t"] }))
      .toThrow(/没有文件/);
  });

  it("refuses a server with no tool names", () => {
    const servers = store();
    // Discovery narrows this allowlist and can never add to it, so a server
    // with none is a server whose Tools are never offered to the model.
    expect(() => servers.save({ name: "a", command: executable(), toolNames: [] }))
      .toThrow(/工具名/);
  });

  it("refuses a tool name that is not a portable identifier", () => {
    const servers = store();
    const command = executable();
    for (const tool of ["Read_File", "read file", "read/file", "_read", "read_"]) {
      expect(() => servers.save({ name: "a", command, toolNames: [tool] }),
        `${tool} must be refused`).toThrow();
    }
  });

  it("refuses one tool name past what a Skill snapshot may declare", () => {
    const servers = store();
    const command = executable();
    const room = MAX_SKILL_TOOL_NAMES - NATIVE_TOOL_NAMES;
    const tools = Array.from({ length: room }, (_, index) => `tool-${index}`);
    servers.save({ name: "a", command, toolNames: tools });
    // One more is not one unusable Tool. `SkillSnapshot::validate` caps the
    // whole declared list at 32 including the workspace Tools this app grants,
    // and an invalid snapshot means every Run is refused at admission.
    expect(() => servers.save({ name: "b", command, toolNames: ["one-too-many"] }))
      .toThrow(/工具名总数/);
  });

  it("checks the ceiling against the runtime's own number", () => {
    // The 32 is `sorted_unique(&self.tool_names, 32)` in the Skill snapshot's
    // validation. Read rather than trusted, because a constant copied into this
    // app would keep agreeing with itself after the runtime moved.
    const protocol = runtimeSource("crates", "protocol", "src", "lib.rs");
    expect(protocol).toContain("sorted_unique(&self.tool_names, 32)");
    expect(MAX_SKILL_TOOL_NAMES).toBe(32);
  });
});

describe("the boundary", () => {
  it("exposes no bridge call that could return a secret", () => {
    // The MCP calls are added to the same preload the provider calls live on,
    // and the same rule applies to them: a surface rendering a transcript this
    // app did not author must not be able to reach anything named like one.
    const preload = readFileSync(
      path.join(import.meta.dirname, "..", "..", "electron", "preload.cjs"), "utf8",
    );
    const exposed = [...preload.matchAll(/^\s*(\w+):\s*\(/gm)].map((match) => match[1]);
    expect(exposed).toContain("mcpServers");
    expect(exposed).toContain("saveMcpServer");
    for (const name of exposed) {
      expect(name).not.toMatch(/secret|apiKey|credential|password|token/i);
    }
  });

  /// The scope each configured server needs is checked where the environment is
  /// built, in `child-env.test.ts`, which reads the object rather than the text
  /// that produces it. What stays this file's business is the other half: that
  /// `config()` reports a scope per server at all, since a config file without
  /// its scopes makes `valid_mcp_servers` refuse every Run -- including the
  /// ones that never mentioned MCP -- while the window looks healthy.
  it("reports a scope for every server it puts in the config file", () => {
    const servers = store();
    const command = executable();
    servers.save({ name: "filesystem", command, toolNames: ["read_file"] });
    servers.save({ name: "docs", command, toolNames: ["search"] });
    const config = servers.config();
    expect(config.scopes.sort()).toEqual(["tool:mcp:docs", "tool:mcp:filesystem"]);
    expect(config.applied.map((entry: { name: string }) => entry.name).sort())
      .toEqual(["docs", "filesystem"]);
  });
});
