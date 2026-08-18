import { Fragment, useEffect, useState } from "react";
import { bridge } from "../runtime";
import { changes, hunks } from "../diff";
import { Mark } from "./Mark";

/// What a write would change, against the file it would change.
///
/// Approving `workspace.write_text` is approving an overwrite, and the card
/// showed the whole new text and nothing about what is there now -- so the one
/// question being asked had to be answered on another surface, from memory.
///
/// The comparison is read through the same bridge call the workspace surface
/// uses, so it is the file the runtime would actually write to. When it cannot
/// be read the card says which of the two that is: a file that is not there
/// yet, or a file this window could not open. A diff against an assumption
/// would be wrong with authority.
export function WriteReview({ path, text, query }: { path: string; text: string; query: string }) {
  const [before, setBefore] = useState<{ text: string } | { missing: string } | null>(null);

  useEffect(() => {
    const api = bridge();
    if (!api?.readFile) return;
    let live = true;
    void api.readFile(path).then((reply) => {
      if (!live) return;
      if (!reply.ok) { setBefore({ missing: reply.error }); return; }
      if (reply.value.binary) { setBefore({ missing: "这是二进制文件，比不了" }); return; }
      if (reply.value.truncated) { setBefore({ missing: "文件太大，只读回了一部分，不做对比" }); return; }
      setBefore({ text: reply.value.text ?? "" });
    });
    return () => { live = false; };
  }, [path]);

  if (!before) return null;
  if ("missing" in before) {
    return (
      <div className="review">
        <Mark
          text={before.missing.includes("no such") || before.missing.includes("不存在")
            ? "这个文件现在还不存在，下面整段都是新加的。"
            : `读不到现在的内容，所以没法对比：${before.missing}`}
          query={query}
        />
      </div>
    );
  }
  const walked = changes(before.text, text);
  if (!walked) {
    return <div className="review"><Mark text="文件太长，这里不做逐行对比。" query={query} /></div>;
  }
  if (walked.every((change) => change.kind === "same")) {
    return <div className="review"><Mark text="写进去的内容和现在完全一样。" query={query} /></div>;
  }
  return (
    <div className="review">
      {hunks(walked).map((hunk, index) => (
        <Fragment key={index}>
          {hunk.skipped > 0 && (
            <div className="skip"><Mark text={`… 中间 ${hunk.skipped} 行没变`} query={query} /></div>
          )}
          {hunk.changes.map((change, at) => (
            <div className={`line ${change.kind}`} key={at}>
              <Mark
                text={`${change.kind === "add" ? "+" : change.kind === "drop" ? "-" : " "}${change.text}`}
                query={query}
              />
            </div>
          ))}
        </Fragment>
      ))}
    </div>
  );
}

