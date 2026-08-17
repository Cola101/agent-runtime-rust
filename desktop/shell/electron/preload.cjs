// The only bridge between the renderer and the host.
//
// Deliberately tiny and explicit: every capability a surface can reach has to
// be named here first. A renderer that can call arbitrary IPC is a renderer
// where one badly-behaved surface reaches the whole machine.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("desk", {
  mounted: (surfaces) => ipcRenderer.send("shell:mounted", surfaces),
  endpoint: () => ipcRenderer.invoke("shell:endpoint"),
  runtime: {
    status: () => ipcRenderer.invoke("runtime:status"),
    connect: (options) => ipcRenderer.invoke("runtime:connect", options),
    readEvents: (request) => ipcRenderer.invoke("runtime:readEvents", request),
    submit: (request) => ipcRenderer.invoke("runtime:submit", request),
    control: (request) => ipcRenderer.invoke("runtime:control", request),
  },
});
