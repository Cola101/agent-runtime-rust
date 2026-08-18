// The local MCP server list, and every rule the runtime would enforce later.
//
// `runtime-host` reads its servers from one JSON file named by
// `AGENT_RUNTIME_LOCAL_MCP_CONFIG`, once, at startup (`load_mcp_servers` in
// runtime-host/src/main.rs). This file writes that list.
//
// The validation here is not defensive politeness. Every rule below is a rule
// the runtime applies afterwards, in a place nobody is watching: a command that
// is not on disk, a name the Worker cannot namespace, or one Tool name too many
// makes `LocalRuntimeHost::start` refuse or `RunExecutionCommand::validate`
// reject *every* Run -- and the window would show a runtime that never listened,
// or an app where nothing works and nothing says why. Each constant names the
// runtime function it mirrors so the two can be checked against each other.
//
// No secret goes in here, and unlike `credentials.cjs` there is nowhere to put
// one. A provider names an environment variable and the key travels in the
// child's environment; an stdio MCP server has no such indirection -- its
// environment is read only out of this config file
// (`LocalMcpTransportConfig::Stdio::env`). So the environment written is empty
// and the form takes none, rather than writing a key down to make the feature
// look finished.
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

/// `RunExecutionCommand::valid_mcp_servers` refuses more than this.
const MAX_SERVERS = 16;

/// `SkillSnapshot::validate` caps the whole declared Tool list at 32, and the
/// local Skill already carries the three workspace Tools the desktop grants
/// (`local_skill_snapshot`). Every MCP Tool name is added to that same list as
/// `mcp:<server>/<tool>`, so this is the real ceiling: past it the snapshot is
/// invalid and every Run is refused at admission, MCP or not.
const MAX_SKILL_TOOL_NAMES = 32;
const NATIVE_TOOL_NAMES = 3;

/// `McpServerSnapshot::has_usable_namespace`. A name carrying `/` or `:` could
/// make one server's Tool resolve as another's, which is why the Worker checks
/// it again on receipt.
const NAME = /^[a-z0-9][a-z0-9_-]{0,63}$/;
/// `portable_identifier`: first and last byte alphanumeric, `.`/`_`/`-` inside.
const TOOL = /^[a-z0-9]([a-z0-9._-]*[a-z0-9])?$/;
/// `skill_tool_name(value, 120)` measures the whole qualified name.
const MAX_QUALIFIED_TOOL_NAME = 120;
/// `LocalRuntimeHost::start`'s bounds on an stdio process configuration.
const MAX_ARGS = 128;
const MAX_ARG_BYTES = 16 * 1024;

function scopeFor(name) {
  return `tool:mcp:${name}`;
}

/// The exact object `LocalMcpServerConfig` deserializes, and nothing else.
///
/// `deny_unknown_fields` is on both this struct and the transport enum, so an
/// extra key here is not ignored -- it is a runtime that exits during startup
/// with a parse error, after the window has already opened.
function configEntry(server) {
  return {
    server_id: server.serverId,
    name: server.name,
    transport: {
      type: "stdio",
      command: server.command,
      args: server.args,
      // Always empty, and asserted to be. This is the whole of "a secret is
      // never written into a config file" as it applies to MCP.
      env: {},
      cwd: server.cwd,
    },
    tool_names: server.toolNames,
    required: server.required,
  };
}

/// What identifies one server's configuration to the runtime.
///
/// Names alone cannot answer "is the runtime running what is on this screen":
/// editing a command leaves the name in place. This digest covers everything
/// the runtime was actually handed, so the surface can tell a server that is
/// live from one that is only saved.
function digestOf(server) {
  return crypto.createHash("sha256")
    .update(JSON.stringify(configEntry(server)))
    .digest("hex")
    .slice(0, 16);
}

class McpServers {
  constructor(dir) {
    this.dir = dir;
    this.file = path.join(dir, "servers.json");
    /// Where the runtime reads from. Separate from the record above: one is
    /// this app's, the other is derived and rewritten on every start.
    this.configFile = path.join(dir, "mcp.json");
  }

  #read() {
    try {
      const parsed = JSON.parse(fs.readFileSync(this.file, "utf8"));
      return Array.isArray(parsed?.servers) ? parsed.servers : [];
    } catch {
      return [];
    }
  }

  #write(servers) {
    fs.mkdirSync(this.dir, { recursive: true });
    // Temporary file and a rename, the way `credentials.cjs` and the runtime's
    // own durable state are written: a crash mid-write must not leave a
    // half-parsed list that silently drops a server.
    const temp = `${this.file}.writing`;
    fs.writeFileSync(temp, `${JSON.stringify({ version: 1, servers }, null, 2)}\n`, {
      mode: 0o600,
    });
    fs.renameSync(temp, this.file);
  }

  /// What the renderer may know. `scope` and `digest` are derived here rather
  /// than in the surface so the screen cannot describe an authority or an
  /// identity the config file does not have.
  list() {
    return this.#read().map((server) => ({
      name: server.name,
      command: server.command,
      args: server.args ?? [],
      cwd: server.cwd ?? null,
      toolNames: server.toolNames ?? [],
      required: Boolean(server.required),
      scope: scopeFor(server.name),
      digest: digestOf(server),
      addedAt: server.addedAt ?? null,
    }));
  }

  /// Adds or replaces one server, by name.
  ///
  /// The id is minted once and kept across edits. It is not decoration: the
  /// server id and the canonical transport go into the Run's MCP binding digest
  /// and into the Checkpoint, so a new id on every save would mean a restored
  /// Run could no longer match the server it was admitted with.
  save({ name, command, args = [], cwd = null, toolNames = [], required = false }, now = new Date()) {
    const cleanName = String(name ?? "").trim();
    if (!NAME.test(cleanName)) {
      throw new Error("名字只能是小写字母、数字、- 和 _，且不能以 - 或 _ 开头");
    }
    const cleanCommand = String(command ?? "").trim();
    if (!path.isAbsolute(cleanCommand)) throw new Error("命令必须是绝对路径");
    if (!fs.existsSync(cleanCommand) || !fs.statSync(cleanCommand).isFile()) {
      throw new Error(`这个路径上没有文件：${cleanCommand}`);
    }
    const cleanArgs = args.map((arg) => String(arg));
    if (cleanArgs.length > MAX_ARGS || cleanArgs.some((arg) => Buffer.byteLength(arg) > MAX_ARG_BYTES)) {
      throw new Error("参数太多或太长");
    }
    const cleanCwd = cwd === null || String(cwd).trim() === "" ? null : String(cwd).trim();
    if (cleanCwd !== null) {
      if (!path.isAbsolute(cleanCwd)) throw new Error("工作目录必须是绝对路径");
      if (!fs.existsSync(cleanCwd) || !fs.statSync(cleanCwd).isDirectory()) {
        throw new Error(`这个路径上没有目录：${cleanCwd}`);
      }
    }
    const cleanTools = [...new Set(toolNames.map((tool) => String(tool).trim()).filter(Boolean))]
      .sort();
    if (cleanTools.length === 0) {
      // Discovery may narrow this list but can never add to it, so a server
      // with no Tool names is a server whose Tools are never offered.
      throw new Error("至少要写一个工具名 —— 没写的工具 Runtime 不会给模型");
    }
    for (const tool of cleanTools) {
      if (!TOOL.test(tool) || `mcp:${cleanName}/${tool}`.length > MAX_QUALIFIED_TOOL_NAME) {
        throw new Error(`工具名 ${tool} 不是 Runtime 认的形状`);
      }
    }

    const others = this.#read().filter((server) => server.name !== cleanName);
    if (others.length + 1 > MAX_SERVERS) {
      throw new Error(`最多 ${MAX_SERVERS} 个 MCP 服务`);
    }
    const declared = others.reduce((total, server) => total + (server.toolNames?.length ?? 0), 0);
    if (declared + cleanTools.length + NATIVE_TOOL_NAMES > MAX_SKILL_TOOL_NAMES) {
      throw new Error(
        `工具名总数超了 —— 加上本机自带的 ${NATIVE_TOOL_NAMES} 个，Runtime 一次最多认 ` +
          `${MAX_SKILL_TOOL_NAMES} 个；再多每个 Run 都会被拒`,
      );
    }

    const previous = this.#read().find((server) => server.name === cleanName);
    others.push({
      serverId: previous?.serverId ?? crypto.randomUUID(),
      name: cleanName,
      command: cleanCommand,
      args: cleanArgs,
      cwd: cleanCwd,
      toolNames: cleanTools,
      required: Boolean(required),
      addedAt: previous?.addedAt ?? now.toISOString(),
    });
    this.#write(others);
    return { name: cleanName };
  }

  forget(name) {
    this.#write(this.#read().filter((server) => server.name !== name));
    return { name };
  }

  /// The file the runtime is started with, and what that start has to grant.
  ///
  /// The scopes are returned rather than assumed by the caller because they are
  /// not optional: `valid_mcp_servers` requires `tool:mcp:<name>` in the Run's
  /// delegated scopes for every configured server, so a runtime started with
  /// this file and without these scopes refuses every Run it is given -- not
  /// just the ones that would have used MCP.
  ///
  /// Null when nothing is configured, so the caller can leave the environment
  /// variable unset rather than pointing the runtime at an empty list.
  config() {
    const servers = this.#read();
    if (servers.length === 0) return null;
    fs.mkdirSync(this.dir, { recursive: true });
    fs.writeFileSync(
      this.configFile,
      `${JSON.stringify(servers.map(configEntry), null, 2)}\n`,
      { mode: 0o600 },
    );
    return {
      file: this.configFile,
      scopes: servers.map((server) => scopeFor(server.name)),
      applied: servers.map((server) => ({ name: server.name, digest: digestOf(server) })),
    };
  }
}

module.exports = {
  McpServers, scopeFor, MAX_SERVERS, MAX_SKILL_TOOL_NAMES, NATIVE_TOOL_NAMES,
};
