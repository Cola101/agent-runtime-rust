import { useEffect, useRef, useState } from "react";
import { register } from "./registry";
import { effectLabel, sandboxLabel, shortId, since } from "./model";
import { LinkBanner } from "./Link";
import { blocked, moveCursor, useDesk } from "../desk";
import type { RunView } from "../store";

/// The three answers, as buttons.
///
/// Rendered from one list so the digit shown beside an option and the digit
/// the shell dispatches come from the same place. A key hint written by hand
/// beside a handler bound somewhere else is how a screen ends up promising
/// keys that do nothing.
export const DECISIONS = [
  { key: "1", label: "执行", action: "approve" as const, lead: true, destructive: false },
  { key: "2", label: "不执行，让它换个做法", action: "deny" as const, lead: false, destructive: false },
  // Ending the Run is the only one of the three that destroys work: approve and
  // deny both leave the Run alive to carry on. A single unmodified key that
  // irreversibly ends a Run, in an application that takes focus when it opens,
  // is a key that will one day be pressed by accident -- so this one arms
  // first and acts on the second press.
  { key: "3", label: "结束这个 Run", action: "cancel" as const, lead: false, destructive: true },
];

export function Decisions({ run }: { run: RunView }) {
  const desk = useDesk();
  const [armed, setArmed] = useState<string | null>(null);

  // Any other choice disarms: an armed destructive key must not survive the
  // person deciding to do something else.
  const choose = (decision: (typeof DECISIONS)[number]) => {
    if (!decision.destructive) {
      setArmed(null);
      void desk.decide(run.id, decision.action);
      return;
    }
    if (armed === decision.key) {
      setArmed(null);
      void desk.decide(run.id, decision.action);
      return;
    }
    setArmed(decision.key);
  };

  return (
    <ol className="picks">
      {DECISIONS.map((decision) => (
        <li key={decision.key}>
          <button
            type="button"
            className={decision.lead ? "pick on" : "pick"}
            onClick={() => choose(decision)}
          >
            <kbd>{decision.key}</kbd>
            <span>
              {decision.label}
              {armed === decision.key && <b className="arm">　再按一次确认</b>}
            </span>
          </button>
        </li>
      ))}
    </ol>
  );
}

/// The queue you actually work from.
///
/// Deliberately not a filter over the run list: "什么在等我" and "跑过什么" are
/// different questions, and answering the first by scanning the second is how
/// a decision sits unnoticed for an hour.
function ApprovalsView() {
  const desk = useDesk();
  const queue = blocked(desk);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    box.current?.querySelector<HTMLElement>('[data-on="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [desk.selected]);

  return (
    <div className="pane flow" ref={box}>
      <LinkBanner link={desk.link} />

      {queue.map((run) => {
        const approval = run.approval;
        const on = run.id === desk.selected;
        return (
          <div
            key={run.id}
            data-on={on}
            className={`gate${approval ? "" : " unknown"}${on ? " sel" : ""}`}
            onClick={() => desk.select(run.id)}
          >
            <div className="h">
              {approval ? "等你决定" : "结果无法判定"}
              <span className="of"> ・ Run {shortId(run.id)} ・ {since(run.updatedAt)}</span>
            </div>
            {run.asked && <p className="q">{run.asked}</p>}
            {approval ? (
              <>
                <code className="cmd">
                  {approval.toolName}({JSON.stringify(approval.arguments)})
                </code>
                {/* Why the runtime is asking, in the runtime's own terms. An
                    approval without its effect class is a yes/no with the
                    reason removed. */}
                <div className="facts">
                  <span>{effectLabel(approval.effect)}</span>
                  <span>{sandboxLabel(approval.sandbox)}</span>
                  {approval.requiredScopes.map((scope) => (
                    <span key={scope} className="mono">{scope}</span>
                  ))}
                </div>
                <Decisions run={run} />
                <div className="bind mono">
                  绑定 {approval.bindingDigest.slice(0, 16)}…・只对这一次调用有效
                </div>
              </>
            ) : (
              <>
                <p className="q">
                  工具执行过程中断了，Runtime 无法确定这次副作用到底有没有生效。
                  它不会替你猜 —— 只有你能定。
                </p>
                <div className="bind">这个 Run 在你回答之前不会继续</div>
              </>
            )}
          </div>
        );
      })}

      {desk.link.state === "live" && queue.length === 0 && (
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
    const count = blocked(desk).length;
    return count === 0 ? undefined : count;
  },
  view: ApprovalsView,
  toolbar: () => <b>等你决定</b>,
  keys: [
    { key: "j", hint: "下一个", when: (d) => blocked(d).length > 1,
      run: (d) => moveCursor(d, blocked(d).map((r) => r.id), 1) },
    { key: "k", hint: "上一个", when: (d) => blocked(d).length > 1,
      run: (d) => moveCursor(d, blocked(d).map((r) => r.id), -1) },
    // Only the non-destructive decisions get a bare key. Ending a Run is
    // irreversible and stays behind the button, which arms before it acts.
    ...DECISIONS.filter((decision) => !decision.destructive).map((decision) => ({
      key: decision.key,
      hint: decision.label,
      // Only offered while the cursor is on a run that is actually asking.
      when: (d: import("../desk").Desk) =>
        blocked(d).some((run) => run.id === d.selected && run.approval !== null),
      run: (d: import("../desk").Desk) => {
        if (d.selected) void d.decide(d.selected, decision.action);
      },
    })),
  ],
  commands: [{ id: "approvals:open", title: "查看待决定", hint: "卡住工作的决定" }],
});
