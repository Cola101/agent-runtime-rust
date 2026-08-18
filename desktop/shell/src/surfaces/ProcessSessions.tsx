/// What the durable log actually holds about a `process.*` session.
///
/// This is deliberately not a terminal, and it is not shaped like one. A
/// terminal implies two things this client does not have: a live stream, and
/// somewhere to type.
///
///  * The renderer's only channel is the run event cursor. The local adapter
///    has Submit, Attach, EventCursor, List and the control verbs, and no
///    `process.*` call at all — so nothing here can read a session or write a
///    byte to one.
///  * Output reaches the log only when the agent calls `process.poll`,
///    `process.attach`, `process.wait`, `process.write` or `process.start`
///    with a yield. Measured, not assumed: in a real session here the shell's
///    entire output landed in the log only because `process.close` happened to
///    read it on the way out, and would otherwise have existed nowhere a
///    client can reach.
///
/// So what this renders is exactly what a `tool.result` carried: byte ranges,
/// each labelled with the call that fetched it. The ranges are the honest
/// part — they say where the bytes came from, and where nothing did.
import { register } from "./registry";
import { isOver, lifecycleLabel, since, shortId } from "./model";
import { LinkBanner } from "./Link";
import { currentRun, moveCursor, useDesk, withProcessSessions, type Desk } from "../desk";
import type { ProcessCall, ProcessSession } from "../store";

/// Whether moving the run cursor would land somewhere other than what is on
/// screen.
///
/// Not "is there more than one run with a session": the cursor may already be
/// on the only one, and then j is a key that does nothing. The empty state
/// points people at j, and it may only do that when this is true — a printed
/// key and a live key drifting apart is the failure this shell keeps having.
function canMove(desk: Desk): boolean {
  const here = currentRun(desk)?.id;
  return withProcessSessions(desk).some((run) => run.id !== here);
}

/// The runtime's `ProcessSessionState`, in the language the person reading it
/// uses. The runtime's own word stays beside it on screen — this is a gloss,
/// not a replacement.
const STATE: Record<string, string> = {
  starting: "正在启动",
  running: "在跑",
  terminating: "正在结束",
  exited: "自己退出了",
  terminated: "被结束了",
  indeterminate: "结果无法判定",
};

/// `ProcessSessionTerminationReason`, same rule.
const REASON: Record<string, string> = {
  cpu_limit: "超出 CPU 上限",
  execution_deadline: "超过执行期限",
  idle_timeout: "闲置超时",
  output_limit: "输出超出上限",
  start_failed: "启动失败",
  closed: "被 process.close 关掉",
  recovered_missing: "恢复时进程已不在",
  legacy_terminal: "旧版终端记录",
};

/// Control bytes, shown rather than obeyed.
///
/// CRLF is the PTY's line ending and becomes a line break — that is decoding,
/// not interpretation. Every other C0 byte is printed as its Unicode picture,
/// so an escape sequence reads as `␛[32m` instead of silently colouring the
/// page or, worse, vanishing into it. Nothing is dropped and nothing is
/// executed: this screen is not a terminal emulator and must not look like one
/// that is misbehaving.
function visible(bytes: string): string {
  return bytes
    .replace(/\r\n/g, "\n")
    .replace(/[\x00-\x08\x0B-\x1F\x7F]/g, (char) =>
      char === "\x7F" ? "␡" : String.fromCharCode(0x2400 + char.charCodeAt(0)));
}

function range(from: number, to: number): string {
  return from === to ? `${from}` : `${from}–${to}`;
}

/// One call, with the byte offsets already assembled for each stream when it
/// happened. Computed before render rather than accumulated during it: a
/// running total mutated inside a child's render is a total that depends on
/// the order React chose to call things in.
type Placed = { call: ProcessCall; seenOut: number; seenErr: number };

function place(session: ProcessSession): Placed[] {
  let seenOut = 0;
  let seenErr = 0;
  return session.calls.map((call) => {
    const placed = { call, seenOut, seenErr };
    if (call.output) {
      seenOut = Math.max(seenOut, call.output.stdoutTo);
      seenErr = Math.max(seenErr, call.output.stderrTo);
    }
    return placed;
  });
}

/// Whether a read has anything to draw for one stream: bytes, or a hole in
/// front of them. Shared by the block and by the call above it, so "no byte
/// block" and "this call brought nothing back" cannot disagree.
function carries(from: number, to: number, seen: number): boolean {
  return to > from || from > seen;
}

/// One stream of one read, with whatever is true of it in front.
///
/// `seen` is the highest byte offset already assembled for this stream. Three
/// things can be true of the next read and all three are said out loud: it
/// continues (`from === seen`), it starts past bytes the agent never read
/// (`from > seen`), or it covers ground the log already has (`from < seen`,
/// which is what `process.attach` does — it re-reads a tail).
function Stream({
  label, text, from, to, truncated, seen,
}: {
  label: string; text: string; from: number; to: number; truncated: boolean; seen: number;
}) {
  const gap = from > seen ? from - seen : 0;
  const reread = from < seen;
  if (!carries(from, to, seen)) return null;
  return (
    <div className="ps-read">
      {gap > 0 && (
        <div className="ps-gap">
          第 {seen}–{from} 字节没有进日志 —— 这 {gap} 个字节 Agent 没读过
        </div>
      )}
      <div className="ps-meta mono">
        <span>{label} {range(from, to)}</span>
        {reread && <span className="flag">重读了已有的字节</span>}
        {truncated && <span className="flag">只读到尾部</span>}
      </div>
      {to > from
        ? <pre className="ps-bytes mono">{visible(text)}</pre>
        : <div className="ps-none">这次读到 0 字节</div>}
    </div>
  );
}

/// What the agent sent into the session on this call.
///
/// Its own line, because it is a different fact from the output: on a PTY the
/// terminal echoes it back and it appears in stdout as well, so the two
/// together are what happened rather than one thing shown twice.
function Wrote({ bytes }: { bytes: string }) {
  return (
    <div className="ps-wrote">
      <span className="ps-caret" aria-hidden="true">›</span>
      <pre className="mono">{visible(bytes)}</pre>
    </div>
  );
}

/// The terminal size a call carried — `process.start` opening a PTY, or
/// `process.resize` changing one. Drawn rather than dropped: it is the reason
/// the bytes after it wrap where they do.
function Shape({ call }: { call: ProcessCall }) {
  const cols = call.arguments.cols;
  const rows = call.arguments.rows;
  if (typeof cols !== "number" || typeof rows !== "number") return null;
  return <span className="ps-shape">{cols}×{rows} 字符</span>;
}

function Call({ placed }: { placed: Placed }) {
  const { call, seenOut, seenErr } = placed;
  // A call that came back with no new bytes is a fact, not an absence: the
  // agent asked and the process had written nothing since. Said in words,
  // because an empty space under a heading reads as a rendering failure.
  const quiet = call.output !== null
    && !carries(call.output.stdoutFrom, call.output.stdoutTo, seenOut)
    && !carries(call.output.stderrFrom, call.output.stderrTo, seenErr);
  return (
    <div className="ps-call">
      <div className="ps-call-hd">
        <b className="mono">{call.tool}</b>
        <Shape call={call} />
        <span className="ps-when" title={call.timestamp}>{since(call.timestamp)}</span>
      </div>
      {call.wrote !== null && <Wrote bytes={call.wrote} />}
      {call.outcome === "waiting" && <div className="ps-note">这次调用还停在你那里，没有结果</div>}
      {call.outcome === "denied" && <div className="ps-note">策略拒绝了它，进程没有收到这次调用</div>}
      {call.outcome === "error" && (
        <div className="ps-note">
          <span className="mono">{call.error?.code}</span>
          {call.error?.message ? ` —— ${call.error.message}` : ""}
        </div>
      )}
      {quiet && <div className="ps-none">没有新字节</div>}
      {call.output && (
        <>
          <Stream
            label="stdout" text={call.output.stdout}
            from={call.output.stdoutFrom} to={call.output.stdoutTo}
            truncated={call.output.stdoutTruncated} seen={seenOut}
          />
          <Stream
            label="stderr" text={call.output.stderr}
            from={call.output.stderrFrom} to={call.output.stderrTo}
            truncated={call.output.stderrTruncated} seen={seenErr}
          />
        </>
      )}
    </div>
  );
}

/// The last thing the log says about the session, which is not the same as
/// what is true now. A session outlives the run that started it; once the run
/// reaches a boundary, no further byte and no further state can enter its log.
function Head({ session, over }: { session: ProcessSession; over: boolean }) {
  const last = [...session.calls].reverse().find((call) => call.output)?.output ?? null;
  return (
    <div className="ps-head">
      <div className="ps-id mono">
        {session.id ? `session ${shortId(session.id)}` : "还没有 session id"}
      </div>
      {last ? (
        <dl className="facts-list">
          <dt>最后记录的状态</dt>
          <dd>
            {STATE[last.state] ?? last.state} <span className="mono dim">{last.state}</span>
          </dd>
          {last.pid !== null && <><dt>pid</dt><dd className="mono">{last.pid}</dd></>}
          {last.exitCode !== null && (
            <><dt>exit code</dt><dd className="mono">{last.exitCode}</dd></>
          )}
          {last.terminationReason !== null && (
            <>
              <dt>结束原因</dt>
              <dd>
                {REASON[last.terminationReason] ?? last.terminationReason}{" "}
                <span className="mono dim">{last.terminationReason}</span>
              </dd>
            </>
          )}
          <dt>进了日志的</dt>
          <dd>
            stdout {last.stdoutTo} 字节{last.stderrTo > 0 ? `・stderr ${last.stderrTo} 字节` : ""}
          </dd>
        </dl>
      ) : (
        <div className="ps-none">这个会话还没有任何结果回到日志里</div>
      )}
      {over && (
        <p className="ps-caption">
          这个 Run 已经结束，它的日志不会再长。会话本身是持久的、可能还活着 ——
          但不会再有任何字节写进这个 Run 的事件里。
        </p>
      )}
    </div>
  );
}

function ProcessToolbar() {
  const desk = useDesk();
  const run = currentRun(desk);
  const elsewhere = withProcessSessions(desk).filter((other) => other.id !== run?.id).length;
  return (
    <>
      <b>进程会话</b>
      <span className="tb-r">
        {desk.link.state !== "live"
          ? "未连接"
          : `这个 Run 里 ${run ? run.processSessions.length : 0} 个`}
        {elsewhere > 0 && ` ・ 另外 ${elsewhere} 个 Run 里也有`}
      </span>
    </>
  );
}

function ProcessView() {
  const desk = useDesk();
  const run = currentRun(desk);
  const others = withProcessSessions(desk).filter((other) => other.id !== run?.id);

  return (
    <div className="pane">
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && !run && desk.listedAt !== null && (
        <div className="empty">还没有 Run，也就没有进程会话。</div>
      )}

      {run && run.processSessions.length === 0 && (
        <div className="empty">
          {shortId(run.id)} 这个 Run 没有调用过 <code>process.*</code>。
          <span className="sub">
            这不等于这台 Runtime 没有这些工具 —— 事件日志里没有已安装工具的清单，
            所以这里只能说"没有调用过"。它们只在启动 runtime-host 时设置了
            <code> AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE</code> 的情况下才存在。
          </span>
          {others.length > 0 && (
            <span className="sub">另外 {others.length} 个 Run 里有会话，按 j / k 换过去。</span>
          )}
        </div>
      )}

      {run && run.processSessions.length > 0 && (
        <>
          <p className="ps-caption">
            这不是终端。客户端到 Runtime 只有一条事件日志，没有任何通向进程的调用 ——
            读不到实时输出，也没法往里打字。下面每一段字节都是 Agent 某次
            <span className="mono"> process.*</span> 调用读回来的，字节区间就写在旁边；
            它没读的，日志里就没有。
          </p>
          {run.processSessions.map((session) => (
            <section className="ps" key={session.key}>
              <Head session={session} over={isOver(run.lifecycle)} />
              {place(session).map((placed) => (
                <Call key={placed.call.toolCallId} placed={placed} />
              ))}
            </section>
          ))}
        </>
      )}
    </div>
  );
}

/// The status line, which is about the log this screen is reading.
///
/// The byte total is the one number worth a permanent row here: it is how much
/// of the session ever became durable, and it is bounded by what the agent
/// read rather than by what the process wrote.
function ProcessStatus() {
  const desk = useDesk();
  const run = currentRun(desk);
  if (!run || run.processSessions.length === 0) return <span className="now">—</span>;
  const bytes = run.processSessions.reduce((total, session) => {
    const last = [...session.calls].reverse().find((call) => call.output)?.output;
    return total + (last ? last.stdoutTo + last.stderrTo : 0);
  }, 0);
  return (
    <>
      <span className="now">{run.processSessions.length} 个会话</span>
      <i>・</i><span className="mono">{shortId(run.id)}</span>
      <i>・</i><span>进日志 {bytes.toLocaleString()} 字节</span>
      <i>・</i><span>{lifecycleLabel(run.lifecycle)}</span>
    </>
  );
}

/// What this surface cannot reach, and why.
///
/// Every line here is something a person would reasonably expect a screen
/// called 进程会话 to do. Each is missing for a specific reason rather than
/// because nobody got to it, and the reason is the useful part.
function ProcessDrawer() {
  return (
    <div className="ps-cannot">
      <p>
        <b>看不到实时输出。</b>
        字节只在 Agent 调用 <span className="mono">process.poll</span> /
        <span className="mono"> attach</span> / <span className="mono">wait</span> 时才进日志。
        进程写了而 Agent 没读的，只留在会话自己的日志文件里。
      </p>
      <p>
        <b>不能往里打字。</b>
        本地适配器只有 Submit / Attach / EventCursor / List 和几个控制动作，
        没有任何 <span className="mono">process.*</span> 调用；渲染进程更没有。
      </p>
      <p>
        <b>不知道跑的是什么程序。</b>
        <span className="mono"> process.start</span> 不带命令：程序由启动 runtime-host 的人用
        <span className="mono"> AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE</span> 指定，
        整条日志里没有它的路径。策略快照里的
        <span className="mono"> implementation_digest</span> 是那个二进制的哈希，不是名字。
      </p>
      <p>
        <b>控制字符按字面显示。</b>
        <span className="mono">␛[…]</span> 这样的转义序列不会被解释成颜色或光标移动 ——
        解释它们需要一个终端模拟器，这里没有。
      </p>
    </div>
  );
}

register({
  id: "process-sessions",
  label: "进程会话",
  group: "work",
  // No badge. The only count available is "sessions whose last recorded state
  // was not terminal", and a number in the rail cannot carry "last recorded" —
  // it would read as "running now", which is a claim the log cannot support.
  view: ProcessView,
  toolbar: ProcessToolbar,
  drawer: ProcessDrawer,
  drawerLabel: "这个面看不到的",
  status: ProcessStatus,
  keys: [
    {
      key: "j", hint: "下一个有会话的 Run", when: canMove,
      run: (desk) => moveCursor(desk, withProcessSessions(desk).map((run) => run.id), 1),
    },
    {
      key: "k", hint: "上一个有会话的 Run", when: canMove,
      run: (desk) => moveCursor(desk, withProcessSessions(desk).map((run) => run.id), -1),
    },
  ],
  commands: [
    { id: "process-sessions:open", title: "查看进程会话", hint: "process.* 读回来的字节" },
    {
      id: "process-sessions:first",
      title: "跳到有进程会话的 Run",
      when: (desk) => withProcessSessions(desk).length > 0,
      run: (desk) => {
        const first = withProcessSessions(desk)[0];
        desk.select(first.id);
        desk.go("process-sessions");
      },
    },
  ],
});
