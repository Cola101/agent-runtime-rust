/// The renderer's view of the host bridge.
///
/// Everything here goes through `window.desk`, which the preload defines and
/// nothing else can widen. The renderer has no socket, no stream and no token
/// — only these calls.
import type { Lifecycle } from "./surfaces/model";

/// Every bridge call answers with one of these. Errors are values rather than
/// thrown promises: "the runtime is not running" and "that run has a corrupt
/// log" are things a person needs to read on the screen, and an exception that
/// unmounts a surface is how they become a blank panel instead.
export type Reply<T> = { ok: true; value: T } | { ok: false; error: string };

export type Link =
  /// Not running inside the desktop host at all — a browser tab, or a preload
  /// that failed to load. No runtime is reachable and none will become so.
  | { state: "no-bridge" }
  /// The host was not told where a runtime is. Not an error: the client
  /// deliberately has no default path to go looking in.
  | { state: "unconfigured" }
  | { state: "unreachable"; socketPath: string; reason: string }
  | { state: "live"; socketPath: string };

export type RuntimeStatus = {
  transport: string;
  stateRoot: string | null;
  socketPath: string | null;
  connected: boolean;
  error: string | null;
};

export type RunEvent = {
  event_id: string;
  sequence: number;
  run_id: string;
  timestamp: string;
  type: string;
  payload: Record<string, unknown>;
  digest: string;
};

/// Mirrors `RuntimeEventCursorPage`. Note what is carried separately from the
/// events: the lifecycle boundary, whether history was lost, and how far the
/// log actually goes. None of those can be inferred from a page of events.
export type CursorPage = {
  run_id: string;
  requested_after_sequence: number;
  next_after_sequence: number;
  earliest_available_sequence: number | null;
  highest_committed_sequence: number;
  history_gap: boolean;
  has_more: boolean;
  state: Record<string, unknown>;
  events: RunEvent[];
};

export type CursorError = {
  code: string;
  message?: string;
};

type Bridge = {
  status(): Promise<Reply<RuntimeStatus>>;
  probe(): Promise<Reply<RuntimeStatus>>;
  list(): Promise<Reply<string[]>>;
  events(request: { runId: string; afterSequence?: number; limit?: number }): Promise<
    Reply<{ ok: true; page: CursorPage } | { ok: false; error: CursorError }>
  >;
  submit(input: string): Promise<Reply<string>>;
  control(request: { action: string; runId: string }): Promise<Reply<unknown>>;
};

declare global {
  interface Window {
    desk?: {
      mounted(surfaces: number): void;
      drew?(summary: Record<string, unknown>): void;
      runtime?: Bridge;
    };
  }
}

export function bridge(): Bridge | null {
  return window.desk?.runtime ?? null;
}

function linkFrom(status: RuntimeStatus): Link {
  if (!status.socketPath) return { state: "unconfigured" };
  if (status.connected) return { state: "live", socketPath: status.socketPath };
  return {
    state: "unreachable",
    socketPath: status.socketPath,
    reason: status.error ?? "the socket did not answer",
  };
}

export async function probe(): Promise<Link> {
  const api = bridge();
  if (!api) return { state: "no-bridge" };
  const reply = await api.probe();
  if (!reply.ok) return { state: "unreachable", socketPath: "?", reason: reply.error };
  return linkFrom(reply.value);
}

/// Translates the typed boundary the runtime reports. Never derived from the
/// event list.
///
/// A client that concludes a run is over by reading the last event it happened
/// to receive is wrong exactly when it matters — a retired log, a replaced
/// host, a run parked on a person. An unfamiliar boundary stays unfamiliar:
/// calling it running would follow a dead run forever, calling it terminal
/// would drop a live one, and neither mistake is visible to the person using it.
export function lifecycleFromCursor(state: Record<string, unknown> | undefined): Lifecycle {
  const kind = state?.state;
  switch (kind) {
    case "running": return { kind: "running" };
    case "cancelling": return { kind: "cancelling" };
    case "waiting_approval": return { kind: "waiting_approval" };
    case "suspended": return { kind: "suspended" };
    case "interrupted": return { kind: "interrupted" };
    case "terminal": return { kind: "terminal", status: String(state?.status ?? "") };
    case "retired": return { kind: "retired", status: String(state?.status ?? "") };
    default: return { kind: "unrecognised" };
  }
}
