// Owning a runtime-host process, and letting go of one this app does not own.
//
// Two facts from `runtime-host` shape everything here.
//
// `shutdown` does not end the process. It drains through the Controller and
// stops taking work, and the accept loop in `ipc.rs::serve` keeps running --
// deliberately, because reads stay available on a stopped Runtime and a client
// looking at what one left behind is not asking it to do anything. The
// process's actual exit path is a signal, which drains through that same
// Controller. So stopping is: ask it to drain, then signal, then wait.
//
// And ownership is never inferred. A state root that already has a runtime
// listening belongs to whoever started it; killing that on quit would destroy
// work this app did not begin. `owned` is set only where this file spawns.
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");

/// How long to wait for the process to go after asking it to. Past this the
/// signal escalates. A quit that hangs forever on a wedged child is a quit
/// that gets force-killed by the user, which is strictly worse.
const TERM_DEADLINE_MS = 8_000;
const KILL_DEADLINE_MS = 2_000;

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

class RuntimeProcess {
  constructor() {
    this.child = null;
    this.owned = false;
    this.exit = null;
    /// The last lines the runtime printed. Kept because a runtime that refuses
    /// to start says why on stderr and then is gone -- and "no local runtime"
    /// with no reason is the least useful thing this app could report.
    this.log = [];
  }

  get running() {
    return this.child !== null && this.child.exitCode === null && this.child.signalCode === null;
  }

  /// Starts a runtime-host and takes ownership of it.
  ///
  /// `args` is a parameter rather than a literal `["serve"]` so the subcommand
  /// is the caller's, and so a test can drive this against a process it fully
  /// controls without a shell in between.
  ///
  /// Deliberately does not treat an existing socket *file* as a running
  /// runtime. A socket left by a crashed host and one held by a live host look
  /// identical on disk -- only connecting tells them apart, which is what the
  /// caller's probe does and what `LocalRuntimeDaemon::bind` does again before
  /// binding: it refuses only when something answers, and removes the file when
  /// nothing does. A file-existence check here would have made one crash block
  /// every later launch, with the debris the runtime was about to clean up.
  start({ binary, args = ["serve"], stateRoot, env = {} }) {
    if (this.child) throw new Error("a runtime is already running under this app");
    if (!binary) throw new Error("no runtime binary configured");
    const child = spawn(binary, args, {
      env: {
        ...process.env,
        ...env,
        AGENT_RUNTIME_LOCAL_STATE_ROOT: stateRoot,
        // Pinned so both sides compute the same socket path by construction.
        //
        // A state root too long for a `sockaddr` makes both the client and the
        // daemon fall back to a socket in the temp directory, and each of them
        // asks its own language for where that is: `os.tmpdir()` here,
        // `std::env::temp_dir()` there. Both read `TMPDIR`, so both are right
        // and they can still disagree -- an app launched without one connected
        // to nothing while the runtime it had just started was listening
        // perfectly well in `/var/folders`. Passing this makes the two answers
        // the same string rather than the same rule applied to two
        // environments.
        TMPDIR: os.tmpdir(),
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    this.child = child;
    this.owned = true;
    this.exit = new Promise((resolve) => {
      child.once("exit", (code, signal) => resolve({ code, signal }));
    });
    const keep = (chunk) => {
      for (const line of String(chunk).split("\n").filter(Boolean)) {
        this.log.push(line);
        if (this.log.length > 200) this.log.shift();
      }
    };
    child.stdout.on("data", keep);
    child.stderr.on("data", keep);
    child.once("error", (error) => keep(`runtime failed to start: ${error.message}`));
    return child.pid;
  }

  /// Attaches to a runtime this app did not start.
  ///
  /// Recorded so that quitting cannot stop it. The distinction exists because
  /// both cases look identical from the socket.
  attach() {
    this.owned = false;
  }

  /// Ends the runtime this app started, and reports what it took.
  ///
  /// `drain` is the owner-socket shutdown, passed in rather than called here so
  /// this file never needs to know the protocol. Its report is what the app
  /// logs: how much was still in flight when the person quit.
  async stop({ drain = null } = {}) {
    if (!this.owned || !this.child) return { stopped: false, reason: "not this app's runtime" };
    const child = this.child;
    let report = null;
    if (drain) {
      try {
        report = await drain();
      } catch (error) {
        // A runtime that cannot be asked to drain still has to be stopped.
        this.log.push(`drain refused: ${error.message}`);
      }
    }
    if (!this.running) {
      this.child = null;
      return { stopped: true, report, signal: null };
    }

    child.kill("SIGTERM");
    let outcome = await Promise.race([this.exit, wait(TERM_DEADLINE_MS).then(() => null)]);
    let escalated = false;
    if (outcome === null) {
      // Only after asking twice. SIGKILL skips the Controller entirely, which
      // is why it is last: whatever it leaves behind, the next start has to
      // recover from a Checkpoint instead of from a clean drain.
      escalated = true;
      child.kill("SIGKILL");
      outcome = await Promise.race([this.exit, wait(KILL_DEADLINE_MS).then(() => null)]);
    }
    this.child = null;
    return { stopped: true, report, escalated, exit: outcome };
  }
}

module.exports = { RuntimeProcess, TERM_DEADLINE_MS, KILL_DEADLINE_MS };
