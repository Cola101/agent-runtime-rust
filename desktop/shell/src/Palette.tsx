import { useEffect, useMemo, useRef, useState } from "react";
import { commands, type ResolvedCommand } from "./surfaces/registry";
import { useDesk } from "./desk";

/// Subsequence match, the way a command palette is expected to behave: "gr"
/// finds "go to runs". Exact substring matching would make a palette you have
/// to spell for, which is a palette people stop opening.
function score(query: string, text: string): number | null {
  if (!query) return 0;
  const haystack = text.toLowerCase();
  let at = 0;
  let gaps = 0;
  for (const ch of query.toLowerCase()) {
    const found = haystack.indexOf(ch, at);
    if (found === -1) return null;
    gaps += found - at;
    at = found + 1;
  }
  return gaps;
}

export function Palette({ close }: { close(): void }) {
  const desk = useDesk();
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  const matches = useMemo(() => {
    const available = commands().filter((command) => command.when?.(desk) ?? true);
    return available
      .map((command) => ({
        command,
        rank: score(query, `${command.title} ${command.hint ?? ""} ${command.id}`),
      }))
      .filter((entry): entry is { command: ResolvedCommand; rank: number } => entry.rank !== null)
      .sort((a, b) => a.rank - b.rank)
      .map((entry) => entry.command);
  }, [query, desk]);

  // Opening a palette that is not focused is opening a palette you then have
  // to click. The focus goes in on mount and the caller puts it back on close.
  useEffect(() => input.current?.focus(), []);
  useEffect(() => setCursor(0), [query]);
  useEffect(() => {
    list.current?.querySelector<HTMLElement>('[data-on="true"]')
      ?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  const invoke = (command: ResolvedCommand | undefined) => {
    if (!command) return;
    close();
    // A command that declares no action navigates to the surface that declared
    // it. Everything else does its own work; the palette does not guess.
    if (command.run) command.run(desk);
    else desk.go(command.surface);
  };

  return (
    <div className="scrim" onMouseDown={close}>
      <div className="pal" onMouseDown={(event) => event.stopPropagation()}>
        <input
          ref={input}
          className="in"
          value={query}
          placeholder="输入命令"
          spellCheck={false}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "ArrowDown" || (event.ctrlKey && event.key === "n")) {
              event.preventDefault();
              setCursor((at) => Math.min(at + 1, matches.length - 1));
            } else if (event.key === "ArrowUp" || (event.ctrlKey && event.key === "p")) {
              event.preventDefault();
              setCursor((at) => Math.max(at - 1, 0));
            } else if (event.key === "Enter") {
              event.preventDefault();
              invoke(matches[cursor]);
            } else if (event.key === "Escape") {
              event.preventDefault();
              close();
            }
          }}
        />
        <div className="pal-list" ref={list} role="listbox">
          {matches.map((command, index) => (
            <button
              key={command.id}
              type="button"
              role="option"
              aria-selected={index === cursor}
              data-on={index === cursor}
              className={index === cursor ? "it on" : "it"}
              onMouseMove={() => setCursor(index)}
              onClick={() => invoke(command)}
            >
              <span className="t">{command.title}</span>
              <span className="s">{command.surfaceLabel}</span>
              {command.hint && <span className="d">{command.hint}</span>}
            </button>
          ))}
          {matches.length === 0 && <div className="pal-none">没有匹配的命令</div>}
        </div>
      </div>
    </div>
  );
}
