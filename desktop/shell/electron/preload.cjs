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
  },
  remote: {
    status: () => ipcRenderer.invoke("remote:status"),
    readEvents: (request) => ipcRenderer.invoke("remote:readEvents", request),
    submit: (request) => ipcRenderer.invoke("remote:submit", request),
    control: (request) => ipcRenderer.invoke("remote:control", request),
  },
});
