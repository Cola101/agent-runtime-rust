// Provider configuration, split by what is allowed to be written down.
//
// Endpoint, model and protocol go in a file. The secret never does. It lives in
// the login Keychain and reaches the runtime only as an environment variable on
// the child process this app spawns.
//
// The runtime's own routing format was built for exactly this split: a
// candidate names an *environment variable* (`api_key_env`) rather than
// carrying a key, and `runtime-host` refuses to start when the variable it
// names is absent. So nothing here invents a scheme -- it fills in the one the
// runtime already requires.
//
// The secret is passed to `security` on stdin rather than in an argument.
// Anything in argv is readable by every process this user runs, so `-w <value>`
// would put an API key in `ps` output for as long as the write took.
const { execFile } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

/// The Keychain service every entry is filed under. The account is the
/// provider id, so one app can hold credentials for several providers without
/// them being able to collide.
const SERVICE = "agent-runtime-desk";

/// `api_key_env` accepts `[A-Za-z0-9_]` up to 128 bytes, which is checked by
/// the runtime and rejected there. Deriving the name rather than storing one
/// keeps a provider id from ever producing a variable the runtime will refuse.
function envNameFor(id) {
  const cleaned = id.toUpperCase().replace(/[^A-Z0-9_]/g, "_").slice(0, 100);
  return `AGENT_RUNTIME_PROVIDER_${cleaned || "DEFAULT"}`;
}

function security(args, { input = null } = {}) {
  return new Promise((resolve) => {
    const child = execFile("/usr/bin/security", args, (error, stdout, stderr) => {
      resolve({ ok: !error, stdout: String(stdout ?? ""), stderr: String(stderr ?? "") });
    });
    if (input !== null) {
      child.stdin.end(input);
    }
  });
}

/// macOS only, deliberately. This milestone is an Apple Silicon Alpha, and a
/// cross-platform abstraction over one implementation would be a guess about
/// the other platforms rather than support for them.
class Credentials {
  constructor(dir) {
    this.dir = dir;
    this.file = path.join(dir, "providers.json");
  }

  #read() {
    try {
      const parsed = JSON.parse(fs.readFileSync(this.file, "utf8"));
      return Array.isArray(parsed?.providers) ? parsed.providers : [];
    } catch {
      return [];
    }
  }

  #write(providers) {
    fs.mkdirSync(this.dir, { recursive: true });
    // Written the same way the runtime writes durable state: a temporary file
    // and a rename, so a crash mid-write cannot leave a half-parsed config that
    // silently drops a provider.
    const temp = `${this.file}.writing`;
    fs.writeFileSync(temp, `${JSON.stringify({ version: 1, providers }, null, 2)}\n`, {
      mode: 0o600,
    });
    fs.renameSync(temp, this.file);
  }

  /// What the renderer is allowed to know: which providers exist, where they
  /// point, and whether a secret is on file. Never the secret.
  async list() {
    const providers = this.#read();
    const out = [];
    for (const provider of providers) {
      const found = await security(["find-generic-password", "-s", SERVICE, "-a", provider.id, "-w"]);
      out.push({
        id: provider.id,
        protocol: provider.protocol,
        endpoint: provider.endpoint,
        model: provider.model,
        // Read back from the Keychain rather than from a flag in the file: a
        // file that claims a secret exists is a file that will one day be
        // wrong about it, and the failure lands at runtime start.
        hasSecret: found.ok && found.stdout.trim().length > 0,
        secretSetAt: provider.secretSetAt ?? null,
      });
    }
    return out;
  }

  /// Adds or replaces a provider. The secret is optional: changing an endpoint
  /// should not require retyping a key that is already on file.
  async save({ id, protocol, endpoint, model, secret = null, maxOutputTokens = null }, now = new Date()) {
    if (!id || !/^[a-zA-Z0-9._-]{1,64}$/.test(id)) throw new Error("provider id is not usable");
    if (!endpoint || !/^https?:\/\//.test(endpoint)) throw new Error("endpoint must be http or https");
    if (!model) throw new Error("a model is required");
    // A ceiling that is not a positive whole number is not a ceiling. Refused
    // here rather than written out and rejected later by a provider, where the
    // complaint arrives as a failed conversation instead of a failed save.
    if (maxOutputTokens !== null
      && (!Number.isInteger(maxOutputTokens) || maxOutputTokens <= 0)) {
      throw new Error("the per-reply ceiling must be a positive whole number of tokens");
    }
    // The spellings `ProviderProtocol::from_str` takes, which are also what
    // `provider_registry`, `edge-node` and the gateway CLI take. They reach the
    // routing file's serde path through the aliases added for exactly this --
    // before those existed, this list was rejected by the config parser and the
    // runtime exited before it listened.
    const protocols = ["openai_compatible", "openai_responses", "anthropic_messages"];
    if (!protocols.includes(protocol)) throw new Error(`unsupported protocol ${protocol}`);

    if (secret !== null) {
      if (typeof secret !== "string" || secret.length === 0) throw new Error("the secret is empty");
      const written = await security(
        ["add-generic-password", "-U", "-s", SERVICE, "-a", id, "-w"],
        // Twice: `security` asks for the value and then to confirm it, and with
        // a pipe both reads come from here.
        { input: `${secret}\n${secret}\n` },
      );
      if (!written.ok) throw new Error(`the Keychain refused the secret: ${written.stderr.trim()}`);
    }

    const providers = this.#read().filter((provider) => provider.id !== id);
    const previous = this.#read().find((provider) => provider.id === id);
    providers.push({
      id, protocol, endpoint, model,
      // Kept across a save that does not mention it, the same way the secret's
      // timestamp is: editing an endpoint should not silently reset a ceiling
      // the operator set on purpose.
      maxOutputTokens: maxOutputTokens ?? previous?.maxOutputTokens ?? null,
      secretSetAt: secret !== null ? now.toISOString() : previous?.secretSetAt ?? null,
    });
    this.#write(providers);
    return { id };
  }

  async forget(id) {
    this.#write(this.#read().filter((provider) => provider.id !== id));
    await security(["delete-generic-password", "-s", SERVICE, "-a", id]);
    return { id };
  }

  /// The routing file and the environment the runtime process needs.
  ///
  /// Returns null when nothing is configured, so the caller can say "no
  /// provider" rather than starting a runtime that will exit complaining about
  /// a missing variable.
  ///
  /// The file is written every time rather than kept in step by hand: it is
  /// derived state, and derived state that is only updated on change is state
  /// that will one day disagree with what it was derived from.
  async routing() {
    const providers = this.#read();
    if (providers.length === 0) return null;
    const env = {};
    const candidates = [];
    for (const provider of providers) {
      const name = envNameFor(provider.id);
      const found = await security(["find-generic-password", "-s", SERVICE, "-a", provider.id, "-w"]);
      if (!found.ok) continue;
      env[name] = found.stdout.replace(/\n$/, "");
      candidates.push({
        id: provider.id,
        protocol: provider.protocol,
        endpoint: provider.endpoint,
        model: provider.model,
        api_key_env: name,
        region: "local",
        accepted_data_classes: ["public", "internal", "confidential"],
        capabilities: ["text", "tool_use"],
        healthy: true,
        latency_ms: 250,
        cost_per_million_tokens_micros: 0,
        response_timeout_ms: 120_000,
        stream_idle_timeout_ms: 30_000,
        // The longest single reply this model will be asked for.
        //
        // Not optional in practice. The worker fills a turn's
        // `max_output_tokens` with the Run's *remaining budget* and the adapter
        // forwards what it is given, so with nothing here a Run asks a real
        // server for a 400,000-token reply and is refused outright:
        //
        //   max_tokens=400000 cannot be greater than max_model_len=204800.
        //
        // Measured against a self-hosted vLLM; no conversation could be started
        // at all. The default below is a floor low enough that every model in
        // current use accepts it and high enough for a long answer -- it is a
        // number that keeps the app working, not a claim about this model, so
        // an operator who knows the real ceiling overrides it.
        max_output_tokens: provider.maxOutputTokens ?? 8192,
      });
    }
    if (candidates.length === 0) return null;
    fs.mkdirSync(this.dir, { recursive: true });
    const file = path.join(this.dir, "model-routing.json");
    fs.writeFileSync(file, `${JSON.stringify({
      candidates,
      allowed_regions: ["local"],
      data_class: "confidential",
      max_cost_per_million_tokens_micros: 1_000_000_000,
    }, null, 2)}\n`, { mode: 0o600 });
    return { file, env };
  }
}

module.exports = { Credentials, envNameFor, SERVICE };
