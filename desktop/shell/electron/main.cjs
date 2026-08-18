// The desktop shell's host process.
//
// Electron rather than a system webview: the client is meant to grow surfaces
// that embed arbitrary content — a board, a mailbox, a browser — and a bundled
// Chromium means one rendering target instead of one per platform. That is the
// trade Electron makes, and it is the right side of it for an app whose job is
// to host other things.
const { app, BrowserWindow, ipcMain, shell } = require("electron");
const path = require("node:path");
const fs = require("node:fs");
const grpcRuntime = require("./runtime.cjs");
const { LocalRuntime } = require("./localRuntime.cjs");
const { RuntimeProcess } = require("./runtimeProcess.cjs");
const { Credentials } = require("./credentials.cjs");
const { Workspace } = require("./workspace.cjs");

/// Where the shell expects to find a Runtime.
///
/// Two transports, chosen explicitly and never guessed:
///
///   RUNTIME_DESK_STATE_ROOT  a runtime-host on this machine, over its Unix
///                            socket. The adapter with `List`, so this is the
///                            one that can populate a run list.
///   RUNTIME_DESK_ENDPOINT    a runtime elsewhere, over gRPC with mTLS.
///
/// Neither has a default *from configuration*. A client that silently falls
/// back to some likely path or address is a client that will one day be
/// talking to a runtime nobody meant to reach.
///
/// An installed build is the one case that is not a guess: it has no
/// configuration to read, and the directory it falls back to is one it made
/// under its own data. That is not "some likely path" -- it is this app's own,
/// and the runtime living in it is one this app started. The rule still holds
/// where it was aimed: at reaching a runtime someone else owns.
let stateRoot =
  process.env.RUNTIME_DESK_STATE_ROOT ?? process.env.AGENT_RUNTIME_LOCAL_STATE_ROOT ?? null;
const endpoint = process.env.RUNTIME_DESK_ENDPOINT ?? null;

/// The operator bearer token for the remote transport, read once at launch.
///
/// It stays in the main process and is never sent to the renderer. A surface
/// rendering transcript content it did not author must not be able to read the
/// credential that transcript was fetched with.
const token = process.env.RUNTIME_DESK_TOKEN ?? null;

/// The runtime-host binary this app may start for itself.
///
/// Without it the app only ever attaches to a runtime someone else started,
/// which is the development setup. With it the app owns a process, and owning
/// one is what obliges it to stop it again on the way out.
/// In a packaged build the binary ships beside the app, so there is nothing to
/// configure. In a checkout there is no plausible guess -- debug and release
/// are different builds and picking one would be picking wrong half the time.
let runtimeBinary = process.env.RUNTIME_DESK_RUNTIME_BIN ?? null;

/// The folder the agent is allowed to work in.
///
/// App-owned, with a default under the app's own data, because a distributable
/// build cannot ask a person to set an environment variable before it will
/// start. The override exists for development, where the workspace is a
/// checkout rather than a folder this app made.
/// What the runtime this app starts may be asked to do.
///
/// Each of these still stops on a person: the runtime asks before every call
/// whose effect it cannot take back, and this window renders that question.
/// Withholding the scopes does not make the app safer -- it makes the agent
/// unable to do the work while the approval machinery it would have gone
/// through sits unused.
const DELEGATED_SCOPES = [
  "tool:workspace.read",
  "tool:workspace.write",
  "tool:shell.exec",
  // Delegation needs two things and the roles above are only one of them: the
  // `agent.*` family is installed when the roles are non-empty *and* the parent
  // holds this scope (`worker/src/lib.rs`, where the tool family is built).
  // Configured roles alone offer the model no way to use them -- checked by
  // running a turn and reading the tool list the provider was sent.
  "agent:spawn",
];

/// Roles a Run may delegate to.
///
/// Without this the host loads an empty role list, and a Run that tries to
/// delegate is refused because its role matches nothing. Every `agent.*` tool
/// and every `subagent.*` event is then unreachable -- including the tree this
/// client already renders, which could never have had anything in it.
///
/// Each role is narrower than the parent, not equal to it. A reviewer that can
/// write the workspace is not a reviewer, and a scope granted here is one the
/// parent cannot take back once it has delegated.
const SUBAGENT_ROLES = [
  {
    name: "reader",
    instructions:
      "Read what you are pointed at and report what is there. Quote the file and line "
      + "you are describing. If the answer is not in what you can read, say that rather "
      + "than inferring it.",
    delegated_scopes: ["tool:workspace.read"],
  },
  {
    name: "editor",
    instructions:
      "Make the change you were asked for and nothing beside it. Read before you write, "
      + "and report what you changed as a path and a description rather than as a claim "
      + "that it is finished.",
    delegated_scopes: ["tool:workspace.read", "tool:workspace.write"],
  },
  {
    name: "runner",
    instructions:
      "Run the command you were asked to run and report exactly what it printed, "
      + "including a failure. Do not interpret a non-zero exit as success.",
    delegated_scopes: ["tool:workspace.read", "tool:shell.exec"],
  },
];

const workspaceRoot = process.env.RUNTIME_DESK_WORKSPACE
  ?? process.env.AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT
  ?? null;

/// Set once the folder exists, which is why it is not built here: the default
/// lives under `app.getPath("userData")` and that is not readable until Electron
/// is ready.
let workspace = new Workspace(null);

let local = new LocalRuntime(stateRoot);
const runtime = new RuntimeProcess();
/// Provider configuration lives beside the app's own data, not in the state
/// root: the state root is the runtime's, and a client writing its settings
/// into it would be a second writer in a directory with one owner.
const credentials = new Credentials(path.join(app.getPath("userData"), "providers"));

function createWindow() {
  const win = new BrowserWindow({
    width: 1100,
    height: 720,
    minWidth: 640,
    minHeight: 420,
    title: "Runtime Desk",
    titleBarStyle: "hiddenInset",
    backgroundColor: "#131312",
    show: false,
    webPreferences: {
      // The renderer gets no Node and no direct IPC. Everything it may do
      // arrives through the preload's narrow surface, so a surface rendering
      // untrusted transcript content cannot reach the filesystem.
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });

  // Paint only once there is something to show, instead of flashing an empty
  // window while the bundle parses.
  win.once("ready-to-show", () => win.show());

  // Anything trying to open a new window goes to the real browser. A surface
  // that wants to embed a page will do it deliberately, not by side effect.
  win.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });

  win.loadFile(path.join(__dirname, "..", "dist", "index.html"));
  return win;
}

/// The renderer reports that it mounted.
///
/// Without it, "the window is white" has two indistinguishable causes: assets
/// that never loaded, and a UI that rendered nothing. This makes the process
/// say which, rather than asking a person to look at the window again.
ipcMain.on("shell:mounted", (_event, surfaces) => {
  console.log(`runtime-desk: shell mounted, ${surfaces} surface(s) registered`);
});

/// What the shell actually drew, once its first load came back.
///
/// "Mounted" only proves React ran. This proves the surfaces got real rows out
/// of a real runtime — the difference between a shell and a client, and the
/// difference this process should be able to state on its own rather than by
/// asking someone to look at the window.
ipcMain.on("shell:drew", (_event, summary) => {
  console.log(`runtime-desk: drew ${JSON.stringify(summary)}`);
});

/// Every call is a plain value in and a plain value out. The renderer never
/// holds a socket, a channel or a token.
///
/// Errors are returned as data rather than thrown across the bridge, because a
/// rejected IPC promise reaches the renderer as a mangled string and the
/// surfaces have to be able to say precisely what went wrong.
function guarded(handler) {
  return async (_event, ...args) => {
    try {
      return { ok: true, value: await handler(...args) };
    } catch (error) {
      return { ok: false, error: String(error?.message ?? error) };
    }
  };
}

ipcMain.handle("runtime:status", guarded(() => local.status()));
ipcMain.handle("runtime:probe", guarded(() => local.probe()));
ipcMain.handle("runtime:list", guarded(() => local.listRuns()));
ipcMain.handle("runtime:lifecycle", guarded(() => local.lifecycle()));
ipcMain.handle("runtime:startRuntime", guarded(() => local.start()));
ipcMain.handle("runtime:shutdown", guarded(() => local.shutdown()));
ipcMain.handle("runtime:events", guarded((request) => local.eventCursor(request)));
ipcMain.handle("runtime:submit", guarded((input) => local.submit(input)));
ipcMain.handle("runtime:control", guarded((request) => local.control(request)));
ipcMain.handle("runtime:steer", guarded((request) => local.steer(request)));
ipcMain.handle("runtime:resolveMcpInput", guarded((request) => local.resolveMcpInput(request)));
// Session operations. Every one of them is an owner request, so none carries an
// invocation and none of it is the renderer's to choose.
ipcMain.handle("session:start", guarded((request) => local.sessionStart(request)));
ipcMain.handle("session:continue", guarded((request) => local.sessionContinue(request)));
ipcMain.handle("session:read", guarded((request) => local.sessionRead(request)));
ipcMain.handle("session:list", guarded((request) => local.sessionList(request ?? {})));
ipcMain.handle("session:history", guarded((request) => local.sessionHistory(request)));

// Provider configuration. `list` answers with what a person may see; there is
// deliberately no call that returns a secret, because a bridge method that
// could return one is a bridge method that will one day be called by a surface
// rendering someone else's transcript.
/// Held-open streams, one per run being followed.
///
/// Not `guarded`, because these need the sender: an event has to go back to the
/// window that asked for it, and the handler owns a connection rather than
/// answering once. Everything else on this bridge is request/response, and
/// keeping these visibly different is better than widening `guarded` until it
/// covers two shapes.
const watchers = new Map();

ipcMain.handle("runtime:watch", (event, { runId, afterSequence = 0 } = {}) => {
  if (!runId) return { ok: false, error: "no run to watch" };
  if (watchers.has(runId)) return { ok: true, value: { watching: true } };
  const sender = event.sender;
  try {
    const watcher = local.watchRun({
      runId,
      afterSequence,
      onEvent: (streamed) => {
        if (!sender.isDestroyed()) sender.send("runtime:event", { runId, event: streamed });
      },
      onEnd: (reason) => {
        watchers.delete(runId);
        if (!sender.isDestroyed()) sender.send("runtime:watchEnded", { runId, reason });
      },
    });
    watchers.set(runId, watcher);
    return { ok: true, value: { watching: true } };
  } catch (error) {
    return { ok: false, error: String(error?.message ?? error) };
  }
});

ipcMain.handle("runtime:unwatch", (_event, runId) => {
  watchers.get(runId)?.stop();
  watchers.delete(runId);
  return { ok: true, value: {} };
});

// The workspace, read for the person. Every path is relative and contained;
// see `workspace.cjs` for why the check happens after `realpath`.
ipcMain.handle("workspace:status", guarded(() => workspace.status()));
ipcMain.handle("workspace:list", guarded((relative) => workspace.list(relative ?? "")));
ipcMain.handle("workspace:read", guarded((relative) => workspace.read(relative)));

/// Starts a runtime now, for the case that made this necessary: a freshly
/// installed app has no provider, so the first launch brings up a window with
/// nothing behind it. Once a provider is configured there is no reason to make
/// a person quit and reopen to get what they just configured.
///
/// Safe to ask twice -- `openRuntime` probes first and attaches rather than
/// starting a second host over one state root.
ipcMain.handle("runtime:launch", guarded(async () => {
  await openRuntime();
  return { started: runtime.running, owned: runtime.owned };
}));

ipcMain.handle("providers:list", guarded(() => credentials.list()));
ipcMain.handle("providers:save", guarded((request) => credentials.save(request)));
ipcMain.handle("providers:forget", guarded((id) => credentials.forget(id)));

// The remote transport, unchanged and still explicit. Kept separate rather
// than hidden behind the same calls: "this runtime is on my machine" and "this
// runtime is somewhere else, reached with a credential" are different enough
// that collapsing them would make the client vague about which one it is on.
ipcMain.handle("remote:status", guarded(() => grpcRuntime.status()));
ipcMain.handle("remote:readEvents", guarded((request) => grpcRuntime.readEvents(request, token)));
ipcMain.handle("remote:submit", guarded((request) => grpcRuntime.submit(request, token)));
ipcMain.handle("remote:control", guarded((request) => grpcRuntime.control(request, token)));

/// Brings a runtime up, or attaches to the one already there.
///
/// The order matters: probe first, and only start when nothing answers. A
/// state root with a runtime already on it belongs to whoever started it, and
/// this app has to know which of the two cases it is in before it can promise
/// anything about quitting.
/// Which folder the window may show.
///
/// Set from configuration when there is any, and otherwise only when this app
/// starts a runtime -- because then the folder is one it made. Attached to a
/// runtime someone else started, with no configuration, this app genuinely does
/// not know where that runtime's workspace is, and the surface says exactly
/// that rather than showing a plausible folder.
function openWorkspace({ mayDefault }) {
  const folder = workspaceRoot
    ?? (mayDefault ? path.join(app.getPath("userData"), "workspace") : null);
  if (!folder) return null;
  fs.mkdirSync(folder, { recursive: true });
  workspace = new Workspace(folder);
  return folder;
}

async function openRuntime() {
  if (!stateRoot) {
    console.log("runtime-desk: no RUNTIME_DESK_STATE_ROOT set — no local runtime");
    return;
  }
  const first = await local.probe();
  if (first.connected) {
    runtime.attach();
    const known = openWorkspace({ mayDefault: false });
    console.log(
      `runtime-desk: attached to a runtime already at ${first.socketPath}` +
        (known ? ` (workspace ${known})` : " (its workspace is not known to this app)"),
    );
    return;
  }
  if (!runtimeBinary) {
    console.log(`runtime-desk: no local runtime at ${first.socketPath} — ${first.error}`);
    return;
  }
  // Read once, here, and handed to the child. The secret exists in this
  // process for the length of a spawn and never reaches the renderer, the
  // config file, or a log line.
  // Written next to the routing file, for the same reason: it is derived state
  // the host reads at startup, and deriving it every launch keeps it from
  // disagreeing with the roles this app actually offers.
  const rolesFile = path.join(app.getPath("userData"), "providers", "subagent-roles.json");
  fs.mkdirSync(path.dirname(rolesFile), { recursive: true });
  fs.writeFileSync(rolesFile, `${JSON.stringify(SUBAGENT_ROLES, null, 2)}\n`, { mode: 0o600 });

  const routing = await credentials.routing();
  if (!routing) {
    console.log("runtime-desk: no provider configured — set one in 设置 before starting a runtime");
    return;
  }
  const folder = openWorkspace({ mayDefault: true });
  try {
    const pid = runtime.start({
      binary: runtimeBinary,
      stateRoot,
      env: {
        ...routing.env,
        AGENT_RUNTIME_LOCAL_MODEL_ROUTING_CONFIG: routing.file,
        AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT: folder,
        // The runtime binary is also the trusted workspace tool -- it re-execs
        // itself for that role. Pointing at the binary this app just spawned
        // means the two can never be different builds.
        AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN: runtimeBinary,
        // What the agent may be *asked* to do. Without this the host falls back
        // to `tool:workspace.read` alone, and an app that had passed its own
        // acceptance shipped an agent that could read a folder and nothing
        // else: no shell, no writes. The tools were compiled, sandboxed and
        // approval-gated, and never offered to the model at all.
        //
        // Granting a scope is not granting a use. `AGENT_RUNTIME_LOCAL_TOOL_CONSENT`
        // below keeps every call stopping on a person, which is the boundary
        // that matters and the one this window is built to show.
        AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES: DELEGATED_SCOPES.join(","),
        // Explicit rather than inherited from the host's default. A security
        // boundary that holds because nobody set a variable is a boundary that
        // moves the first time someone does.
        AGENT_RUNTIME_LOCAL_TOOL_CONSENT: "ask",
        AGENT_RUNTIME_LOCAL_SUBAGENT_CONFIG: rolesFile,
      },
    });
    console.log(`runtime-desk: started runtime-host (pid ${pid})`);
  } catch (error) {
    console.error(`runtime-desk: could not start a runtime — ${error.message}`);
    return;
  }
  // The socket appears a moment after the process does. Waiting here rather
  // than letting the window open onto "unreachable" and correct itself keeps
  // the first thing on screen true.
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const status = await local.probe();
    if (status.connected) {
      console.log(`runtime-desk: local runtime at ${status.socketPath}`);
      return;
    }
    if (!runtime.running) {
      console.error(
        `runtime-desk: runtime-host exited before it listened — ${runtime.log.slice(-3).join(" / ")}`,
      );
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  // Says where it looked. "Did not begin listening" without a path is a
  // message that sends the next person to read the runtime's source to find
  // out which socket was meant, and the answer is usually that the two sides
  // disagreed about the path rather than that nothing was listening.
  console.error(
    `runtime-desk: runtime-host did not begin listening at ${local.status().socketPath}` +
      ` (state root ${stateRoot})`,
  );
}

/// Stops the runtime this app started, and says what that cost.
///
/// Only the one it started. `stop` refuses on an attached runtime, which is
/// the case the development setup is in: quitting the window must not take
/// down a host someone else is using.
let stopping = null;
function closeRuntime() {
  // Held-open connections first. A drain that races sockets still reading the
  // log is a drain reporting on a Runtime that has clients attached to it.
  for (const watcher of watchers.values()) watcher.stop();
  watchers.clear();
  if (!stopping) {
    stopping = runtime.stop({ drain: () => local.shutdown() }).then((outcome) => {
      if (!outcome.stopped) return outcome;
      const report = outcome.report;
      console.log(
        `runtime-desk: runtime stopped${outcome.escalated ? " (forced)" : ""}` +
          (report
            ? ` — ${report.active_before_drain} active and ${report.queued_before_drain} queued before draining, ` +
              `${report.completed_during_drain} finished, ${report.interrupted} interrupted`
            : ""),
      );
      return outcome;
    });
  }
  return stopping;
}

/// Fills in what an installed build has no configuration for.
///
/// Only when packaged, and only what is missing: a checkout that forgot to set
/// a state root should hear so rather than silently get a second one under the
/// app's data, where its runs would be invisible next to the ones it meant.
function settleInstalledPaths() {
  if (!app.isPackaged) return;
  if (!stateRoot) {
    stateRoot = path.join(app.getPath("userData"), "state");
    fs.mkdirSync(stateRoot, { recursive: true });
    local = new LocalRuntime(stateRoot);
  }
  if (!runtimeBinary) {
    // `extraResource` puts it here. Not in `app.asar`: an archived file is not
    // executable, and a binary that cannot be spawned would surface as a
    // runtime that never listened.
    runtimeBinary = path.join(process.resourcesPath, "agent-runtime-host");
  }
}

app.whenReady().then(async () => {
  settleInstalledPaths();
  await openRuntime();

  if (endpoint) {
    try {
      const opened = grpcRuntime.connect({ endpoint });
      console.log(`runtime-desk: remote runtime at ${endpoint}${opened.secure ? " (mTLS)" : ""}`);
    } catch (error) {
      // Refusing to connect is not a reason to refuse to start: the window can
      // still show why, which is more useful than a process that exits.
      console.error(`runtime-desk: remote not connected — ${error.message}`);
    }
  }

  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

/// Quitting takes the runtime with it -- if this app started it.
///
/// The quit is held open for it. `before-quit` fires before the process tears
/// down, and letting it proceed while a drain is in flight is how a Run that
/// was one second from finishing becomes a Run that has to be recovered from a
/// Checkpoint on the next launch.
let quitting = false;
app.on("before-quit", (event) => {
  if (quitting || !runtime.owned) return;
  event.preventDefault();
  quitting = true;
  void closeRuntime().then(() => app.quit());
});

// A terminal interrupt is a quit. Without this, the ordinary way this app is
// stopped during development -- Ctrl+C where it was launched -- would leave the
// runtime it started still running, and the next launch would attach to an
// orphan instead of starting cleanly.
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => app.quit());
}
