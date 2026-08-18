/// A conversation, as the runtime holds it.
///
/// The distinction that matters here: a Run's *event log* and a Session Turn's
/// *transcript* are two different sources. The event log is what happened while
/// a Turn was running and can be retired; the transcript is what the runtime
/// froze into the Session, and it is what the next Turn is actually given as
/// history. Rendering a conversation from events would show a conversation the
/// model was never told about.
import type { SessionHead, SessionTurn } from "./runtime";

export type SessionView = {
  sessionId: string;
  branchId: string;
  generation: number;
  turnCount: number;
  /// Set while a Turn is in flight. The branch refuses a second Turn until it
  /// clears, which is why this disables the composer rather than a guess about
  /// whether the last thing on screen looked finished.
  activeRunId: string | null;
  turns: SessionTurn[];
  title: string;
};

/// Every text part of one role's messages in a Turn, joined.
///
/// Parts that are not text are skipped rather than stringified: a tool result
/// rendered as `[object Object]` is worse than not rendered at all, and the
/// tool call itself is already on the Run.
export function textOf(turn: SessionTurn, role: string): string {
  return turn.transcript
    .filter((message) => message.role === role)
    .flatMap((message) => message.content)
    .filter((part) => part.type === "text" && typeof part.text === "string")
    .map((part) => part.text as string)
    .join("");
}

/// What to call a conversation: the first thing the person said in it.
///
/// A Session that has not committed its first Turn has no title yet, and this
/// says so by returning empty rather than inventing one -- the caller can fall
/// back to the live Run's input, which is the same sentence from the other
/// durable source.
export function titleFrom(turns: SessionTurn[]): string {
  const first = turns.find((turn) => turn.turn_ordinal === 1);
  return first ? textOf(first, "user") : "";
}

export function viewOf(head: SessionHead, turns: SessionTurn[], fallbackTitle = ""): SessionView {
  return {
    sessionId: head.session_id,
    branchId: head.branch_id,
    generation: head.generation,
    turnCount: head.turn_count,
    activeRunId: head.active_run_id,
    turns,
    title: titleFrom(turns) || fallbackTitle,
  };
}

/// Newest first.
///
/// Sound only because this client mints v7 session ids: the runtime returns
/// heads sorted by `(session_id, branch_id)`, so with time-ordered ids that
/// sort is chronological. See `ids.ts` -- with v4 ids this function would be
/// putting a confident label on an arbitrary order.
export function newestFirst(views: SessionView[]): SessionView[] {
  return [...views].sort((a, b) => (a.sessionId < b.sessionId ? 1 : a.sessionId > b.sessionId ? -1 : 0));
}
