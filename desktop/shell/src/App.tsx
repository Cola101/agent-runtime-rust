import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { all, byId, type KeyBinding } from "./surfaces/registry";
import { DeskContext, useDesk, type Desk } from "./desk";
import { DRAWER_KEY, PALETTE_KEY, SHELL_KEYS, keyLabel, type Shell } from "./shell-keys";
import { useRuntime } from "./store";
import { Palette } from "./Palette";
import "./surfaces/Chat";
import "./surfaces/Conversations";
import "./surfaces/Workspace";
import "./surfaces/Runs";
import "./surfaces/Approvals";
import "./surfaces/Settings";
import "./surfaces/Keys";

const GROUPS = [
  { key: "work", title: "工作" },
  { key: "setup", title: "配置" },
] as const;

/// True while the person is typing somewhere that owns the keyboard.
///
/// Without this, a surface that claims `1` for "approve" would fire it in the
/// middle of a sentence. Single-key bindings and a text field cannot both have
/// the keyboard, and the text field wins.
function typing(target: EventTarget | null): boolean {
  const node = target as HTMLElement | null;
  if (!node) return false;
  const tag = node.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || node.isContentEditable;
}

/// Who this client is acting as.
///
/// The local adapter has one built-in identity and no login: a runtime-host on
/// your machine is reached by owning the socket, not by presenting a
/// credential. Showing an account row here would invent an authentication
/// story the product does not have. The remote gRPC transport is where an
/// operator token belongs, and that is a different connection.
function Identity() {
  const desk = useDesk();
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const link = desk.link;

  useEffect(() => {
    if (!open) return;
    const away = (event: MouseEvent) => {
      if (!box.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", away);
    return () => document.removeEventListener("mousedown", away);
  }, [open]);

  const state =
    link.state === "live" ? "已连上"
    : link.state === "unreachable" ? "连不上"
    : link.state === "unconfigured" ? "未配置"
    : "无宿主";

  return (
    <div className="who" ref={box}>
      <button type="button" aria-expanded={open} onClick={() => setOpen((was) => !was)}>
        <b>本机 Runtime</b>
        <span>{state}</span>
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
          <button
            type="button"
            className="mi sep act"
            onClick={() => { desk.refresh(); setOpen(false); }}
          >
            重新连接
          </button>
        </div>
      )}
    </div>
  );
}

/// The keys the surface on screen is claiming right now, rendered from the
/// same declarations the dispatcher reads. A hint here is a key that works.
function KeyHints({ desk, surfaceId }: { desk: Desk; surfaceId: string }) {
  const surface = byId(surfaceId);
  const keys = (surface?.keys ?? []).filter((key) => key.when?.(desk) ?? true);
  if (keys.length === 0) return null;
  return (
    <span className="keys">
      {keys.map((key) => (
        <span key={`${key.meta ? "⌘" : ""}${key.key}`}>
          <kbd>{keyLabel(key)}</kbd> {key.hint}
        </span>
      ))}
    </span>
  );
}

/// The shell: a rail, one surface, a drawer, whatever composer that surface
/// declares, and a status line. Adding a surface changes the registry, not
/// this file.
export function App() {
  const store = useRuntime();
  const [active, setActive] = useState("chat");
  const [selected, setSelected] = useState<string | null>(null);
  const [palette, setPalette] = useState(false);
  const [drawer, setDrawer] = useState(false);
  const restore = useRef<HTMLElement | null>(null);
  // One step, not a history stack: where ⌘/ puts you back to after a look at
  // the reference. A stack would be a browser, and this window has no back.
  const before = useRef(active);

  const go = useCallback((id: string) => {
    const was = latest.current.active;
    if (was !== id) before.current = was;
    setActive(id);
  }, []);

  const desk = useMemo<Desk>(
    () => ({ ...store, selected, select: setSelected, go }),
    [store, selected, go],
  );
  // Read through a ref inside the key handler so the listener is installed
  // once rather than re-bound on every poll tick.
  const latest = useRef({ desk, active });
  latest.current = { desk, active };

  // Reported once, after the first load settles: what came out of the runtime.
  // "Mounted" only proves React ran; this proves the surfaces got real rows out
  // of a real runtime, which is the difference between a client and a shell and
  // the difference this process should be able to state on its own rather than
  // by asking someone to look at the window.
  const reported = useRef(false);
  useEffect(() => {
    if (reported.current || store.listedAt === null) return;
    reported.current = true;
    window.desk?.drew?.({
      link: store.link.state,
      runs: store.runs.length,
      waiting: store.runs.filter((run) => run.approval).length,
      // Counted apart from `waiting`: both park a run on a person, but one is
      // a decision about a tool call and the other is an MCP server asking for
      // content. Collapsing them would make the number mean neither.
      input: store.runs.filter((run) => run.mcpInput).length,
      events: store.runs.reduce((total, run) => total + run.events.length, 0),
      policies: store.policies.length,
      // Reported because the launcher's line is how this client is checked
      // without asking a person to look at a window. A conversation count of
      // zero beside a run count that is not is exactly the failure this batch
      // was about.
      sessions: store.sessions.length,
      turns: store.sessions.reduce((total, session) => total + session.turnCount, 0),
    });
  }, [store]);

  const openPalette = useCallback(() => {
    restore.current = document.activeElement as HTMLElement | null;
    setPalette(true);
  }, []);
  const closePalette = useCallback(() => {
    setPalette(false);
    restore.current?.focus();
  }, []);

  // The handle the shell's own keys act through. Everything on it is window
  // state; a surface key gets the Desk instead, and neither can reach the
  // other's half.
  const shell = useMemo<Shell>(
    () => ({
      togglePalette: () => (palette ? closePalette() : openPalette()),
      toggleDrawer: () => setDrawer((open) => !open),
      peek: (id) => go(latest.current.active === id ? before.current : id),
    }),
    [palette, openPalette, closePalette, go],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const meta = event.metaKey || event.ctrlKey;
      if (meta) {
        const claimed = SHELL_KEYS.find((key) => key.key === event.key.toLowerCase());
        // While the palette has the keyboard only the key that closes it still
        // answers. The rest act on a surface the person cannot currently see.
        if (claimed && (!palette || claimed === PALETTE_KEY)) {
          event.preventDefault();
          claimed.run(shell);
          return;
        }
      }
      if (palette) return;
      // ⌘I is not handled here any more: it is one of `SHELL_KEYS`, claimed
      // above with the rest of the chords the shell owns, so the reference page
      // prints it from the same declaration that dispatches it.
      //
      // And `meta` no longer returns early. A surface may claim a ⌘-key, and it
      // has to answer while the composer holds the focus -- the bare-key guard
      // below still stops a letter from firing into a sentence.
      if (event.altKey) return;

      const keys = byId(latest.current.active)?.keys ?? [];
      const fire = (binding: KeyBinding | undefined) => {
        if (!binding || !(binding.when?.(latest.current.desk) ?? true)) return;
        event.preventDefault();
        binding.run(latest.current.desk);
      };
      // A ⌘-key the surface claims runs even while a sentence is being typed.
      // That is the point of one: the composer holds the focus almost all the
      // time here, and a finder you have to click out of the box to open is a
      // finder nobody opens.
      if (meta) {
        fire(keys.find((key) => key.meta && key.key === event.key.toLowerCase()));
        return;
      }
      if (typing(event.target)) return;
      fire(keys.find((key) => !key.meta && key.key === event.key));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [palette, shell]);

  const surface = byId(active);
  const View = surface?.view;
  const Toolbar = surface?.toolbar;
  const Drawer = surface?.drawer;
  const Composer = surface?.composer;
  const Status = surface?.status;
  const showDrawer = drawer && Boolean(Drawer);

  return (
    <DeskContext.Provider value={desk}>
      <div className="win">
        <div className="chrome">
          <span>Runtime Desk</span>
          <button type="button" className="chrome-r" onClick={openPalette}>
            <kbd>{PALETTE_KEY.chord}</kbd> {PALETTE_KEY.hint}
          </button>
        </div>

        <div className={showDrawer ? "grid with-drawer" : "grid"}>
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
                        aria-current={s.id === active}
                        className={s.id === active ? "r on" : "r"}
                        onClick={() => go(s.id)}
                      >
                        {s.label}
                        {count !== undefined && <span className="n">{count}</span>}
                      </button>
                    );
                  })}
                </div>
              );
            })}
            <Identity />
          </aside>

          <main className="body">
            {Toolbar && <div className="tb"><Toolbar /></div>}
            {View ? <View /> : <div className="pane" />}
            {Composer && <Composer />}
            <div className="state">
              {Status ? <Status /> : <span className="now">—</span>}
              <span className="end">
                <KeyHints desk={desk} surfaceId={active} />
                {Drawer ? (
                  <button type="button" className="flat" onClick={() => DRAWER_KEY.run(shell)}>
                    <kbd>{DRAWER_KEY.chord}</kbd> {surface?.drawerLabel ?? "详情"}
                  </button>
                ) : (
                  // Said rather than shown as an inert key. A hint for a key
                  // that does nothing is the same mistake as invented data.
                  <span className="dim">这个面没有详情栏</span>
                )}
              </span>
            </div>
          </main>

          {showDrawer && Drawer && (
            <aside className="drawer">
              <div className="dhd">
                {surface?.drawerLabel ?? "详情"}
                <button type="button" className="flat" onClick={() => setDrawer(false)}>关闭</button>
              </div>
              <Drawer />
            </aside>
          )}
        </div>

        {palette && <Palette close={closePalette} />}
      </div>
    </DeskContext.Provider>
  );
}
