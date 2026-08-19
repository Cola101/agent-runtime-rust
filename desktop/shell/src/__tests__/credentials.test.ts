// @vitest-environment node
/// What this file is for.
///
/// A credential store is the easiest thing in this app to get wrong invisibly:
/// every path here can look like it worked. A secret that was never written
/// reads back as absent only at runtime start; a secret written into the config
/// file instead of the Keychain looks identical from the UI; and a routing file
/// with one field missing is rejected by a runtime that has already been
/// spawned, in a child process nobody is watching.
///
/// So these run against the real Keychain, under a service name that says what
/// it is, and clean up after themselves.
import { afterEach, describe, expect, it } from "vitest";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Credentials, envNameFor, SERVICE } = require("../../electron/credentials.cjs");

/// Not a credential. The stub provider ignores it; it exists because the
/// config requires the field.
const NOT_A_SECRET = "not-a-credential-desktop-test";
const IDS = ["desk-test-provider", "desk-test-other"];
const dirs: string[] = [];

function store() {
  const dir = mkdtempSync(path.join(tmpdir(), "credentials-"));
  dirs.push(dir);
  return new Credentials(dir);
}

afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
  for (const id of IDS) {
    try {
      execFileSync("/usr/bin/security", ["delete-generic-password", "-s", SERVICE, "-a", id], {
        stdio: "ignore",
      });
    } catch {
      // Absent is the state this wants; deleting what is not there is fine.
    }
  }
});

const provider = {
  id: IDS[0],
  protocol: "openai_compatible",
  endpoint: "http://127.0.0.1:9/v1/chat/completions",
  model: "stub",
};

describe("a provider credential", () => {
  it("goes to the Keychain and never into the config file", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });

    const written = readFileSync(credentials.file, "utf8");
    expect(written).toContain(provider.endpoint);
    // The whole point. A config file that carries the key makes the Keychain
    // decoration.
    expect(written).not.toContain(NOT_A_SECRET);

    const fromKeychain = execFileSync(
      "/usr/bin/security", ["find-generic-password", "-s", SERVICE, "-a", provider.id, "-w"],
    ).toString().trim();
    expect(fromKeychain).toBe(NOT_A_SECRET);
  });

  it("is never handed back to the caller that lists providers", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    const listed = await credentials.list();
    expect(listed).toHaveLength(1);
    expect(listed[0].hasSecret).toBe(true);
    expect(listed[0].secretSetAt).toBeTruthy();
    expect(JSON.stringify(listed)).not.toContain(NOT_A_SECRET);
  });

  it("survives an endpoint change without being retyped", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    await credentials.save({ ...provider, endpoint: "http://127.0.0.1:10/v1/chat/completions" });
    const listed = await credentials.list();
    expect(listed[0].endpoint).toContain(":10/");
    expect(listed[0].hasSecret).toBe(true);
  });

  it("reports absent after being forgotten", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    await credentials.forget(provider.id);
    expect(await credentials.list()).toEqual([]);
    expect(await credentials.routing()).toBeNull();
  });

  it("refuses configuration the runtime would reject anyway", async () => {
    const credentials = store();
    await expect(credentials.save({ ...provider, endpoint: "ftp://nope" })).rejects.toThrow(/http/);
    await expect(credentials.save({ ...provider, protocol: "telepathy" })).rejects.toThrow(/protocol/);
    await expect(credentials.save({ ...provider, id: "a b" })).rejects.toThrow(/id/);
    await expect(credentials.save({ ...provider, secret: "" })).rejects.toThrow(/empty/);
  });
});

describe("the protocol names", () => {
  /// Checked against the runtime's own source rather than against a list in
  /// this file. A list here would have agreed with itself while the runtime
  /// rejected every one of them -- which is exactly what happened: the app
  /// wrote `openai_compatible`, serde wanted `open_ai_compatible`, and the
  /// only place that showed up was a child process exiting.
  it("are ones the runtime actually accepts", () => {
    const enumSource = readFileSync(
      path.join(
        import.meta.dirname, "..", "..", "..", "..",
        "runtime", "apps", "model-gateway", "src", "lib.rs",
      ), "utf8",
    );
    const block = enumSource.slice(enumSource.indexOf("pub enum ProviderProtocol"));
    const accepted = new Set<string>();
    for (const [, alias] of block.slice(0, block.indexOf("}")).matchAll(/alias = "([a-z_]+)"/g)) {
      accepted.add(alias);
    }
    for (const [, variant] of block.slice(0, block.indexOf("}")).matchAll(/^\s{4}([A-Z]\w+),/gm)) {
      // `rename_all = "snake_case"`, applied the way serde applies it.
      accepted.add(variant.replace(/(?<!^)([A-Z])/g, "_$1").toLowerCase());
    }
    for (const protocol of ["openai_compatible", "openai_responses", "anthropic_messages"]) {
      expect(accepted, `${protocol} must be a spelling runtime-host parses`).toContain(protocol);
    }
  });
});

describe("the boundary itself", () => {
  /// The preload is the whole surface a renderer has. A call that returns a
  /// secret is not dangerous because someone would misuse it -- it is dangerous
  /// because a surface rendering a transcript this app did not author would be
  /// able to reach it. So the guard is that no such call exists, checked
  /// against the file rather than against intent.
  it("exposes no bridge call that could return a secret", () => {
    const preload = readFileSync(
      path.join(import.meta.dirname, "..", "..", "electron", "preload.cjs"), "utf8",
    );
    const exposed = [...preload.matchAll(/^\s*(\w+):\s*\(/gm)].map((match) => match[1]);
    expect(exposed).toContain("saveProvider");
    expect(exposed).toContain("providers");
    for (const name of exposed) {
      expect(name).not.toMatch(/secret|apiKey|credential|password|token/i);
    }
  });

  it("keeps the secret out of the process argument list", () => {
    const source = readFileSync(
      path.join(import.meta.dirname, "..", "..", "electron", "credentials.cjs"), "utf8",
    );
    // `-w` with a value puts the key in argv, which every process this user
    // runs can read out of `ps`. The value has to arrive on stdin instead.
    expect(source).not.toMatch(/"-w",\s*secret/);
    expect(source).toContain("input: `${secret}");
  });
});

describe("the routing file handed to the runtime", () => {
  it("names an environment variable and carries no key", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    const routing = await credentials.routing();
    expect(routing).not.toBeNull();

    const written = readFileSync(routing!.file, "utf8");
    expect(written).not.toContain(NOT_A_SECRET);
    const parsed = JSON.parse(written);
    const candidate = parsed.candidates[0];
    expect(candidate.api_key_env).toBe(envNameFor(provider.id));
    // The secret travels in the child's environment instead.
    expect(routing!.env[candidate.api_key_env]).toBe(NOT_A_SECRET);

    // `LocalModelRoutingFile` and `LocalProviderFile` are `deny_unknown_fields`
    // with no defaults except the health policy, so a missing field is a
    // runtime that exits after being spawned. Every required name is checked
    // here rather than discovered in a child process nobody is watching.
    expect(Object.keys(parsed).sort()).toEqual(
      ["allowed_regions", "candidates", "data_class", "max_cost_per_million_tokens_micros"],
    );
    expect(Object.keys(candidate).sort()).toEqual([
      "accepted_data_classes", "api_key_env", "capabilities",
      "cost_per_million_tokens_micros", "endpoint", "healthy", "id", "latency_ms",
      "max_output_tokens", "model", "protocol", "region", "response_timeout_ms",
      "stream_idle_timeout_ms",
    ]);
    // The region a candidate declares has to be one the routing allows, or
    // ranking drops it and the runtime starts with nothing to call.
    expect(parsed.allowed_regions).toContain(candidate.region);
  });

  it("leaves out a provider whose secret is gone from the Keychain", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    await credentials.save({ id: IDS[1], protocol: "openai_compatible", endpoint: provider.endpoint, model: "stub" });
    const routing = await credentials.routing();
    // Two configured, one with a key. A routing file listing the other would
    // be a runtime that refuses to start over a provider nobody finished
    // setting up.
    expect(routing!.file && existsSync(routing!.file)).toBe(true);
    const parsed = JSON.parse(readFileSync(routing!.file, "utf8"));
    expect(parsed.candidates.map((c: { id: string }) => c.id)).toEqual([provider.id]);
  });
});

/// Why the derived routing file has to carry a reply ceiling.
///
/// `max_tokens` is not optional in practice for an OpenAI-compatible provider.
/// The worker fills a turn's `max_output_tokens` with the Run's *remaining
/// budget* and the adapter forwards what it is given, so with no ceiling a
/// desktop Run asks a real server for a 400,000-token reply and is refused:
///
///   max_tokens=400000 cannot be greater than max_model_len=204800.
///
/// Measured against a self-hosted vLLM. No conversation could be started at
/// all. The ceiling is what makes such a provider usable, so the file the app
/// derives always states one.
describe("派生出来的模型路由文件", () => {
  it("给每个 provider 写上单条回复的上限", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET });
    const routing = await credentials.routing();
    const written = JSON.parse(readFileSync(routing!.file, "utf8"));
    expect(written.candidates[0].max_output_tokens).toBeGreaterThan(0);
  });

  it("上限是操作者说了算的，写死的默认值不覆盖它", async () => {
    const credentials = store();
    await credentials.save({ ...provider, secret: NOT_A_SECRET, maxOutputTokens: 1234 });
    const routing = await credentials.routing();
    const written = JSON.parse(readFileSync(routing!.file, "utf8"));
    expect(written.candidates[0].max_output_tokens).toBe(1234);
  });
});
