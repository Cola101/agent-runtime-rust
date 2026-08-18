// What the app hands the runtime it spawns.
//
// Its own module, and not for tidiness. This object decides what the agent can
// do, and getting it wrong is silent in both directions: an app that had passed
// its own acceptance once shipped an agent that was never offered `shell.exec`,
// because the scope was missing and nothing anywhere said so. The window looked
// healthy, a turn ran, and the model could read a folder and nothing else.
//
// Guarded by requiring this file and reading the object. The previous guard
// matched strings in `main.cjs`, because `main.cjs` requires Electron at import
// time and a test cannot load it -- so the check could only ever confirm that
// some text was present, not that the child gets it.
"use strict";

const fs = require("node:fs");

/// Every scope the agent may be *asked* to use.
///
/// Granting a scope is not granting a use: `AGENT_RUNTIME_LOCAL_TOOL_CONSENT`
/// keeps every call stopping on a person. Without these the host falls back to
/// `tool:workspace.read` alone.
///
/// `tool:process.session` covers all eight `process.*` tools -- the host uses
/// one scope for the family -- and is added beside these rather than listed
/// among them, because it goes with the executable that turns those tools on
/// and neither is set without the other.
const BASE_SCOPES = [
  "tool:workspace.read",
  "tool:workspace.write",
  "tool:shell.exec",
  // Delegation needs two things and the configured roles are only one of them:
  // the `agent.*` family is installed when the roles are non-empty *and* the
  // parent holds this scope (`worker/src/lib.rs`, where the family is built).
  // Roles alone offer the model no way to use them -- checked by running a turn
  // and reading the tool list the provider was sent.
  "agent:spawn",
];

const PROCESS_SESSION_SCOPE = "tool:process.session";

/// Whether the runtime would accept this path as a process executable.
///
/// `TrustedNativeExecutor::new` reads it with `symlink_metadata` and refuses a
/// symlink, a non-regular file, or one with no execute bit. Checked the same
/// way here rather than approximated: `statSync` follows symlinks, so a
/// homebrew shell -- which is usually a link into the Cellar -- would pass a
/// check written with it and then be refused by the host it was named to.
function usableExecutable(candidate) {
  try {
    const stats = fs.lstatSync(candidate);
    return stats.isFile() && (stats.mode & 0o111) !== 0;
  } catch {
    return false;
  }
}

/// The shell a persistent process session runs, or null when this machine has
/// none the runtime would accept.
///
/// The person's own login shell first, so a session behaves like their
/// terminal; `/bin/sh` after it, which is a regular file on macOS and usually
/// a symlink to dash on Linux. Null rather than a guess: naming a path the
/// runtime refuses gives a host that starts and then fails every
/// `process.start`, which is worse than a host that says it has no process
/// tools -- something the process-session surface already knows how to say.
function loginShell(environment = process.env) {
  const named = environment.SHELL;
  const candidates = [
    ...(typeof named === "string" && named.startsWith("/") ? [named] : []),
    "/bin/sh",
  ];
  return candidates.find(usableExecutable) ?? null;
}

/// What the agent is told about where it is.
///
/// The host's own default is one sentence and names nothing: not the folder,
/// not the tools, not the fact that every call stops on a person. A model that
/// does not know a tool call will be shown to someone before it runs writes
/// different calls, and one that does not know the workspace root guesses at
/// paths. This is the app's answer because the app is what knows all three.
function instructions({ workspace, processSession }) {
  const lines = [
    "You are a local coding agent running on the person's own machine.",
    "Explain the evidence before the conclusion, and say plainly when you do not know.",
    workspace
      ? `The workspace is ${workspace}. Every path you read or write is checked against it after resolving symlinks, so a path outside it will be refused rather than followed.`
      : "No workspace folder is configured, so file tools will refuse every path until one is chosen.",
    "Every tool call is shown to the person and waits for their decision before it runs. Write calls that are worth reading: one that says what it does is one they can approve quickly.",
  ];
  if (processSession) {
    lines.push(
      "A persistent shell session is available through the process.* tools. It survives across calls, so start one when a task needs a working directory or an environment to persist, and close it when you are done.",
    );
  }
  return lines.join(" ");
}

/// The environment for the spawned `agent-runtime-host`.
///
/// `routing.env` comes first and is the only place a secret appears -- it names
/// a variable, and the value lives in the login keychain. Nothing below reads
/// it and nothing here is written to a file this app controls.
function runtimeEnv({
  routing,
  mcp = null,
  workspace = null,
  runtimeBinary,
  rolesFile,
  subagentRoles = true,
  environment = process.env,
}) {
  // Both or neither. A scope for a tool family the host will not install grants
  // nothing while reading as though it grants something, and an executable the
  // host refuses is a runtime that starts and then fails every `process.start`.
  const shell = loginShell(environment);
  const scopes = [
    ...BASE_SCOPES,
    ...(shell ? [PROCESS_SESSION_SCOPE] : []),
    ...(mcp?.scopes ?? []),
  ];
  return {
    ...routing.env,
    AGENT_RUNTIME_LOCAL_MODEL_ROUTING_CONFIG: routing.file,
    ...(mcp ? { AGENT_RUNTIME_LOCAL_MCP_CONFIG: mcp.file } : {}),
    ...(workspace ? { AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT: workspace } : {}),
    // The runtime binary is also the trusted workspace tool -- it re-execs
    // itself for that role. Pointing at the binary this app just spawned means
    // the two can never be different builds.
    AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN: runtimeBinary,
    // The same binary again, and again for a role it plays itself: the PTY
    // supervisor is `runtime-host __pty-session-supervisor`, wired by the host
    // rather than by this app. Setting the executable is the whole of what
    // turns the eight `process.*` tools on; without it the host installs none
    // of them and the process-session surface can only ever be empty.
    ...(shell ? { AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE: shell } : {}),
    AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES: scopes.join(","),
    AGENT_RUNTIME_LOCAL_INSTRUCTIONS: instructions({
      workspace,
      processSession: Boolean(shell),
    }),
    ...(subagentRoles ? { AGENT_RUNTIME_LOCAL_SUBAGENT_CONFIG: rolesFile } : {}),
    // Explicit rather than inherited from the host's default. A security
    // boundary that holds because nobody set a variable is a boundary that
    // moves when someone else's default does.
    AGENT_RUNTIME_LOCAL_TOOL_CONSENT: "ask",
  };
}

module.exports = { runtimeEnv, loginShell, instructions, BASE_SCOPES, PROCESS_SESSION_SCOPE };
