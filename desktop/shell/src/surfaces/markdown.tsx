import type { ReactNode } from "react";
import { Mark } from "./Mark";

/// Drawing what the model actually wrote.
///
/// Models answer in markdown. Until this existed the transcript drew that
/// markdown as one flat paragraph, so a fenced block arrived as prose with
/// three backticks in it and a five-step list arrived as one long line. The
/// content was all there and none of it was readable.
///
/// This is deliberately a small parser rather than a dependency. What a reply
/// contains is a known, short list -- paragraphs, fences, lists, headings,
/// emphasis, links -- and the failure mode that matters is a half-written
/// document, because every reply is a half-written document until the last
/// token lands. A fence with no closing fence is a code block here, not a
/// parse error, and that is the case a general-purpose renderer gets to be
/// pedantic about and this one does not.

type Block =
  | { kind: "code"; lang: string; text: string }
  | { kind: "heading"; level: number; text: string }
  | { kind: "list"; ordered: boolean; start: number; items: string[] }
  | { kind: "quote"; text: string }
  | { kind: "rule" }
  | { kind: "para"; text: string };

const FENCE = /^\s*```+\s*([A-Za-z0-9_+-]*)\s*$/;
const HEADING = /^(#{1,6})\s+(.*)$/;
const BULLET = /^\s*[-*+]\s+(.*)$/;
const NUMBER = /^\s*(\d{1,9})[.)]\s+(.*)$/;
const QUOTE = /^\s*>\s?(.*)$/;
const RULE = /^\s*(?:---+|\*\*\*+|___+)\s*$/;

export function parse(text: string): Block[] {
  const lines = text.split("\n");
  const blocks: Block[] = [];
  let at = 0;

  while (at < lines.length) {
    const line = lines[at];

    const fence = FENCE.exec(line);
    if (fence) {
      const body: string[] = [];
      at += 1;
      // Runs to the closing fence, or to the end of what has arrived. The
      // second case is every streamed reply mid-flight.
      while (at < lines.length && !FENCE.test(lines[at])) {
        body.push(lines[at]);
        at += 1;
      }
      if (at < lines.length) at += 1;
      blocks.push({ kind: "code", lang: fence[1] ?? "", text: body.join("\n") });
      continue;
    }

    if (line.trim() === "") { at += 1; continue; }

    if (RULE.test(line)) { blocks.push({ kind: "rule" }); at += 1; continue; }

    const heading = HEADING.exec(line);
    if (heading) {
      blocks.push({ kind: "heading", level: heading[1].length, text: heading[2] });
      at += 1;
      continue;
    }

    const bullet = BULLET.exec(line);
    const numbered = NUMBER.exec(line);
    if (bullet || numbered) {
      const ordered = numbered !== null;
      const start = numbered ? Number(numbered[1]) : 1;
      const items: string[] = [];
      while (at < lines.length) {
        const b = BULLET.exec(lines[at]);
        const n = NUMBER.exec(lines[at]);
        if (ordered && n) items.push(n[2]);
        else if (!ordered && b) items.push(b[1]);
        else break;
        at += 1;
      }
      blocks.push({ kind: "list", ordered, start, items });
      continue;
    }

    const quote = QUOTE.exec(line);
    if (quote) {
      const body = [quote[1]];
      at += 1;
      while (at < lines.length) {
        const more = QUOTE.exec(lines[at]);
        if (!more) break;
        body.push(more[1]);
        at += 1;
      }
      blocks.push({ kind: "quote", text: body.join("\n") });
      continue;
    }

    // A paragraph runs until a blank line or until some other block starts on
    // its own line, so a list that follows prose without a blank line between
    // them is still a list.
    const body: string[] = [];
    while (at < lines.length) {
      const next = lines[at];
      if (next.trim() === "") break;
      if (body.length > 0 && (FENCE.test(next) || HEADING.test(next) || RULE.test(next)
        || BULLET.test(next) || NUMBER.test(next) || QUOTE.test(next))) break;
      body.push(next);
      at += 1;
    }
    blocks.push({ kind: "para", text: body.join("\n") });
  }

  return blocks;
}

// Code first: whatever is inside a span of code is text, including the
// characters that would otherwise be emphasis.
const INLINE = /`([^`\n]+)`|\*\*([\s\S]+?)\*\*|\[([^\]\n]+)\]\(([^)\s]+)\)|\*([^*\n]+)\*/g;

function inline(text: string, query: string): ReactNode {
  const out: ReactNode[] = [];
  let at = 0;
  let key = 0;
  INLINE.lastIndex = 0;
  for (;;) {
    const found = INLINE.exec(text);
    if (!found) break;
    if (found.index > at) {
      out.push(<Mark key={key++} text={text.slice(at, found.index)} query={query} />);
    }
    const [, code, strong, label, href, em] = found;
    if (code !== undefined) {
      out.push(<code key={key++}><Mark text={code} query={query} /></code>);
    } else if (strong !== undefined) {
      out.push(<strong key={key++}><Mark text={strong} query={query} /></strong>);
    } else if (label !== undefined) {
      // Denied in-window by the main process's window-open handler, which
      // hands the URL to the real browser instead.
      out.push(
        <a key={key++} href={href} target="_blank" rel="noreferrer noopener">
          <Mark text={label} query={query} />
        </a>,
      );
    } else if (em !== undefined) {
      out.push(<em key={key++}><Mark text={em} query={query} /></em>);
    }
    at = found.index + found[0].length;
  }
  if (at < text.length) {
    out.push(<Mark key={key++} text={text.slice(at)} query={query} />);
  }
  return <>{out}</>;
}

function Heading({ level, children }: { level: number; children: ReactNode }) {
  // Past three the visual scale has nothing left to say, and a reply is not a
  // document with six levels of structure.
  const Tag = `h${Math.min(level, 3)}` as "h1" | "h2" | "h3";
  return <Tag>{children}</Tag>;
}

/// One block of a reply, drawn.
export function Markdown({ text, query }: { text: string; query: string }) {
  return (
    <>
      {parse(text).map((block, index) => {
        switch (block.kind) {
          case "code":
            return (
              <pre key={index} data-lang={block.lang || undefined}>
                <code><Mark text={block.text} query={query} /></code>
              </pre>
            );
          case "heading":
            return (
              <Heading key={index} level={block.level}>
                {inline(block.text, query)}
              </Heading>
            );
          case "list":
            return block.ordered
              ? (
                <ol key={index} start={block.start}>
                  {block.items.map((item, at) => <li key={at}>{inline(item, query)}</li>)}
                </ol>
              )
              : (
                <ul key={index}>
                  {block.items.map((item, at) => <li key={at}>{inline(item, query)}</li>)}
                </ul>
              );
          case "quote":
            return <blockquote key={index}>{inline(block.text, query)}</blockquote>;
          case "rule":
            return <hr key={index} />;
          case "para":
            return <p key={index}>{inline(block.text, query)}</p>;
        }
      })}
    </>
  );
}

/// A reply as one line of plain text.
///
/// For lists that have room for one line and no room for markup. Handed the
/// raw text, the conversation list drew "## 改了什么 ... ```rust let path ="
/// -- syntax the reader was never meant to see, spending the one line it had
/// on backticks.
///
/// Fenced code is dropped rather than flattened. A block of code with its
/// newlines squeezed out is not a summary of anything, and a reply that opens
/// with a fence would otherwise have its whole line eaten by the first
/// statement. A reply that is nothing but code therefore summarises to
/// nothing, which is the honest answer -- the caller draws no line at all.
export function plain(text: string): string {
  const parts: string[] = [];
  for (const block of parse(text)) {
    switch (block.kind) {
      case "code":
      case "rule":
        break;
      case "list":
        parts.push(...block.items);
        break;
      case "heading":
      case "quote":
      case "para":
        parts.push(block.text);
        break;
    }
  }
  return parts
    .join(" ")
    .replace(/`([^`\n]+)`/g, "$1")
    .replace(/\*\*([\s\S]+?)\*\*/g, "$1")
    .replace(/\[([^\]\n]+)\]\([^)\s]+\)/g, "$1")
    .replace(/\*([^*\n]+)\*/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
}
