import { all, commands, register } from "./registry";
import { KEYS_SURFACE, PALETTE_KEY, SHELL_KEYS, printedKey } from "../shell-keys";
import { useDesk } from "../desk";

/// One line of the reference: what you press and what it does.
///
/// `live` is the binding's own `when` evaluated a moment ago, not a guess
/// about the key. A reference that quietly listed a key whose condition does
/// not hold would be the same failure as a hint for a key nobody bound.
function Row({ chord, what, where, live = true }: {
  chord: string;
  what: string;
  where?: string;
  live?: boolean;
}) {
  return (
    <div className={live ? "krow" : "krow off"}>
      <span className="k"><kbd>{chord}</kbd></span>
      <span className="what">{what}</span>
      {where && <span className="where">{where}</span>}
      {!live && <span className="no">现在不生效</span>}
    </div>
  );
}

/// Every key and every command, read out of the same declarations the shell
/// dispatches from.
///
/// Nothing on this page is written here. The shell's keys come from
/// `SHELL_KEYS`, a surface's keys come from its own `keys:`, the commands come
/// from `commands()`, and the surfaces come from the registry — so a binding
/// that is renamed, conditioned or deleted changes this page in the same
/// commit. A hand-written cheatsheet is precisely the drift the registry
/// exists to prevent, and it would go stale the first time someone rebound a
/// key without thinking of this file.
function KeysView() {
  const desk = useDesk();
  const surfaces = all();

  return (
    <div className="pane keyref">
      <p className="lede">
        这一页由外壳和每个面自己声明的键位、命令生成，不是一份手写清单 —— 改了绑定，这一页跟着变。
      </p>

      <section>
        <h3>外壳</h3>
        <p className="sub">不管在哪个面，这几个键都在，光标在输入框里也一样。</p>
        {SHELL_KEYS.map((key) => {
          const where = key.where?.(surfaces) ?? [];
          return (
            <Row
              key={key.chord}
              chord={key.chord}
              what={key.hint}
              where={where.length > 0 ? `只有这些面有：${where.join("、")}` : undefined}
            />
          );
        })}
      </section>

      <section>
        <h3>各个面</h3>
        <p className="sub">
          这些键要两个条件同时成立：那个面正显示着，而且光标不在输入框里 ——
          光标一落进输入框，这一格的键就归输入框，按下去是打字，不是这里写的动作。
          标了「现在不生效」的，是这个键自己声明的条件此刻不成立。
        </p>
        {surfaces.map((surface) => {
          const keys = surface.keys ?? [];
          return (
            <div className="kface" key={surface.id}>
              <h4>{surface.label}</h4>
              {keys.map((key) => (
                <Row
                  key={`${surface.id}:${key.key}`}
                  chord={printedKey(key.key)}
                  what={key.hint}
                  live={key.when?.(desk) ?? true}
                />
              ))}
              {keys.length === 0 && <div className="krow none">这个面没有声明键位</div>}
            </div>
          );
        })}
      </section>

      <section>
        <h3>命令</h3>
        <p className="sub">
          命令没有各自的键位，都从 <kbd>{PALETTE_KEY.chord}</kbd> {PALETTE_KEY.hint} 里进。
        </p>
        {commands().map((command) => {
          const live = command.when?.(desk) ?? true;
          return (
            <div className={live ? "crow" : "crow off"} key={command.id}>
              <span className="what">{command.title}</span>
              <span className="s">{command.surfaceLabel}</span>
              {command.hint && <span className="d">{command.hint}</span>}
              {!live && <span className="no">现在不生效</span>}
            </div>
          );
        })}
      </section>

      <p className="lede foot">
        光标落在输入框里时的键位不在这一页：那些键只在框里存在，不由面声明，也就没有可以生成的来源。
        对话的输入框把它们写在框下面那一行。
      </p>
    </div>
  );
}

register({
  id: KEYS_SURFACE,
  label: "键位",
  group: "setup",
  view: KeysView,
  toolbar: () => <b>键位</b>,
  commands: [{ id: "keys:open", title: "键位速查", hint: "所有键位与命令" }],
});
