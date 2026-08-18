// Which of the Runs waiting on a person this process has already said out loud.
//
// Split out of main.cjs because it is the part that can be wrong in a way
// nobody notices for a while: a banner that repeats every poll, or one that
// never comes. Nothing Electron-shaped lives here — main.cjs owns the window
// and the Notification, this owns the rule.

/// The most banners one report may produce.
///
/// `known` starts empty in a fresh process, so the first report after launch
/// is entirely new. Someone who left eight Runs parked overnight and opens
/// this app with the window behind something else would be handed eight
/// banners at once — not eight times as useful as one, just a wall to dismiss,
/// and on macOS the later ones push the earlier ones out of the corner before
/// they have been read. Three is the most that still reads as a list.
const BURST = 3;

/// A record of what the person has been told, and what counts as new.
///
/// The window polls the runtime about once a second, so the same approval is
/// reported over and over. What makes two reports the same thing is the
/// approval's own id — the runtime's, durable in its log — and never when it
/// was seen. A Run parked for an hour is one question, not three thousand.
function createAttention() {
  /// Keys this process has been told about. Deliberately never pruned: an
  /// approval can drop out of a report without having been answered — one
  /// failed read of that Run's log is enough — and forgetting it would
  /// announce the same question again as soon as the next read succeeds.
  const known = new Set();

  return {
    /// `items` is everything the window says is waiting right now; `focused`
    /// is whether the person was at that window when it said so. Returns the
    /// banners worth interrupting someone for — at most `BURST` of them.
    arrived(items, focused) {
      const fresh = [];
      for (const item of Array.isArray(items) ? items : []) {
        const key = typeof item?.key === "string" ? item.key : "";
        // An item with no durable name cannot be deduplicated, and an
        // announcement that cannot be deduplicated is one per poll. Saying
        // nothing is the better of the two failures.
        if (!key || known.has(key)) continue;
        known.add(key);
        // Recorded as known even when nothing is raised. The window has
        // already shown this to someone sitting in front of it; telling them
        // again later, when they switch to another app, would be an
        // interruption about something they have seen.
        if (focused) continue;
        fresh.push(item);
      }
      if (fresh.length <= BURST) return fresh;
      // Over the bound the last banner is a count instead of a name. Nothing
      // is dropped — what will not fit is still said, as a number — and
      // clicking it lands on the first of the ones it stands for, which is
      // the queue they are all in.
      const named = fresh.slice(0, BURST - 1);
      const rest = fresh.slice(BURST - 1);
      return [...named, { kind: "several", count: rest.length, runId: rest[0].runId }];
    },
  };
}

module.exports = { createAttention };
