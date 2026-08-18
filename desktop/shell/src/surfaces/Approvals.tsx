import { register } from "./registry";
import { effectLabel, sandboxLabel, shortId, since } from "./model";
import { LinkBanner } from "./Link";
import { useDesk } from "../desk";

/// The queue you actually work from.
///
/// Deliberately not a filter over the run list: "什么在等我" and "跑过什么" are
/// different questions, and answering the first by scanning the second is how
/// a decision sits unnoticed for an hour.
function ApprovalsView() {
  const desk = useDesk();
  const waiting = desk.runs.filter((run) => run.approval);
  const unresolved = desk.runs.filter(
    (run) =>
      (run.lifecycle.kind === "terminal" || run.lifecycle.kind === "retired") &&
      run.lifecycle.status === "indeterminate",
  );

  return (
    <div className="pane flow">
      <LinkBanner link={desk.link} />

      {waiting.map((run) => {
        const approval = run.approval!;
        return (
          <div key={run.id} className="gate">
            <div className="h">
              等你决定
              <span className="of"> ・ Run {shortId(run.id)} ・ {since(run.updatedAt)}</span>
            </div>
            {run.asked && <p className="q">{run.asked}</p>}
            <code className="cmd">
              {approval.toolName}({JSON.stringify(approval.arguments)})
            </code>
            {/* Why the runtime is asking, in the runtime's own terms. An
                approval without its effect class is a yes/no with the reason
                removed. */}
            <div className="facts">
              <span>{effectLabel(approval.effect)}</span>
              <span>{sandboxLabel(approval.sandbox)}</span>
              {approval.requiredScopes.map((scope) => (
                <span key={scope} className="mono">{scope}</span>
              ))}
            </div>
            <ol>
              <li className="pick" onClick={() => void desk.decide(run.id, "approve")}>
                <span className="k">1</span> 执行
              </li>
              <li onClick={() => void desk.decide(run.id, "deny")}>
                <span className="k">2</span> 不执行，让它换个做法
              </li>
              <li onClick={() => void desk.decide(run.id, "cancel")}>
                <span className="k">3</span> 结束这个 Run
              </li>
            </ol>
            <div className="bind mono">绑定 {approval.bindingDigest.slice(0, 16)}…・只对这一次调用有效</div>
          </div>
        );
      })}

      {unresolved.map((run) => (
        <div key={run.id} className="gate unknown">
          <div className="h">
            结果无法判定
            <span className="of"> ・ Run {shortId(run.id)} ・ {since(run.updatedAt)}</span>
          </div>
          <p className="q">
            工具执行过程中断了，Runtime 无法确定这次副作用到底有没有生效。
            它不会替你猜 —— 只有你能定。
          </p>
          <div className="bind">这个 Run 在你回答之前不会继续</div>
        </div>
      ))}

      {desk.link.state === "live" && waiting.length === 0 && unresolved.length === 0 && (
        <div className="empty">没有事情等你。</div>
      )}
    </div>
  );
}

register({
  id: "approvals",
  label: "待决定",
  group: "work",
  badge: (desk) => {
    const count = desk.runs.filter(
      (run) =>
        run.approval !== null ||
        ((run.lifecycle.kind === "terminal" || run.lifecycle.kind === "retired") &&
          run.lifecycle.status === "indeterminate"),
    ).length;
    return count === 0 ? undefined : count;
  },
  view: ApprovalsView,
  toolbar: () => <b>等你决定</b>,
  commands: [{ id: "approvals:open", title: "查看待决定", hint: "卡住工作的决定" }],
});
