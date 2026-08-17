/// The renderer's view of the host bridge.
///
/// Everything here goes through `window.desk.runtime`, which the preload
/// defines and nothing else can widen. The renderer has no channel, no stream
/// and no token — only these calls.
import type { Lifecycle } from "./surfaces/model";

export type Connection =
  | { state: "absent" }
  | { state: "connected"; endpoint: string };

type Bridge = {
  status(): Promise<{ connected: boolean; endpoint: string | null }>;
  connect(options: { endpoint: string }): Promise<{ endpoint: string; secure: boolean }>;
  readEvents(request: unknown): Promise<unknown>;
  submit(request: unknown): Promise<unknown>;
  control(request: unknown): Promise<unknown>;
};

declare global {
  interface Window {
    desk?: {
      mounted(surfaces: number): void;
      endpoint(): Promise<string | null>;
      runtime?: Bridge;
    };
  }
}

export async function connection(): Promise<Connection> {
  const bridge = window.desk?.runtime;
  if (!bridge) return { state: "absent" };
  const status = await bridge.status();
  return status.connected && status.endpoint
    ? { state: "connected", endpoint: status.endpoint }
    : { state: "absent" };
}

/// Translates the wire boundary. Never derived from the event list.
///
/// A client that concludes a run is over by reading the last event it happened
/// to receive is wrong exactly when it matters — a retired log, a replaced
/// host, a run parked on a person. An unfamiliar boundary stays unfamiliar:
/// calling it running would follow a dead run forever, calling it terminal
/// would drop a live one, and neither mistake is visible to the person using it.
export function lifecycleFromWire(boundary: Record<string, unknown> | undefined): Lifecycle {
  if (!boundary) return { kind: "unrecognised" };
  if (boundary.running) return { kind: "running" };
  if (boundary.cancelling) return { kind: "cancelling" };
  if (boundary.waiting_approval) return { kind: "waiting_approval" };
  if (boundary.suspended) return { kind: "suspended" };
  if (boundary.interrupted) return { kind: "interrupted" };
  const terminal = boundary.terminal as { status?: string } | undefined;
  if (terminal) return { kind: "terminal", status: terminal.status ?? "" };
  const retired = boundary.retired as { status?: string } | undefined;
  if (retired) return { kind: "retired", status: retired.status ?? "" };
  return { kind: "unrecognised" };
}
