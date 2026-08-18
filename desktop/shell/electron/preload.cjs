// The only bridge between the renderer and the host.
//
// Deliberately tiny and explicit: every capability a surface can reach has to
// be named here first. A renderer that can call arbitrary IPC is a renderer
// where one badly-behaved surface reaches the whole machine.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("desk", {
  mounted: (surfaces) => ipcRenderer.send("shell:mounted", surfaces),
  drew: (summary) => ipcRenderer.send("shell:drew", summary),
  /// Everything the window can see waiting on a person, reported whole every
  /// time that changes. Reporting rather than requesting: the host decides
  /// what to raise, so the window cannot talk it into repeating itself.
  waiting: (items) => ipcRenderer.send("shell:waiting", items),
  /// The host asking for one waiting Run to be put in front of the person,
  /// because they clicked its notification.
  onAttend: (handler) => {
    // The IPC event never crosses the bridge: it carries a handle to the
    // sender, and a renderer that can reach one can reach every channel.
    const listener = (_event, runId) => handler(runId);
    ipcRenderer.on("host:attend", listener);
    return () => ipcRenderer.removeListener("host:attend", listener);
  },
  runtime: {
    status: () => ipcRenderer.invoke("runtime:status"),
    probe: () => ipcRenderer.invoke("runtime:probe"),
    list: () => ipcRenderer.invoke("runtime:list"),
    lifecycle: () => ipcRenderer.invoke("runtime:lifecycle"),
    startRuntime: () => ipcRenderer.invoke("runtime:startRuntime"),
    shutdown: () => ipcRenderer.invoke("runtime:shutdown"),
    restart: () => ipcRenderer.invoke("runtime:restart"),
    events: (request) => ipcRenderer.invoke("runtime:events", request),
    submit: (input) => ipcRenderer.invoke("runtime:submit", input),
    control: (request) => ipcRenderer.invoke("runtime:control", request),
    steer: (request) => ipcRenderer.invoke("runtime:steer", request),
    resolveMcpInput: (request) => ipcRenderer.invoke("runtime:resolveMcpInput", request),
    sessionStart: (request) => ipcRenderer.invoke("session:start", request),
    sessionContinue: (request) => ipcRenderer.invoke("session:continue", request),
    sessionFork: (request) => ipcRenderer.invoke("session:fork", request),
    sessionRollback: (request) => ipcRenderer.invoke("session:rollback", request),
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
    // Configured MCP servers, plus what the runtime behind this window was
    // started with. There is no call that asks the runtime whether a server
    // came up, because the runtime's local socket has none.
    mcpServers: () => ipcRenderer.invoke("mcp:list"),
    saveMcpServer: (request) => ipcRenderer.invoke("mcp:save", request),
    forgetMcpServer: (name) => ipcRenderer.invoke("mcp:forget", name),
  },
  remote: {
    status: () => ipcRenderer.invoke("remote:status"),
    readEvents: (request) => ipcRenderer.invoke("remote:readEvents", request),
    submit: (request) => ipcRenderer.invoke("remote:submit", request),
    control: (request) => ipcRenderer.invoke("remote:control", request),
  },
});
