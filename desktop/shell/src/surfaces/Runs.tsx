import { register } from "./registry";
import { lifecycleLabel, lifecycleTone } from "./model";
import { runs } from "../sample";
import { useConnection } from "../useConnection";

function RunsToolbar() {
  return (
    <>
      <b>Runs</b>
      <span className="seg">
        <span className="on">all</span><span>waiting</span><span>unresolved</span><span>failed</span>
      </span>
      <span className="tb-r">⌕ filter</span>
    </>
  );
}

function RunsView() {
  const link = useConnection();
  return (
    <div className="pane">
      {link.state === "absent" && (
        <div className="offline">
          Not connected to a runtime — these rows are placeholders, not runs.
        </div>
      )}
      <table className="rows">
        <thead>
          <tr><th>Run</th><th>State</th><th>Tokens</th><th>Cost</th><th>When</th></tr>
        </thead>
        <tbody>
          {runs.map((run) => (
            <tr key={run.id}>
              <td className="p">{run.title}</td>
              <td>
                <span className={`dot t-${lifecycleTone(run.lifecycle)}`} />
                {lifecycleLabel(run.lifecycle)}
              </td>
              <td className="num">{run.tokens.toLocaleString()}</td>
              <td className="num">${(run.costCents / 100).toFixed(2)}</td>
              <td className="num">{run.when}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

register({
  id: "runs",
  label: "Runs",
  group: "work",
  badge: () => runs.length,
  view: RunsView,
  toolbar: RunsToolbar,
  commands: [
    { id: "runs:open", title: "Go to Runs", hint: "everything this runtime has executed" },
  ],
});
