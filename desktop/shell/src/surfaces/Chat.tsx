import { useEffect, useRef, useState } from "react";
import { register } from "./registry";
import {
  costLabel, effectLabel, eventNote, lifecycleLabel, lifecycleTone, sandboxLabel, shortId, since,
} from "./model";
import { LinkBanner } from "./Link";
import { DECISIONS, Decisions } from "./Approvals";
import { currentRun, useDesk, type Desk } from "../desk";
import type { RunEvent } from "../runtime";
import type { RunView } from "../store";

/// A tool call is two lines, not a card.
///
/// Boxing each one puts a border around every third element and the column
/// stops reading as a conversation.
function Act({ event }: { event: RunEvent }) {
  const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
  const args = call.arguments;
  return (
    <div className="act">
      <b>{String(call.name ?? "")}</b>
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
      <Decisions run={run} />
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
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // Follows the tail while you are at the tail, and stops the moment you
  // scroll up. A transcript that yanks itself down while you are reading is
  // worse than one that never scrolls.
  useEffect(() => {
    const node = scroller.current;
    if (!node || !pinned.current) return;
    node.scrollTop = node.scrollHeight;
  }, [run?.events.length, run?.id]);

  return (
    <div
      className="flow"
      ref={scroller}
      onScroll={(event) => {
        const node = event.currentTarget;
        pinned.current = node.scrollHeight - node.scrollTop - node.clientHeight < 40;
      }}
    >
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && !run && desk.listedAt !== null && (
        <div className="empty">还没有 Run。在下面写一句话就开始。</div>
      )}

      {run && (
        <>
          {run.truncated && <Note>这个 Run 的事件太多，只读到了前面一段</Note>}
          {run.historyGap && (
            <Note>
              更早的事件已被回收，这段转录不完整 —— 最早还能读到第 {run.earliestSequence} 条
            </Note>
          )}
          <div className="ask">{run.asked}</div>
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

/// The raw log for the run on screen. Sequence, type, digest, payload.
///
/// The rendered transcript is a reading of the log; this is the log. When the
/// two disagree the log is right, and there has to be somewhere to look.
function ChatDrawer() {
  const desk = useDesk();
  const run = currentRun(desk);
  const [open, setOpen] = useState<number | null>(null);
  if (!run) return <div className="empty">没有选中的 Run。</div>;
  return (
    <div className="raw">
      <div className="raw-hd mono">
        {shortId(run.id)} ・ 已提交到第 {run.highestSequence} 条
        {run.earliestSequence !== null && ` ・ 最早 ${run.earliestSequence}`}
      </div>
      {run.events.map((event) => (
        <div key={event.event_id || event.sequence} className="raw-row">
          <button type="button" onClick={() => setOpen(open === event.sequence ? null : event.sequence)}>
            <span className="seq mono">{event.sequence}</span>
            <span className="ty mono">{event.type}</span>
          </button>
          {open === event.sequence && (
            <pre className="mono">
              {JSON.stringify(event.payload, null, 2)}
              {"\n"}digest {event.digest.slice(0, 24)}…
            </pre>
          )}
        </div>
      ))}
      {run.events.length === 0 && <div className="empty">这个 Run 还没有事件。</div>}
    </div>
  );
}

/// The composer. Only Chat has one, because only Chat is a place where typing
/// a sentence is the action.
export function Composer() {
  const desk = useDesk();
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const [at, setAt] = useState(-1);
  const box = useRef<HTMLTextAreaElement>(null);
  const live = desk.link.state === "live";

  const send = async () => {
    const input = draft.trim();
    if (!input || !live || sending) return;
    setSending(true);
    setDraft("");
    setHistory((past) => [input, ...past]);
    setAt(-1);
    const failure = await desk.submit(input);
    setError(failure);
    setSending(false);
    // A new run is the one you want to watch. Clearing the cursor rather than
    // pointing it at the new id keeps "newest" honest if the submit failed.
    if (!failure) desk.select(null);
    box.current?.focus();
  };

  return (
    <div className="write">
      <div className="write-row">
        <textarea
          ref={box}
          className="in"
          rows={1}
          value={draft}
          disabled={!live || sending}
          placeholder={live ? "说一句话，或者告诉它换个做法" : "没有连上 Runtime"}
          onChange={(event) => { setDraft(event.target.value); setAt(-1); }}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              void send();
              return;
            }
            // History, only from an empty or unedited line, so it never eats a
            // cursor move inside something you are writing.
            if (event.key === "ArrowUp" && at + 1 < history.length &&
                (draft === "" || draft === history[at])) {
              event.preventDefault();
              setAt(at + 1);
              setDraft(history[at + 1]);
            } else if (event.key === "ArrowDown" && at > -1 && draft === history[at]) {
              event.preventDefault();
              setAt(at - 1);
              setDraft(at - 1 === -1 ? "" : history[at - 1]);
            }
          }}
        />
        <button type="button" className="send" disabled={!live || sending || !draft.trim()} onClick={() => void send()}>
          {sending ? "发送中" : "发送"}
        </button>
      </div>
      <div className="write-hint">
        <kbd>↵</kbd> 发送 ・ <kbd>⇧↵</kbd> 换行 ・ <kbd>↑</kbd> 上一条
      </div>
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
  const moving = run.lifecycle.kind === "running" || run.lifecycle.kind === "cancelling";
  return (
    <>
      <span className={`now t-${lifecycleTone(run.lifecycle)}`}>
        {lifecycleLabel(run.lifecycle)}
      </span>
      <i>・</i><span className="mono">{shortId(run.id)}</span>
      <i>・</i><span>{run.tokens.toLocaleString()} token</span>
      <i>・</i><span>{costLabel(run.costMicros)}</span>
      <i>・</i><span title={run.updatedAt ?? ""}>{since(run.updatedAt)}</span>
      {moving && (
        <button type="button" className="flat stop" onClick={() => void desk.decide(run.id, "cancel")}>
          停止
        </button>
      )}
    </>
  );
}

const asking = (desk: Desk) => currentRun(desk)?.approval != null;

register({
  id: "chat",
  label: "对话",
  group: "work",
  view: ChatView,
  drawer: ChatDrawer,
  drawerLabel: "原始事件",
  composer: Composer,
  status: ChatStatus,
  // Same rule as the queue: the irreversible one is not a bare key.
  keys: DECISIONS.filter((decision) => !decision.destructive).map((decision) => ({
    key: decision.key,
    hint: decision.label,
    when: asking,
    run: (desk: Desk) => {
      const run = currentRun(desk);
      if (run) void desk.decide(run.id, decision.action);
    },
  })),
  commands: [
    { id: "chat:open", title: "回到对话", hint: "当前 Run 的转录" },
    {
      id: "chat:cancel",
      title: "停止当前 Run",
      when: (desk) => {
        const run = currentRun(desk);
        return run?.lifecycle.kind === "running";
      },
      run: (desk) => {
        const run = currentRun(desk);
        if (run) void desk.decide(run.id, "cancel");
      },
    },
  ],
});
