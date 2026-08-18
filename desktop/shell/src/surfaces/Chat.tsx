import { useState } from "react";
import { register } from "./registry";
import {
  costLabel, effectLabel, eventNote, lifecycleLabel, lifecycleTone, sandboxLabel, shortId, since,
} from "./model";
import { LinkBanner } from "./Link";
import { currentRun, useDesk } from "../desk";
import type { RunEvent } from "../runtime";
import type { RunView } from "../store";

/// A tool call is two lines, not a card.
///
/// Boxing each one puts a border around every third element and the column
/// stops reading as a conversation.
function Act({ event }: { event: RunEvent }) {
  const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
  const name = String(call.name ?? "");
  const args = call.arguments;
  return (
    <div className="act">
      <b>{name}</b>
      <span className="out mono">{args ? JSON.stringify(args) : ""}</span>
    </div>
  );
}

/// Facts the runtime reports about the log itself — a chosen provider, a
/// resumed run, a retired history — drawn where they happened. A hairline and
/// a few words: never hidden, never loud.
function Note({ children }: { children: React.ReactNode }) {
  return <div className="note"><span>{children}</span></div>;
}

/// The only coloured thing on the screen.
///
/// The decision is bound to one call. There is deliberately no "approve
/// whatever is current" affordance, because that races a transcript that is
/// still moving.
function Gate({ run }: { run: RunView }) {
  const desk = useDesk();
  const approval = run.approval;
  if (!approval) return null;
  return (
    <div className="gate">
      <div className="h">等你决定</div>
      <code className="cmd">{approval.toolName}({JSON.stringify(approval.arguments)})</code>
      <div className="facts">
        <span>{effectLabel(approval.effect)}</span>
        <span>{sandboxLabel(approval.sandbox)}</span>
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
}

/// The transcript, rendered from the durable log.
///
/// Text deltas are joined into one block rather than drawn per event: the
/// runtime streams a word at a time and a person reads paragraphs.
function Transcript({ run }: { run: RunView }) {
  const blocks: React.ReactNode[] = [];
  let text = "";

  const flush = (key: string) => {
    if (!text) return;
    blocks.push(<div className="rep" key={`t-${key}`}><p>{text}</p></div>);
    text = "";
  };

  for (const event of run.events) {
    if (event.type === "model.output.delta") {
      text += String(event.payload.text ?? "");
      continue;
    }
    flush(String(event.sequence));
    if (event.type === "model.tool_call") {
      blocks.push(<Act event={event} key={event.event_id || event.sequence} />);
      continue;
    }
    const note = eventNote(event.type);
    if (note && event.type !== "approval.required") {
      blocks.push(
        <Note key={event.event_id || event.sequence}>
          {note} <span className="mono dim">{event.type}</span>
        </Note>,
      );
    }
  }
  flush("end");

  return <>{blocks}</>;
}

function ChatView() {
  const desk = useDesk();
  const run = currentRun(desk);

  return (
    <div className="flow">
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && !run && (
        <div className="empty">
          还没有 Run。在下面写一句话就开始。
        </div>
      )}

      {run && (
        <>
          {run.truncated && (
            <Note>这个 Run 的事件太多，只读到了前面一段</Note>
          )}
          {run.historyGap && (
            <Note>
              更早的事件已被回收，这段转录不完整 —— 最早还能读到第 {run.earliestSequence} 条
            </Note>
          )}
          {run.asked !== null ? (
            <div className="ask">{run.asked}</div>
          ) : (
            <Note>这个 Run 不是这台客户端发起的，问的是什么只有发起方知道</Note>
          )}
          {run.error ? (
            <div className="offline">
              这个 Run 的日志读不出来：<span className="mono">{run.error.code}</span>
              {run.error.message ? ` —— ${run.error.message}` : ""}
            </div>
          ) : (
            <Transcript run={run} />
          )}
          <Gate run={run} />
        </>
      )}
    </div>
  );
}

/// The composer. Only Chat has one, because only Chat is a place where typing
/// a sentence is the action.
export function Composer() {
  const desk = useDesk();
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const live = desk.link.state === "live";

  const send = async () => {
    const input = draft.trim();
    if (!input || !live) return;
    setDraft("");
    // A new run is the one you want to watch. Selecting it here rather than
    // letting "newest" drift means the transcript never jumps to a different
    // run while you are reading this one.
    const failure = await desk.submit(input);
    setError(failure);
    if (!failure) desk.select(null);
  };

  return (
    <div className="write">
      <textarea
        className="in"
        rows={1}
        value={draft}
        disabled={!live}
        placeholder={live ? "说一句话，或者告诉它换个做法" : "没有连上 Runtime"}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            void send();
          }
        }}
      />
      {error && <div className="err">{error}</div>}
    </div>
  );
}

/// The status line, which is about the run you are looking at rather than
/// about the app. An app that spends that row on its own name has wasted it.
export function ChatStatus() {
  const desk = useDesk();
  const run = currentRun(desk);
  if (!run) return <span className="now">—</span>;
  return (
    <>
      <span className={`now t-${lifecycleTone(run.lifecycle)}`}>{lifecycleLabel(run.lifecycle)}</span>
      <i>・</i><span className="mono">{shortId(run.id)}</span>
      <i>・</i><span>{run.tokens.toLocaleString()} token</span>
      <i>・</i><span>{costLabel(run.costMicros)}</span>
      <i>・</i><span>{since(run.updatedAt)}</span>
    </>
  );
}

register({
  id: "chat",
  label: "对话",
  group: "work",
  view: ChatView,
  composer: Composer,
  status: ChatStatus,
  commands: [{ id: "chat:open", title: "回到对话", hint: "当前 Run 的转录" }],
});
