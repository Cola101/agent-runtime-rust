import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Markdown, plain } from "../surfaces/markdown";

/// What the model writes is markdown, and until this existed the transcript
/// drew it as one flat paragraph: a fenced block came out as prose with three
/// backticks in it, and a list came out as a line starting with a hyphen.

function draw(text: string, query = "") {
  return render(<Markdown text={text} query={query} />).container;
}

describe("对话里的 Markdown", () => {
  it("把围栏代码画成代码块，而不是带反引号的正文", () => {
    const box = draw("先看这个：\n\n```rust\nfn main() {}\n```\n");
    const code = box.querySelector("pre code");
    expect(code?.textContent).toBe("fn main() {}");
    expect(box.textContent).not.toContain("```");
  });

  it("记住围栏上写的语言，好让人知道在读什么", () => {
    const box = draw("```rust\nfn main() {}\n```");
    expect(box.querySelector("pre")?.getAttribute("data-lang")).toBe("rust");
  });

  it("没写完的围栏也照样是代码块——流式读到一半就是这样", () => {
    const box = draw("```py\nimport os\nprint(1)");
    expect(box.querySelector("pre code")?.textContent).toBe("import os\nprint(1)");
  });

  it("行内反引号是代码，不是字面的反引号", () => {
    const box = draw("改 `Cargo.toml` 就行");
    expect(box.querySelector("code")?.textContent).toBe("Cargo.toml");
    expect(box.textContent).toBe("改 Cargo.toml 就行");
  });

  it("列表是列表", () => {
    const box = draw("- 第一条\n- 第二条\n");
    expect(box.querySelectorAll("li")).toHaveLength(2);
    expect(box.querySelectorAll("li")[1].textContent).toBe("第二条");
  });

  it("有序列表保住它自己的起始序号", () => {
    const box = draw("3. 第三步\n4. 第四步\n");
    expect(box.querySelector("ol")?.getAttribute("start")).toBe("3");
  });

  it("标题是标题", () => {
    const box = draw("## 结论\n\n就这样。");
    expect(box.querySelector("h2")?.textContent).toBe("结论");
  });

  it("粗体不是四个星号", () => {
    const box = draw("这里**很重要**。");
    expect(box.querySelector("strong")?.textContent).toBe("很重要");
    expect(box.textContent).toBe("这里很重要。");
  });

  it("链接在新窗口打开——主进程把它交给真正的浏览器", () => {
    const box = draw("见 [文档](https://example.com/d)。");
    const a = box.querySelector("a");
    expect(a?.getAttribute("href")).toBe("https://example.com/d");
    expect(a?.getAttribute("target")).toBe("_blank");
    expect(a?.getAttribute("rel")).toContain("noreferrer");
  });

  it("空行分段，不把两段挤成一段", () => {
    const box = draw("头一段。\n\n第二段。");
    expect(box.querySelectorAll("p")).toHaveLength(2);
  });

  /// 换行怎么留住，本轮换了做法。
  ///
  /// 原来靠 `white-space: pre-wrap` 把原文里的 `\n` 原样留在一个 `<p>` 里；
  /// 现在是 `marked` 的 `breaks: true`，同一段里的单个换行变成一个 `<br>`。
  /// 画出来一模一样，但 `textContent` 里不再有 `\n`——所以这条断言跟着改，
  /// 改的是判据不是行为。**推翻的旧结论**：段内换行靠 CSS 保留。
  /// 之所以换：CommonMark 默认会把段内单换行当成空格，而模型是按行写字的，
  /// 一段五行的说明会被挤成一行。两家桌面端都开 `breaks`。
  it("同一段里的换行留着，不当成分段", () => {
    const box = draw("上一行\n下一行");
    expect(box.querySelectorAll("p")).toHaveLength(1);
    expect(box.querySelectorAll("br")).toHaveLength(1);
    expect(box.textContent).toBe("上一行下一行");
  });

  it("⌘F 找的词照样标出来——正文里", () => {
    draw("改 Cargo.toml 就行", "cargo");
    expect(screen.getByText("Cargo", { selector: "mark" })).toBeTruthy();
  });

  it("⌘F 找的词照样标出来——代码块里", () => {
    const box = draw("```\nfn main\n```", "main");
    expect(box.querySelector("pre mark")?.textContent).toBe("main");
  });
});

/// One line of a reply, for a list that has room for one line.
///
/// The conversation list draws the last reply under each title. Handed the raw
/// text it drew "## 改了什么 ... ```rust let path = ..." -- the markup a reader
/// was never meant to see, spending the one line it had on backticks.
describe("把回复压成一行", () => {
  it("扔掉标题的井号，留下标题的字", () => {
    expect(plain("## 改了什么\n\n写好了。")).toBe("改了什么 写好了。");
  });

  it("围栏里的代码不进摘要——一行放不下，也不是给人扫的", () => {
    expect(plain("改好了。\n\n```rust\nfn main() {}\n```\n\n就这样。"))
      .toBe("改好了。 就这样。");
  });

  it("行内标记只留字", () => {
    expect(plain("写入 `notes.txt` 用的是 **工作区工具**，不是 *shell*"))
      .toBe("写入 notes.txt 用的是 工作区工具，不是 shell");
  });

  it("链接留下看得懂的那半边", () => {
    expect(plain("见 [工作区工具说明](https://example.com/w)。")).toBe("见 工作区工具说明。");
  });

  it("列表压成一行时不把两项黏成一个词", () => {
    expect(plain("- 第一条\n- 第二条")).toBe("第一条 第二条");
  });

  it("整段都是代码时，如实地什么都不说", () => {
    expect(plain("```\nfn main() {}\n```")).toBe("");
  });
});

/// The shapes a real answer actually contains.
///
/// The hand-written parser this file first guarded covered fences, headings,
/// lists, quotes and emphasis -- the shapes I thought of. A model writes
/// tables constantly, nests its lists, ticks checkboxes, strikes text out and
/// pastes bare URLs, and every one of those came out as literal punctuation.
///
/// Both shipping desktop clients on this machine parse with `marked`
/// (/Applications/Claude.app and /Applications/ChatGPT.app both bundle it;
/// ChatGPT adds katex and shiki on top). This corpus is why: the list of
/// things a person can type is not a list anyone finishes guessing.
describe("一段回答里真正会出现的东西", () => {
  it("表格是表格，不是一行竖线", () => {
    const box = draw("| 名字 | 大小 |\n| --- | ---: |\n| a.txt | 12 |\n| b.txt | 34 |\n");
    expect(box.querySelectorAll("thead th")).toHaveLength(2);
    expect(box.querySelectorAll("tbody tr")).toHaveLength(2);
    expect(box.querySelector("thead th")!.textContent).toBe("名字");
    expect(box.textContent).not.toContain("---");
  });

  it("表格记住每一列的对齐", () => {
    const box = draw("| 左 | 右 |\n| :-- | --: |\n| a | 1 |\n");
    const cells = box.querySelectorAll("tbody td");
    expect(cells[1].getAttribute("align")).toBe("right");
  });

  it("嵌套列表是嵌套的，不是拍平的", () => {
    const box = draw("- 外层\n  - 里层甲\n  - 里层乙\n- 外层二\n");
    expect(box.querySelectorAll("ul > li")).toHaveLength(4);
    expect(box.querySelectorAll("li ul")).toHaveLength(1);
    expect(box.querySelectorAll("li ul > li")).toHaveLength(2);
  });

  it("任务清单画成勾选框，而且是只读的", () => {
    const box = draw("- [x] 做完了\n- [ ] 还没做\n");
    const boxes = box.querySelectorAll<HTMLInputElement>('input[type="checkbox"]');
    expect(boxes).toHaveLength(2);
    expect(boxes[0].checked).toBe(true);
    expect(boxes[1].checked).toBe(false);
    // 转录是记录，不是表单：勾它不该改变任何东西。
    expect(boxes[0].disabled).toBe(true);
  });

  it("删除线是删除线", () => {
    const box = draw("~~不要这个~~ 要这个");
    expect(box.querySelector("del")?.textContent).toBe("不要这个");
    expect(box.textContent).not.toContain("~~");
  });

  it("光秃秃的网址也是链接", () => {
    const box = draw("详见 https://example.com/a 这一页");
    expect(box.querySelector("a")?.getAttribute("href")).toBe("https://example.com/a");
  });

  it("被转义的星号就是星号", () => {
    const box = draw("文件名是 \\*.rs，不是强调");
    expect(box.querySelector("strong")).toBeNull();
    expect(box.textContent).toContain("*.rs");
  });

  it("行尾两个空格是硬换行", () => {
    const box = draw("上一行  \n下一行");
    expect(box.querySelectorAll("br")).toHaveLength(1);
  });

  it("行内代码里的反引号留得住", () => {
    const box = draw("写作 `` `x` `` 就行");
    expect(box.querySelector("code")?.textContent).toBe("`x`");
  });

  it("模型写出来的 HTML 是给人看的字，不是要执行的标签", () => {
    const box = draw("别这样写：<img src=x onerror=alert(1)> 和 <script>alert(2)</script>");
    expect(box.querySelector("img")).toBeNull();
    expect(box.querySelector("script")).toBeNull();
    expect(box.textContent).toContain("<script>");
  });

  it("javascript: 链接不给点", () => {
    const box = draw("[点我](javascript:alert(1))");
    const href = box.querySelector("a")?.getAttribute("href");
    expect(href === null || href === undefined || !href.startsWith("javascript:")).toBe(true);
  });
});

/// 流式读到一半的样子。
///
/// 每一条回复在最后一个 token 落地之前都是残缺文档，所以这些不是边角料，
/// 这是每一次回答的必经状态。streamdown 的做法是把补闭合符和分块渲染分开：
/// 补闭合符只处理**行内**标记（粗体、行内代码、链接），围栏和表格靠分块层
/// ——完整块各自记忆化，只有末尾那个残块每 tick 重画。我们照这个分工。
describe("读到一半的文档", () => {
  it("表格只写了表头也画得出来，不掉成一行竖线", () => {
    const box = draw("| 名字 | 大小 |\n| --- | --- |\n");
    expect(box.querySelectorAll("thead th")).toHaveLength(2);
    expect(box.textContent).not.toContain("---");
  });

  it("表格写到一半的那一行不会把整张表吃掉", () => {
    const box = draw("| 名字 | 大小 |\n| --- | --- |\n| a.txt | 12 |\n| b.tx");
    expect(box.querySelectorAll("thead th")).toHaveLength(2);
    expect(box.textContent).toContain("a.txt");
  });

  it("链接写到一半时，先把已经到的字给人看", () => {
    const box = draw("详见 [工作区说明](https://exa");
    expect(box.textContent).toContain("工作区说明");
  });

  it("粗体只开了头，不会把后面整段变成星号", () => {
    const box = draw("这里**很重要");
    expect(box.textContent).toContain("很重要");
  });
});
