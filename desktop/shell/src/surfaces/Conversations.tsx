import { useEffect, useRef } from "react";
import { register } from "./registry";
import { since } from "./model";
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

function move(desk: Desk, delta: number): void {
  const ids = desk.sessions.map((session) => session.sessionId);
  if (ids.length === 0) return;
  const at = desk.current ? ids.indexOf(desk.current.sessionId) : -1;
  const next = at === -1
    ? (delta > 0 ? 0 : ids.length - 1)
    : Math.min(Math.max(at + delta, 0), ids.length - 1);
  desk.selectSession(ids[next]);
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
  const body = useRef<HTMLTableSectionElement>(null);

  useEffect(() => {
    body.current?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [desk.current?.sessionId]);

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
        <table className="rows">
          <thead>
            <tr>
              <th>说的是</th><th>状态</th>
              <th className="num">轮次</th><th className="num">最后更新</th>
            </tr>
          </thead>
          <tbody ref={body}>
            {desk.sessions.map((session) => {
              const open = desk.current?.sessionId === session.sessionId;
              return (
                <tr
                  key={session.sessionId}
                  tabIndex={0}
                  role="button"
                  aria-selected={open}
                  className={open ? "on" : ""}
                  onClick={() => desk.selectSession(session.sessionId)}
                  onDoubleClick={() => { desk.selectSession(session.sessionId); desk.go("chat"); }}
                  onFocus={() => desk.selectSession(session.sessionId)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      desk.selectSession(session.sessionId);
                      desk.go("chat");
                    }
                  }}
                >
                  <td className="ask" title={name(session)}>{name(session)}</td>
                  <td>
                    {session.activeRunId
                      ? <><span className="dot t-live" />这轮还在跑</>
                      : <><span className="dot t-done" />停着</>}
                    {/* A branch past generation 1 has been rolled back. Worth
                        saying: it is why an earlier Turn is no longer here. */}
                    {session.generation > 1 && (
                      <span className="flag">已回滚到第 {session.generation} 代</span>
                    )}
                  </td>
                  <td className="num">{session.turnCount}</td>
                  <td className="num">{since(touchedAt(desk, session))}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
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
