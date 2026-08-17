// The desktop shell's host process.
//
// Electron rather than a system webview: the client is meant to grow surfaces
// that embed arbitrary content — a board, a mailbox, a browser — and a bundled
// Chromium means one rendering target instead of one per platform. That is the
// trade Electron makes, and it is the right side of it for an app whose job is
// to host other things.
const { app, BrowserWindow, ipcMain, shell } = require("electron");
const path = require("node:path");
const runtime = require("./runtime.cjs");

/// Where the shell expects to find a Runtime.
///
/// Absent by default and never guessed. A client that silently falls back to
/// some default address is a client that will one day be pointed at a Runtime
/// nobody meant to reach.
const endpoint = process.env.RUNTIME_DESK_ENDPOINT ?? null;

/// The operator bearer token, read once at launch.
///
/// It stays in the main process and is never sent to the renderer. A surface
/// rendering transcript content it did not author must not be able to read the
/// credential that transcript was fetched with.
const token = process.env.RUNTIME_DESK_TOKEN ?? null;

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

ipcMain.handle("shell:endpoint", () => endpoint);

// The renderer asks; the main process owns the connection. Every reply is a
// plain value — the renderer never holds a channel, a stream or a token.
ipcMain.handle("runtime:status", () => runtime.status());
ipcMain.handle("runtime:connect", (_e, options) => runtime.connect(options));
ipcMain.handle("runtime:readEvents", (_e, request) => runtime.readEvents(request, token));
ipcMain.handle("runtime:submit", (_e, request) => runtime.submit(request, token));
ipcMain.handle("runtime:control", (_e, request) => runtime.control(request, token));

app.whenReady().then(() => {
  console.log(
    endpoint
      ? `runtime-desk: runtime at ${endpoint}`
      : "runtime-desk: no RUNTIME_DESK_ENDPOINT set — running without a Runtime connection",
  );
  if (endpoint) {
    try {
      const opened = runtime.connect({ endpoint });
      console.log(`runtime-desk: connected${opened.secure ? " over mTLS" : " (loopback, plaintext)"}`);
    } catch (error) {
      // Refusing to connect is not a reason to refuse to start: the window can
      // still show why, which is more useful than a process that exits.
      console.error(`runtime-desk: not connected — ${error.message}`);
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
