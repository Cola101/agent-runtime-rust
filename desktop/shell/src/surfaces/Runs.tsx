import { useEffect, useRef } from "react";
import { register } from "./registry";
import { costLabel, lifecycleLabel, lifecycleTone, since, shortId } from "./model";
import { LinkBanner } from "./Link";
import { byRecency, moveCursor, useDesk } from "../desk";

function RunsToolbar() {
  const desk = useDesk();
  const waiting = desk.runs.filter((run) => run.approval).length;
  return (
    <>
      <b>Run</b>
      <span className="tb-r">
        {desk.link.state === "live" ? `共 ${desk.runs.length} 个` : "未连接"}
        {waiting > 0 && ` ・ ${waiting} 个等你决定`}
      </span>
    </>
  );
}

/// Everything the host has started since it came up.
///
/// Not "every run that exists": the daemon's List is an in-memory order, so a
/// restarted host reports nothing while the runs are still on disk. The empty
/// state says which of those it is, because a person who reads "没有 Run" about
/// a directory full of runs has been told something false.
function RunsView() {
  const desk = useDesk();
  const rows = byRecency(desk);
  const body = useRef<HTMLTableSectionElement>(null);

  // The keyboard cursor has to stay on screen or it is not a cursor.
  useEffect(() => {
    body.current?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [desk.selected]);

  return (
    <div className="pane">
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && rows.length === 0 && desk.listedAt !== null && (
        <div className="empty">
          这个状态目录里还没有 Run。
          <span className="sub">在对话里写一句话就开始。</span>
        </div>
      )}

      {rows.length > 0 && (
        <table className="rows">
          <thead>
            <tr>
              <th>Run</th><th>状态</th><th>问的是</th>
              <th className="num">token</th><th className="num">花费</th><th className="num">最后更新</th>
            </tr>
          </thead>
          <tbody ref={body}>
            {rows.map((run) => (
              <tr
                key={run.id}
                // Rows are reachable and operable from the keyboard. A clickable
                // <tr> with no tabindex and no key handler is a control that
                // only exists for a mouse.
                tabIndex={0}
                role="button"
                aria-selected={run.id === desk.selected}
                className={run.id === desk.selected ? "on" : ""}
                onClick={() => desk.select(run.id)}
                onDoubleClick={() => { desk.select(run.id); desk.go("chat"); }}
                onFocus={() => desk.select(run.id)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") { desk.select(run.id); desk.go("chat"); }
                }}
              >
                <td className="p mono">{shortId(run.id)}</td>
                <td>
                  <span className={`dot t-${lifecycleTone(run.lifecycle)}`} />
                  {lifecycleLabel(run.lifecycle)}
                  {run.historyGap && <span className="flag">日志有缺口</span>}
                  {run.truncated && <span className="flag">只读到前一段</span>}
                  {run.error && <span className="flag">读不出来・{run.error.code}</span>}
                </td>
                {/* From the durable record, so it is filled for every Run this
                    state root holds -- not only the ones this client started. */}
                <td className="ask" title={run.asked}>{run.asked}</td>
                <td className="num">{run.tokens.toLocaleString()}</td>
                <td className="num">{costLabel(run.costMicros)}</td>
                <td className="num" title={run.updatedAt ?? ""}>{since(run.updatedAt)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

register({
  id: "runs",
  label: "Run",
  group: "work",
  // Runs in flight, not runs that exist: the rail says how much is moving.
  badge: (desk) => {
    const live = desk.runs.filter((run) => run.lifecycle.kind === "running").length;
    return live === 0 ? undefined : live;
  },
  view: RunsView,
  toolbar: RunsToolbar,
  keys: [
    { key: "j", hint: "下一个", when: (d) => d.runs.length > 0,
      run: (d) => moveCursor(d, byRecency(d).map((r) => r.id), 1) },
    { key: "k", hint: "上一个", when: (d) => d.runs.length > 0,
      run: (d) => moveCursor(d, byRecency(d).map((r) => r.id), -1) },
    { key: "Enter", hint: "看转录", when: (d) => d.selected !== null, run: (d) => d.go("chat") },
  ],
  commands: [
    { id: "runs:open", title: "查看所有 Run", hint: "这个 Runtime 跑过什么" },
    { id: "runs:refresh", title: "重新读取 Run 列表", run: (d) => d.refresh() },
  ],
});
