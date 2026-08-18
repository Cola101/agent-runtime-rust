// The local adapter: a Unix socket carrying one JSON object per line.
//
// This client used to speak only gRPC. That was the wrong first transport for
// a single-user desktop, for a reason that is visible in the contract itself:
// `RuntimeInvocation` has Submit, Control, ReadEvents and WatchEvents, and no
// way to enumerate runs. A client that cannot ask "what runs exist" cannot
// render a run list from anything but invention.
//
// The Unix adapter in `runtime-host/src/ipc.rs` has `List`, and its EventCursor
// and Control variants are documented there as the protocol-neutral surface
// "for SDKs and future GUI adapters". This is that adapter. gRPC stays for
// reaching a runtime on another machine, where mTLS and an operator token are
// the point rather than an obstacle.
const net = require("node:net");
const path = require("node:path");
const os = require("node:os");
const crypto = require("node:crypto");

/// The daemon's built-in local identity, mirrored from
/// `runtime-host::local_invocation_context()`.
///
/// EventCursor takes the invocation context as an argument and checks it, so a
/// local client has to know these. They are constants in the runtime rather
/// than anything served over the socket, which means this copy can drift.
/// `desktop/scripts/check-local-invocation.sh` fails if it ever does.
const LOCAL_INVOCATION = Object.freeze({
  schema_version: 1,
  tenant_id: "00000000-0000-4000-8000-000000000010",
  application_id: "00000000-0000-4000-8000-000000000013",
  workload_identity_id: "00000000-0000-4000-8000-000000000015",
  workspace_id: "00000000-0000-4000-8000-000000000011",
  agent_version_id: "00000000-0000-4000-8000-000000000012",
  model_policy_id: "00000000-0000-4000-8000-000000000001",
});

const EVENT_CURSOR_SCHEMA_VERSION = 1;
/// `RUNTIME_EVENT_CURSOR_MAX_EVENTS`. The daemon rejects a larger limit as an
/// invalid request rather than clamping it, so asking for more is not a bigger
/// page — it is no page at all.
const EVENT_CURSOR_MAX_EVENTS = 256;
const MAX_SOCKET_PATH_BYTES = 100;
const CONNECT_TIMEOUT_MS = 3_000;
const CALL_TIMEOUT_MS = 30_000;

/// Mirrors `runtime-host::ipc::default_socket_path`.
///
/// The fallback is not cosmetic: a state root deep enough to push the socket
/// path past the platform's sockaddr limit makes the daemon bind in the temp
/// directory instead, and a client that only looked in the state root would
/// report "no runtime" about a runtime that is running.
function socketPathFor(stateRoot) {
  const inside = path.join(stateRoot, "runtime-host.sock");
  if (Buffer.byteLength(inside) <= MAX_SOCKET_PATH_BYTES) return inside;
  const digest = crypto.createHash("sha256").update(Buffer.from(stateRoot)).digest("hex");
  return path.join(os.tmpdir(), `agent-runtime-host-${digest.slice(0, 16)}.sock`);
}

/// One request, one connection.
///
/// That is the daemon's protocol, not a simplification: `client_request` in
/// runtime-host opens a stream per request and the daemon reads a single line
/// from it. Multiplexing would be inventing a framing the server does not have.
function call(socketPath, request, { collectUntil = null } = {}) {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    const lines = [];
    let buffer = "";
    let settled = false;

    const timer = setTimeout(() => {
      finish(new Error(`runtime did not answer within ${CALL_TIMEOUT_MS}ms`));
    }, CALL_TIMEOUT_MS);
    socket.setTimeout(CONNECT_TIMEOUT_MS, () => {
      if (!socket.readable) finish(new Error("timed out connecting to the runtime socket"));
    });

    function finish(error, value) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      if (error) reject(error);
      else resolve(value);
    }

    socket.on("connect", () => {
      socket.write(`${JSON.stringify(request)}\n`);
    });

    socket.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      let index;
      while ((index = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, index);
        buffer = buffer.slice(index + 1);
        if (line.trim() === "") continue;
        let parsed;
        try {
          parsed = JSON.parse(line);
        } catch {
          finish(new Error("runtime sent a line that is not JSON"));
          return;
        }
        if (!collectUntil) {
          finish(null, parsed);
          return;
        }
        lines.push(parsed);
        if (collectUntil(parsed)) {
          finish(null, lines);
          return;
        }
      }
    });

    // A closed connection before any reply is the ordinary shape of "there is
    // no daemon here", so it is reported as such rather than as a parse error.
    socket.on("end", () => {
      if (collectUntil) finish(null, lines);
      else finish(new Error("runtime closed the connection without replying"));
    });
    socket.on("error", (error) => finish(error));
  });
}

/// A connection is a state root plus a socket that answers.
///
/// Nothing is guessed. If the shell was not told where the runtime keeps its
/// state, it has no connection — it does not go looking in likely directories,
/// for the same reason it does not guess a network endpoint.
class LocalRuntime {
  constructor(stateRoot) {
    this.stateRoot = stateRoot;
    this.socketPath = stateRoot ? socketPathFor(stateRoot) : null;
    this.reachable = false;
    this.lastError = null;
  }

  status() {
    return {
      transport: "local",
      stateRoot: this.stateRoot,
      socketPath: this.socketPath,
      connected: this.reachable,
      error: this.lastError,
    };
  }

  async probe() {
    if (!this.socketPath) {
      this.lastError = "no state root configured";
      return this.status();
    }
    try {
      await this.list();
      this.reachable = true;
      this.lastError = null;
    } catch (error) {
      this.reachable = false;
      this.lastError = error.message;
    }
    return this.status();
  }

  async #request(body, options) {
    if (!this.socketPath) throw new Error("no runtime configured");
    const reply = await call(this.socketPath, body, options);
    const first = Array.isArray(reply) ? reply[0] : reply;
    if (first && first.type === "error") throw new Error(first.message);
    return reply;
  }

  /// Run ids the daemon has started, newest first.
  ///
  /// Note the daemon keeps this order in memory, so a restarted host reports
  /// an empty list even though the runs are still on disk. The shell shows
  /// that as "this host has started no runs since it came up" rather than as
  /// "there are no runs", because those are different facts.
  async list() {
    const reply = await this.#request({ type: "list" });
    if (reply.type !== "runs") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.run_ids;
  }

  /// One bounded page of a run's durable log, with its typed lifecycle.
  ///
  /// Errors here are returned, not thrown: `history_gap`, `corrupt_log` and
  /// `cursor_ahead` are things the person needs to see on the run, and turning
  /// them into a rejected promise is how they become a blank panel instead.
  async eventCursor({ runId, afterSequence = 0, limit = EVENT_CURSOR_MAX_EVENTS }) {
    const bounded = Math.min(Math.max(1, limit), EVENT_CURSOR_MAX_EVENTS);
    const reply = await this.#request({
      type: "event_cursor",
      request: {
        schema_version: EVENT_CURSOR_SCHEMA_VERSION,
        invocation: LOCAL_INVOCATION,
        run_id: runId,
        after_sequence: afterSequence,
        limit: bounded,
      },
    });
    if (reply.type === "event_cursor") return { ok: true, page: reply.page };
    if (reply.type === "event_cursor_error") return { ok: false, error: reply.error };
    throw new Error(`unexpected reply: ${reply.type}`);
  }

  async submit(input) {
    const reply = await this.#request({ type: "submit", input });
    if (reply.type !== "accepted") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.run_id;
  }

  /// Approve, deny, cancel or resume. The daemon translates each of these into
  /// the full `RuntimeControlCommand`, including the owner epoch and the
  /// binding digest, which is why the client does not construct one: a client
  /// that guessed an epoch would be racing the host for authority over a run.
  async control({ action, runId }) {
    const allowed = new Set(["approve", "deny", "cancel", "resume"]);
    if (!allowed.has(action)) throw new Error(`unsupported control action: ${action}`);
    const reply = await this.#request({ type: action, run_id: runId });
    return reply;
  }
}

module.exports = {
  LocalRuntime, socketPathFor, LOCAL_INVOCATION, EVENT_CURSOR_MAX_EVENTS,
};
