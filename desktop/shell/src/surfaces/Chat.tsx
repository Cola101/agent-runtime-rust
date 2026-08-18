import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { register } from "./registry";
import {
  belongsInConversation, callLine, costLabel, doing, effectLabel, elapsed, eventNote,
  eventWords,
  size,
  knownEvent, lifecycleLabel, lifecycleTone, sandboxLabel, shortId, since,
} from "./model";
import { LinkBanner } from "./Link";
import { DECISIONS, Decisions } from "./Approvals";
import { closeFind, findOpened, has, openFind, watchFind } from "./find";
import { mentionAt, narrow, walkWorkspace, withMention } from "../mentions";
import { McpInputForm } from "./McpInput";
import { currentRun, useDesk, type Desk } from "../desk";
import { bridge, type RunEvent } from "../runtime";
import type { RunView } from "../store";
import { textOf, type SessionView } from "../session";
import { lineage, subagentsOf, type SubagentState } from "../subagents";
import { Mark } from "./Mark";
import { WriteReview } from "./WriteReview";

/// What one tool call draws: the tool's name, and its arguments as the line
/// shows them. The fold asks this what it contains and the line renders from
/// it, so a fold cannot decide it holds no match over a line that draws one.
function callSpans(event: RunEvent): [string, string] {
  const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
  const name = String(call.name ?? "");
  const args = (call.arguments ?? {}) as Record<string, unknown>;
  // A write's argument is the file, and the result below this line already
  // draws it. Spelling it out here would print the whole new content twice,
  // once as an argument and once as what was written.
  if (name === "workspace.write_text" && typeof args.path === "string") return [name, args.path];
  return [name, call.arguments ? JSON.stringify(args) : ""];
}

/// What a finished call answered with.
///
/// The trusted workspace tool speaks two shapes and this reads both: a shell
/// call answers with `exit_code`, `stdout` and `stderr`; a file read or write
/// answers with `path`, `text` and `bytes`. Between them they are every tool
/// this app installs, and they are the reason someone approved the call in the
/// first place. A tool that answers in some third shape says nothing here
/// rather than having its content guessed at -- the raw-event drawer has it.
///
/// A non-zero exit is not an error event -- the call ran and the command said
/// no -- so it is drawn as the command's own answer rather than as a failure
/// of the runtime.
/// One MCP `content` array, as text.
///
/// The MCP protocol says a tool answers with a list of parts, each with a
/// `type`. Text parts are the answer; a part this build cannot draw is named
/// rather than dropped, because a transcript showing only the text of a reply
/// that also carried an image is quietly hiding half of it.
function mcpParts(content: unknown): string {
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      const shape = (part ?? {}) as Record<string, unknown>;
      if (typeof shape.text === "string") return shape.text;
      const kind = typeof shape.type === "string" ? shape.type : "未知";
      return `（这个版本画不了的 ${kind} 部分）`;
    })
    .join("\n");
}

function said(result: RunEvent | undefined): {
  exit: number | null; out: string; err: string; cut: boolean; bytes: number | null;
} | null {
  if (!result) return null;
  const parts = mcpParts(result.payload.content);
  if (parts) return { exit: null, out: parts, err: "", cut: false, bytes: null };
  const content = (result.payload.content ?? {}) as Record<string, unknown>;
  const out = typeof content.stdout === "string"
    ? content.stdout
    : typeof content.text === "string" ? content.text : "";
  const err = typeof content.stderr === "string" ? content.stderr : "";
  const exit = typeof content.exit_code === "number" ? content.exit_code : null;
  const bytes = typeof content.bytes === "number" ? content.bytes : null;
  if (!out && !err && exit === null && bytes === null) return null;
  return {
    exit,
    out,
    err,
    bytes,
    cut: content.stdout_truncated === true || content.stderr_truncated === true,
  };
}

/// A tool call is two lines, not a card.
///
/// Boxing each one puts a border around every third element and the column
/// stops reading as a conversation.
function Act(
  { event, result, query }: { event: RunEvent; result?: RunEvent; query: string },
) {
  const [name, args] = callSpans(event);
  const answer = said(result);
  return (
    <div className="act">
      <b><Mark text={name} query={query} /></b>
      <span className="out mono"><Mark text={args} query={query} /></span>
      {answer && (
        <div className="said mono">
          {answer.out && <pre><Mark text={answer.out} query={query} /></pre>}
          {answer.err && <pre className="err"><Mark text={answer.err} query={query} /></pre>}
          <div className="exit">
            {answer.exit !== null && (
              <span className={answer.exit === 0 ? "" : "warn"}>
                <Mark text={`退出码 ${answer.exit}`} query={query} />
              </span>
            )}
            {answer.bytes !== null && (
              <span><Mark text={size(answer.bytes)} query={query} /></span>
            )}
            {answer.cut && <span><Mark text="输出被截断了" query={query} /></span>}
          </div>
        </div>
      )}
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
///
/// The card stands in the column ⌘F searches, so its words go through `Mark`
/// like the rest of that column. The command it is asking about is the most
/// searchable text on the screen -- it is the thing a person came to look at --
/// and a card the finder stepped over would be a "没有匹配" said over a line
/// the reader can point at. The buttons underneath are not searched: they are
/// the choice, not the transcript.
function Gate({ run, query }: { run: RunView; query: string }) {
  const approval = run.approval;
  if (!approval) return null;
  const path = typeof approval.arguments.path === "string" ? approval.arguments.path : null;
  const writing = typeof approval.arguments.text === "string" ? approval.arguments.text : null;
  return (
    <div className="gate">
      <div className="h"><Mark text="等你决定" query={query} /></div>
      <code className="cmd">
        <Mark text={callLine(approval.toolName, approval.arguments)} query={query} />
      </code>
      {approval.toolName === "workspace.write_text" && path && writing !== null && (
        <WriteReview path={path} text={writing} query={query} />
      )}
      <div className="facts">
        <span><Mark text={effectLabel(approval.effect)} query={query} /></span>
        <span><Mark text={sandboxLabel(approval.sandbox)} query={query} /></span>
      </div>
      <Decisions run={run} />
      <div className="bind mono">
        <Mark
          text={`绑定 ${approval.bindingDigest.slice(0, 16)}…・只对这一次调用有效`}
          query={query}
        />
      </div>
    </div>
  );
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
function Acts(
  { events, results, query }:
    { events: RunEvent[]; results: Map<string, RunEvent>; query: string },
) {
  const [open, setOpen] = useState(false);
  /// The call's own id, from wherever this build of the runtime put it: flat
  /// on the payload for `model.tool_call`, nested under `call` for the shapes
  /// that carry an execution. `callSpans` reads the name and arguments the
  /// same way, and a second rule here would be a second answer to "which call
  /// is this".
  const answer = (event: RunEvent) => {
    const call = (event.payload.call ?? event.payload) as Record<string, unknown>;
    return results.get(String(call.id ?? ""));
  };
  if (events.length === 1) {
    return <Act event={events[0]} result={answer(events[0])} query={query} />;
  }

  const counted = new Map<string, number>();
  for (const event of events) {
    const [name] = callSpans(event);
    counted.set(name, (counted.get(name) ?? 0) + 1);
  }
  const named = [...counted.entries()]
    .map(([name, count]) => (count > 1 ? `${name} ×${count}` : name))
    .join("・");

  // A hit nobody can see is not a hit. While the finder is holding a query one
  // of these calls answers, the group is open whatever the caret last said --
  // otherwise the count in the finder would include lines the fold is hiding.
  // The summary row is left unmarked on purpose: it is a tally of the very
  // lines being counted, and marking it too would count each call twice.
  // A hit inside what a call printed counts too: the output is in this column
  // and the finder counts the marks standing in it, so a fold that stayed shut
  // over a match in a command's output would be counting a line nobody can see.
  const found = events.some((event) =>
    callSpans(event).some((span) => has(span, query))
    || [...Object.values((answer(event)?.payload.content ?? {}) as Record<string, unknown>)]
      .some((value) => typeof value === "string" && has(value, query)));
  const showing = open || found;

  return (
    <div className="acts">
      <button type="button" className="fold" aria-expanded={showing} onClick={() => setOpen(!showing)}>
        <span className="caret">{showing ? "▾" : "▸"}</span>
        {events.length} 个工具调用
        <span className="mono dim">{named}</span>
      </button>
      {showing && events.map((event) => (
        <Act
          event={event}
          result={answer(event)}
          key={event.event_id || event.sequence}
          query={query}
        />
      ))}
    </div>
  );
}

/// Events this build has no account of.
///
/// The runtime is versioned separately from this window and adds event types
/// without asking it. Until now anything unlisted was dropped on the floor,
/// which made the newest thing the runtime can report the one thing the client
/// is guaranteed to hide.
///
/// It draws what the log actually carries and nothing more: the type verbatim,
/// the sequence, and the payload. It does not guess a label, a severity or a
/// place in the conversation -- deciding an unknown event is routine is a
/// judgement this client is in no position to make.
///
/// Folded, and folded together, for the reason `Acts` is: a runtime that began
/// emitting an unlisted type per token would otherwise wallpaper the column,
/// and the failure mode being avoided is the transcript becoming the log. The
/// payload opens on a click; `⌘I` still has all of it either way.
function Unheard({ events }: { events: RunEvent[] }) {
  const [open, setOpen] = useState(false);
  const types = [...new Set(events.map((event) => event.type))].join("・");
  return (
    <div className="acts unheard">
      <button type="button" className="fold" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="caret">{open ? "▾" : "▸"}</span>
        {events.length === 1
          ? "本版本不认识的事件"
          : `${events.length} 条本版本不认识的事件`}
        <span className="mono dim">{types}</span>
      </button>
      {open && events.map((event) => (
        <div className="heard" key={event.event_id || event.sequence}>
          <div className="mono dim">第 {event.sequence} 条・{event.type}</div>
          <pre className="mono">{JSON.stringify(event.payload, null, 2)}</pre>
        </div>
      ))}
    </div>
  );
}

/// The other thing that stops a run on a person.
///
/// Beside the approval gate and drawn the same way, because it is the same
/// situation: the Run is parked and nothing moves until someone answers. What
/// differs is who is asking — an MCP server, by name — and that the answer is
/// content rather than a yes.
function InputGate({ run, query }: { run: RunView; query: string }) {
  const input = run.mcpInput;
  if (!input) return null;
  return (
    <div className="gate">
      {/* Marked like the rest of the column. The review of the finder caught
          the approval card being drawn inside the searched column and silently
          left out of the count; this gate is the same kind of block and would
          have arrived with the same hole. The server's name is the word most
          worth finding here. */}
      <div className="h">
        <Mark text="等你回答" query={query} />
        <span className="of"> ・ MCP <Mark text={input.serverName} query={query} /></span>
      </div>
      <McpInputForm run={run} />
    </div>
  );
}

/// The events a gate below draws in full, so the transcript does not also
/// draw a hairline for them. Both park the Run on a person -- one asks for a
/// decision about a tool call, the other for content an MCP server wants.
const PARKS_THE_RUN: ReadonlySet<string> = new Set([
  "approval.required",
  "mcp.input.required",
]);

/// The transcript, rendered from the durable log.
///
/// Text deltas are joined into one block rather than drawn per event: the
/// runtime streams a word at a time and a person reads paragraphs. Consecutive
/// tool calls are folded for the same reason at a larger scale.
function Transcript({ run, writing, query }: { run: RunView; writing: boolean; query: string }) {
  const blocks: React.ReactNode[] = [];
  let text = "";
  let acts: RunEvent[] = [];
  let unheard: RunEvent[] = [];

  const flushText = (key: string, last = false) => {
    if (!text) return;
    blocks.push(
      <div className="rep" key={`t-${key}`}>
        <p>
          <Mark text={text} query={query} />
          {/* Only on the block still being written, and only while the Run is
              producing text. A sentence that stops mid-clause reads the same
              whether more is coming or the model finished there, and the
              status line is too far from the words to answer it. */}
          {last && writing && <span className="writing" aria-label="还在写" />}
        </p>
      </div>,
    );
    text = "";
  };
  const results = new Map<string, RunEvent>();
  const flushActs = (key: string) => {
    if (acts.length === 0) return;
    blocks.push(<Acts events={acts} results={results} key={`a-${key}`} query={query} />);
    acts = [];
  };
  const flushUnheard = (key: string) => {
    if (unheard.length === 0) return;
    blocks.push(<Unheard events={unheard} key={`u-${key}`} />);
    unheard = [];
  };

  // Which block the text being accumulated came from. The runtime now says so
  // (`model.output.delta` carries `block`), and two blocks are two things the
  // model said rather than one cut in half -- a distinction adjacency cannot
  // make. Undefined means the provider supplied none, which is also every
  // record written before the field existed; those still group by adjacency,
  // which is the best a log without the answer allows.
  let block: number | undefined;

  for (const event of run.events) {
    // An unlisted type is folded with its neighbours, so it ends every other
    // run and every other run ends it.
    if (!knownEvent(event.type)) {
      flushText(String(event.sequence));
      flushActs(String(event.sequence));
      unheard.push(event);
      continue;
    }
    flushUnheard(String(event.sequence));
    if (event.type === "model.output.delta") {
      // Text ends a run of calls: what the model says after using a tool is a
      // new part of the conversation, not more of the same fold.
      flushActs(String(event.sequence));
      const at = typeof event.payload.block === "number" ? event.payload.block : undefined;
      if (text && at !== block) flushText(String(event.sequence));
      block = at;
      text += String(event.payload.text ?? "");
      continue;
    }
    if (event.type === "model.tool_call") {
      flushText(String(event.sequence));
      acts.push(event);
      continue;
    }
    // A result belongs to the call above it, not between it and the next one.
    // Drawn as its own line it separated every pair of consecutive calls, so
    // the fold -- which exists precisely so that a turn with eleven calls is
    // not eleven blocks -- had never fired outside a test that emitted calls
    // with nothing in between. A success is folded in and says nothing of its
    // own; a failure is not routine and gets the line, which is why it is the
    // failure the note is now worded for.
    if (event.type === "tool.result" && event.payload.is_error !== true) {
      flushText(String(event.sequence));
      // Kept, keyed by the call it answers, so the fold can draw it under that
      // call. Folding it away entirely is what made the fold work and left a
      // transcript that said a command had been run and never what it said.
      const answered = String(event.payload.tool_call_id ?? "");
      if (answered) results.set(answered, event);
      continue;
    }
    flushText(String(event.sequence));
    const note = eventNote(event.type, event.payload);
    // Two reasons an event that has a note still does not get a hairline here.
    // Routine bookkeeping stays out of the column: it is state, and the status
    // line and the raw-event drawer are where state belongs -- leaving it here
    // made a running Turn read as a machine log and a committed one as a
    // conversation, which is the same exchange rendered two ways. And the
    // events that park the Run on a person are drawn as their own gate below,
    // with what they are asking for; a hairline saying it happened as well
    // would be the same fact twice.
    if (note && !PARKS_THE_RUN.has(event.type) && belongsInConversation(event.type, event.payload)) {
      flushActs(String(event.sequence));
      blocks.push(
        <Note key={event.event_id || event.sequence}>
          <Mark text={note} query={query} />{" "}
          <span className="mono dim"><Mark text={event.type} query={query} /></span>
        </Note>,
      );
      // A refusal and a reasoning summary are words the model produced, and
      // the note above only names them. Drawn under the note that says which
      // it is, so it cannot be read as the ordinary reply it sits beside.
      const words = eventWords(event.type, event.payload);
      if (words) {
        blocks.push(
          <div className="rep said" key={`w-${event.event_id || event.sequence}`}>
            {words.map((part, index) => (
              <p key={index}><Mark text={part} query={query} /></p>
            ))}
          </div>,
        );
      }
    }
  }
  flushText("end", true);
  flushActs("end");
  flushUnheard("end");
  return <>{blocks}</>;
}

/// What this Run delegated.
///
/// Drawn beside the transcript rather than inside it: a delegation is not a
/// line the model said, it is work happening somewhere else. Each row links to
/// the child's own Run, because that is where what the child actually did
/// lives -- this side only knows what was asked, what came back, and what it
/// cost.
///
/// These rows stand in the searched column, so what the runtime reported about
/// each delegation -- the role it was given, the sentence it was handed, where
/// it got to -- goes through `Mark`. The heading above them is a tally of the
/// rows themselves and is left alone, for the same reason the fold's summary
/// row is.
function Delegations({ run, query }: { run: RunView; query: string }) {
  const desk = useDesk();
  const rows = lineage(subagentsOf(run.events));
  if (rows.length === 0) return null;

  const running = rows.filter((row) => row.view.state.kind === "running").length;
  const stateLabel = (state: SubagentState) => {
    if (state.kind === "requested") return "已请求";
    if (state.kind === "running") return "在跑";
    if (state.kind === "closed") return "被关掉";
    return state.error
      ? `失败・${state.status}`
      : lifecycleLabel({ kind: "terminal", status: state.status });
  };
  return (
    <div className="kids">
      <div className="kids-hd">
        子代理 {rows.length}
        {running > 0 && <span className="live">{running} 个在跑</span>}
      </div>
      {rows.map(({ view, depth }) => (
        <div className={`kid d${depth}`} key={view.id}>
          <div className="kid-top">
            <b><Mark text={view.role || "（未命名角色）"} query={query} /></b>
            <span className={`kid-state s-${view.state.kind}`}>
              <Mark text={stateLabel(view.state)} query={query} />
            </span>
            {view.queued > 0 && (
              <span className="kid-flag"><Mark text={`${view.queued} 条排队`} query={query} /></span>
            )}
            {view.generation > 1 && (
              <span className="kid-flag"><Mark text={`第 ${view.generation} 代`} query={query} /></span>
            )}
          </div>
          {view.asked && <div className="kid-ask"><Mark text={view.asked} query={query} /></div>}
          <div className="kid-facts mono">
            {view.forkedFrom && (
              <span>
                <Mark
                  text={`从 ${shortId(view.forkedFrom.id)} 的第 ${view.forkedFrom.generation} 代分叉`}
                  query={query}
                />
              </span>
            )}
            {/* Against its cap when the log carries one. A number of tokens on
                its own does not say whether a child was close to being cut
                off, which is the only thing a person reads this figure for. */}
            {view.tokens > 0 && (
              <span>
                <Mark
                  text={view.budget && view.budget.maxTokens > 0
                    ? `${view.tokens.toLocaleString()} / ${view.budget.maxTokens.toLocaleString()} token`
                    : `${view.tokens.toLocaleString()} token`}
                  query={query}
                />
              </span>
            )}
            {view.costMicros > 0 && (
              <span><Mark text={costLabel(view.costMicros)} query={query} /></span>
            )}
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
              : <span className="dim"><Mark text="还没有子 Run 可看" query={query} /></span>}
          </div>
        </div>
      ))}
    </div>
  );
}

/// What one arming is aimed at.
///
/// Not the ordinal on its own. "回到第 1 轮" drops two Turns in a three-Turn
/// conversation and three in a four-Turn one, and the head moves under a person
/// who has armed and not yet fired: a Turn lands from another window, or a
/// Rollback takes the branch to another generation and the Turns after this one
/// are different Turns. So an arming names the head it was armed against, and a
/// press that no longer matches arms again instead of firing -- what the second
/// press destroys is what the first press named, or it destroys nothing.
type Aim = { ordinal: number; generation: number; turnCount: number };

const aimedAt = (armed: Aim | null, at: Aim): boolean =>
  armed !== null && armed.ordinal === at.ordinal && armed.generation === at.generation
    && armed.turnCount === at.turnCount;

/// The committed conversation.
///
/// Drawn from the Session's frozen transcripts rather than from the event log,
/// because those are the two different things they look like: the event log is
/// what happened while a Turn ran, and the transcript is what the runtime
/// carried into the next Turn as history. When a log is retired the events go
/// and the conversation stays.
///
/// This is also where branching lives, because a Turn is the only place a
/// person can point at when they mean "from here". Both operations take an
/// ordinal, and the ordinal is on the row.
function Turns({ session, query }: { session: SessionView; query: string }) {
  const desk = useDesk();
  /// One arming for the whole conversation. Reaching for a different Rollback
  /// re-arms rather than fires, and a Fork disarms -- same rule as the approval
  /// queue, where an armed destructive key must not survive the person deciding
  /// to do something else.
  const [armed, setArmed] = useState<Aim | null>(null);
  const [failed, setFailed] = useState<string | null>(null);

  const fork = async (ordinal: number) => {
    setArmed(null);
    setFailed(await desk.fork(ordinal));
  };
  const rollback = async (at: Aim) => {
    if (!aimedAt(armed, at)) {
      setArmed(at);
      return;
    }
    setArmed(null);
    setFailed(await desk.rollback(at.ordinal));
  };

  return (
    <>
      {session.turns.map((turn) => {
        const said = textOf(turn, "user");
        const back = textOf(turn, "assistant");
        // Ordinals are 1..turn_count with nothing missing -- the runtime numbers
        // each committed Turn by the length of the history before it -- so this
        // is how many Turns a Rollback here would drop, counted rather than
        // guessed. Zero on the last Turn, where the runtime refuses a Rollback
        // that would remove nothing.
        const after = session.turnCount - turn.turn_ordinal;
        // The head this row was drawn from, carried into the arming: 撤掉后面
        // N 轮 is that count, and the generation is which N Turns those are.
        // Together they are what the person confirmed, rather than a number
        // that merely happened to be on screen when they first pressed.
        const at = {
          ordinal: turn.turn_ordinal,
          generation: session.generation,
          turnCount: session.turnCount,
        };
        return (
          <div className="turn" key={turn.digest || turn.turn_ordinal}>
            {said && <div className="ask"><Mark text={said} query={query} /></div>}
            {back && <div className="rep"><p><Mark text={back} query={query} /></p></div>}
            {/* Both are refused while a Turn is in flight, so neither is
                offered then. A control that is certain to be refused is the
                same mistake as a key hint for a key that does nothing. */}
            {!session.activeRunId && (
              <div className="branch">
                <button type="button" className="flat" onClick={() => void fork(turn.turn_ordinal)}>
                  从这里分叉
                </button>
                {after > 0 && (
                  <button
                    type="button"
                    className="flat back"
                    onClick={() => void rollback(at)}
                  >
                    回到这里
                    <span className="dim">撤掉后面 {after} 轮</span>
                    {aimedAt(armed, at) && <b className="arm">再按一次确认</b>}
                  </button>
                )}
              </div>
            )}
          </div>
        );
      })}
      {failed && <div className="err">{failed}</div>}
    </>
  );
}

/// Which Run this surface is about.
///
/// One rule, used by the transcript and by the status line, because they were
/// two answers to one question: the transcript respected the selection and the
/// open conversation while the status line took whichever Run was touched last.
/// A window that describes one Run above a transcript of another is worse than
/// either alone.
///
/// Two cursors meet here and the explicit one wins. Picking a Run out of a list
/// and coming here has to show that Run; inside a conversation the only Run
/// worth drawing is the Turn still running, since "the newest Run anywhere"
/// would be a different conversation's.
function shownRun(desk: Desk): RunView | null {
  const chosen = desk.selected
    ? desk.runs.find((candidate) => candidate.id === desk.selected) ?? null
    : null;
  if (chosen) return chosen;
  const session = desk.current;
  if (session) {
    return session.activeRunId
      ? desk.runs.find((candidate) => candidate.id === session.activeRunId) ?? null
      : null;
  }
  return currentRun(desk);
}

/// The finder, over the conversation column.
///
/// It says how many matches there are and which one you are on, and refuses to
/// imply anything past that: when this client only read part of a Run's log,
/// the count is a count over the part it read, and the row says so. A bare
/// "没有匹配" over a truncated log would answer a question nobody asked.
function Finder({
  box, query, onQuery, hits, current, onStep, onClose, partial,
}: {
  box: React.RefObject<HTMLInputElement | null>;
  query: string;
  onQuery(next: string): void;
  hits: number;
  current: number;
  onStep(delta: number): void;
  onClose(): void;
  partial: boolean;
}) {
  return (
    <div className="find">
      <input
        ref={box}
        className="in"
        value={query}
        placeholder="在这段对话里找"
        aria-label="在这段对话里找"
        spellCheck={false}
        onChange={(event) => onQuery(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            onStep(event.shiftKey ? -1 : 1);
          } else if (event.key === "Escape") {
            event.preventDefault();
            onClose();
          }
        }}
      />
      {query !== "" && (
        <span className="tally">
          {hits === 0 ? "没有匹配" : `${current + 1}/${hits}`}
          {partial && <span className="dim">・只在读到的这段里找</span>}
        </span>
      )}
      <span className="find-keys">
        <kbd>↵</kbd> 下一个 ・ <kbd>⇧↵</kbd> 上一个
      </span>
      <button type="button" className="flat" onClick={onClose}>关闭</button>
    </div>
  );
}

function ChatView() {
  const desk = useDesk();
  const chosen = desk.selected
    ? desk.runs.find((candidate) => candidate.id === desk.selected) ?? null
    : null;
  const session = chosen ? null : desk.current;
  const run = shownRun(desk);
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  const opened = useSyncExternalStore(watchFind, findOpened);
  const finding = opened >= 0;
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState(0);
  const [at, setAt] = useState(0);
  const box = useRef<HTMLInputElement>(null);
  const restore = useRef<HTMLElement | null>(null);
  const current = hits === 0 ? -1 : ((at % hits) + hits) % hits;

  const close = () => {
    closeFind();
    setQuery("");
    setAt(0);
    restore.current?.focus();
  };

  // Leaving the surface closes it. The open flag outlives this component -- it
  // has to, since the key that sets it is dispatched by the shell -- and a
  // finder that reappeared over a transcript nobody was searching would be the
  // cost of that.
  useEffect(() => () => closeFind(), []);

  // ⌘F puts the focus in the box, and a second ⌘F selects what is already
  // there: pressing it again is how a person searches for something else.
  useEffect(() => {
    if (!finding) return;
    const active = document.activeElement as HTMLElement | null;
    if (active && active !== box.current) restore.current = active;
    box.current?.focus();
    box.current?.select();
  }, [finding, opened]);

  // The hits are counted off the rendered column, after it is rendered.
  //
  // Draw order is the only order a finder can count in, and the drawn document
  // is the one place that order actually exists -- a parallel list of blocks
  // would be a second source for it, free to drift from what is on screen. A
  // fold holding a match has already opened by the time this runs, so every hit
  // counted here is one a person can see.
  useEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const marks = node.querySelectorAll<HTMLElement>("mark");
    if (marks.length !== hits) setHits(marks.length);
    marks.forEach((mark, index) => mark.classList.toggle("on", index === current));
  });

  // Only when the hit moves. Re-running this on every render would drag the
  // column back to the match each time an event arrives.
  useEffect(() => {
    if (current < 0) return;
    scroller.current?.querySelectorAll<HTMLElement>("mark")[current]
      ?.scrollIntoView({ block: "center" });
  }, [current, query]);

  // Follows the tail while you are at the tail, and stops the moment you
  // scroll up. A transcript that yanks itself down while you are reading is
  // worse than one that never scrolls -- and reading a match is reading, so a
  // search in progress holds the column still too.
  useEffect(() => {
    const node = scroller.current;
    if (!node || !pinned.current || query !== "") return;
    node.scrollTop = node.scrollHeight;
  }, [run?.events.length, run?.id, session?.turnCount, session?.key, query]);

  return (
    <>
      {finding && (
        <Finder
          box={box}
          query={query}
          onQuery={(next) => { setQuery(next); setAt(0); }}
          hits={hits}
          current={current}
          // Counted from where the step lands rather than from what this render
          // saw: two presses inside one batch would otherwise both step from the
          // same place and move one hit between them.
          onStep={(delta) => setAt((was) => was + delta)}
          onClose={close}
          // What this client actually read. Neither flag is a guess: one is the
          // paging ceiling this client stopped at, the other is the runtime
          // saying the earlier events are gone.
          partial={Boolean(run?.truncated || run?.historyGap)}
        />
      )}
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

        {session && <Turns session={session} query={query} />}

        {run && (
          <>
            {/* Both of these are notes like any other note, and the finder has
                no way to know they were written by hand rather than read off an
                event -- so they are marked like any other note. Text drawn
                outside `Mark` is text the count denies. */}
            {run.truncated && (
              <Note>
                <Mark text="这个 Run 的事件太多，只读到了前面一段" query={query} />
              </Note>
            )}
            {run.historyGap && (
              <Note>
                <Mark
                  text={`更早的事件已被回收，这段转录不完整 —— 最早还能读到第 ${run.earliestSequence} 条`}
                  query={query}
                />
              </Note>
            )}
            {(!session || run.id === session.activeRunId) && (
              <div className="ask"><Mark text={run.asked} query={query} /></div>
            )}
            {run.error ? (
              // Also marked: the finder can be open over a Run whose log would
              // not read, and the code is the one string a person searching a
              // broken transcript is actually looking for.
              <div className="offline">
                <Mark text="这个 Run 的日志读不出来：" query={query} />
                <span className="mono"><Mark text={run.error.code} query={query} /></span>
                {run.error.message
                  && <Mark text={` —— ${run.error.message}`} query={query} />}
              </div>
            ) : (
              <Transcript
                run={run}
                query={query}
                // The Run is producing text right now: it is moving, and the last
                // thing it wrote was text rather than a tool call or a question.
                // Both come from the log; neither is a guess about the model.
                writing={
                  (run.lifecycle.kind === "running")
                  && run.events[run.events.length - 1]?.type === "model.output.delta"
                }
              />
            )}
            <Delegations run={run} query={query} />
            <Gate run={run} query={query} />
            {/* The other way a Run stops on a person. Same place as the
                approval gate because it is the same kind of stop -- and it is
                the one that used to say 等你决定 with nothing to answer it. */}
            <InputGate run={run} query={query} />
          </>
        )}
      </div>
    </>
  );
}

/// The raw log for the run on screen. Sequence, type, digest, payload.
///
/// The rendered transcript is a reading of the log; this is the log. When the
/// two disagree the log is right, and there has to be somewhere to look.
function ChatDrawer() {
  const desk = useDesk();
  // The same Run the transcript is drawing. It read the newest Run anywhere,
  // so "the log behind what you are reading" could be another Run's log --
  // with its own id in the header saying so, which is the kind of disagreement
  // a person resolves by trusting the wrong one.
  const run = shownRun(desk);
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
  /// The workspace, read once and narrowed locally. Read once because it is
  /// the folder the runtime was started on and does not change under this
  /// window; narrowed locally because a listing call per keystroke would put
  /// the filesystem between someone and their own typing.
  const [files, setFiles] = useState<string[] | null>(null);
  const [mention, setMention] = useState<{ at: number; query: string } | null>(null);
  /// Whether the walk saw the whole workspace. A completion that quietly
  /// stopped at a bound would read as "that file is not there".
  const [whole, setWhole] = useState(true);
  const [pick, setPick] = useState(0);
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

  useEffect(() => {
    const api = bridge();
    if (!api?.listFiles) return;
    void walkWorkspace((path) => api.listFiles(path)).then((walked) => {
      setFiles(walked.files);
      setWhole(walked.complete);
    });
  }, []);

  /// Reads the caret rather than the text alone: a mention is where the caret
  /// is, and someone who moves back into an earlier `@` is editing that one.
  const readMention = (value: string, caret: number) => {
    const found = files ? mentionAt(value, caret) : null;
    setMention(found);
    setPick(0);
  };

  const offered = mention && files ? narrow(files, mention.query) : [];

  const choose = (name: string) => {
    const box_ = box.current;
    if (!mention || !box_) return;
    const next = withMention(draft, mention, name, box_.selectionStart ?? draft.length);
    setDraft(next.text);
    setMention(null);
    // The caret goes after the path this put in, not to the end of a line the
    // person may have been typing in the middle of.
    requestAnimationFrame(() => {
      box_.focus();
      box_.setSelectionRange(next.caret, next.caret);
    });
  };

  const send = async () => {
    const input = draft.trim();
    if (!input || !live || sending) return;
    setSending(true);
    setDraft("");
    setHistory((past) => [input, ...past]);
    setAt(-1);
    // While a Turn is running the same box redirects it instead of queueing a
    // next Turn. Two different things, and which one happens is decided by
    // what the Run is doing rather than by a mode the person has to hold in
    // their head.
    const failure = turning ? await desk.steer(input) : await desk.send(input);
    setError(failure);
    setSending(false);
    // A new run is the one you want to watch. Clearing the cursor rather than
    // pointing it at the new id keeps "newest" honest if the send failed.
    if (!failure) desk.select(null);
    box.current?.focus();
  };

  return (
    <div className="write">
      {offered.length > 0 && (
        <ul className="mentions" role="listbox">
          {!whole && (
            <li className="capped">这个工作区太大，下面只是其中一部分</li>
          )}
          {offered.map((name, index) => (
            <li key={name}>
              <button
                type="button"
                className={index === pick ? "on" : ""}
                onMouseDown={(event) => { event.preventDefault(); choose(name); }}
              >
                {name}
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="write-row">
        <textarea
          ref={box}
          className="in"
          rows={1}
          value={draft}
          disabled={!live || sending}
          placeholder={
            !live ? "没有连上 Runtime"
              : turning ? "这轮还在跑 —— 现在说的话会拿去改向"
                : desk.current ? "接着说" : "说一句话，就开始一段对话"
          }
          onChange={(event) => {
            setDraft(event.target.value);
            setAt(-1);
            readMention(event.target.value, event.target.selectionStart ?? event.target.value.length);
          }}
          onKeyDown={(event) => {
            // While the list is open it owns the keys that would otherwise
            // send or move the history. Enter is the one that matters: it is
            // the commonest key in this box, and taking it would send a
            // half-written mention.
            if (offered.length > 0) {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                choose(offered[Math.min(pick, offered.length - 1)]!);
                return;
              }
              if (event.key === "Escape") {
                event.preventDefault();
                setMention(null);
                return;
              }
              if (event.key === "ArrowDown") {
                event.preventDefault();
                setPick((chosen) => Math.min(chosen + 1, offered.length - 1));
                return;
              }
              if (event.key === "ArrowUp") {
                event.preventDefault();
                setPick((chosen) => Math.max(chosen - 1, 0));
                return;
              }
            }
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
          {sending ? "发送中" : turning ? "改向" : "发送"}
        </button>
      </div>
      {/* Key hints only. A paragraph of explanation used to live here -- what a
          steer is, when it applies, where its evidence shows up -- which wrapped
          onto a second line and glued itself to the 新对话 button beside it. The
          placeholder and the button already say which of the two things this box
          is about; a caveat that is true whether or not anyone reads it does not
          earn a permanent row under the input.

          There is no 新对话 button here either: the shell renders `n 新对话`
          from the key registry, and one affordance in two places is one of them
          being noise. */}
      <div className="write-hint">
        <kbd>↵</kbd> {turning ? "改向" : "发送"} ・ <kbd>⇧↵</kbd> 换行 ・ <kbd>↑</kbd> 上一条
      </div>
      {error && <div className="err">{error}</div>}
    </div>
  );
}

/// The status line, which is about the run you are looking at rather than
/// about the app. An app that spends that row on its own name has wasted it.
export function ChatStatus() {
  const desk = useDesk();
  const run = shownRun(desk);
  const moving = run !== null
    && (run.lifecycle.kind === "running" || run.lifecycle.kind === "cancelling");
  // No timer here. The store re-reads every 1.2 seconds and re-renders, so the
  // clock already advances without one. A `setInterval` was written first and
  // taken out: stopping it changed nothing a test could see, which is the
  // definition of a part that is not doing anything.
  if (!run) return <span className="now">—</span>;

  const lastEvent = run.events[run.events.length - 1]?.type ?? null;
  const activity = moving ? doing(lastEvent) : null;
  return (
    <>
      <span className={`now t-${lifecycleTone(run.lifecycle)}`}>
        {lifecycleLabel(run.lifecycle)}
      </span>
      {moving && (
        <>
          <i>・</i>
          {activity
            ? <span>{activity}</span>
            // Named rather than smoothed over: an event with no phrase for it
            // is worth seeing, and the type is what makes it lookupable
            // instead of mysterious.
            //
            // Two different reasons for landing here, and they are not the
            // same admission. Most types this build knows perfectly well and
            // simply has no words for as an activity -- `run.timed_out` is one
            // -- and telling a person the version does not recognise it would
            // be the window lying about itself. Only a type nothing here
            // accounts for gets that sentence, and it is the same sentence the
            // transcript uses for the same event.
            : (
              <span
                className="dim mono"
                title={lastEvent !== null && knownEvent(lastEvent)
                  ? "这个版本没有给这个事件写说法"
                  : "这个版本不认识这个事件类型"}
              >
                {lastEvent}
              </span>
            )}
          <i>・</i>
          {/* Counted from the Run's first event to now. A finished Run is
              measured end to end instead. */}
          <span title={run.startedAt ?? ""}>{elapsed(run.startedAt, null)}</span>
        </>
      )}
      <i>・</i><span className="mono">{shortId(run.id)}</span>
      {/* Against the cap this app configured, when it knows one. A count on
          its own does not say whether a Run is near the limit that will end
          it, and `budget_exhausted` arriving with no warning reads as a broken
          agent rather than as a limit someone chose. */}
      <i>・</i>
      <span title={desk.budget ? `上限 ${desk.budget.maxTokens.toLocaleString()} token` : ""}>
        {desk.budget
          ? `${run.tokens.toLocaleString()} / ${desk.budget.maxTokens.toLocaleString()} token`
          : `${run.tokens.toLocaleString()} token`}
      </span>
      <i>・</i><span>{costLabel(run.costMicros)}</span>
      <i>・</i>
      {moving
        ? <span title={run.updatedAt ?? ""}>{since(run.updatedAt)}</span>
        : <span title={`${run.startedAt ?? ""} → ${run.updatedAt ?? ""}`}>
          用了 {elapsed(run.startedAt, run.updatedAt)}
        </span>}
      {moving && (
        <button type="button" className="flat stop" onClick={() => void desk.decide(run.id, "cancel")}>
          停止
        </button>
      )}
    </>
  );
}

/// Whether the Run this surface is drawing is asking.
///
/// `shownRun`, not `currentRun`: the transcript beside these keys is drawn from
/// the Run this conversation is running, and "the newest Run touched anywhere"
/// is a different Run in a different conversation. Answering a question that is
/// not on screen is not a key that missed -- it is a decision made about
/// something the person never saw.
const asking = (desk: Desk) => shownRun(desk)?.approval != null;

/// Whether there is a conversation on this surface to search: a transcript, or
/// the committed Turns behind it. A finder over an empty column finds nothing,
/// and the shell would be drawing a key hint for it.
const readable = (desk: Desk) =>
  shownRun(desk) !== null || (desk.current?.turns.length ?? 0) > 0;

register({
  id: "chat",
  label: "对话",
  group: "work",
  view: ChatView,
  drawer: ChatDrawer,
  drawerLabel: "原始事件",
  composer: Composer,
  status: ChatStatus,
  keys: [
    // Same rule as the queue: the irreversible one is not a bare key.
    ...DECISIONS.filter((decision) => !decision.destructive).map((decision) => ({
      key: decision.key,
      hint: decision.label,
      when: asking,
      run: (desk: Desk) => {
        const run = shownRun(desk);
        if (run) void desk.decide(run.id, decision.action);
      },
    })),
    {
      key: "n",
      hint: "新对话",
      // Nothing to leave when no conversation is open, and starting one is what
      // typing already does.
      when: (desk: Desk) => desk.current !== null,
      run: (desk: Desk) => desk.newConversation(),
    },
    {
      key: "f",
      meta: true,
      hint: "查找",
      when: readable,
      // The finder is the surface's own state, and this is the whole reason it
      // is not React state: the binding is dispatched by the shell, outside any
      // component, and it still has to reach the transcript's finder.
      run: () => openFind(),
    },
  ],
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
        const run = shownRun(desk);
        return run?.lifecycle.kind === "running";
      },
      run: (desk) => {
        const run = shownRun(desk);
        if (run) void desk.decide(run.id, "cancel");
      },
    },
  ],
});
