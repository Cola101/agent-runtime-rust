/// The one place that turns a runtime's durable event log into what surfaces
/// render. There is no sample data behind it.
///
/// Two facts about the local adapter shape everything here:
///
/// It reads the runtime's owner surface, which answers both questions a run
/// list has to answer: what a Run was asked to do, and where it got to. An
/// earlier version of this file read a list of bare ids and kept its own note
/// of what it had submitted, because the durable *event log* does not carry the
/// input -- but the durable *record* always did, and the owner surface hands it
/// back. The client no longer keeps a note, and no longer has to say "not this
/// client's Run" about work it can read perfectly well.
import { useCallback, useEffect, useRef, useState } from "react";
import {
  bridge, lifecycleFromCursor, probe,
  type CursorError, type CursorPage, type Link, type RunEvent,
} from "./runtime";
import type { Lifecycle } from "./surfaces/model";

/// `RUNTIME_EVENT_CURSOR_MAX_EVENTS`. Larger is not a bigger page — the daemon
/// rejects it as an invalid request, which is how a transcript becomes empty
/// rather than truncated.
const PAGE_LIMIT = 256;
/// How far this client will page before it stops and says so. A run with
/// twenty thousand events should not silently become a spinner.
const MAX_PAGES = 12;
const POLL_MS = 1_200;

export type Approval = {
  approvalId: string;
  toolName: string;
  arguments: Record<string, unknown>;
  effect: string;
  sandbox: string;
  bindingDigest: string;
  requiredScopes: string[];
  policyDigest: string;
};

/// A tool's policy exactly as the runtime froze it into one execution.
///
/// Observed, not configured: the local adapter has no call for reading the
/// policy table, so this is what actually governed a call that actually
/// happened. Settings says so, because "这是你的设置" and "这是上次真的发生的事"
/// are different claims and only the second one is true here.
export type ObservedPolicy = {
  toolName: string;
  effect: string;
  sandbox: string;
  approval: string;
  autoApproval: string;
  requiredScopes: string[];
  policyDigest: string;
  seenAt: string;
};

export type RunView = {
  id: string;
  /// What this Run was asked to do, from its durable record. Filled for
  /// every Run the state root holds, not only the ones this client started.
  asked: string;
  lifecycle: Lifecycle;
  events: RunEvent[];
  text: string;
  toolCalls: { name: string; arguments: unknown }[];
  approval: Approval | null;
  tokens: number;
  costMicros: number;
  startedAt: string | null;
  updatedAt: string | null;
  historyGap: boolean;
  /// True when paging stopped before the log ran out. The transcript then
  /// shows a prefix, and says it is a prefix.
  truncated: boolean;
  earliestSequence: number | null;
  highestSequence: number;
  /// A run whose log could not be read. Kept in the list rather than dropped:
  /// a run that vanishes from the screen because reading it failed is the
  /// worst of the available behaviours.
  error: CursorError | null;
};

function payloadString(payload: Record<string, unknown>, key: string): string {
  const value = payload[key];
  return typeof value === "string" ? value : "";
}

function readApproval(payload: Record<string, unknown>): Approval | null {
  const approval = payload.approval as Record<string, unknown> | undefined;
  if (!approval) return null;
  const execution = approval.execution as Record<string, unknown> | undefined;
  const call = execution?.call as Record<string, unknown> | undefined;
  const policy = approval.policy_snapshot as Record<string, unknown> | undefined;
  return {
    approvalId: String(approval.approval_id ?? ""),
    toolName: String(call?.name ?? ""),
    arguments: (call?.arguments as Record<string, unknown>) ?? {},
    effect: String(execution?.effect ?? ""),
    sandbox: String(execution?.sandbox ?? ""),
    bindingDigest: String(execution?.binding_digest ?? ""),
    requiredScopes: (policy?.required_scopes as string[]) ?? [],
    policyDigest: String(approval.policy_digest ?? ""),
  };
}

/// Reads a run's whole log, one bounded page at a time.
///
/// A single page is not the transcript: the daemon caps a page at 256 events
/// and a run that streamed a paragraph produces more than that on its own. The
/// cursor is exclusive, so each page resumes at `next_after_sequence` with no
/// overlap and no gap.
async function readWholeLog(
  api: NonNullable<ReturnType<typeof bridge>>,
  runId: string,
): Promise<{ page: CursorPage; events: RunEvent[]; truncated: boolean } | CursorError> {
  const events: RunEvent[] = [];
  let after = 0;
  let last: CursorPage | null = null;
  for (let pages = 0; pages < MAX_PAGES; pages += 1) {
    const reply = await api.events({ runId, afterSequence: after, limit: PAGE_LIMIT });
    if (!reply.ok) return { code: "bridge", message: reply.error };
    if (!reply.value.ok) return reply.value.error;
    const page = reply.value.page;
    events.push(...page.events);
    last = page;
    if (!page.has_more) return { page, events, truncated: false };
    // Guards against a page that reports more but returns nothing, which would
    // otherwise spin here forever.
    if (page.next_after_sequence <= after) break;
    after = page.next_after_sequence;
  }
  if (!last) return { code: "empty", message: "the cursor returned no pages" };
  return { page: last, events, truncated: true };
}

function project(
  id: string, asked: string, page: CursorPage, events: RunEvent[], truncated: boolean,
): RunView {
  let tokens = 0;
  let costMicros = 0;
  let text = "";
  const toolCalls: { name: string; arguments: unknown }[] = [];
  let approval: Approval | null = null;

  for (const event of events) {
    switch (event.type) {
      case "model.output.delta":
        text += payloadString(event.payload, "text");
        break;
      case "model.usage":
        tokens += Number(event.payload.input_tokens ?? 0) + Number(event.payload.output_tokens ?? 0);
        costMicros += Number(event.payload.cost_micros ?? 0);
        break;
      case "model.tool_call": {
        const call = event.payload.call as Record<string, unknown> | undefined;
        toolCalls.push({
          name: String(call?.name ?? event.payload.name ?? ""),
          arguments: call?.arguments ?? event.payload.arguments,
        });
        break;
      }
      case "approval.required":
        approval = readApproval(event.payload);
        break;
      // An answered approval clears the one on screen. Without this a decided
      // run keeps showing the question it already answered.
      case "run.resumed":
      case "tool.denied":
        approval = null;
        break;
      default:
        break;
    }
  }

  const lifecycle = lifecycleFromCursor(page.state);
  return {
    id,
    asked,
    lifecycle,
    events,
    text,
    toolCalls,
    approval: lifecycle.kind === "waiting_approval" ? approval : null,
    tokens,
    costMicros,
    startedAt: events[0]?.timestamp ?? null,
    updatedAt: events[events.length - 1]?.timestamp ?? null,
    historyGap: page.history_gap,
    truncated,
    earliestSequence: page.earliest_available_sequence,
    highestSequence: page.highest_committed_sequence,
    error: null,
  };
}

function failed(id: string, asked: string, error: CursorError): RunView {
  return {
    id, asked, lifecycle: { kind: "unrecognised" }, events: [], text: "",
    toolCalls: [], approval: null, tokens: 0, costMicros: 0,
    startedAt: null, updatedAt: null, historyGap: false, truncated: false,
    earliestSequence: null, highestSequence: 0, error,
  };
}

export type Store = {
  link: Link;
  runs: RunView[];
  /// Every tool policy this client has actually seen govern a call.
  policies: ObservedPolicy[];
  loading: boolean;
  /// Null until the first list has come back, so a surface can tell "no runs"
  /// apart from "have not looked yet".
  listedAt: number | null;
  submit(input: string): Promise<string | null>;
  decide(runId: string, action: "approve" | "deny" | "cancel" | "resume"): Promise<string | null>;
  refresh(): void;
};

/// Pulls policy snapshots out of the events that carry them. Both event types
/// embed the same frozen snapshot — one because the runtime stopped to ask,
/// one because the policy said it did not have to.
function readPolicies(runs: RunView[]): ObservedPolicy[] {
  const seen = new Map<string, ObservedPolicy>();
  for (const run of runs) {
    for (const event of run.events) {
      if (event.type !== "approval.required" && event.type !== "tool.execution.auto_approved") {
        continue;
      }
      const holder = (event.payload.approval ?? event.payload) as Record<string, unknown>;
      const snapshot = holder.policy_snapshot as Record<string, unknown> | undefined;
      if (!snapshot) continue;
      const toolName = String(snapshot.tool_name ?? "");
      if (!toolName) continue;
      seen.set(toolName, {
        toolName,
        effect: String(snapshot.effect ?? ""),
        sandbox: String(snapshot.sandbox ?? ""),
        approval: String(snapshot.approval ?? ""),
        autoApproval: String(snapshot.auto_approval ?? ""),
        requiredScopes: (snapshot.required_scopes as string[]) ?? [],
        policyDigest: String(holder.policy_digest ?? event.payload.policy_digest ?? ""),
        seenAt: event.timestamp,
      });
    }
  }
  return [...seen.values()].sort((a, b) => a.toolName.localeCompare(b.toolName));
}

export function useRuntime(): Store {
  const [link, setLink] = useState<Link>({ state: "no-bridge" });
  const [runs, setRuns] = useState<RunView[]>([]);
  const [loading, setLoading] = useState(true);
  const [listedAt, setListedAt] = useState<number | null>(null);
  const busy = useRef(false);

  const load = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    try {
      const next = await probe();
      setLink(next);
      if (next.state !== "live") {
        setRuns([]);
        setListedAt(Date.now());
        return;
      }
      const api = bridge();
      if (!api) return;
      const listed = await api.list();
      if (!listed.ok) return;
      const views = await Promise.all(
        listed.value.runs.map(async (summary) => {
          const read = await readWholeLog(api, summary.run_id);
          return "code" in read
            ? failed(summary.run_id, summary.input, read)
            : project(summary.run_id, summary.input, read.page, read.events, read.truncated);
        }),
      );
      setRuns(views);
      setListedAt(Date.now());
    } finally {
      busy.current = false;
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    // Polling, not streaming. The local adapter can stream (`Attach`), but a
    // stream has to be held open in the host and pushed across the bridge,
    // and this client would rather be obviously correct first.
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const submit = useCallback(async (input: string) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const reply = await api.submit(input);
    if (!reply.ok) return reply.error;
    void load();
    return null;
  }, [load]);

  const decide = useCallback(
    async (runId: string, action: "approve" | "deny" | "cancel" | "resume") => {
      const api = bridge();
      if (!api) return "not running in the desktop host";
      const reply = await api.control({ action, runId });
      void load();
      return reply.ok ? null : reply.error;
    },
    [load],
  );

  return {
    link, runs, policies: readPolicies(runs), loading, listedAt,
    submit, decide, refresh: () => void load(),
  };
}
