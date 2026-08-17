import { useEffect, useState } from "react";
import { all, byId, commands } from "./surfaces/registry";
import { identity } from "./sample";
import "./surfaces/Chat";
import "./surfaces/Runs";
import "./surfaces/Approvals";
import "./surfaces/Settings";

const GROUPS = [
  { key: "work", title: "Work" },
  { key: "setup", title: "Setup" },
] as const;

/// The shell: a rail, one surface, a summoned drawer, a composer and a status
/// line. Adding a surface changes the registry, not this file.
export function App() {
  const [active, setActive] = useState("chat");
  const [palette, setPalette] = useState(false);
  const [drawer, setDrawer] = useState(false);
  const [menu, setMenu] = useState(false);

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
  const isChat = active === "chat";

  return (
    <div className="win">
      <div className="chrome">
        <span>agent-runtime-platform</span>
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
                  const count = s.badge?.();
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

          {/* Who you are acting as. Always visible, because a client that can
              reach more than one runtime must never leave that ambiguous. */}
          <div className="who">
            <button type="button" onClick={() => setMenu((open) => !open)}>
              <b>{identity.who}</b>
              <span>{identity.tenant} · {identity.application}</span>
            </button>
            {menu && (
              <div className="pop">
                <div className="hd">
                  <b>{identity.who}</b>
                  <span>tenant {identity.tenant} · application {identity.application}</span>
                </div>
                <div className="mi">Token expires in {identity.expiresInMinutes} min</div>
                <div className="mi sep">Renew credential</div>
                <div className="mi">Switch runtime…</div>
                <div className="mi sep">Disconnect</div>
              </div>
            )}
          </div>
        </aside>

        <main className="body">
          {Toolbar && <div className="tb"><Toolbar /></div>}
          {View ? <View /> : <div className="pane" />}

          {isChat && (
            <div className="write">
              <div className="in">
                reply, or redirect what it is doing
                <span className="caret" />
              </div>
            </div>
          )}
          <div className="state">
            <span className="now">waiting on you</span>
            <i>·</i><span>6.2k tokens</span>
            <i>·</i><span>$0.31</span>
            <span className="end">⌘I details · esc stop</span>
          </div>
        </main>

        {drawer && Drawer && (
          <aside className="drawer">
            <Drawer />
          </aside>
        )}
      </div>

      {palette && (
        <div className="scrim" onClick={() => setPalette(false)}>
          <div className="pal" onClick={(event) => event.stopPropagation()}>
            <div className="in">type a command</div>
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
  );
}
