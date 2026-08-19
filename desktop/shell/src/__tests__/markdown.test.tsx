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

  it("同一段里的换行留着，不当成分段", () => {
    const box = draw("上一行\n下一行");
    expect(box.querySelectorAll("p")).toHaveLength(1);
    expect(box.textContent).toBe("上一行\n下一行");
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
