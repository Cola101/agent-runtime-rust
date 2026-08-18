import { useEffect, useMemo, useState } from "react";
import { all, byId, commands } from "./surfaces/registry";
import { DeskContext, useDesk, type Desk } from "./desk";
import { useRuntime } from "./store";
import "./surfaces/Chat";
import "./surfaces/Runs";
import "./surfaces/Approvals";
import "./surfaces/Settings";

const GROUPS = [
  { key: "work", title: "工作" },
  { key: "setup", title: "配置" },
] as const;

/// Who this client is acting as.
///
/// The local adapter has one built-in identity and no login: a runtime-host on
/// your machine is reached by owning the socket, not by presenting a
/// credential. Showing a fake account row here would invent an authentication
/// story the product does not have. The remote gRPC transport is where an
/// operator token belongs, and that is a different connection.
function Identity({ open, toggle }: { open: boolean; toggle(): void }) {
  const desk = useDesk();
  const link = desk.link;
  return (
    <div className="who">
      <button type="button" onClick={toggle}>
        <b>本机 Runtime</b>
        <span>
          {link.state === "live" && "已连上"}
          {link.state === "unreachable" && "连不上"}
          {link.state === "unconfigured" && "未配置"}
          {link.state === "no-bridge" && "无宿主"}
        </span>
      </button>
      {open && (
        <div className="pop">
          <div className="hd">
            <b>本机 Runtime</b>
            <span>没有账号，也没有登录 —— 能连上这个 socket 就是凭据</span>
          </div>
          <div className="mi mono">
            {link.state === "live" || link.state === "unreachable" ? link.socketPath : "未配置"}
          </div>
          <div className="mi sep" onClick={() => desk.refresh()}>重新连接</div>
        </div>
      )}
    </div>
  );
}

/// The shell: a rail, one surface, a summoned drawer, whatever composer that
/// surface declares, and a status line. Adding a surface changes the registry,
/// not this file.
export function App() {
  const store = useRuntime();
  const [active, setActive] = useState("chat");
  const [selected, setSelected] = useState<string | null>(null);
  const [palette, setPalette] = useState(false);
  const [drawer, setDrawer] = useState(false);
  const [menu, setMenu] = useState(false);

  const desk = useMemo<Desk>(
    () => ({ ...store, selected, select: setSelected, go: setActive }),
    [store, selected],
  );

  // Reported once, after the first load settles: what came out of the runtime,
  // so the host process can say whether this is a client or a shell.
  const reported = useMemo(() => ({ done: false }), []);
  useEffect(() => {
    if (reported.done || store.listedAt === null) return;
    reported.done = true;
    window.desk?.drew?.({
      link: store.link.state,
      runs: store.runs.length,
      waiting: store.runs.filter((run) => run.approval).length,
      events: store.runs.reduce((total, run) => total + run.events.length, 0),
      policies: store.policies.length,
    });
  }, [store, reported]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const meta = event.metaKey || event.ctrlKey;
      if (meta && event.key === "k") { event.preventDefault(); setPalette((open) => !open); }
      else if (meta && event.key === "i") { event.preventDefault(); setDrawer((open) => !open); }
      else if (event.key === "Escape") { setPalette(false); setMenu(false); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const surface = byId(active);
  const View = surface?.view;
  const Toolbar = surface?.toolbar;
  const Drawer = surface?.drawer;
  const Composer = surface?.composer;
  const Status = surface?.status;

  return (
    <DeskContext.Provider value={desk}>
      <div className="win">
        <div className="chrome">
          <span>Runtime Desk</span>
          <span className="chrome-r">⌘K</span>
        </div>

        <div className={drawer && Drawer ? "grid with-drawer" : "grid"}>
          <aside className="rail">
            {GROUPS.map((group) => {
              const entries = all().filter((s) => s.group === group.key);
              if (entries.length === 0) return null;
              return (
                <div className="rgroup" key={group.key}>
                  <h2>{group.title}</h2>
                  {entries.map((s) => {
                    const count = s.badge?.(desk);
                    return (
                      <button
                        key={s.id}
                        type="button"
                        className={s.id === active ? "r on" : "r"}
                        onClick={() => setActive(s.id)}
                      >
                        {s.label}
                        {count !== undefined && <span className="n">{count}</span>}
                      </button>
                    );
                  })}
                </div>
              );
            })}
            <Identity open={menu} toggle={() => setMenu((open) => !open)} />
          </aside>

          <main className="body">
            {Toolbar && <div className="tb"><Toolbar /></div>}
            {View ? <View /> : <div className="pane" />}
            {Composer && <Composer />}
            <div className="state">
              {Status ? <Status /> : <span className="now">—</span>}
              <span className="end">⌘I 详情</span>
            </div>
          </main>

          {drawer && Drawer && <aside className="drawer"><Drawer /></aside>}
        </div>

        {palette && (
          <div className="scrim" onClick={() => setPalette(false)}>
            <div className="pal" onClick={(event) => event.stopPropagation()}>
              <div className="in">输入命令</div>
              {commands().map((command) => (
                <button
                  key={command.id}
                  type="button"
                  className="it"
                  onClick={() => { setActive(command.surface); setPalette(false); }}
                >
                  <span>{command.title}</span>
                  {command.hint && <span className="d">{command.hint}</span>}
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </DeskContext.Provider>
  );
}
