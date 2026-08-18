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
import { textOf, type SessionView } from "../session";
import { lineage, subagentsOf } from "../subagents";

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

function toolName(event: RunEvent): string {
  const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
  return String(call.name ?? "");
}

/// A run of tool calls, folded.
///
/// A turn that calls a tool eleven times used to be eleven blocks between two
/// sentences, and the sentences are what a person is reading. Folded, the row
/// still says *which* tools -- a fold that only said "11 calls" would have
/// replaced a wall of detail with no detail.
///
/// One call is not folded. Hiding a single line behind a control to reveal it
/// is not a saving.
function Acts({ events }: { events: RunEvent[] }) {
  const [open, setOpen] = useState(false);
  if (events.length === 1) return <Act event={events[0]} />;

  const counted = new Map<string, number>();
  for (const event of events) {
    const name = toolName(event);
    counted.set(name, (counted.get(name) ?? 0) + 1);
  }
  const named = [...counted.entries()]
    .map(([name, count]) => (count > 1 ? `${name} ×${count}` : name))
    .join("・");

  return (
    <div className="acts">
      <button type="button" className="fold" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="caret">{open ? "▾" : "▸"}</span>
        {events.length} 个工具调用
        <span className="mono dim">{named}</span>
      </button>
      {open && events.map((event) => (
        <Act event={event} key={event.event_id || event.sequence} />
      ))}
    </div>
  );
}

/// The transcript, rendered from the durable log.
///
/// Text deltas are joined into one block rather than drawn per event: the
/// runtime streams a word at a time and a person reads paragraphs. Consecutive
/// tool calls are folded for the same reason at a larger scale.
function Transcript({ run }: { run: RunView }) {
  const blocks: React.ReactNode[] = [];
  let text = "";
  let acts: RunEvent[] = [];

  const flushText = (key: string) => {
    if (!text) return;
    blocks.push(<div className="rep" key={`t-${key}`}><p>{text}</p></div>);
    text = "";
  };
  const flushActs = (key: string) => {
    if (acts.length === 0) return;
    blocks.push(<Acts events={acts} key={`a-${key}`} />);
    acts = [];
  };

  for (const event of run.events) {
    if (event.type === "model.output.delta") {
      // Text ends a run of calls: what the model says after using a tool is a
      // new part of the conversation, not more of the same fold.
      flushActs(String(event.sequence));
      text += String(event.payload.text ?? "");
      continue;
    }
    if (event.type === "model.tool_call") {
      flushText(String(event.sequence));
      acts.push(event);
      continue;
    }
    flushText(String(event.sequence));
    const note = eventNote(event.type);
    if (note && event.type !== "approval.required") {
      flushActs(String(event.sequence));
      blocks.push(
        <Note key={event.event_id || event.sequence}>
          {note} <span className="mono dim">{event.type}</span>
        </Note>,
      );
    }
  }
  flushText("end");
  flushActs("end");
  return <>{blocks}</>;
}

/// What this Run delegated.
///
/// Drawn beside the transcript rather than inside it: a delegation is not a
/// line the model said, it is work happening somewhere else. Each row links to
/// the child's own Run, because that is where what the child actually did
/// lives -- this side only knows what was asked, what came back, and what it
/// cost.
function Delegations({ run }: { run: RunView }) {
  const desk = useDesk();
  const rows = lineage(subagentsOf(run.events));
  if (rows.length === 0) return null;

  const running = rows.filter((row) => row.view.state.kind === "running").length;
  return (
    <div className="kids">
      <div className="kids-hd">
        子代理 {rows.length}
        {running > 0 && <span className="live">{running} 个在跑</span>}
      </div>
      {rows.map(({ view, depth }) => (
        <div className={`kid d${depth}`} key={view.id}>
          <div className="kid-top">
            <b>{view.role || "（未命名角色）"}</b>
            <span className={`kid-state s-${view.state.kind}`}>
              {view.state.kind === "requested" && "已请求"}
              {view.state.kind === "running" && "在跑"}
              {view.state.kind === "closed" && "被关掉"}
              {view.state.kind === "finished"
                && (view.state.error ? `失败・${view.state.status}` : lifecycleLabel({
                  kind: "terminal", status: view.state.status,
                }))}
            </span>
            {view.queued > 0 && <span className="kid-flag">{view.queued} 条排队</span>}
            {view.generation > 1 && <span className="kid-flag">第 {view.generation} 代</span>}
          </div>
          {view.asked && <div className="kid-ask">{view.asked}</div>}
          <div className="kid-facts mono">
            {view.forkedFrom && <span>从 {shortId(view.forkedFrom.id)} 的第 {view.forkedFrom.generation} 代分叉</span>}
            {view.tokens > 0 && <span>{view.tokens.toLocaleString()} token</span>}
            {view.costMicros > 0 && <span>{costLabel(view.costMicros)}</span>}
            {view.childRunId
              ? (
                <button
                  type="button"
                  className="flat"
                  onClick={() => { desk.select(view.childRunId!); desk.go("chat"); }}
                >
                  看它的 Run
                </button>
              )
              // Said rather than left blank: the id arrives with the terminal,
              // so its absence means the child has not finished, not that this
              // client lost it.
              : <span className="dim">还没有子 Run 可看</span>}
          </div>
        </div>
      ))}
    </div>
  );
}

/// The committed conversation.
///
/// Drawn from the Session's frozen transcripts rather than from the event log,
/// because those are the two different things they look like: the event log is
/// what happened while a Turn ran, and the transcript is what the runtime
/// carried into the next Turn as history. When a log is retired the events go
/// and the conversation stays.
function Turns({ session }: { session: SessionView }) {
  return (
    <>
      {session.turns.map((turn) => {
        const said = textOf(turn, "user");
        const back = textOf(turn, "assistant");
        return (
          <div className="turn" key={turn.digest || turn.turn_ordinal}>
            {said && <div className="ask">{said}</div>}
            {back && <div className="rep"><p>{back}</p></div>}
          </div>
        );
      })}
    </>
  );
}

function ChatView() {
  const desk = useDesk();
  // Two cursors meet on this surface, and the explicit one wins. Picking a Run
  // out of a list and coming here has to show that Run -- if the open
  // conversation could displace it, the list would be pointing at something
  // the transcript is not showing.
  const chosen = desk.selected
    ? desk.runs.find((candidate) => candidate.id === desk.selected) ?? null
    : null;
  const session = chosen ? null : desk.current;
  // Inside a conversation the only Run worth drawing under it is the Turn
  // still running. Falling back to "the newest Run anywhere" would append a
  // different conversation's transcript to this one.
  const live = session?.activeRunId
    ? desk.runs.find((candidate) => candidate.id === session.activeRunId) ?? null
    : null;
  const run = chosen ?? (session ? live : currentRun(desk));
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // Follows the tail while you are at the tail, and stops the moment you
  // scroll up. A transcript that yanks itself down while you are reading is
  // worse than one that never scrolls.
  useEffect(() => {
    const node = scroller.current;
    if (!node || !pinned.current) return;
    node.scrollTop = node.scrollHeight;
  }, [run?.events.length, run?.id, session?.turnCount, session?.sessionId]);

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

      {desk.link.state === "live" && !run && !session && desk.listedAt !== null && (
        <div className="empty">还没有对话。在下面写一句话就开始。</div>
      )}

      {session && <Turns session={session} />}

      {run && (
        <>
          {run.truncated && <Note>这个 Run 的事件太多，只读到了前面一段</Note>}
          {run.historyGap && (
            <Note>
              更早的事件已被回收，这段转录不完整 —— 最早还能读到第 {run.earliestSequence} 条
            </Note>
          )}
          {(!session || run.id === session.activeRunId) && <div className="ask">{run.asked}</div>}
          {run.error ? (
            <div className="offline">
              这个 Run 的日志读不出来：<span className="mono">{run.error.code}</span>
              {run.error.message ? ` —— ${run.error.message}` : ""}
            </div>
          ) : (
            <Transcript run={run} />
          )}
          <Delegations run={run} />
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
  /// A branch refuses a second Turn while one is in flight. Reading that off
  /// the head and disabling the box is the difference between a rule the
  /// person can see and a rule they discover by being refused.
  const turning = desk.current?.activeRunId != null;

  const send = async () => {
    const input = draft.trim();
    if (!input || !live || sending || turning) return;
    setSending(true);
    setDraft("");
    setHistory((past) => [input, ...past]);
    setAt(-1);
    const failure = await desk.send(input);
    setError(failure);
    setSending(false);
    // A new run is the one you want to watch. Clearing the cursor rather than
    // pointing it at the new id keeps "newest" honest if the send failed.
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
          disabled={!live || sending || turning}
          placeholder={
            !live ? "没有连上 Runtime"
              : turning ? "这轮还在跑，跑完再说下一句"
                : desk.current ? "接着说" : "说一句话，就开始一段对话"
          }
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
        <button type="button" className="send" disabled={!live || sending || turning || !draft.trim()} onClick={() => void send()}>
          {sending ? "发送中" : "发送"}
        </button>
      </div>
      <div className="write-hint">
        <kbd>↵</kbd> 发送 ・ <kbd>⇧↵</kbd> 换行 ・ <kbd>↑</kbd> 上一条
        {desk.current && (
          <button
            type="button"
            className="flat new"
            onClick={() => { desk.newConversation(); box.current?.focus(); }}
          >
            新对话
          </button>
        )}
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
  })).concat([{
    key: "n",
    hint: "新对话",
    // Nothing to leave when no conversation is open, and starting one is what
    // typing already does.
    when: (desk: Desk) => desk.current !== null,
    run: (desk: Desk) => desk.newConversation(),
  }]),
  commands: [
    { id: "chat:open", title: "回到对话", hint: "当前 Run 的转录" },
    {
      id: "chat:new",
      title: "开一段新对话",
      hint: "下一句话会开一段新的",
      when: (desk) => desk.current !== null,
      run: (desk) => desk.newConversation(),
    },
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
