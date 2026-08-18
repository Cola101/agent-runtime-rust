/// The shared handle every surface reads from.
///
/// One store for the whole window: the run list, the transcript and the
/// approval queue are three views of the same event log, and giving each its
/// own poller would let them disagree about the state of the same run.
import { createContext, useContext } from "react";
import type { Waiting } from "./runtime";
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

/// Moves the run cursor through a list the surface is showing.
///
/// The cursor is `desk.selected` on every surface rather than one per view, so
/// moving to a run in the list and pressing a key on the transcript are about
/// the same run. Two cursors is how a person approves something they were not
/// looking at.
export function moveCursor(desk: Desk, ids: string[], delta: number): void {
  if (ids.length === 0) return;
  const at = desk.selected ? ids.indexOf(desk.selected) : -1;
  const next = at === -1
    ? (delta > 0 ? 0 : ids.length - 1)
    : Math.min(Math.max(at + delta, 0), ids.length - 1);
  desk.select(ids[next]);
}

/// Runs newest-touched first, which is the order every list here shows.
export function byRecency(desk: Desk) {
  return [...desk.runs].sort((a, b) => (b.updatedAt ?? "").localeCompare(a.updatedAt ?? ""));
}

/// The runs that are blocked on a person: parked on an approval, or finished
/// in a state only a person can settle.
export function blocked(desk: Desk) {
  return byRecency(desk).filter(
    (run) =>
      run.approval !== null ||
      ((run.lifecycle.kind === "terminal" || run.lifecycle.kind === "retired") &&
        run.lifecycle.status === "indeterminate"),
  );
}

/// The same queue, named for the host.
///
/// Two kinds of waiting, and each is named by the thing the runtime already
/// gives a durable identity to. An approval carries its own id, so two
/// questions on one Run stay two. A Run that ended `indeterminate` has no
/// approval to name it by and needs none: a Run reaches a terminal boundary
/// once and never leaves it, so the Run's own id names that question exactly.
///
/// An approval whose payload carried no id falls back to its Run rather than
/// to an empty string — otherwise every such approval would share one name and
/// only the first would ever be heard.
export function waiting(desk: Desk): Waiting[] {
  return blocked(desk).map((run) =>
    run.approval
      ? {
        kind: "approval",
        key: `approval:${run.approval.approvalId || run.id}`,
        runId: run.id,
        toolName: run.approval.toolName,
      }
      : { kind: "indeterminate", key: `run:${run.id}`, runId: run.id },
  );
}

/// The run the transcript should show: the chosen one, or the most recently
/// touched. Never silently a different run than the one the list highlights.
export function currentRun(desk: Desk) {
  if (desk.selected) return desk.runs.find((run) => run.id === desk.selected) ?? null;
  const sorted = [...desk.runs].sort((a, b) =>
    (b.updatedAt ?? "").localeCompare(a.updatedAt ?? ""));
  return sorted[0] ?? null;
}
