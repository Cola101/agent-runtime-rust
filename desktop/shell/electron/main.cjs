// The desktop shell's host process.
//
// Electron rather than a system webview: the client is meant to grow surfaces
// that embed arbitrary content — a board, a mailbox, a browser — and a bundled
// Chromium means one rendering target instead of one per platform. That is the
// trade Electron makes, and it is the right side of it for an app whose job is
// to host other things.
const { app, BrowserWindow, Notification, dialog, ipcMain, shell } = require("electron");
const path = require("node:path");
const fs = require("node:fs");
const grpcRuntime = require("./runtime.cjs");
const childEnv = require("./childEnv.cjs");
const { LocalRuntime } = require("./localRuntime.cjs");
const { RuntimeProcess } = require("./runtimeProcess.cjs");
const { Credentials } = require("./credentials.cjs");
const { McpServers } = require("./mcpServers.cjs");
const { Workspace } = require("./workspace.cjs");
const { createAttention } = require("./attention.cjs");
const { bannerText } = require("./banner.cjs");

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

/// Where the chosen folder is remembered.
///
/// Read at launch and written when a person picks one. Its own file rather
/// than a key in the provider store: this is not a credential and does not
/// belong beside one.
function chosenWorkspaceFile() {
  return path.join(app.getPath("userData"), "workspace.json");
}

function readChosenWorkspace() {
  try {
    const stored = JSON.parse(fs.readFileSync(chosenWorkspaceFile(), "utf8"));
    const folder = typeof stored?.root === "string" ? stored.root : null;
    // A folder that has since been deleted or unmounted is not a folder. Left
    // unset rather than handed to a runtime that would refuse to start on it.
    return folder && fs.existsSync(folder) && fs.statSync(folder).isDirectory()
      ? folder
      : null;
  } catch {
    return null;
  }
}

/// The environment still wins.
///
/// `RUNTIME_DESK_WORKSPACE` is how a checkout is used during development, and
/// a stored choice quietly overriding it would make a dev runtime work in a
/// folder nobody pointed it at. The chooser writes the stored one, which is
/// what an installed app has.
const environmentWorkspace = process.env.RUNTIME_DESK_WORKSPACE
  ?? process.env.AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT
  ?? null;
let chosenWorkspace = null;

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
/// The MCP server list, beside the provider one and for the same reason.
const mcpServers = new McpServers(path.join(app.getPath("userData"), "mcp"));

/// Which MCP servers the runtime behind this window was actually started with.
///
/// Null means this app cannot say: it attached to a runtime someone else
/// started, or there is no runtime at all. The list is read at startup and
/// never re-read, so "configured" and "running" are different facts and the
/// only one this app can vouch for is what it handed over itself.
let mcpApplied = null;

/// One per process, not per window: what a person has already been told about
/// is a fact about the person, and a second window would otherwise repeat it.
const attention = createAttention();

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
      // Chromium stops a covered window's timers, and the store is a timer.
      // Measured, not assumed: with the default on, putting another app over
      // this window stopped the polling dead, so nothing was read and nothing
      // was reported — in precisely the situation a notification is for.
      backgroundThrottling: false,
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

/// One banner for one Run that cannot go on without a person, in the words the
/// queue itself uses. Nothing is translated on the way out: the tool name is
/// the runtime's, and "等你决定" is what the surface says about the same Run.
/// The words themselves live in banner.cjs, where they can be read back.
///
/// Clicking it lands on that Run. A notification that only raises the window
/// leaves the person to find what it was about, which on a queue of several is
/// close to not having said.
function raise(win, item) {
  const words = bannerText(item);
  // Nothing to say means nothing said. A blank banner is still an interruption.
  if (!words) return;
  const runId = String(item.runId ?? "");
  const banner = new Notification(words);
  banner.on("click", () => {
    if (win.isDestroyed()) return;
    if (win.isMinimized()) win.restore();
    win.show();
    // On macOS raising a window does not bring its app forward. A click on the
    // banner is the person asking for exactly that, which is what `steal` is.
    app.focus({ steal: true });
    win.focus();
    win.webContents.send("host:attend", runId);
  });
  banner.show();
  // Stated for the same reason the process states what the shell drew: a
  // client that interrupts someone has to be checkable without watching a
  // screen and waiting for a banner that may already have gone.
  console.log(`runtime-desk: raised ${banner.title} for run ${runId.slice(0, 8)}`);
}

/// What the window says is waiting on a person, every time that changes.
///
/// Reported repeatedly and on purpose: the window polls, and streams on top of
/// the poll, so the same parked Run arrives here many times a second. Deciding
/// which of those is worth interrupting someone for is this side's job, and
/// `attention.cjs` does it on the ids the runtime already wrote down.
///
/// The decision lives here for two reasons, and both are about this process
/// knowing something the renderer does not: Electron's Notification exists
/// only in the main process, and only the main process can say whether anyone
/// is looking at the window. The window reports what its poll read out of the
/// durable log — it never decides that something is new, and never decides to
/// interrupt anyone.
ipcMain.on("shell:waiting", (event, items) => {
  if (!Notification.isSupported()) return;
  const win = BrowserWindow.fromWebContents(event.sender);
  if (!win || win.isDestroyed()) return;
  for (const item of attention.arrived(items, win.isFocused())) raise(win, item);
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
ipcMain.handle("session:fork", guarded((request) => local.sessionFork(request)));
ipcMain.handle("session:rollback", guarded((request) => local.sessionRollback(request)));
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
ipcMain.handle("workspace:status", guarded(() => ({
  ...workspace.status(),
  // Whether this window can change it, and why not when it cannot. An app that
  // offered the chooser and then quietly did nothing would be worse than one
  // that says the environment is holding the folder.
  choosable: environmentWorkspace === null,
  fixedBy: environmentWorkspace === null ? null : "environment",
})));

/// Lets a person point the agent at their own folder.
///
/// The single largest thing between this app and being usable: without it the
/// agent works in a directory under the app's own data, and there was no way
/// to change that from the window at all -- only an environment variable set
/// before launch, which an installed app cannot ask anyone for.
///
/// Choosing is a grant. The folder is where every workspace read and write is
/// contained after `realpath`, so this is the boundary itself being moved, and
/// it is a deliberate act with a system dialog rather than a text field that
/// takes effect as it is typed.
///
/// It does not restart anything. `runtime-host` reads the root at startup, so
/// the new folder is what the *next* runtime gets; the window says so and the
/// restart control beside it is the one that costs something.
ipcMain.handle("workspace:choose", guarded(async () => {
  if (environmentWorkspace !== null) {
    return { chosen: null, reason: "environment" };
  }
  const picked = await dialog.showOpenDialog({
    title: "选择工作目录",
    properties: ["openDirectory", "createDirectory"],
    defaultPath: chosenWorkspace ?? app.getPath("home"),
  });
  if (picked.canceled || picked.filePaths.length === 0) {
    return { chosen: null, reason: "cancelled" };
  }
  const [folder] = picked.filePaths;
  fs.writeFileSync(
    chosenWorkspaceFile(),
    `${JSON.stringify({ root: folder }, null, 2)}\n`,
    { mode: 0o600 },
  );
  chosenWorkspace = folder;
  return { chosen: folder, reason: null };
}));
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

/// Restarts the runtime this app started, so a provider change takes effect.
///
/// The routing file, the MCP config and the delegated scopes are read by
/// `runtime-host` once, at startup. Changing a provider therefore needed the
/// whole app quit and reopened -- which is the app asking a person to work
/// around the fact that it already knows how to stop and start a runtime.
///
/// Refused rather than faked for a runtime this app did not start. `stop()`
/// only ends its own child, so a restart here would drain someone else's
/// runtime and then start a second host over the same state root.
///
/// The reopen is `mustOwn`: this app has just stopped its own runtime and
/// waited for the exit, so it must start another rather than attach to
/// whatever answers. Attaching would hand ownership away and quitting would
/// then leave the runtime running -- the one thing the quit path is for.
///
/// It drains first: the report says what was still in flight, and the runtime
/// comes back recovering from its own durable state rather than from nothing.
ipcMain.handle("runtime:restart", guarded(async () => {
  if (!runtime.owned) {
    return { restarted: false, reason: "not this app's runtime", report: null };
  }
  const stopped = await runtime.stop({ drain: () => local.shutdown() });
  await openRuntime({ mustOwn: true });
  return {
    restarted: runtime.running,
    reason: null,
    report: stopped.report ?? null,
    escalated: stopped.escalated === true,
  };
}));

/// The Run budget this app hands the runtime.
///
/// Read from the same constant the environment is built from, so the window
/// cannot show a cap the runtime is not holding the Run to. A number a person
/// sees their usage against is the difference between "the agent stopped" and
/// "it reached the limit you set".
ipcMain.handle("runtime:budget", guarded(() => childEnv.RUN_BUDGET));

ipcMain.handle("providers:list", guarded(() => credentials.list()));
ipcMain.handle("providers:save", guarded((request) => credentials.save(request)));
ipcMain.handle("providers:forget", guarded((id) => credentials.forget(id)));

// MCP servers. The list is this app's own configuration; `applied` is the only
// thing it knows about the runtime, and it is deliberately null rather than
// empty when this app did not start that runtime.
ipcMain.handle("mcp:list", guarded(() => ({ servers: mcpServers.list(), applied: mcpApplied })));
ipcMain.handle("mcp:save", guarded((request) => mcpServers.save(request)));
ipcMain.handle("mcp:forget", guarded((name) => mcpServers.forget(name)));

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
  const folder = environmentWorkspace
    ?? chosenWorkspace
    ?? (mayDefault ? path.join(app.getPath("userData"), "workspace") : null);
  if (!folder) return null;
  fs.mkdirSync(folder, { recursive: true });
  workspace = new Workspace(folder);
  return folder;
}

/// `mustOwn` is for the restart, and it exists to remove a state rather than to
/// report one. Attaching hands ownership away, and the quit path stops only what
/// this app owns -- so an app that attached to whatever answered on the state
/// root it had just cleared would leave the next runtime running when a person
/// closed the window. After a restart there is nothing this app should attach
/// to: it stopped its own runtime a moment ago and waited for the exit, so
/// anything answering is a stranger over the same state root, which is worth
/// saying rather than joining.
async function openRuntime({ mustOwn = false } = {}) {
  if (!stateRoot) {
    console.log("runtime-desk: no RUNTIME_DESK_STATE_ROOT set — no local runtime");
    return;
  }
  const first = await local.probe();
  if (first.connected && mustOwn) {
    console.log(
      `runtime-desk: something is already answering at ${first.socketPath} after a restart` +
        " — not attaching, because this app could not then stop it on quit",
    );
    return;
  }
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
    // A packaged app without its runtime is a broken download, and nothing in
    // this window can fix it. Worth saying, because the alternative is a
    // person editing settings that were never the problem.
    local.declined("no-binary", null);
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
    // Told to the window as well as to the console. This is the state a fresh
    // install is in, and the only screen every new person is guaranteed to see.
    local.declined("no-provider", null);
    console.log("runtime-desk: no provider configured — set one in 设置 before starting a runtime");
    return;
  }
  local.declined(null, null);
  // Written beside the routing file, and read by the runtime at startup only.
  // Null when no server is configured, so the variable stays unset rather than
  // naming an empty list.
  const mcp = mcpServers.config();
  const folder = openWorkspace({ mayDefault: true });
  try {
    const pid = runtime.start({
      binary: runtimeBinary,
      stateRoot,
      // Built in its own module so it can be required and read. It is the
      // object that decides what the agent can do, and every mistake in it so
      // far has been silent -- a missing scope, and the model was never offered
      // a tool at all.
      env: childEnv.runtimeEnv({ routing, mcp, workspace: folder, runtimeBinary, rolesFile }),
    });
    // Recorded only on the path that actually spawned. This is the whole basis
    // for the MCP section saying a server is live rather than merely saved.
    mcpApplied = mcp ? mcp.applied : [];
    console.log(
      `runtime-desk: started runtime-host (pid ${pid})` +
        (mcp ? ` with ${mcp.applied.length} MCP server(s)` : ""),
    );
  } catch (error) {
    local.declined("start-failed", error.message);
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
      const said = runtime.log.slice(-3).join(" / ");
      local.declined("start-failed", said || null);
      console.error(`runtime-desk: runtime-host exited before it listened — ${said}`);
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
  // Before the runtime starts, because the folder it is given is read once at
  // spawn. Reading it after would give the first runtime of every launch the
  // default and the next one the choice.
  chosenWorkspace = readChosenWorkspace();
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
