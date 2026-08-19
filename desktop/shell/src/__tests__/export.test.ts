/// What this file is for.
///
/// A conversation that only exists inside this window is a conversation nobody
/// can keep, quote, or hand to someone else. Gap-list row 16.
///
/// The guards are about honesty rather than formatting: an export that looks
/// complete and is not is worse than no export, because a file carries no
/// context about where it came from. Two ways it can be incomplete — the
/// client paged part of the history, and a Turn is still running — and both
/// have to be visible in the file itself.
import { describe, expect, it } from "vitest";
import { asMarkdown } from "../export";
import type { SessionView } from "../session";

function turn(ordinal: number, said: string, answered: string) {
  return {
    turn_ordinal: ordinal,
    run_id: `run-${ordinal}`,
    digest: "d".repeat(64),
    transcript: [
      { role: "user", content: [{ type: "text", text: said }] },
      { role: "assistant", content: [{ type: "text", text: answered }] },
    ],
  };
}

function session(over: Partial<SessionView> = {}): SessionView {
  return {
    key: "s/b",
    sessionId: "s",
    branchId: "b",
    generation: 1,
    turnCount: 2,
    activeRunId: null,
    title: "看一下目录",
    turns: [turn(1, "看一下目录", "有三个文件。"), turn(2, "读第一个", "第一行写着 hello。")],
    ...over,
  } as SessionView;
}

describe("a conversation as text", () => {
  it("carries what was said and what was answered, in order", () => {
    const said = asMarkdown(session());
    expect(said).toContain("# 看一下目录");
    expect(said).toContain("有三个文件。");
    expect(said).toContain("第一行写着 hello。");
    expect(said.indexOf("有三个文件。")).toBeLessThan(said.indexOf("第一行写着 hello。"));
  });

  /// The client pages history with a ceiling. An export that stops there and
  /// says nothing reads as the whole conversation.
  it("says so when it is only part of the conversation", () => {
    const said = asMarkdown(session({ turnCount: 40 }));
    expect(said).toMatch(/只有 2 轮.*一共 40 轮/);
  });

  it("says nothing about paging when it has the whole thing", () => {
    expect(asMarkdown(session())).not.toContain("翻页");
  });

  /// A Turn in flight has not committed a transcript, so it genuinely is not
  /// in here — which is fine, and has to be said.
  it("says so when a Turn is still running", () => {
    expect(asMarkdown(session({ activeRunId: "run-3" }))).toContain("还有一轮在跑");
  });

  /// A turn whose transcript carried no text at all — a Run that only made
  /// tool calls, say. Better to mark the turn than to emit a blank that reads
  /// as the model saying nothing.
  it("marks a turn with no text rather than leaving a blank", () => {
    const bare = {
      turn_ordinal: 1,
      run_id: "run-1",
      digest: "d".repeat(64),
      transcript: [{ role: "assistant", content: [{ type: "tool_call", tool_call_id: "c1" }] }],
    };
    const said = asMarkdown(session({ turns: [bare] as SessionView["turns"], turnCount: 1 }));
    expect(said).toContain("没有可导出的文本");
  });
});
