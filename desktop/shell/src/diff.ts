/// What one write would change, line by line.
///
/// Its own module because it is the only piece of this client that computes
/// something rather than reading it, and computing is where a client starts
/// telling people things the runtime never said. So it computes exactly one
/// thing -- which lines differ -- and says nothing about what that means.
///
/// A longest-common-subsequence walk, which is the ordinary way and is right
/// here for a reason beyond convention: it never claims a line moved. A
/// smarter diff that matched a line to one far away would be guessing at
/// intent, and intent is what the person reading this is deciding about.
export type Change = { kind: "same" | "add" | "drop"; text: string };

/// Bounded, because a write can be a megabyte and this runs while someone is
/// waiting to decide. Beyond the bound the comparison is refused rather than
/// truncated: half a diff is a diff you cannot trust, and the caller says so.
export const MAX_LINES = 4_000;

export function changes(before: string, after: string): Change[] | null {
  const old = before.split("\n");
  const now = after.split("\n");
  if (old.length > MAX_LINES || now.length > MAX_LINES) return null;

  // The classic table. Rows are `old`, columns are `now`, and each cell is the
  // length of the longest common subsequence of the suffixes.
  const common: number[][] = Array.from(
    { length: old.length + 1 },
    () => new Array<number>(now.length + 1).fill(0),
  );
  for (let left = old.length - 1; left >= 0; left -= 1) {
    for (let right = now.length - 1; right >= 0; right -= 1) {
      common[left]![right] = old[left] === now[right]
        ? common[left + 1]![right + 1]! + 1
        : Math.max(common[left + 1]![right]!, common[left]![right + 1]!);
    }
  }

  const walked: Change[] = [];
  let left = 0;
  let right = 0;
  while (left < old.length && right < now.length) {
    if (old[left] === now[right]) {
      walked.push({ kind: "same", text: old[left]! });
      left += 1;
      right += 1;
    } else if (common[left + 1]![right]! >= common[left]![right + 1]!) {
      walked.push({ kind: "drop", text: old[left]! });
      left += 1;
    } else {
      walked.push({ kind: "add", text: now[right]! });
      right += 1;
    }
  }
  while (left < old.length) {
    walked.push({ kind: "drop", text: old[left]! });
    left += 1;
  }
  while (right < now.length) {
    walked.push({ kind: "add", text: now[right]! });
    right += 1;
  }
  return walked;
}

/// The changed lines with a little of what surrounds them.
///
/// A write that changes one line in a thousand should show that line, not the
/// thousand. Unchanged runs longer than `context * 2 + 1` are replaced by a
/// gap, which the caller draws as a gap rather than silently closing up -- a
/// diff that looks continuous when it is not is a diff about a file that does
/// not exist.
export type Hunk = { changes: Change[]; skipped: number };

export function hunks(walked: Change[], context = 3): Hunk[] {
  const keep = new Set<number>();
  walked.forEach((change, index) => {
    if (change.kind === "same") return;
    for (let near = index - context; near <= index + context; near += 1) {
      if (near >= 0 && near < walked.length) keep.add(near);
    }
  });
  const out: Hunk[] = [];
  let run: Change[] = [];
  let skipped = 0;
  const flush = () => {
    if (run.length > 0) out.push({ changes: run, skipped: 0 });
    run = [];
  };
  walked.forEach((change, index) => {
    if (!keep.has(index)) {
      skipped += 1;
      return;
    }
    // A gap closes the run before it, so the gap sits between the two pieces
    // it separates rather than inside either of them.
    if (skipped > 0) {
      flush();
      out.push({ changes: [], skipped });
      skipped = 0;
    }
    run.push(change);
  });
  flush();
  // A run of unchanged lines at the very end is dropped too, and saying so is
  // the same obligation as saying it in the middle: a diff that stops without
  // a gap reads as a file that stops there.
  if (skipped > 0) out.push({ changes: [], skipped });
  return out;
}
