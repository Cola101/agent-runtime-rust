/// Finding text in the conversation column.
///
/// Two things live here, and they are here together on purpose.
///
/// What counts as a match, which the transcript uses to draw its highlights and
/// the tool-call fold uses to decide it has to open. One rule for both, so a
/// fold cannot stay shut over a hit the finder counted.
///
/// And whether the finder is open, which belongs to the surface but cannot live
/// inside it: `⌘F` is declared as data and dispatched by the shell, so the
/// binding runs outside React and needs something it can reach.

export type Part = { text: string; hit: boolean };

/// One run of text, cut at the matches.
///
/// Case-insensitive substring, deliberately not the palette's subsequence
/// match. A palette is spelling a command name out of its initials; a finder is
/// looking for a string that is on the screen, and `ls` matching half the
/// transcript would leave the count meaning nothing.
export function split(text: string, query: string): Part[] {
  if (!query) return [{ text, hit: false }];
  const lowered = text.toLowerCase();
  const wanted = query.toLowerCase();
  // Every offset below indexes the original string, and lowercasing is not
  // always length-preserving -- `İ` lowercases to two code units. When the
  // lengths disagree those offsets cannot be trusted, so the search falls back
  // to the literal text rather than slicing in the wrong place.
  const shifted = lowered.length !== text.length || wanted.length !== query.length;
  const haystack = shifted ? text : lowered;
  const needle = shifted ? query : wanted;

  const parts: Part[] = [];
  let at = 0;
  for (;;) {
    const found = haystack.indexOf(needle, at);
    if (found === -1) break;
    if (found > at) parts.push({ text: text.slice(at, found), hit: false });
    parts.push({ text: text.slice(found, found + needle.length), hit: true });
    at = found + needle.length;
  }
  if (at < text.length) parts.push({ text: text.slice(at), hit: false });
  return parts;
}

export function has(text: string, query: string): boolean {
  return query !== "" && split(text, query).some((part) => part.hit);
}

let showing = false;
let opened = 0;
const watchers = new Set<() => void>();

function announce(): void {
  for (const watcher of watchers) watcher();
}

/// Opening an already-open finder is not nothing: it puts the focus back in the
/// box and selects what is in it, which is what a second ⌘F means everywhere
/// else. The count is what the view watches to know one happened.
export function openFind(): void {
  showing = true;
  opened += 1;
  announce();
}

export function closeFind(): void {
  if (!showing) return;
  showing = false;
  announce();
}

/// How many times the finder has been asked to open, or -1 while it is closed.
export function findOpened(): number {
  return showing ? opened : -1;
}

export function watchFind(watcher: () => void): () => void {
  watchers.add(watcher);
  return () => {
    watchers.delete(watcher);
  };
}
