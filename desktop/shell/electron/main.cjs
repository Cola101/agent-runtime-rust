// The desktop shell's host process.
//
// Electron rather than a system webview: the client is meant to grow surfaces
// that embed arbitrary content — a board, a mailbox, a browser — and a bundled
// Chromium means one rendering target instead of one per platform. That is the
// trade Electron makes, and it is the right side of it for an app whose job is
// to host other things.
const { app, BrowserWindow, ipcMain, shell } = require("electron");
const path = require("node:path");

/// Where the shell expects to find a Runtime.
///
/// Absent by default and never guessed. A client that silently falls back to
/// some default address is a client that will one day be pointed at a Runtime
/// nobody meant to reach.
const endpoint = process.env.RUNTIME_DESK_ENDPOINT ?? null;

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

app.whenReady().then(() => {
  console.log(
    endpoint
      ? `runtime-desk: runtime at ${endpoint}`
      : "runtime-desk: no RUNTIME_DESK_ENDPOINT set — running without a Runtime connection",
  );
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
