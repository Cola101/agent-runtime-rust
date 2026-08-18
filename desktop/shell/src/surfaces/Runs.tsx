import { register } from "./registry";
import { costLabel, lifecycleLabel, lifecycleTone, since, shortId } from "./model";
import { LinkBanner } from "./Link";
import { useDesk } from "../desk";

function RunsToolbar() {
  const desk = useDesk();
  const waiting = desk.runs.filter((run) => run.lifecycle.kind === "waiting_approval").length;
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
  const rows = [...desk.runs].sort((a, b) =>
    (b.updatedAt ?? "").localeCompare(a.updatedAt ?? ""));

  return (
    <div className="pane">
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && rows.length === 0 && desk.listedAt !== null && (
        <div className="empty">
          这个 runtime-host 自启动以来还没有跑过 Run。
          <span className="sub">
            磁盘上可能仍有更早的 Run —— List 走的是内存里的顺序，重启后就空了。
          </span>
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
          <tbody>
            {rows.map((run) => (
              <tr
                key={run.id}
                className={run.id === desk.selected ? "on" : ""}
                onClick={() => { desk.select(run.id); desk.go("chat"); }}
              >
                <td className="p mono">{shortId(run.id)}</td>
                <td>
                  <span className={`dot t-${lifecycleTone(run.lifecycle)}`} />
                  {lifecycleLabel(run.lifecycle)}
                  {run.historyGap && <span className="flag">日志有缺口</span>}
                  {run.error && <span className="flag">读不出来・{run.error.code}</span>}
                </td>
                {/* The runtime does not store the prompt, so this column is
                    only ever filled for runs this client submitted itself. */}
                <td className="ask">{run.asked ?? <span className="dim">不是这台客户端发起的</span>}</td>
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
  commands: [{ id: "runs:open", title: "查看所有 Run", hint: "这个 Runtime 跑过什么" }],
});
