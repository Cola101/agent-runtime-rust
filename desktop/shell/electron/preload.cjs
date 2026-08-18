// The only bridge between the renderer and the host.
//
// Deliberately tiny and explicit: every capability a surface can reach has to
// be named here first. A renderer that can call arbitrary IPC is a renderer
// where one badly-behaved surface reaches the whole machine.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("desk", {
  mounted: (surfaces) => ipcRenderer.send("shell:mounted", surfaces),
  drew: (summary) => ipcRenderer.send("shell:drew", summary),
  runtime: {
    status: () => ipcRenderer.invoke("runtime:status"),
    probe: () => ipcRenderer.invoke("runtime:probe"),
    list: () => ipcRenderer.invoke("runtime:list"),
    lifecycle: () => ipcRenderer.invoke("runtime:lifecycle"),
    startRuntime: () => ipcRenderer.invoke("runtime:startRuntime"),
    shutdown: () => ipcRenderer.invoke("runtime:shutdown"),
    events: (request) => ipcRenderer.invoke("runtime:events", request),
    submit: (input) => ipcRenderer.invoke("runtime:submit", input),
    control: (request) => ipcRenderer.invoke("runtime:control", request),
    steer: (request) => ipcRenderer.invoke("runtime:steer", request),
    resolveMcpInput: (request) => ipcRenderer.invoke("runtime:resolveMcpInput", request),
    sessionStart: (request) => ipcRenderer.invoke("session:start", request),
    sessionContinue: (request) => ipcRenderer.invoke("session:continue", request),
    sessionRead: (request) => ipcRenderer.invoke("session:read", request),
    sessionList: (request) => ipcRenderer.invoke("session:list", request),
    sessionHistory: (request) => ipcRenderer.invoke("session:history", request),
    // One way in. There is no `providers:get` and there is not meant to be:
    // the renderer can set a secret and can be told one exists, and has no
    // call that would hand it back.
    watch: (request) => ipcRenderer.invoke("runtime:watch", request),
    unwatch: (runId) => ipcRenderer.invoke("runtime:unwatch", runId),
    /// Returns its own unsubscribe. A renderer that could only add listeners
    /// would leak one per mount, and the leak would look like a transcript
    /// that gets faster and then wrong.
    onEvent: (handler) => {
      const wrapped = (_event, payload) => handler(payload);
      ipcRenderer.on("runtime:event", wrapped);
      return () => ipcRenderer.off("runtime:event", wrapped);
    },
    onWatchEnded: (handler) => {
      const wrapped = (_event, payload) => handler(payload);
      ipcRenderer.on("runtime:watchEnded", wrapped);
      return () => ipcRenderer.off("runtime:watchEnded", wrapped);
    },
    launch: () => ipcRenderer.invoke("runtime:launch"),
    workspace: () => ipcRenderer.invoke("workspace:status"),
    listFiles: (relative) => ipcRenderer.invoke("workspace:list", relative),
    readFile: (relative) => ipcRenderer.invoke("workspace:read", relative),
    providers: () => ipcRenderer.invoke("providers:list"),
    saveProvider: (request) => ipcRenderer.invoke("providers:save", request),
    forgetProvider: (id) => ipcRenderer.invoke("providers:forget", id),
  },
  remote: {
    status: () => ipcRenderer.invoke("remote:status"),
    readEvents: (request) => ipcRenderer.invoke("remote:readEvents", request),
    submit: (request) => ipcRenderer.invoke("remote:submit", request),
    control: (request) => ipcRenderer.invoke("remote:control", request),
  },
});
