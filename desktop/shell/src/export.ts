import { textOf, type SessionView } from "./session";

/// One conversation, as text a person can keep.
///
/// Built from the same `textOf` the transcript renders with, so the file and
/// the screen cannot disagree about what was said. Anything that is not text --
/// a tool call, a tool result, a reasoning summary -- is left out for the same
/// reason `textOf` leaves it out: rendered as prose it would be worse than
/// absent, and it is all still in the Run's own log.
///
/// What it will not do is imply completeness it cannot see. A conversation
/// whose history was paged short, and one with a Turn still running, each get a
/// line saying so at the top -- the same two facts the window shows, in the
/// place a person reading the file would look for them.
export function asMarkdown(session: SessionView): string {
  const lines: string[] = [];
  lines.push(`# ${session.title || "（还没有标题）"}`);
  lines.push("");

  const notes: string[] = [];
  // `turnCount` is the branch head's count; `turns` is what this window paged
  // in. Fewer means the export is a prefix, and a prefix that does not say so
  // reads as the whole conversation.
  if (session.turns.length < session.turnCount) {
    notes.push(
      `这里只有 ${session.turns.length} 轮，这段对话一共 ${session.turnCount} 轮`
      + " —— 客户端翻页有上限，前面的没有取回来。",
    );
  }
  if (session.activeRunId) {
    notes.push("还有一轮在跑，它还没落成 Turn，所以不在这里。");
  }
  if (notes.length > 0) {
    for (const note of notes) lines.push(`> ${note}`);
    lines.push("");
  }

  for (const turn of session.turns) {
    const said = textOf(turn, "user");
    const answered = textOf(turn, "assistant");
    lines.push(`## 第 ${turn.turn_ordinal} 轮`);
    lines.push("");
    lines.push("**你**");
    lines.push("");
    lines.push(said || "（这一轮没有可导出的文本）");
    lines.push("");
    lines.push("**模型**");
    lines.push("");
    lines.push(answered || "（这一轮没有可导出的文本）");
    lines.push("");
  }

  return lines.join("\n");
}
