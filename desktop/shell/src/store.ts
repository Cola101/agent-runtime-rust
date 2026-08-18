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
  type SessionHead, type SessionTurn,
} from "./runtime";
import { newestFirst, viewOf, type SessionView } from "./session";
import { uuidv7 } from "./ids";
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

/// How many history pages this client will walk before it stops and says so.
/// Eight pages of the daemon's ceiling is over a thousand Turns.
const MAX_HISTORY_PAGES = 8;

/// Reads a branch's committed Turns.
///
/// `limit: 1` is not a smaller version of the same request -- it is a different
/// question. The full history is what the selected conversation renders; one
/// Turn is only what the conversation is *called*, which is all a list row
/// needs. Asking every conversation for its whole transcript on every poll
/// would be the client doing work nobody asked to see.
async function readHistory(
  api: NonNullable<ReturnType<typeof bridge>>,
  head: SessionHead,
  whole: boolean,
): Promise<{ turns: SessionTurn[]; truncated: boolean }> {
  const turns: SessionTurn[] = [];
  let after = 0;
  for (let pages = 0; pages < MAX_HISTORY_PAGES; pages += 1) {
    const reply = await api.sessionHistory({
      sessionId: head.session_id,
      branchId: head.branch_id,
      generation: head.generation,
      afterTurnOrdinal: after,
      ...(whole ? {} : { limit: 1 }),
    });
    if (!reply.ok) return { turns, truncated: false };
    turns.push(...reply.value.turns);
    const next = reply.value.nextAfterTurnOrdinal;
    if (!whole || next === null) return { turns, truncated: false };
    // A page that reports more but returns nothing would otherwise spin here.
    if (next <= after) break;
    after = next;
  }
  return { turns, truncated: true };
}

type HistoryCache = Map<string, { generation: number; turnCount: number; turns: SessionTurn[] }>;

/// Fills in every conversation, re-reading as little as the runtime allows.
///
/// A committed Turn is immutable, so history is only re-read when the head says
/// there is more of it, or when a rollback moved the generation out from under
/// what was cached.
async function readSessions(
  api: NonNullable<ReturnType<typeof bridge>>,
  heads: SessionHead[],
  current: string | null,
  cache: HistoryCache,
  runs: RunView[],
): Promise<SessionView[]> {
  const views: SessionView[] = [];
  for (const head of heads) {
    const selected = head.session_id === current;
    const cached = cache.get(head.session_id);
    const stale = !cached
      || cached.generation !== head.generation
      || cached.turnCount !== head.turn_count
      || (selected && cached.turns.length < head.turn_count);
    if (stale && head.turn_count > 0) {
      const read = await readHistory(api, head, selected);
      cache.set(head.session_id, {
        generation: head.generation,
        turnCount: head.turn_count,
        turns: read.turns,
      });
    }
    // A Session whose first Turn is still running has committed no title yet.
    // Its Run record carries the sentence -- the same sentence, from the other
    // durable source, rather than a guess or a copy this client kept.
    const live = head.active_run_id
      ? runs.find((run) => run.id === head.active_run_id)?.asked ?? ""
      : "";
    views.push(viewOf(head, cache.get(head.session_id)?.turns ?? [], live));
  }
  return newestFirst(views);
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
  /// Conversations, newest first. Each is one branch of one Session.
  sessions: SessionView[];
  /// The conversation the composer is writing into, or null for "the next
  /// thing typed starts a new one".
  current: SessionView | null;
  selectSession(sessionId: string | null): void;
  /// Leave the current conversation, so the next thing typed starts a new one.
  newConversation(): void;
  /// Say something in the current conversation, starting one if there is none.
  ///
  /// This is what `submit` should have been. `submit` starts a bare Run, which
  /// carries no history: every sentence sent that way is the first sentence of
  /// its own conversation, and the model is never told what was said before.
  send(input: string): Promise<string | null>;
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
  const [sessions, setSessions] = useState<SessionView[]>([]);
  const [current, setCurrent] = useState<string | null>(null);
  const busy = useRef(false);
  /// Read inside the poll, which must not be rebuilt every time the selection
  /// changes -- that would restart the interval on every click.
  const currentRef = useRef<string | null>(null);
  /// Whether the first list has already chosen a conversation. Without this
  /// the poll would re-open the newest one a second after the person asked for
  /// a new one, which reads as the button not working.
  const opened = useRef(false);
  const branches = useRef(new Map<string, string>());
  const history = useRef<HistoryCache>(new Map());

  const load = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    try {
      const next = await probe();
      setLink(next);
      if (next.state !== "live") {
        setRuns([]);
        setSessions([]);
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

      const heads = await api.sessionList();
      if (heads.ok) {
        for (const head of heads.value.heads) branches.current.set(head.session_id, head.branch_id);
        // Chosen before the histories are read, not after. Reading first would
        // fetch the conversation about to be opened as if it were a list row --
        // one Turn, its name -- and the person would watch the rest of it
        // arrive a poll later. Which conversation is open decides how much of
        // it to read, so it has to be decided first.
        if (!opened.current && heads.value.heads.length > 0) {
          const newest = [...heads.value.heads]
            .sort((a, b) => (a.session_id < b.session_id ? 1 : -1))[0];
          opened.current = true;
          currentRef.current = newest.session_id;
          setCurrent(newest.session_id);
        }
        setSessions(await readSessions(
          api, heads.value.heads, currentRef.current, history.current, views,
        ));
      }
      // Last, and after the conversations rather than after the runs. This is
      // what says "the first list is in", and the launcher's self-report reads
      // it: set before the Sessions land, it reported zero conversations about
      // a state root holding two. A diagnostic that measures the moment before
      // the thing arrives is worse than none.
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

  const selectSession = useCallback((sessionId: string | null) => {
    opened.current = true;
    currentRef.current = sessionId;
    setCurrent(sessionId);
    void load();
  }, [load]);

  const newConversation = useCallback(() => {
    opened.current = true;
    currentRef.current = null;
    setCurrent(null);
  }, []);

  /// Start a conversation, or add a Turn to the current one.
  ///
  /// Two things here are read from the runtime immediately before the write
  /// rather than remembered: whether a Turn is already in flight, and which
  /// generation the branch is on. Remembering either is how a client sends a
  /// Turn into history the person already rolled back, or asks for a second
  /// Turn the branch is bound to refuse.
  const send = useCallback(async (input: string) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const sessionId = currentRef.current;
    const branchId = sessionId ? branches.current.get(sessionId) : undefined;

    if (!sessionId || !branchId) {
      const ids = { sessionId: uuidv7(), branchId: uuidv7(), runId: uuidv7() };
      const started = await api.sessionStart({ ...ids, input });
      if (!started.ok) return started.error;
      branches.current.set(ids.sessionId, ids.branchId);
      currentRef.current = ids.sessionId;
      setCurrent(ids.sessionId);
      void load();
      return null;
    }

    const head = await api.sessionRead({ sessionId, branchId });
    if (!head.ok) return head.error;
    if (head.value.active_run_id) return "这轮还没结束，等它停下再说下一句";
    const reply = await api.sessionContinue({
      sessionId, branchId, generation: head.value.generation, runId: uuidv7(), input,
    });
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
    sessions,
    current: sessions.find((session) => session.sessionId === current) ?? null,
    selectSession, newConversation, send,
    submit, decide, refresh: () => void load(),
  };
}
