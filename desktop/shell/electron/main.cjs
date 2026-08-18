// The desktop shell's host process.
//
// Electron rather than a system webview: the client is meant to grow surfaces
// that embed arbitrary content — a board, a mailbox, a browser — and a bundled
// Chromium means one rendering target instead of one per platform. That is the
// trade Electron makes, and it is the right side of it for an app whose job is
// to host other things.
const { app, BrowserWindow, ipcMain, shell } = require("electron");
const path = require("node:path");
const grpcRuntime = require("./runtime.cjs");
const { LocalRuntime } = require("./localRuntime.cjs");

/// Where the shell expects to find a Runtime.
///
/// Two transports, chosen explicitly and never guessed:
///
///   RUNTIME_DESK_STATE_ROOT  a runtime-host on this machine, over its Unix
///                            socket. The adapter with `List`, so this is the
///                            one that can populate a run list.
///   RUNTIME_DESK_ENDPOINT    a runtime elsewhere, over gRPC with mTLS.
///
/// Neither has a default. A client that silently falls back to some likely
/// path or address is a client that will one day be talking to a runtime
/// nobody meant to reach.
const stateRoot =
  process.env.RUNTIME_DESK_STATE_ROOT ?? process.env.AGENT_RUNTIME_LOCAL_STATE_ROOT ?? null;
const endpoint = process.env.RUNTIME_DESK_ENDPOINT ?? null;

/// The operator bearer token for the remote transport, read once at launch.
///
/// It stays in the main process and is never sent to the renderer. A surface
/// rendering transcript content it did not author must not be able to read the
/// credential that transcript was fetched with.
const token = process.env.RUNTIME_DESK_TOKEN ?? null;

const local = new LocalRuntime(stateRoot);

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
ipcMain.handle("runtime:list", guarded(() => local.list()));
ipcMain.handle("runtime:events", guarded((request) => local.eventCursor(request)));
ipcMain.handle("runtime:submit", guarded((input) => local.submit(input)));
ipcMain.handle("runtime:control", guarded((request) => local.control(request)));

// The remote transport, unchanged and still explicit. Kept separate rather
// than hidden behind the same calls: "this runtime is on my machine" and "this
// runtime is somewhere else, reached with a credential" are different enough
// that collapsing them would make the client vague about which one it is on.
ipcMain.handle("remote:status", guarded(() => grpcRuntime.status()));
ipcMain.handle("remote:readEvents", guarded((request) => grpcRuntime.readEvents(request, token)));
ipcMain.handle("remote:submit", guarded((request) => grpcRuntime.submit(request, token)));
ipcMain.handle("remote:control", guarded((request) => grpcRuntime.control(request, token)));

app.whenReady().then(async () => {
  if (stateRoot) {
    const status = await local.probe();
    console.log(
      status.connected
        ? `runtime-desk: local runtime at ${status.socketPath}`
        : `runtime-desk: no local runtime at ${status.socketPath} — ${status.error}`,
    );
  } else {
    console.log("runtime-desk: no RUNTIME_DESK_STATE_ROOT set — no local runtime");
  }

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
