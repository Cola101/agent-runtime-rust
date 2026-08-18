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

/// A Run as the owner surface reports it: what it was asked to do, and where it
/// got to. A list of ids cannot answer either, which is why this replaced one.
export type RunSummary = {
  run_id: string;
  input: string;
  state: Record<string, unknown>;
};

/// A branch head, exactly as the runtime reports it.
///
/// `active_run_id` is the only thing that says a Turn is still in flight, and
/// the branch refuses a second Turn while it is set. A client that continued on
/// hope rather than on this field gets a refusal it could have avoided asking
/// for.
export type SessionHead = {
  session_id: string;
  branch_id: string;
  generation: number;
  turn_count: number;
  history_digest: string;
  active_run_id: string | null;
};

export type SessionTurnReceipt = {
  head: SessionHead;
  run_id: string;
  owner_epoch: number | null;
  state: Record<string, unknown>;
};

export type ContentPart = { type: string; text?: string; [key: string]: unknown };

/// One committed Turn's frozen transcript. Not a rendering of the event log:
/// the events can be retired while this survives, and only one of the two is
/// what the next Turn is actually given as history.
export type SessionTurn = {
  turn_ordinal: number;
  run_id: string;
  transcript: { role: string; content: ContentPart[] }[];
  digest: string;
};

/// A provider as the host is willing to describe it.
///
/// There is no secret on this type, and no bridge call returns one. `hasSecret`
/// is read back from the Keychain rather than from a flag, so it cannot claim a
/// key that is not there.
export type ProviderView = {
  id: string;
  protocol: string;
  endpoint: string;
  model: string;
  hasSecret: boolean;
  secretSetAt: string | null;
};

export type WorkspaceStatus = { root: string | null; configured: boolean };

export type WorkspaceEntry = {
  name: string;
  kind: "folder" | "file" | "link";
  size: number | null;
  modified: string | null;
};

export type WorkspaceListing = { path: string; entries: WorkspaceEntry[]; truncated: boolean };

export type WorkspaceFile = {
  path: string;
  binary: boolean;
  size: number;
  truncated?: boolean;
  text?: string;
};

export type RuntimeLifecycle = {
  lifecycle: string;
  recovery: { completed_profiles: number; total_profiles: number };
  active_runs: number;
  queued_runs: number;
  recovery_failures: number;
  previous_shutdown: Record<string, unknown> | null;
};

type Bridge = {
  status(): Promise<Reply<RuntimeStatus>>;
  probe(): Promise<Reply<RuntimeStatus>>;
  list(): Promise<Reply<{ runs: RunSummary[]; nextAfterRunId: string | null }>>;
  lifecycle(): Promise<Reply<RuntimeLifecycle>>;
  startRuntime(): Promise<Reply<boolean>>;
  shutdown(): Promise<Reply<Record<string, unknown>>>;
  events(request: { runId: string; afterSequence?: number; limit?: number }): Promise<
    Reply<{ ok: true; page: CursorPage } | { ok: false; error: CursorError }>
  >;
  submit(input: string): Promise<Reply<string>>;
  control(request: { action: string; runId: string }): Promise<Reply<unknown>>;
  sessionStart(request: {
    sessionId: string; branchId: string; runId: string; input: string;
  }): Promise<Reply<SessionTurnReceipt>>;
  sessionContinue(request: {
    sessionId: string; branchId: string; generation: number; runId: string; input: string;
  }): Promise<Reply<SessionTurnReceipt>>;
  sessionRead(request: { sessionId: string; branchId: string }): Promise<Reply<SessionHead>>;
  sessionList(request?: {
    afterSessionId?: string | null; afterBranchId?: string | null; limit?: number;
  }): Promise<Reply<{ heads: SessionHead[]; nextAfter: [string, string] | null }>>;
  sessionHistory(request: {
    sessionId: string; branchId: string; generation: number;
    afterTurnOrdinal?: number; limit?: number | null;
  }): Promise<Reply<{ turns: SessionTurn[]; nextAfterTurnOrdinal: number | null }>>;
  /// Follows one run's log as it is written. The events arrive through
  /// `onEvent`; the lifecycle does not, and must keep coming from the cursor.
  watch(request: { runId: string; afterSequence?: number }): Promise<Reply<{ watching: boolean }>>;
  unwatch(runId: string): Promise<Reply<unknown>>;
  onEvent(handler: (payload: { runId: string; event: RunEvent }) => void): () => void;
  onWatchEnded(handler: (payload: { runId: string; reason: Record<string, unknown> }) => void): () => void;
  workspace(): Promise<Reply<WorkspaceStatus>>;
  listFiles(relative: string): Promise<Reply<WorkspaceListing>>;
  readFile(relative: string): Promise<Reply<WorkspaceFile>>;
  providers(): Promise<Reply<ProviderView[]>>;
  saveProvider(request: {
    id: string; protocol: string; endpoint: string; model: string; secret?: string | null;
  }): Promise<Reply<{ id: string }>>;
  forgetProvider(id: string): Promise<Reply<{ id: string }>>;
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
