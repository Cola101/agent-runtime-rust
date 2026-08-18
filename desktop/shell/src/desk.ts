/// The shared handle every surface reads from.
///
/// One store for the whole window: the run list, the transcript and the
/// approval queue are three views of the same event log, and giving each its
/// own poller would let them disagree about the state of the same run.
import { createContext, useContext } from "react";
import type { Store } from "./store";

export type Desk = Store & {
  /// Which run the transcript is showing. Null means "the newest one", which
  /// is what a person means when they have not chosen.
  selected: string | null;
  select(runId: string | null): void;
  go(surfaceId: string): void;
};

export const DeskContext = createContext<Desk | null>(null);

export function useDesk(): Desk {
  const desk = useContext(DeskContext);
  if (!desk) throw new Error("a surface was rendered outside the desk");
  return desk;
}

/// The run the transcript should show: the chosen one, or the most recently
/// touched. Never silently a different run than the one the list highlights.
export function currentRun(desk: Desk) {
  if (desk.selected) return desk.runs.find((run) => run.id === desk.selected) ?? null;
  const sorted = [...desk.runs].sort((a, b) =>
    (b.updatedAt ?? "").localeCompare(a.updatedAt ?? ""));
  return sorted[0] ?? null;
}
