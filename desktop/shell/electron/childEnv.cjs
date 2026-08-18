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
/// among them, because it belongs with the executable that turns those tools
/// on. Neither is conditional: a shell is always found.
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

/// The shell a persistent process session runs.
///
/// The person's own login shell when it is an absolute path that exists, so a
/// session behaves like their terminal; `/bin/sh` otherwise. Not taken on
/// trust: `SHELL` is an environment variable, and a relative or missing path
/// would become a runtime that starts and then fails every `process.start`.
function loginShell(environment = process.env) {
  const named = environment.SHELL;
  if (typeof named === "string" && named.startsWith("/")) {
    try {
      if (fs.statSync(named).isFile()) return named;
    } catch {
      // Falls through to the portable one rather than naming a path that is not
      // there. A session that cannot start is worse than a plainer shell.
    }
  }
  return "/bin/sh";
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
  // Always found -- `loginShell` falls back rather than returning nothing -- so
  // the scope and the executable are not conditional. Writing them as though
  // they were would read as a decision this makes and does not.
  const shell = loginShell(environment);
  const scopes = [...BASE_SCOPES, PROCESS_SESSION_SCOPE, ...(mcp?.scopes ?? [])];
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
    AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE: shell,
    AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES: scopes.join(","),
    AGENT_RUNTIME_LOCAL_INSTRUCTIONS: instructions({ workspace, processSession: true }),
    ...(subagentRoles ? { AGENT_RUNTIME_LOCAL_SUBAGENT_CONFIG: rolesFile } : {}),
    // Explicit rather than inherited from the host's default. A security
    // boundary that holds because nobody set a variable is a boundary that
    // moves when someone else's default does.
    AGENT_RUNTIME_LOCAL_TOOL_CONSENT: "ask",
  };
}

module.exports = { runtimeEnv, loginShell, instructions, BASE_SCOPES, PROCESS_SESSION_SCOPE };
