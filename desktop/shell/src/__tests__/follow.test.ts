import { describe, expect, it } from "vitest";
import { FOLLOW_SLACK_PX, staysWithTheTail } from "../surfaces/follow";

/// 跟不跟着流走，是一条判据，不是散落在事件处理里的几个数。
///
/// 三家参考实现的阈值：opencode 10px（`create-auto-scroll.tsx:19`）、
/// openhands 20px（`use-scroll-to-bottom.ts:17-21`）、assistant-ui 1px
/// （`useThreadViewportAutoScroll.ts:118`）。我们原来是 40px——往上翻 30px
/// 想重读一句，还会被拽回底部。
describe("转录跟着流走的判据", () => {
  const at = (distance: number, selecting = false) =>
    staysWithTheTail({ distanceFromBottom: distance, selecting });

  it("贴着底就跟着走", () => {
    expect(at(0)).toBe(true);
    expect(at(FOLLOW_SLACK_PX - 1)).toBe(true);
  });

  it("往上翻一点点就交给人——留的余量比一行字还小", () => {
    expect(at(FOLLOW_SLACK_PX + 1)).toBe(false);
    // 余量必须小于一行的高度，否则「往上翻一行重读」这个动作会被吃掉。
    expect(FOLLOW_SLACK_PX).toBeLessThan(22);
  });

  it("人正在选字的时候绝不动它——哪怕就贴在底上", () => {
    // opencode 的 create-auto-scroll.tsx:148-154 也是这么做的：选区非空即接管。
    // 流式期间被拽一下，选区就没了，等于选不中。
    expect(at(0, true)).toBe(false);
    expect(at(2, true)).toBe(false);
  });

  it("负的距离也算贴底——橡皮筋回弹时距离会是负数", () => {
    expect(at(-30)).toBe(true);
  });
});
