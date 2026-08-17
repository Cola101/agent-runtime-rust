import { register } from "./registry";
import { blocked } from "../sample";

/// The queue you actually work from.
///
/// Deliberately not a filter over Runs: "what is blocked on me" and "what has
/// run" are different questions, and answering the first by scanning the
/// second is how a decision sits unnoticed for an hour.
function ApprovalsView() {
  return (
    <div className="pane flow">
      {blocked.map((item) => (
        <div key={item.runId} className={item.kind === "approval" ? "gate" : "gate unknown"}>
          <div className="h">
            {item.kind === "approval" ? "Waiting on you" : "Outcome unknown"}
            <span className="of"> · {item.runTitle}</span>
          </div>
          {item.kind === "approval" ? (
            <>
              <code className="cmd">{item.command}</code>
              <ol>
                <li className="pick"><span className="k">1</span> Run it</li>
                <li><span className="k">2</span> Run it, and stop asking for this tool here</li>
                <li><span className="k">3</span> No — say what to do instead</li>
              </ol>
              <div className="bind">bound to call {item.digest} — applies to that command only</div>
            </>
          ) : (
            <>
              <p className="q">{item.question}</p>
              <ol>
                <li className="pick"><span className="k">1</span> It happened — continue from there</li>
                <li><span className="k">2</span> It didn't — nothing landed</li>
                <li><span className="k">3</span> Can't tell — leave it unresolved</li>
              </ol>
              <div className="bind">this run does not continue until you answer</div>
            </>
          )}
        </div>
      ))}
      {blocked.length === 0 && <div className="empty">Nothing is waiting on you.</div>}
    </div>
  );
}

register({
  id: "approvals",
  label: "Approvals",
  group: "work",
  badge: () => (blocked.length === 0 ? undefined : blocked.length),
  view: ApprovalsView,
  toolbar: () => (<><b>Waiting on you</b><span className="tb-r">j/k move · ↵ decide</span></>),
  commands: [{ id: "approvals:open", title: "Go to Approvals", hint: "decisions blocking work" }],
});
