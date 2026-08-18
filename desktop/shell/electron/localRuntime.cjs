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

const EVENT_CURSOR_SCHEMA_VERSION = 1;
/// `RUNTIME_EVENT_CURSOR_MAX_EVENTS`. The daemon rejects a larger limit as an
/// invalid request rather than clamping it, so asking for more is not a bigger
/// page — it is no page at all.
const EVENT_CURSOR_MAX_EVENTS = 256;
/// `OWNER_MAX_HISTORY_PAGE`. Only an upper clamp for a caller that names a
/// page size; the common path sends no limit at all and takes the daemon's.
///
/// This is a page ceiling, not an identity: if it drifts low the client asks
/// for smaller pages and pages again, and if the daemon's own bound is what
/// moved it answers with a refusal that says so. The identity constants this
/// client used to mirror had neither property, which is why they are gone and
/// this is not.
const SESSION_HISTORY_MAX_TURNS = 128;
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
/// A held-open connection, one line per event.
///
/// Distinct from `call` on purpose: `call` has a deadline, because a request
/// that never answers is a broken runtime. A watch has none, because a run that
/// says nothing for a minute is a run that is thinking. The two would be one
/// function only by making the deadline optional, and an optional deadline is
/// how a request ends up waiting forever.
///
/// The daemon ends the stream itself -- on the terminal boundary, on a retired
/// history, or on an error -- so this never has to decide when a run is over.
/// Which matters: deciding that from the events is exactly the mistake the
/// cursor's typed boundary exists to prevent.
function watch(socketPath, request, { onEvent, onEnd }) {
  const socket = net.createConnection(socketPath);
  let buffer = "";
  let ended = false;

  const finish = (reason) => {
    if (ended) return;
    ended = true;
    socket.destroy();
    onEnd(reason);
  };

  socket.setTimeout(CONNECT_TIMEOUT_MS, () => {
    if (!socket.readable) finish({ error: "timed out connecting to the runtime socket" });
  });
  socket.on("connect", () => {
    socket.setTimeout(0);
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
        finish({ error: "runtime sent a line that is not JSON" });
        return;
      }
      if (parsed.type === "event") onEvent(parsed.event);
      else if (parsed.type === "finished") finish({ status: parsed.status });
      else if (parsed.type === "error") finish({ error: parsed.message });
      else finish({ error: `unexpected line on the stream: ${parsed.type}` });
      if (ended) return;
    }
  });
  socket.on("end", () => finish({ closed: true }));
  socket.on("error", (error) => finish({ error: error.message }));

  return { stop: () => finish({ stopped: true }) };
}

class LocalRuntime {
  constructor(stateRoot) {
    this.stateRoot = stateRoot;
    this.socketPath = stateRoot ? socketPathFor(stateRoot) : null;
    this.reachable = false;
    this.lastError = null;
  }

  /// Why there is no runtime, when the app knows a reason better than the
  /// socket's silence.
  ///
  /// A fresh install has no provider, so the app declines to start a runtime
  /// and nothing is listening -- which the window used to report as a socket
  /// that did not answer, naming a path and reading as a fault. The one thing
  /// the person has to do is add a provider, and this is how the window gets
  /// to say that instead. Null when the app has no better account than the
  /// connection error itself.
  declined(reason) {
    this.reason = reason ?? null;
  }

  status() {
    return {
      transport: "local",
      stateRoot: this.stateRoot,
      socketPath: this.socketPath,
      connected: this.reachable,
      error: this.lastError,
      reason: this.reachable ? null : (this.reason ?? null),
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

  /// One line on the owner surface.
  ///
  /// Owner requests carry no invocation. The daemon owns exactly one state root
  /// and one identity, so supplying one would only be an opportunity to supply
  /// the wrong one -- and it is why this client no longer mirrors the runtime's
  /// identity constants at all.
  async #owner(body) {
    if (!this.socketPath) throw new Error("no runtime configured");
    const reply = await call(this.socketPath, { scope: "owner", ...body });
    if (reply && reply.type === "error") throw new Error(reply.message);
    return reply;
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
  /// On the owner surface, so it carries no invocation. That is the last thing
  /// that required this client to mirror the runtime's identity constants, and
  /// with it gone the mirror and its drift guard are gone too -- one fewer
  /// thing that can silently disagree with the runtime.
  ///
  /// Errors are returned, not thrown: `history_gap`, `corrupt_log` and
  /// `cursor_ahead` are things the person needs to see on the run, and turning
  /// them into a rejected promise is how they become a blank panel instead.
  async eventCursor({ runId, afterSequence = 0, limit = EVENT_CURSOR_MAX_EVENTS }) {
    const bounded = Math.min(Math.max(1, limit), EVENT_CURSOR_MAX_EVENTS);
    const reply = await this.#owner({
      type: "run_events",
      run_id: runId,
      after_sequence: afterSequence,
      limit: bounded,
    });
    if (reply.type === "run_events") return { ok: true, page: reply.page };
    if (reply.type === "run_events_error") return { ok: false, error: reply.error };
    throw new Error(`unexpected reply: ${reply.type}`);
  }

  /// Recover every Profile and open for work. Safe to ask twice.
  async start() {
    const reply = await this.#owner({ type: "start" });
    if (reply.type === "started") return true;
    if (reply.type === "not_ready") return false;
    throw new Error(`unexpected reply: ${reply.type}`);
  }

  /// Lifecycle, recovery progress, what is in flight, and -- once -- what the
  /// previous shutdown left behind.
  async lifecycle() {
    const reply = await this.#owner({ type: "snapshot" });
    if (reply.type !== "snapshot") throw new Error(`unexpected reply: ${reply.type}`);
    return reply;
  }

  /// Stop taking work, drain within the deadline, and report.
  async shutdown() {
    const reply = await this.#owner({ type: "shutdown" });
    if (reply.type !== "shutdown") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.report;
  }

  /// Every Run this state root holds, newest first, with what it was asked to
  /// do and where it got to.
  ///
  /// Distinct from `list()`, which returns bare ids for this daemon's own
  /// order. That difference is the reason this exists: a list of ids cannot say
  /// what a Run was asked to do, and that is the column a person reads.
  async listRuns({ afterRunId = null, limit = 256 } = {}) {
    const reply = await this.#owner({
      type: "list_runs",
      ...(afterRunId ? { after_run_id: afterRunId } : {}),
      limit,
    });
    if (reply.type !== "runs") throw new Error(`unexpected reply: ${reply.type}`);
    return { runs: reply.runs, nextAfterRunId: reply.next_after_run_id ?? null };
  }

  /// Start a Session's first Turn.
  ///
  /// The three ids are the caller's to choose, and `runId` is the idempotency
  /// key rather than a name: the daemon answers a repeated `runId` from what it
  /// already accepted, without charging admission for it. A client that
  /// generated a fresh id on retry would turn one Turn into two.
  ///
  /// Returns as soon as the Turn is *durably accepted*, not when it is done --
  /// the Turn runs detached. `head.active_run_id` is what says whether it is
  /// still running, and the branch refuses a second Turn until it clears.
  async sessionStart({ sessionId, branchId, runId, input }) {
    const reply = await this.#owner({
      type: "session_start",
      session_id: sessionId,
      branch_id: branchId,
      run_id: runId,
      input,
    });
    if (reply.type !== "session_turn") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.receipt;
  }

  /// Continue a Session on a branch, at a generation.
  ///
  /// `generation` is a fence, not a formality: it is what a rollback moves, so
  /// a client holding a stale one is a client whose Turn would land on history
  /// the person already retired. Read the head immediately before continuing
  /// rather than remembering one -- the daemon will refuse a stale generation,
  /// which is the correct outcome but a worse experience than not sending it.
  async sessionContinue({ sessionId, branchId, generation, runId, input }) {
    const reply = await this.#owner({
      type: "session_continue",
      session_id: sessionId,
      branch_id: branchId,
      generation,
      run_id: runId,
      input,
    });
    if (reply.type !== "session_turn") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.receipt;
  }

  /// Cut a second branch from a Session, carrying history through one Turn.
  ///
  /// `targetBranchId` is the caller's, and it is what identifies the Fork
  /// afterwards: the daemon answers a repeated request from the branch that
  /// request already produced, so a client that minted a fresh id on retry
  /// would cut a second branch instead of finding the first.
  ///
  /// `sourceGeneration` is the same fence `continue` carries. A Fork from a
  /// generation the branch has already left is refused rather than quietly cut
  /// from history nobody is looking at any more.
  async sessionFork(
    { sessionId, sourceBranchId, sourceGeneration, throughTurnOrdinal, targetBranchId },
  ) {
    const reply = await this.#owner({
      type: "session_fork",
      session_id: sessionId,
      source_branch_id: sourceBranchId,
      source_generation: sourceGeneration,
      through_turn_ordinal: throughTurnOrdinal,
      target_branch_id: targetBranchId,
    });
    if (reply.type !== "session_head") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.head;
  }

  /// Take a branch back to a Turn, dropping every Turn after it.
  ///
  /// The branch moves to a new generation, and what it carries into the next
  /// Turn is the shorter history. The daemon refuses an ordinal that is not
  /// strictly earlier than the last committed Turn -- a Rollback that removes
  /// nothing is a mistake, not a no-op -- which is why the surface only offers
  /// this where there is something after the Turn to drop.
  async sessionRollback({ sessionId, branchId, generation, throughTurnOrdinal }) {
    const reply = await this.#owner({
      type: "session_rollback",
      session_id: sessionId,
      branch_id: branchId,
      generation,
      through_turn_ordinal: throughTurnOrdinal,
    });
    if (reply.type !== "session_head") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.head;
  }

  /// A branch's head: generation, turn count, history digest, and the run id of
  /// a Turn still in flight.
  async sessionRead({ sessionId, branchId }) {
    const reply = await this.#owner({
      type: "session_read",
      session_id: sessionId,
      branch_id: branchId,
    });
    if (reply.type !== "session_head") throw new Error(`unexpected reply: ${reply.type}`);
    return reply.head;
  }

  /// Every branch head this state root holds.
  async sessionList({ afterSessionId = null, afterBranchId = null, limit = 256 } = {}) {
    const reply = await this.#owner({
      type: "session_list",
      ...(afterSessionId ? { after_session_id: afterSessionId } : {}),
      ...(afterBranchId ? { after_branch_id: afterBranchId } : {}),
      limit,
    });
    if (reply.type !== "session_list") throw new Error(`unexpected reply: ${reply.type}`);
    return { heads: reply.page.heads, nextAfter: reply.page.next_after ?? null };
  }

  /// One bounded page of committed Turns, each carrying the transcript the
  /// runtime froze for it -- roles and content parts, not a rendering of the
  /// event log. The two are different sources and only this one survives the
  /// event log being retired.
  async sessionHistory({ sessionId, branchId, generation, afterTurnOrdinal = 0, limit = null }) {
    const reply = await this.#owner({
      type: "session_history",
      session_id: sessionId,
      branch_id: branchId,
      generation,
      after_turn_ordinal: afterTurnOrdinal,
      // Omitted rather than mirrored: the daemon has a default and this client
      // has no better opinion than the server about its own page ceiling. A
      // caller that does ask is clamped down, because the daemon *rejects* an
      // oversized page instead of clamping it -- asking for too much is how a
      // history becomes empty rather than paged.
      ...(limit === null ? {} : { limit: Math.min(Math.max(1, limit), SESSION_HISTORY_MAX_TURNS) }),
    });
    if (reply.type !== "session_history") throw new Error(`unexpected reply: ${reply.type}`);
    return { turns: reply.page.turns, nextAfterTurnOrdinal: reply.page.next_after_turn_ordinal ?? null };
  }

  /// Follows one run's durable log as it is written.
  ///
  /// The events arrive; the *lifecycle* does not, and deliberately. A boundary
  /// ends the stream but the state a surface renders still comes from the
  /// cursor, because concluding a run is over from the last event received is
  /// wrong exactly when it matters -- a retired log, a replaced host, a run
  /// parked on a person.
  watchRun({ runId, afterSequence = 0, onEvent, onEnd }) {
    if (!this.socketPath) throw new Error("no runtime configured");
    return watch(
      this.socketPath,
      { type: "attach", run_id: runId, after_sequence: afterSequence },
      { onEvent, onEnd },
    );
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
  /// Redirects a Run that is already moving.
  ///
  /// `steeringId` is the caller's idempotency key, minted per attempt rather
  /// than per keystroke: resending the same one is answered from what the
  /// Runtime recorded instead of redirecting twice.
  ///
  /// Acceptance here means the steer reached the Run, not that it was applied.
  /// The Runtime refuses to redirect while a tool call or an approval is
  /// unresolved, and the only honest evidence that it took is
  /// `run.steer.applied` in the Run's own log.
  async steer({ runId, steeringId, input }) {
    const reply = await this.#request({
      type: "steer",
      run_id: runId,
      steering_id: steeringId,
      input,
    });
    return reply;
  }

  async control({ action, runId }) {
    const allowed = new Set(["approve", "deny", "cancel", "resume"]);
    if (!allowed.has(action)) throw new Error(`unsupported control action: ${action}`);
    const reply = await this.#request({ type: action, run_id: runId });
    return reply;
  }

  /// Answers the MCP input request a suspended Run is parked on.
  ///
  /// Not folded into `control`: that call is a verb and a run id, and this one
  /// carries the identity of a specific question — the input id, the response
  /// binding version, the binding digest, and one response per pending request
  /// key. Every one of those is echoed from `mcp.input.required`, because the
  /// daemon checks all of them against the exact pending request and refuses
  /// anything else. Like the decisions above, the owner epoch and the full
  /// `RuntimeControlCommand` stay the daemon's to construct.
  async resolveMcpInput({ runId, inputId, inputVersion, bindingDigest, responses }) {
    const reply = await this.#request({
      type: "resolve_mcp_input",
      run_id: runId,
      input_id: inputId,
      input_version: inputVersion,
      binding_digest: bindingDigest,
      responses,
    });
    return reply;
  }
}

module.exports = { LocalRuntime, socketPathFor, EVENT_CURSOR_MAX_EVENTS, SESSION_HISTORY_MAX_TURNS };
