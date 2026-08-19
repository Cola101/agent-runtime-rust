/// Whether the transcript keeps following the tail.
///
/// One predicate rather than a number buried in a scroll handler, because the
/// answer depends on two things and the second one used to be missing.
///
/// The slack was 40px. Three reference clients disagree with that by a wide
/// margin -- opencode allows 10 (`create-auto-scroll.tsx:19`), openhands 20
/// (`use-scroll-to-bottom.ts:17-21`), assistant-ui 1
/// (`useThreadViewportAutoScroll.ts:118`) -- and 40 is wrong for a reason that
/// has nothing to do with taste: it is wider than a line of this transcript,
/// so scrolling up one line to re-read a sentence left the column still
/// "pinned" and the next delta yanked it back down.
///
/// It is not 0 because `scrollTop` and `scrollHeight` are fractional under
/// display scaling, and because scroll events are delivered asynchronously --
/// content can arrive between `scrollTop = scrollHeight` and the event it
/// causes, so the handler reads a distance that is briefly non-zero through
/// nobody's fault. opencode writes this down at `create-auto-scroll.tsx:37-40`.
export const FOLLOW_SLACK_PX = 10;

export function staysWithTheTail(
  { distanceFromBottom, selecting }: { distanceFromBottom: number; selecting: boolean },
): boolean {
  // Selecting text is reading, and following while someone reads destroys the
  // selection on the next delta -- which means text inside a live reply cannot
  // be copied at all. opencode gives selection the same veto
  // (`create-auto-scroll.tsx:148-154`).
  if (selecting) return false;
  return distanceFromBottom <= FOLLOW_SLACK_PX;
}
