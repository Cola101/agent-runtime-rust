/// Shapes the surfaces render.
///
/// These mirror the wire contract in `contracts/proto/runtime.proto` rather
/// than inventing a client-side model. When the gRPC client lands it fills
/// these in; nothing in a view has to change.
///
/// `Lifecycle` in particular is a translation of the runtime's typed boundary,
/// never something derived from the event list. A client that concludes a run
/// is over by looking at the last event it happened to receive will be wrong
/// exactly when it matters: a retired log, a replaced host, a run parked on a
/// person.
export type Lifecycle =
  | { kind: "running" }
  | { kind: "cancelling" }
  | { kind: "waiting_approval" }
  | { kind: "suspended" }
  | { kind: "interrupted" }
  | { kind: "terminal"; status: string }
  | { kind: "retired"; status: string }
  /// The wire carried a boundary this build does not understand. Not an error
  /// and not a guess — mapping it onto running or terminal would be a lie in
  /// whichever direction is wrong.
  | { kind: "unrecognised" };

export function isOver(lifecycle: Lifecycle): boolean {
  return lifecycle.kind === "terminal" || lifecycle.kind === "retired";
}

export function lifecycleLabel(lifecycle: Lifecycle): string {
  switch (lifecycle.kind) {
    case "waiting_approval": return "waiting on you";
    case "terminal": return lifecycle.status;
    case "retired": return `${lifecycle.status} · retired`;
    case "unrecognised": return "unrecognised state";
    default: return lifecycle.kind;
  }
}

/// The colour a state is allowed to use. Only three states earn one; the rest
/// are carried by the word alone.
export function lifecycleTone(lifecycle: Lifecycle): "attention" | "unknown" | "plain" {
  if (lifecycle.kind === "waiting_approval") return "attention";
  if (lifecycle.kind === "interrupted" || lifecycle.kind === "unrecognised") return "unknown";
  if ((lifecycle.kind === "terminal" || lifecycle.kind === "retired") &&
      lifecycle.status === "indeterminate") return "unknown";
  return "plain";
}

export type Run = {
  id: string;
  title: string;
  lifecycle: Lifecycle;
  tokens: number;
  costCents: number;
  when: string;
};

export type Blocked =
  | { kind: "approval"; runId: string; runTitle: string; command: string; digest: string }
  /// An effect that may or may not have landed. Structurally the same moment
  /// as an approval — only a person can settle it — so it is answered the
  /// same way rather than shown as a badge somewhere.
  | { kind: "indeterminate"; runId: string; runTitle: string; question: string };
