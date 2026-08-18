import { useCallback, useEffect, useState } from "react";
import { register } from "./registry";
import { since } from "./model";
import { LinkBanner } from "./Link";
import { useDesk, type Desk } from "../desk";
import { bridge, type WorkspaceEntry, type WorkspaceFile, type WorkspaceStatus } from "../runtime";

/// Paths the agent actually touched, read out of the durable log.
///
/// Not a diff and not a file watcher: it is what the runtime recorded a tool
/// being *asked* to do. That is a different claim from "this file changed", and
/// the difference is the honest one -- a call parked on an approval was asked
/// and never ran, and this shows it as asked.
function touched(desk: Desk): { path: string; tool: string; at: string | null; runId: string }[] {
  const seen = new Map<string, { path: string; tool: string; at: string | null; runId: string }>();
  for (const run of desk.runs) {
    for (const event of run.events) {
      if (event.type !== "model.tool_call") continue;
      const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
      const args = (call.arguments ?? {}) as Record<string, unknown>;
      const named = args.path ?? args.file ?? args.relative_path;
      if (typeof named !== "string" || named === "") continue;
      seen.set(`${named} ${run.id}`, {
        path: named,
        tool: String(call.name ?? ""),
        at: event.timestamp ?? null,
        runId: run.id,
      });
    }
  }
  return [...seen.values()].sort((a, b) => (b.at ?? "").localeCompare(a.at ?? ""));
}

function size(bytes: number | null): string {
  if (bytes === null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/// Which folder the agent works in, and how to change it.
///
/// Its own row above the listing because it is not part of browsing: it is the
/// containment boundary every read and write is checked against. The path is
/// shown in full -- a person about to hand an agent a directory should see
/// which one, not its last component.
function Folder({ status }: { status: WorkspaceStatus | null }) {
  const [chosen, setChosen] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  if (!status) return null;

  const choose = async () => {
    const api = bridge();
    if (!api?.chooseWorkspace) return;
    setBusy(true);
    const reply = await api.chooseWorkspace();
    setBusy(false);
    // A cancelled picker changed nothing and is not news. Only a folder that
    // was actually chosen gets a line, because that line is a promise about
    // what the next runtime will be given.
    if (reply.ok && reply.value.chosen) setChosen(reply.value.chosen);
  };

  return (
    <div className="folder">
      <div className="folder-row">
        <span className="mono">{status.root}</span>
        {status.choosable === false
          ? (
            <span className="note-inline">
              由环境变量固定，这个窗口改不了
            </span>
          )
          : (
            <button type="button" onClick={() => void choose()} disabled={busy}>
              {busy ? "选择中" : "选择工作目录"}
            </button>
          )}
      </div>
      {chosen && (
        <p className="note">
          下一个 Runtime 会用 <b>{chosen}</b>。Runtime 是在启动时读这个目录的，
          所以<b>重启 Runtime 之后才生效</b> —— 在设置里。现在跑着的这个仍然在
          上面那个目录里工作。
        </p>
      )}
    </div>
  );
}

function WorkspaceView() {
  const desk = useDesk();
  const [status, setStatus] = useState<WorkspaceStatus | null>(null);
  const [at, setAt] = useState("");
  const [entries, setEntries] = useState<WorkspaceEntry[]>([]);
  const [truncated, setTruncated] = useState(false);
  const [file, setFile] = useState<WorkspaceFile | null>(null);
  const [error, setError] = useState<string | null>(null);

  const open = useCallback(async (relative: string) => {
    const api = bridge();
    if (!api?.listFiles) return;
    const reply = await api.listFiles(relative);
    if (!reply.ok) { setError(reply.error); return; }
    setError(null);
    setAt(reply.value.path);
    setEntries(reply.value.entries);
    setTruncated(reply.value.truncated);
    setFile(null);
  }, []);

  useEffect(() => {
    const api = bridge();
    if (!api?.workspace) return;
    void api.workspace().then((reply) => {
      if (!reply.ok) return;
      setStatus(reply.value);
      if (reply.value.configured) void open("");
    });
  }, [open]);

  const show = async (entry: WorkspaceEntry) => {
    const api = bridge();
    if (!api) return;
    const next = at === "" ? entry.name : `${at}/${entry.name}`;
    if (entry.kind === "folder") { await open(next); return; }
    const reply = await api.readFile(next);
    if (!reply.ok) { setError(reply.error); return; }
    setError(null);
    setFile(reply.value);
  };

  const changed = touched(desk);

  if (status && !status.configured) {
    return (
      <div className="pane">
        <LinkBanner link={desk.link} />
        <div className="empty">
          这个应用不知道 Runtime 的工作目录在哪。
          <span className="sub">
            它挂在一个别人启动的 Runtime 上，而那个 Runtime 的工作目录没有告诉它 ——
            与其显示一个看着像的目录，不如说不知道。
          </span>
        </div>
      </div>
    );
  }


  return (
    <div className="pane split">
      <nav className="idx">
        <button type="button" className={at === "" ? "on" : ""} onClick={() => void open("")}>
          工作目录
        </button>
        {at !== "" && (
          <button
            type="button"
            onClick={() => void open(at.split("/").slice(0, -1).join("/"))}
          >
            上一层
          </button>
        )}
      </nav>
      <div className="body">
        <Folder status={status} />
        {error && <div className="err">{error}</div>}

        {file ? (
          <>
            <div className="raw-hd mono">
              {file.path}
              {` ・ ${size(file.size)}`}
              {file.truncated && " ・ 只显示前一段"}
            </div>
            {file.binary ? (
              <div className="empty">
                这是二进制文件。
                <span className="sub">不渲染它，比渲染成一屏乱码有用。</span>
              </div>
            ) : (
              <pre className="mono file">{file.text}</pre>
            )}
          </>
        ) : (
          <table className="rows">
            <thead>
              <tr><th>名字</th><th className="num">大小</th><th className="num">改动时间</th></tr>
            </thead>
            <tbody>
              {entries.map((entry) => (
                <tr
                  key={entry.name}
                  tabIndex={0}
                  role="button"
                  onClick={() => void show(entry)}
                  onKeyDown={(event) => { if (event.key === "Enter") void show(entry); }}
                >
                  <td className="p">
                    {entry.kind === "folder" ? "目录 " : entry.kind === "link" ? "链接 " : ""}
                    {entry.name}
                  </td>
                  <td className="num">{size(entry.size)}</td>
                  <td className="num" title={entry.modified ?? ""}>{since(entry.modified)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        {entries.length === 0 && !file && <div className="empty">这个目录是空的。</div>}
        {truncated && <div className="note"><span>目录太大，只列了前面一部分</span></div>}

        {changed.length > 0 && (
          <>
            <div className="note"><span>代理动过的路径</span></div>
            <table className="rows">
              <thead>
                <tr><th>路径</th><th>工具</th><th className="num">什么时候</th></tr>
              </thead>
              <tbody>
                {changed.slice(0, 20).map((entry) => (
                  <tr key={`${entry.runId}-${entry.path}`}>
                    <td className="p mono">{entry.path}</td>
                    <td>{entry.tool}</td>
                    <td className="num" title={entry.at ?? ""}>{since(entry.at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="note">
              <span>
                这是 Runtime 记录的「某个工具被要求做什么」，不是文件系统的改动记录 ——
                停在审批上的调用也在这里，它被要求过，但没有执行。
              </span>
            </p>
          </>
        )}
      </div>
    </div>
  );
}

register({
  id: "workspace",
  label: "工作区",
  group: "work",
  view: WorkspaceView,
  toolbar: () => <b>工作区</b>,
  commands: [
    { id: "workspace:open", title: "查看工作目录", hint: "Runtime 能动的那个目录" },
  ],
});
