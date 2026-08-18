// @vitest-environment node
/// What this file is for.
///
/// This object decides what the agent can do, and every mistake in it so far
/// has been silent. A missing `tool:shell.exec` shipped an agent that was never
/// offered the tool at all: the window looked healthy, a turn ran, and the
/// model could read a folder and nothing else. Nothing on screen said so,
/// because from the client's side nothing was wrong.
///
/// It was guarded by matching strings in `main.cjs`, which requires Electron at
/// import time and cannot be loaded from a test -- so the check could confirm
/// that some text was present and never that the child gets it. The builder is
/// its own module now and this reads the object it returns.
import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";
import path from "node:path";

/// Typed here rather than by importing the module's own types: `electron/` is
/// outside this package's `tsconfig` include, and pulling it in to type one
/// require would put the host process into the renderer's compilation.
type ChildEnv = {
  runtimeEnv(request: {
    routing: { env: Record<string, string>; file: string };
    mcp?: { file: string; scopes: string[] } | null;
    workspace?: string | null;
    runtimeBinary: string;
    rolesFile: string;
    subagentRoles?: boolean;
    environment?: Record<string, string | undefined>;
  }): Record<string, string>;
  loginShell(environment?: Record<string, string | undefined>): string;
};

const require_ = createRequire(import.meta.url);
const childEnv = require_(
  path.join(import.meta.dirname, "..", "..", "electron", "childEnv.cjs"),
) as ChildEnv;

const base = {
  routing: { env: { AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY_ENV: "K" }, file: "/routing.json" },
  runtimeBinary: "/opt/rt",
  rolesFile: "/roles.json",
};

const scopesOf = (env: Record<string, string>) =>
  env.AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES.split(",");

describe("what the app hands the runtime", () => {
  it("grants every tool family it means the agent to be able to use", () => {
    const env = childEnv.runtimeEnv({ ...base, workspace: "/w" });
    const scopes = scopesOf(env);
    // Each of these was, at some point, the whole difference between an agent
    // and a window: without the scope the host does not install the tool and
    // the model is never offered it.
    expect(scopes).toContain("tool:shell.exec");
    expect(scopes).toContain("tool:workspace.write");
    expect(scopes).toContain("agent:spawn");
    expect(scopes).toContain("tool:process.session");
  });

  /// The eight `process.*` tools exist only when the executable is set. Without
  /// it the host installs none of them, and the process-session surface -- a
  /// whole screen -- can only ever show its empty state.
  it("names a shell, so the process tools exist at all", () => {
    const env = childEnv.runtimeEnv({
      ...base, workspace: "/w", environment: { SHELL: "/bin/zsh" },
    });
    expect(env.AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE).toBe("/bin/zsh");
  });

  /// `SHELL` is an environment variable, so it is not evidence. A relative or
  /// missing path would start a runtime that then fails every `process.start`,
  /// which is worse than a plainer shell that works.
  it("falls back rather than naming a shell that is not there", () => {
    for (const named of ["zsh", "/nowhere/zsh", undefined]) {
      const env = childEnv.runtimeEnv({
        ...base, workspace: "/w", environment: named ? { SHELL: named } : {},
      });
      expect(env.AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE).toBe("/bin/sh");
    }
  });

  it("tells the agent where it is and that a person sees every call", () => {
    const said = childEnv.runtimeEnv({ ...base, workspace: "/Users/x/code" })
      .AGENT_RUNTIME_LOCAL_INSTRUCTIONS;
    // The host's own default names none of these. A model that does not know
    // the root guesses at paths, and one that does not know a call will be
    // shown to someone writes different calls.
    expect(said).toContain("/Users/x/code");
    expect(said).toMatch(/waits for their decision/);
    expect(said).toMatch(/process\.\*/);
  });

  it("says so when there is no workspace, rather than leaving it unsaid", () => {
    const env = childEnv.runtimeEnv({ ...base, workspace: null });
    // The variable is absent rather than empty: an empty root is a path, and
    // the host would take it as one.
    expect(env.AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT).toBeUndefined();
    expect(env.AGENT_RUNTIME_LOCAL_INSTRUCTIONS).toMatch(/No workspace folder is configured/);
  });

  it("adds each configured MCP server's own scope beside the rest", () => {
    // Not an enhancement: `valid_mcp_servers` requires the scope for every
    // server a Run carries, so a runtime given the config file without these
    // refuses every Run -- including ones that never mentioned MCP.
    const env = childEnv.runtimeEnv({
      ...base, workspace: "/w", mcp: { file: "/mcp.json", scopes: ["tool:mcp:docs"] },
    });
    expect(scopesOf(env)).toContain("tool:mcp:docs");
    expect(env.AGENT_RUNTIME_LOCAL_MCP_CONFIG).toBe("/mcp.json");
  });

  it("keeps the consent boundary explicit rather than inherited", () => {
    // A boundary that holds because nobody set a variable is one that moves
    // when someone else's default does.
    expect(childEnv.runtimeEnv({ ...base, workspace: "/w" }).AGENT_RUNTIME_LOCAL_TOOL_CONSENT)
      .toBe("ask");
  });

  it("carries no secret of its own", () => {
    // The routing env names a variable; the value is in the login keychain.
    // Nothing this builder adds may look like a key.
    const env = childEnv.runtimeEnv({ ...base, workspace: "/w" });
    for (const [name, value] of Object.entries(env)) {
      if (name === "AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY_ENV") continue;
      expect(String(value)).not.toMatch(/sk-|Bearer |secret/i);
    }
  });
});
