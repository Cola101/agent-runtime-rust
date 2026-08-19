import { useEffect, useRef } from "react";
import { register } from "./registry";
import { shortId, since } from "./model";
import { textOf } from "../session";
import { plain } from "./markdown";
import { LinkBanner } from "./Link";
import { useDesk, type Desk } from "../desk";
import type { SessionView } from "../session";

/// When a conversation was last touched.
///
/// Derived rather than read: a Session head carries no timestamp, and its Turns
/// do not either. What is dated is the Run each Turn was executed as, which
/// this client already holds. A conversation whose Runs have been retired
/// therefore has no date -- and says so by showing nothing rather than by
/// showing now.
function touchedAt(desk: Desk, session: SessionView): string | null {
  const ids = new Set(session.turns.map((turn) => turn.run_id));
  if (session.activeRunId) ids.add(session.activeRunId);
  const dates = desk.runs
    .filter((run) => ids.has(run.id))
    .map((run) => run.updatedAt)
    .filter((date): date is string => date !== null);
  return dates.sort().at(-1) ?? null;
}

function name(session: SessionView): string {
  return session.title || "（还没说出第一句）";
}

/// The last thing the model said in a conversation, as one line.
///
/// The second line of a row. A list of nothing but first sentences answers
/// "which one was this" and no more, and the conversation a person is looking
/// for is often the one where they remember the answer rather than the
/// question.
///
/// Empty when the branch holds no reply -- a conversation whose Turns have not
/// been read yet, or one that has only ever been asked. The row then draws no
/// second line at all rather than a blank one, because an empty line under
/// every title is a list that has grown a gutter for nothing.
function lastReply(session: SessionView): string {
  for (let at = session.turns.length - 1; at >= 0; at -= 1) {
    const said = textOf(session.turns[at], "assistant").trim();
    // Through the markdown summariser, not a raw slice: the raw text opens
    // with whatever markup the model used, and a row would spend its one line
    // on "## 改了什么" and three backticks.
    const line = plain(said);
    if (line) return line;
  }
  return "";
}

function move(desk: Desk, delta: number): void {
  const rows = desk.sessions;
  if (rows.length === 0) return;
  const at = desk.current ? rows.findIndex((row) => row.key === desk.current!.key) : -1;
  const next = at === -1
    ? (delta > 0 ? 0 : rows.length - 1)
    : Math.min(Math.max(at + delta, 0), rows.length - 1);
  desk.selectSession(rows[next]);
}

function ConversationsToolbar() {
  const desk = useDesk();
  const turning = desk.sessions.filter((session) => session.activeRunId).length;
  return (
    <>
      <b>会话</b>
      <span className="tb-r">
        {desk.link.state === "live" ? `共 ${desk.sessions.length} 段` : "未连接"}
        {turning > 0 && ` ・ ${turning} 段正在跑`}
      </span>
    </>
  );
}

/// Every conversation this state root holds.
///
/// The list the client could not offer before this existed: a person could
/// start a conversation and continue the open one, and had no way back to an
/// earlier one. Selecting here *is* opening -- there is no second cursor,
/// because a list that highlights one conversation while the transcript shows
/// another is two answers to one question.
function ConversationsView() {
  const desk = useDesk();
  const body = useRef<HTMLUListElement>(null);

  useEffect(() => {
    body.current?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [desk.current?.key]);

  // How many rows each Session has. A Session with one branch is just a
  // conversation and its branch id is noise; a Session with two is two rows
  // with the same first sentence, and then the branch is the only thing that
  // tells them apart.
  const strands = new Map<string, number>();
  for (const session of desk.sessions) {
    strands.set(session.sessionId, (strands.get(session.sessionId) ?? 0) + 1);
  }

  return (
    <div className="pane">
      <LinkBanner link={desk.link} />

      {desk.link.state === "live" && desk.sessions.length === 0 && desk.listedAt !== null && (
        <div className="empty">
          这个状态目录里还没有对话。
          <span className="sub">去对话面写一句话就开始一段。</span>
        </div>
      )}

      {desk.sessions.length > 0 && (
        <ul className="convos" role="listbox" aria-label="会话" ref={body}>
          {desk.sessions.map((session) => {
            const open = desk.current?.key === session.key;
            const reply = lastReply(session);
            const when = since(touchedAt(desk, session));
            return (
              <li key={session.key}>
                <div
                  tabIndex={0}
                  role="option"
                  aria-selected={open}
                  className={open ? "convo on" : "convo"}
                  onClick={() => desk.selectSession(session)}
                  onDoubleClick={() => { desk.selectSession(session); desk.go("chat"); }}
                  onFocus={() => desk.selectSession(session)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      desk.selectSession(session);
                      desk.go("chat");
                    }
                  }}
                >
                  <div className="hd">
                    <span className="ask" title={name(session)}>{name(session)}</span>
                    {when && <span className="when">{when}</span>}
                  </div>
                  {reply && <div className="last">{reply}</div>}
                  <div className="meta">
                    {session.activeRunId
                      ? <span className="live"><span className="dot t-live" />这轮还在跑</span>
                      : <span>{session.turnCount} 轮</span>}
                    {/* A branch past generation 1 has been rolled back. Worth
                        saying: it is why an earlier Turn is no longer here. */}
                    {session.generation > 1 && (
                      <span className="flag">已回滚到第 {session.generation} 代</span>
                    )}
                    {/* Which strand this row is, and nothing about where it came
                        from: a head carries no parent, so "分叉自……" would be a
                        sentence the runtime never said. */}
                    {(strands.get(session.sessionId) ?? 0) > 1 && (
                      <span className="flag mono">分支 {shortId(session.branchId)}</span>
                    )}
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

register({
  id: "conversations",
  label: "会话",
  group: "work",
  badge: (desk) => {
    const turning = desk.sessions.filter((session) => session.activeRunId).length;
    return turning === 0 ? undefined : turning;
  },
  view: ConversationsView,
  toolbar: ConversationsToolbar,
  keys: [
    { key: "j", hint: "下一段", when: (d) => d.sessions.length > 0, run: (d) => move(d, 1) },
    { key: "k", hint: "上一段", when: (d) => d.sessions.length > 0, run: (d) => move(d, -1) },
    { key: "Enter", hint: "接着说", when: (d) => d.current !== null, run: (d) => d.go("chat") },
    {
      key: "n",
      hint: "新对话",
      when: (d) => d.current !== null,
      run: (d) => { d.newConversation(); d.go("chat"); },
    },
  ],
  commands: [
    { id: "conversations:open", title: "查看所有会话", hint: "这个 Runtime 里说过的话" },
  ],
});
