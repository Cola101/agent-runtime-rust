import type { ReactNode } from "react";
import { marked } from "marked";
import { Mark } from "./Mark";

type Token = marked.Token;

/// Drawing what the model actually wrote.
///
/// Models answer in markdown. Before this existed the transcript drew that
/// markdown as one flat paragraph, so a fenced block arrived as prose with
/// three backticks in it and a five-step list arrived as one long line.
///
/// The parser is `marked`, and that choice is evidence rather than taste: both
/// shipping desktop clients on this machine bundle it and nothing else of the
/// kind -- `/Applications/Claude.app/Contents/Resources/app.asar` carries
/// `marked` alone, and ChatGPT's carries `marked` plus `katex` and `shiki` for
/// maths and highlighting. Neither ships the remark/react-markdown pipeline.
/// The first version of this file was a hand-written parser covering the
/// shapes I happened to think of; it had no tables, no nested lists, no task
/// lists, no strikethrough and no autolinks, and a model writes all of those
/// constantly. The list of things a person can type is not a list anyone
/// finishes guessing.
///
/// What is deliberately *not* taken is `marked.parse()`. That returns an HTML
/// string, and putting a model's output through `dangerouslySetInnerHTML`
/// makes every reply an injection surface -- the model is quoting the web, and
/// some of the web is hostile. This walks the token tree instead and builds
/// React elements, which costs one function and buys three things: no raw HTML
/// is ever inserted, every text leaf can still go through `Mark` so ⌘F counts
/// what it counted before, and the shapes we choose not to draw are simply not
/// drawn rather than passed through.

marked.use({ gfm: true, breaks: true });

/// Where a link is allowed to point.
///
/// Anything else is drawn as text with no href. `javascript:` is the reason --
/// it is a script the person runs by clicking something the model wrote -- and
/// an allow-list is the only form of this check that stays correct as new
/// schemes are invented.
const REACHABLE = /^(https?:|mailto:|#|\/|\.)/i;

function safeHref(href: string): string | null {
  const cleaned = href.trim();
  return REACHABLE.test(cleaned) ? cleaned : null;
}

function inline(tokens: Token[] | undefined, query: string, text = ""): ReactNode {
  if (!tokens || tokens.length === 0) return <Mark text={text} query={query} />;
  return (
    <>
      {tokens.map((token, index) => {
        switch (token.type) {
          case "strong":
            return (
              <strong key={index}>
                {inline((token as marked.Tokens.Strong).tokens, query, token.text)}
              </strong>
            );
          case "em":
            return (
              <em key={index}>{inline((token as marked.Tokens.Em).tokens, query, token.text)}</em>
            );
          case "del":
            return (
              <del key={index}>{inline((token as marked.Tokens.Del).tokens, query, token.text)}</del>
            );
          case "codespan":
            // `marked` has already resolved the doubled-backtick form and
            // decoded the entities it introduced, so this is the literal text
            // the person meant to show.
            return (
              <code key={index}>
                <Mark text={decode((token as marked.Tokens.Codespan).text)} query={query} />
              </code>
            );
          case "br":
            return <br key={index} />;
          case "link": {
            const link = token as marked.Tokens.Link;
            const href = safeHref(link.href);
            const body = inline(link.tokens, query, link.text);
            // Denied in-window by the main process's window-open handler,
            // which hands the URL to the real browser instead.
            return href
              ? (
                <a key={index} href={href} target="_blank" rel="noreferrer noopener">
                  {body}
                </a>
              )
              : <span key={index}>{body}</span>;
          }
          case "image": {
            // Drawn as its own text rather than fetched. A remote image in a
            // reply is a request this app makes to a third party because a
            // model named it, which is a tracking pixel with extra steps.
            const image = token as marked.Tokens.Image;
            return (
              <span key={index} className="img-ref">
                <Mark text={image.text || image.href} query={query} />
              </span>
            );
          }
          case "html":
            // The model wrote a tag. It is a thing it said, not a thing to run.
            return <Mark key={index} text={(token as marked.Tokens.HTML).raw} query={query} />;
          case "escape":
            return <Mark key={index} text={(token as marked.Tokens.Escape).text} query={query} />;
          default:
            return (
              <Mark key={index} text={decode((token as marked.Tokens.Text).text ?? "")} query={query} />
            );
        }
      })}
    </>
  );
}

/// `marked` escapes text for HTML output; we are not producing HTML.
///
/// Without this a reply containing `a < b` reaches the screen as `a &lt; b`,
/// which is the one thing worse than not rendering markdown at all: it is
/// wrong text presented as if it were what the model said.
const ENTITIES: Record<string, string> = {
  "&amp;": "&", "&lt;": "<", "&gt;": ">", "&quot;": '"', "&#39;": "'", "&#x27;": "'",
};
function decode(text: string): string {
  return text.replace(/&(?:amp|lt|gt|quot|#39|#x27);/g, (found) => ENTITIES[found] ?? found);
}

function Heading({ depth, children }: { depth: number; children: ReactNode }) {
  // Past three the visual scale has nothing left to say, and a reply is not a
  // document with six levels of structure.
  const Tag = `h${Math.min(depth, 3)}` as "h1" | "h2" | "h3";
  return <Tag>{children}</Tag>;
}

function Block({ token, query }: { token: Token; query: string }): ReactNode {
  switch (token.type) {
    case "space":
      return null;
    case "code": {
      const code = token as marked.Tokens.Code;
      return (
        <pre data-lang={code.lang || undefined}>
          <code><Mark text={code.text} query={query} /></code>
        </pre>
      );
    }
    case "heading": {
      const heading = token as marked.Tokens.Heading;
      return (
        <Heading depth={heading.depth}>{inline(heading.tokens, query, heading.text)}</Heading>
      );
    }
    case "table": {
      const table = token as marked.Tokens.Table;
      return (
        // Its own scroller: a wide table must not stretch the reading column,
        // and wrapped cells are a table nobody can read down.
        <div className="tbl">
          <table>
            <thead>
              <tr>
                {table.header.map((cell, at) => (
                  <th key={at} align={table.align[at] ?? undefined}>
                    {inline(cell.tokens, query, cell.text)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {table.rows.map((row, at) => (
                <tr key={at}>
                  {row.map((cell, column) => (
                    <td key={column} align={table.align[column] ?? undefined}>
                      {inline(cell.tokens, query, cell.text)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }
    case "hr":
      return <hr />;
    case "blockquote":
      return (
        <blockquote>
          {(token as marked.Tokens.Blockquote).tokens.map((inner, at) => (
            <Block key={at} token={inner} query={query} />
          ))}
        </blockquote>
      );
    case "list": {
      const list = token as marked.Tokens.List;
      const items = list.items.map((item, at) => (
        <li key={at} className={item.task ? "task" : undefined}>
          {item.task && (
            // Readable, not operable. The transcript is a record of what was
            // said; a checkbox a person could tick would be a control that
            // changes nothing, which is worse than no control.
            <input type="checkbox" checked={item.checked === true} disabled readOnly />
          )}
          {item.tokens.map((inner, index) => <Block key={index} token={inner} query={query} />)}
        </li>
      ));
      return list.ordered
        ? <ol start={typeof list.start === "number" ? list.start : undefined}>{items}</ol>
        : <ul>{items}</ul>;
    }
    case "html":
      return <p>{<Mark text={(token as marked.Tokens.HTML).raw} query={query} />}</p>;
    case "paragraph":
      return <p>{inline((token as marked.Tokens.Paragraph).tokens, query, token.text)}</p>;
    case "text": {
      const text = token as marked.Tokens.Text;
      // Inside a list item this is the item's own words and must not open a
      // paragraph, or every tight list becomes a loose one.
      return <>{inline(text.tokens, query, text.text)}</>;
    }
    default:
      return null;
  }
}

/// One reply, drawn.
export function Markdown({ text, query }: { text: string; query: string }) {
  return (
    <>
      {marked.lexer(text).map((token, index) => (
        <Block key={index} token={token} query={query} />
      ))}
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
  // Inline pieces of one block join with nothing and blocks join with a
  // space. Joining everything with a space put a gap before every comma that
  // followed a bold run -- "工作区工具 ，不是 shell" -- which is the sort of
  // wrongness that reads as a rendering bug in the row itself.
  // Not every token kind carries `text` -- `space` and `def` do not -- so the
  // read is guarded rather than asserted.
  const textOf = (token: Token): string =>
    "text" in token && typeof token.text === "string" ? token.text : "";
  const blocks: string[] = [];
  const inlineText = (tokens: Token[] | undefined, fallback: string): string => {
    if (!tokens || tokens.length === 0) return fallback;
    return tokens.map((token) => {
      if (token.type === "image") return "";
      const carried = (token as marked.Tokens.Paragraph).tokens;
      if (carried && carried.length > 0) return inlineText(carried, textOf(token));
      if (token.type === "br") return " ";
      return (token as marked.Tokens.Text).text ?? "";
    }).join("");
  };
  const walk = (tokens: Token[]) => {
    for (const token of tokens) {
      switch (token.type) {
        case "code":
        case "space":
        case "hr":
          break;
        case "table": {
          const table = token as marked.Tokens.Table;
          for (const cell of table.header) blocks.push(cell.text);
          for (const row of table.rows) for (const cell of row) blocks.push(cell.text);
          break;
        }
        case "list":
          for (const item of (token as marked.Tokens.List).items) walk(item.tokens);
          break;
        case "blockquote":
          walk((token as marked.Tokens.Blockquote).tokens);
          break;
        default: {
          const said = inlineText((token as marked.Tokens.Paragraph).tokens, textOf(token));
          if (said.trim()) blocks.push(said);
        }
      }
    }
  };
  walk(marked.lexer(text));
  return decode(blocks.join(" ")).replace(/\s+/g, " ").trim();
}
