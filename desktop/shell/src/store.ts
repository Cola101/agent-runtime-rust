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
  type SessionHead, type SessionTurn, type ProviderView, type McpServers,
} from "./runtime";
import { keyOf, newestFirst, viewOf, type SessionView } from "./session";
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

/// How many sentences may wait for the Turn in flight to end.
///
/// A ceiling is needed because every sentence in this queue becomes a Turn of
/// its own, sent one after another with nobody watching -- so the ceiling is a
/// spend ceiling as much as a list ceiling. Ten, because type-ahead is two or
/// three sentences in practice and ten is already a run of Turns nobody is
/// reading; a person who reaches it is queueing into a Turn that has been stuck
/// for a long time, which is worth being told about rather than absorbing.
///
/// What happens at the ceiling is a refusal, not a summary. OpenClaw's
/// gateway-side followup queue caps at twenty and summarises the overflow,
/// which it can do because those followups are the gateway's own. These are
/// sentences a person wrote and has not sent: summarising one would put words
/// in their mouth that they never typed, so the eleventh is refused and stays
/// in the box, under their hand.
const QUEUE_LIMIT = 10;
const QUEUE_FULL = `排队已经满了（最多 ${QUEUE_LIMIT} 句），等它发出去几句再说`;

/// One `ProcessSessionOutput` as a `tool.result` carried it.
///
/// Every field is the runtime's, including the words: `state` and
/// `terminationReason` are `ProcessSessionState` and
/// `ProcessSessionTerminationReason` verbatim.
///
/// The cursors are the load-bearing part. `from`/`to` are byte offsets into the
/// session's own stdout or stderr log, so a client can say exactly which bytes
/// it is holding and exactly where it is holding nothing — which is the whole
/// difference between replaying a session and drawing one.
export type ProcessOutput = {
  sessionId: string;
  state: string;
  pid: number | null;
  exitCode: number | null;
  terminationReason: string | null;
  stdout: string;
  stdoutFrom: number;
  stdoutTo: number;
  stdoutTruncated: boolean;
  stderr: string;
  stderrFrom: number;
  stderrTo: number;
  stderrTruncated: boolean;
};

/// One `process.*` call, and whatever became of it.
///
/// `wrote` is the only record of what the agent sent: `process.start` carries
/// `initial_stdin` and `process.write` carries `stdin`. Nothing anywhere in the
/// log names the program those bytes are being typed at — see `ProcessSession`.
export type ProcessCall = {
  sequence: number;
  timestamp: string;
  /// The runtime's tool name, verbatim.
  tool: string;
  toolCallId: string;
  arguments: Record<string, unknown>;
  wrote: string | null;
  output: ProcessOutput | null;
  /// A failed `process.start` names the session it failed to start, so the
  /// error carries a session id where the output would have.
  error: { code: string; message: string; sessionId: string | null } | null;
  /// Why this call has no output, when it has none. Both are separate event
  /// types in the log, never inferred from the absence of a result.
  outcome: "output" | "error" | "waiting" | "denied";
};

/// The calls of one durable process session, in log order.
///
/// What this deliberately does not have is a command. `process.start` takes no
/// argv — the program is `AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE`, chosen by
/// whoever launched the runtime-host, and it appears in no event. The only
/// thing resembling an identity in the log is the policy snapshot's
/// `implementation_digest`, which is a hash of the binary, not its path.
export type ProcessSession = {
  /// The runtime's session id, or null while a `process.start` has not come
  /// back with one.
  id: string | null;
  /// Stable across polls so React can keep a row. Not shown.
  key: string;
  calls: ProcessCall[];
};

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

/// One field of an MCP form request, as `requested_schema` declares it.
///
/// `type` is the JSON Schema type the server asked for, kept verbatim because
/// it is the whole rule for what may be sent back: the runtime compares the
/// value's JSON type against it and refuses the entire resolution when they
/// disagree. A type this client has no widget for is carried here anyway and
/// said on screen — dropping the field would hide half of what was asked.
export type McpField = {
  name: string;
  type: string;
  title: string | null;
  description: string | null;
  /// The closed set of values the schema allows, or null when it declares none.
  choices: string[] | null;
  required: boolean;
};

/// One interaction an MCP server asked a person for.
///
/// `mode` is the runtime's own word for the kind of request. It is not mapped
/// onto a boolean: a mode this build does not know stays unknown on screen and
/// unanswerable, because rendering a form for it would be inventing what the
/// server asked.
export type McpRequest = {
  key: string;
  mode: string;
  message: string;
  /// Form mode. Empty in every other mode.
  fields: McpField[];
  /// URL mode. Null in every other mode.
  url: string | null;
  elicitationId: string | null;
  /// What the server attached to the request. The runtime does not interpret
  /// it and neither does this — it is shown as it arrived.
  meta: unknown;
};

/// The MCP input request a suspended Run is parked on.
///
/// The three the answer is bound to — `inputId`, `inputVersion`,
/// `bindingDigest` — are echoed back verbatim, and the runtime checks each of
/// them against the exact pending request. A client that reconstructed any of
/// them would be answering a different question than the one on screen. The
/// rest is what a person needs in order to answer: which server is asking,
/// which tool call it belongs to, and which round this is.
export type McpInput = {
  inputId: string;
  inputVersion: number;
  bindingDigest: string;
  serverName: string;
  toolCallId: string;
  round: number;
  requests: McpRequest[];
};

/// One answer to one request. `content` belongs only to an accepted form: the
/// runtime rejects content on a declined, cancelled or URL-mode response.
export type McpResponse = {
  action: "accept" | "decline" | "cancel";
  content?: Record<string, unknown>;
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

/// A required MCP server that was not there when a Run started.
///
/// This is the only thing the runtime tells this client about whether a server
/// came up, and it only exists for servers marked required: `run.failed` with
/// `kind: "required_mcp_unavailable"` names them in the durable log. An optional
/// server that failed leaves nothing behind but a line in the runtime process's
/// own tracing output, which this client cannot read.
export type McpUnavailable = {
  server: string;
  runId: string;
  at: string;
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
  /// The durable process sessions this run's log contains. Empty is a real
  /// answer — it means no `process.*` call was recorded, not that the runtime
  /// has no such tools. Nothing in the log states which tools are installed.
  ///
  /// Named apart from the Session/Turn conversation this client also holds:
  /// they are different objects that happen to share a word, and one field
  /// called `sessions` on two types is how they get confused.
  processSessions: ProcessSession[];
  approval: Approval | null;
  /// The MCP input request this run is suspended on, or null. Like `approval`,
  /// it is only ever set while the runtime's own boundary says the run is
  /// parked — never inferred from having seen the event go past.
  mcpInput: McpInput | null;
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

/// Matched by prefix rather than against a list of the eight tools this build
/// knows. A ninth `process.*` tool must show up as a call that happened, not
/// vanish because this file was written before it existed.
function isProcessTool(name: string): boolean {
  return name.startsWith("process.");
}

/// `model.tool_call` in the durable log is flat — `{id, name, arguments}`. The
/// nested `{call: {…}}` shape is what other payloads carry the same tool in,
/// so both are read here rather than trusting one.
function readCall(payload: Record<string, unknown>): {
  id: string; name: string; arguments: Record<string, unknown>;
} | null {
  const call = (payload.call ?? payload) as Record<string, unknown>;
  const name = call.name;
  const id = call.id;
  if (typeof name !== "string" || typeof id !== "string") return null;
  return {
    id, name,
    arguments: (call.arguments as Record<string, unknown>) ?? {},
  };
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function readProcessOutput(content: unknown): ProcessOutput | null {
  if (!content || typeof content !== "object") return null;
  const body = content as Record<string, unknown>;
  // A result is a session output only if it identifies a session and says what
  // state it is in. Anything else — an error envelope, a shape from a future
  // schema — is not decoded into one.
  if (typeof body.session_id !== "string" || typeof body.state !== "string") return null;
  return {
    sessionId: body.session_id,
    state: body.state,
    pid: typeof body.pid === "number" ? body.pid : null,
    exitCode: typeof body.exit_code === "number" ? body.exit_code : null,
    terminationReason:
      typeof body.termination_reason === "string" ? body.termination_reason : null,
    stdout: typeof body.stdout === "string" ? body.stdout : "",
    stdoutFrom: num(body.stdout_start_cursor),
    stdoutTo: num(body.stdout_cursor),
    stdoutTruncated: body.stdout_truncated === true,
    stderr: typeof body.stderr === "string" ? body.stderr : "",
    stderrFrom: num(body.stderr_start_cursor),
    stderrTo: num(body.stderr_cursor),
    stderrTruncated: body.stderr_truncated === true,
  };
}

/// The bytes the agent sent on this call, or null if it sent none.
///
/// Read from the arguments the model actually produced, not from a guess about
/// which tools take input: `process.start` and `process.write` are the only two
/// that carry stdin, and they name the field differently.
function readWrite(tool: string, args: Record<string, unknown>): string | null {
  const field = tool === "process.start" ? "initial_stdin"
    : tool === "process.write" ? "stdin"
    : null;
  if (!field) return null;
  const value = args[field];
  return typeof value === "string" && value.length > 0 ? value : null;
}

/// Every `process.*` call in one run's log, grouped into the sessions they name.
///
/// The grouping key is the runtime's `session_id` wherever the log has one: in
/// the arguments of every call but `process.start`, and in the result — or the
/// start-failure error — of that one. A `process.start` still parked on a
/// person has no session id anywhere, and gets its own group rather than being
/// filed under a session that does not exist yet.
function readProcessSessions(events: RunEvent[]): ProcessSession[] {
  const calls = new Map<string, ProcessCall>();
  const order: string[] = [];

  for (const event of events) {
    if (event.type === "model.tool_call") {
      const call = readCall(event.payload);
      if (!call || !isProcessTool(call.name)) continue;
      if (calls.has(call.id)) continue;
      calls.set(call.id, {
        sequence: event.sequence,
        timestamp: event.timestamp,
        tool: call.name,
        toolCallId: call.id,
        arguments: call.arguments,
        wrote: readWrite(call.name, call.arguments),
        output: null,
        error: null,
        outcome: "waiting",
      });
      order.push(call.id);
      continue;
    }
    if (event.type === "tool.result") {
      const id = event.payload.tool_call_id;
      if (typeof id !== "string") continue;
      const call = calls.get(id);
      if (!call) continue;
      const output = readProcessOutput(event.payload.content);
      if (output) {
        call.output = output;
        call.outcome = "output";
        continue;
      }
      const error = (event.payload.content as Record<string, unknown> | null)?.error as
        | Record<string, unknown>
        | undefined;
      call.error = {
        code: String(error?.code ?? ""),
        message: String(error?.message ?? ""),
        sessionId: typeof error?.session_id === "string" ? error.session_id : null,
      };
      call.outcome = "error";
      continue;
    }
    if (event.type === "tool.denied") {
      const execution = event.payload.execution as Record<string, unknown> | undefined;
      const call = execution ? readCall(execution) : null;
      const denied = call ? calls.get(call.id) : undefined;
      if (denied) denied.outcome = "denied";
    }
  }

  const sessions = new Map<string, ProcessSession>();
  for (const id of order) {
    const call = calls.get(id)!;
    const fromArguments = call.arguments.session_id;
    const sessionId =
      call.output?.sessionId
      ?? (typeof fromArguments === "string" ? fromArguments : null)
      ?? call.error?.sessionId
      ?? null;
    const key = sessionId ?? `call:${call.toolCallId}`;
    const existing = sessions.get(key);
    if (existing) existing.calls.push(call);
    else sessions.set(key, { id: sessionId, key, calls: [call] });
  }
  return [...sessions.values()];
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

function readMcpRequest(key: string, request: Record<string, unknown>): McpRequest {
  const schema = request.requested_schema as Record<string, unknown> | undefined;
  const properties = (schema?.properties ?? {}) as Record<string, Record<string, unknown>>;
  const required = new Set((schema?.required as string[] | undefined) ?? []);
  return {
    key,
    mode: String(request.mode ?? ""),
    message: String(request.message ?? ""),
    fields: Object.entries(properties).map(([name, property]) => ({
      name,
      type: String(property.type ?? ""),
      title: typeof property.title === "string" ? property.title : null,
      description: typeof property.description === "string" ? property.description : null,
      choices: Array.isArray(property.enum) ? property.enum.map(String) : null,
      required: required.has(name),
    })),
    url: typeof request.url === "string" ? request.url : null,
    elicitationId: typeof request.elicitation_id === "string" ? request.elicitation_id : null,
    meta: request.meta ?? null,
  };
}

/// Reads `mcp.input.required`.
///
/// Note where `input_version` comes from: the event, beside the request rather
/// than inside it. It is the version of the *response binding*, and the runtime
/// refuses a resolution that carries the wrong one — so a client that hard-coded
/// today's number would be answering a contract it never read.
function readMcpInput(payload: Record<string, unknown>): McpInput | null {
  const input = payload.input as Record<string, unknown> | undefined;
  if (!input) return null;
  const requests = (input.requests ?? {}) as Record<string, Record<string, unknown>>;
  return {
    inputId: String(input.input_id ?? ""),
    inputVersion: Number(payload.input_version ?? 0),
    bindingDigest: String(input.binding_digest ?? ""),
    serverName: String(input.server_name ?? ""),
    toolCallId: String(input.tool_call_id ?? ""),
    round: Number(input.round ?? 0),
    requests: Object.entries(requests).map(([key, request]) => readMcpRequest(key, request)),
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
  let mcpInput: McpInput | null = null;

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
      // Both carry a whole `ToolApprovalRequest`, and the rebound one is the
      // request re-issued under a fresh binding after a recovery. Reading only
      // the first left the gate offering a digest the runtime had already
      // replaced -- a decision taken against it does not bind, so the screen
      // was showing a button that could not do what it said.
      case "approval.required":
      case "approval.rebound":
        approval = readApproval(event.payload);
        break;
      case "mcp.input.required":
        mcpInput = readMcpInput(event.payload);
        break;
      // The answer the runtime recorded, which is the only thing that clears
      // the question. A later round arrives as its own `mcp.input.required`.
      case "mcp.input.resolved":
        mcpInput = null;
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
    processSessions: readProcessSessions(events),
    approval: lifecycle.kind === "waiting_approval" ? approval : null,
    // `suspended` is the boundary the local adapter reports for a run parked
    // on an MCP input request. Requiring both it and the unanswered event
    // keeps a build that meets some future suspension from drawing a form for
    // a question nobody asked.
    mcpInput: lifecycle.kind === "suspended" ? mcpInput : null,
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

/// Folds streamed events into a Run already read from the cursor.
///
/// The split is the point. Events are appended as the runtime writes them, so a
/// reply appears while it is being produced instead of at the next poll. The
/// *boundary* -- running, waiting on a person, terminal, retired -- is left
/// exactly as the cursor reported it, because concluding a run is over from the
/// last event that happened to arrive is wrong precisely when it matters.
///
/// Merged by sequence rather than appended: the poll and the stream overlap by
/// design, and the sequence is what makes that harmless.
function withStreamed(run: RunView, streamed: RunEvent[]): RunView {
  if (streamed.length === 0) return run;
  const bySequence = new Map(run.events.map((event) => [event.sequence, event]));
  let added = false;
  for (const event of streamed) {

    if (bySequence.has(event.sequence)) continue;
    bySequence.set(event.sequence, event);
    added = true;
  }
  if (!added) return run;
  const events = [...bySequence.values()].sort((a, b) => a.sequence - b.sequence);
  const projected = project(run.id, run.asked, {
    run_id: run.id,
    requested_after_sequence: 0,
    next_after_sequence: 0,
    earliest_available_sequence: run.earliestSequence,
    highest_committed_sequence: Math.max(run.highestSequence, events[events.length - 1].sequence),
    history_gap: run.historyGap,
    has_more: false,
    // The cursor's, not re-derived. Everything below keeps the boundary the
    // runtime typed for this run.
    state: {},
    events,
  }, events, run.truncated);
  return {
    ...projected,
    lifecycle: run.lifecycle,
    approval: run.approval,
    // Same rule as the lifecycle above, for the same reason. A streamed
    // `mcp.input.required` is re-projected against a page with no state, so
    // `projected.mcpInput` is null however loudly the stream said otherwise;
    // whether this Run is parked on a person is the cursor's to say.
    mcpInput: run.mcpInput,
    error: run.error,
  };
}

function failed(id: string, asked: string, error: CursorError): RunView {
  return {
    id, asked, lifecycle: { kind: "unrecognised" }, events: [], text: "",
    toolCalls: [], processSessions: [], approval: null, mcpInput: null,
    tokens: 0, costMicros: 0,
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

/// Keyed by branch rather than by Session: two branches of one Session are two
/// different conversations with two different histories, and one entry for both
/// would hand a Fork its source's Turns.
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
    const key = keyOf({ sessionId: head.session_id, branchId: head.branch_id });
    const selected = key === current;
    const cached = cache.get(key);
    const stale = !cached
      || cached.generation !== head.generation
      || cached.turnCount !== head.turn_count
      || (selected && cached.turns.length < head.turn_count);
    if (stale && head.turn_count > 0) {
      const read = await readHistory(api, head, selected);
      cache.set(key, {
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
    views.push(viewOf(head, cache.get(key)?.turns ?? [], live));
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
  /// Opens one branch. A Session is not enough to name a conversation once it
  /// has been forked, so the caller passes the branch it is pointing at.
  selectSession(conversation: { sessionId: string; branchId: string }): void;
  /// Leave the current conversation, so the next thing typed starts a new one.
  newConversation(): void;
  /// Configured providers. Never carries a secret -- no bridge call returns
  /// one.
  providers: ProviderView[];
  saveProvider(request: {
    id: string; protocol: string; endpoint: string; model: string; secret?: string | null;
  }): Promise<string | null>;
  forgetProvider(id: string): Promise<string | null>;
  /// What one Run may spend, as this app configured the runtime it started.
  /// Null when the host will not say -- a runtime this app did not start has a
  /// budget of its own and this window does not know it, which is a different
  /// answer from "no limit" and is rendered as one.
  budget: { maxTokens: number; maxCostCents: number; maxDurationSeconds: number } | null;
  /// Configured MCP servers, and what the runtime was started with. Never a
  /// claim that a server is running — nothing on the socket can say that.
  mcp: McpServers;
  /// Required MCP servers a Run was refused for, read from that Run's own log.
  mcpFailures: McpUnavailable[];
  saveMcpServer(request: {
    name: string; command: string; args: string[]; cwd: string | null;
    toolNames: string[]; required: boolean;
  }): Promise<string | null>;
  forgetMcpServer(name: string): Promise<string | null>;
  /// Say something in the current conversation, starting one if there is none.
  ///
  /// This is what `submit` should have been. `submit` starts a bare Run, which
  /// carries no history: every sentence sent that way is the first sentence of
  /// its own conversation, and the model is never told what was said before.
  send(input: string): Promise<string | null>;
  /// Redirect the Turn in flight.
  ///
  /// Returns null when the steer reached the Run -- which is not the same as
  /// it having been applied. The Runtime refuses to redirect while a tool call
  /// or an approval is unresolved, and the only honest evidence is
  /// `run.steer.applied` appearing in that Run's log.
  steer(input: string): Promise<string | null>;
  /// What was typed while a Turn was in flight, in the order it was typed,
  /// waiting to be sent one at a time as Turns end.
  ///
  /// This queue is this window's, and that is the decision rather than an
  /// omission. Nothing in it has ever been sent to a model: it is not Kernel
  /// state, it is in no transcript, and it changes no durable contract -- it is
  /// a draft that has not been sent yet. Codex places its own client queue the
  /// same way, and its `input_restore.rs` says why in the same breath: when a
  /// Turn is interrupted the core drops what it held and the *client* is what
  /// merges the queued lines back into the composer, because they were never
  /// the core's to keep.
  ///
  /// The Kernel could not hold this even if it should. `SteeringMailbox` is a
  /// slot rather than a queue on purpose (`runtime-host/src/lib.rs`): a steer
  /// is about the Turn in flight, so a second one replaces the first. Building
  /// type-ahead on it would lose sentences silently.
  ///
  /// The price, said plainly: this lives in one window's memory. It does not
  /// cross windows, and closing the window loses it. Which is also why a Turn
  /// that ends any way but cleanly hands what is queued back to the box rather
  /// than holding it -- text a person can see and edit is not lost, and text
  /// held by a window they are about to close is.
  queued: string[];
  /// How many may wait at once. Read by the composer so the ceiling is on
  /// screen, rather than being a number the code knows and the person meets.
  queueLimit: number;
  /// Hold a sentence until the Turn in flight ends.
  ///
  /// Returns null when it was taken, or why it was not -- the same shape
  /// `send` answers in, so a refusal leaves the sentence in the box the same
  /// way a refused send does.
  queue(input: string): string | null;
  /// Take one back out, and say what it said, so the box can be refilled with
  /// it. Null when the index names nothing.
  unqueue(index: number): string | null;
  /// Queued sentences that will not be sent after all, given back to whatever
  /// box is on screen, with the reason they came back.
  ///
  /// A store cannot type into a textarea, and the composer is the only thing
  /// that knows what is already in one, so the hand-back is left here and the
  /// composer takes it. Cleared by whoever takes it.
  handback: { text: string; why: string } | null;
  clearHandback(): void;
  /// Cut a new branch carrying this conversation through one Turn, and open it.
  ///
  /// Nothing is lost: the branch this was cut from keeps every Turn it had.
  /// What the person gets is a second strand to say something else in, from a
  /// point they chose.
  fork(throughTurnOrdinal: number): Promise<string | null>;
  /// Take this branch back to a Turn, dropping the Turns after it.
  ///
  /// Irreversible from the client's side: there is no call here that puts them
  /// back, and the next Turn is given the shorter history.
  rollback(throughTurnOrdinal: number): Promise<string | null>;
  submit(input: string): Promise<string | null>;
  /// `reason` is what the person said when refusing, and it reaches the model
  /// as the refused Tool's own result. Read only for `deny`: the other three
  /// have nothing to tell a model that is about to be told something else.
  decide(
    runId: string,
    action: "approve" | "deny" | "cancel" | "resume",
    reason?: string,
  ): Promise<string | null>;
  /// Why the last decision on this Run did not land, if it did not.
  ///
  /// A decision can be refused -- a binding the runtime has moved past, a
  /// socket that is not there any more -- and both surfaces that offer one
  /// dropped the reason on the floor. What that looks like is a button that
  /// does nothing: the gate stays up, the person presses again, and the app
  /// has said nothing at all. Kept here rather than in either component
  /// because the keys are dispatched by the shell, outside both of them.
  decisionRefusal(runId: string): string | null;
  /// Answer the MCP input request a suspended run is parked on.
  ///
  /// `responses` must answer every pending key and no others — the runtime
  /// rejects a resolution that answers a different set. The identity it is
  /// bound to is not a parameter: it is read from the pending request this
  /// client is showing, so what is answered is always what is on screen.
  answerMcpInput(runId: string, responses: Record<string, McpResponse>): Promise<string | null>;
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

/// Pulls the named servers out of the terminal event that carries them.
///
/// Matched on `kind` rather than on the event type alone: `run.failed` is also
/// how a budget or a provider failure ends, and a client that read the server
/// list out of every failure would attribute unrelated ones to MCP.
function readMcpFailures(runs: RunView[]): McpUnavailable[] {
  const found: McpUnavailable[] = [];
  for (const run of runs) {
    for (const event of run.events) {
      if (event.type !== "run.failed") continue;
      if (event.payload.kind !== "required_mcp_unavailable") continue;
      const servers = event.payload.servers;
      if (!Array.isArray(servers)) continue;
      for (const server of servers) {
        found.push({ server: String(server), runId: run.id, at: event.timestamp });
      }
    }
  }
  return found.sort((a, b) => b.at.localeCompare(a.at));
}

export function useRuntime(): Store {
  const [link, setLink] = useState<Link>({ state: "no-bridge" });
  const [runs, setRuns] = useState<RunView[]>([]);
  const [loading, setLoading] = useState(true);
  const [listedAt, setListedAt] = useState<number | null>(null);
  const [sessions, setSessions] = useState<SessionView[]>([]);
  const [providers, setProviders] = useState<ProviderView[]>([]);
  /// `applied: null` until the host answers, which is also what it answers for
  /// a runtime this app did not start. Both mean "this client cannot say", and
  /// collapsing either into an empty list would be a claim.
  const [mcp, setMcp] = useState<McpServers>({ servers: [], applied: null });
  const [budget, setBudget] = useState<Store["budget"]>(null);
  /// Events that arrived on the stream ahead of the poll.
  ///
  /// Dropped per run once the poll's own read has reached them, so this stays a
  /// short lead rather than growing into a second copy of the log this client
  /// would then have to keep agreeing with.
  const [streamed, setStreamed] = useState<Map<string, RunEvent[]>>(new Map());
  const watching = useRef<string | null>(null);
  const [current, setCurrent] = useState<string | null>(null);
  const busy = useRef(false);
  /// The open branch, read inside the poll and by every write. A ref rather
  /// than state because the poll must not be rebuilt every time the selection
  /// changes -- that would restart the interval on every click -- and because a
  /// write has to use the branch that is open now, not the one that was open
  /// when its callback was built.
  const open = useRef<{ sessionId: string; branchId: string } | null>(null);
  /// Whether the first list has already chosen a conversation. Without this
  /// the poll would re-open the newest one a second after the person asked for
  /// a new one, which reads as the button not working.
  const opened = useRef(false);
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
      // What the poll has now read needs no lead. Keeping it would mean holding
      // every event of every followed run for as long as the window is open.
      setStreamed((previous) => {
        if (previous.size === 0) return previous;
        const next = new Map<string, RunEvent[]>();
        for (const [runId, events] of previous) {
          const read = views.find((view) => view.id === runId)?.highestSequence ?? 0;
          const ahead = events.filter((event) => event.sequence > read);
          if (ahead.length > 0) next.set(runId, ahead);
        }
        return next;
      });

      const heads = await api.sessionList();
      if (heads.ok) {
        // Chosen before the histories are read, not after. Reading first would
        // fetch the conversation about to be opened as if it were a list row --
        // one Turn, its name -- and the person would watch the rest of it
        // arrive a poll later. Which conversation is open decides how much of
        // it to read, so it has to be decided first.
        if (!opened.current && heads.value.heads.length > 0) {
          const newest = [...heads.value.heads].sort((a, b) => {
            if (a.session_id !== b.session_id) return a.session_id < b.session_id ? 1 : -1;
            return a.branch_id < b.branch_id ? 1 : -1;
          })[0];
          opened.current = true;
          open.current = { sessionId: newest.session_id, branchId: newest.branch_id };
          setCurrent(keyOf(open.current));
        }
        setSessions(await readSessions(
          api, heads.value.heads, open.current && keyOf(open.current), history.current, views,
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

  /// Read on mount and after a change, never on the poll.
  ///
  /// Each provider costs a `security` process to answer, and putting that on a
  /// 1.2s timer would spawn one per provider per second and a quarter for an
  /// answer that changes when a person changes it.
  const refreshProviders = useCallback(async () => {
    const api = bridge();
    if (!api) return;
    const reply = await api.providers();
    if (reply.ok) setProviders(reply.value);
  }, []);

  useEffect(() => { void refreshProviders(); }, [refreshProviders]);

  const saveProvider = useCallback(async (request: {
    id: string; protocol: string; endpoint: string; model: string; secret?: string | null;
  }) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const reply = await api.saveProvider(request);
    await refreshProviders();
    if (!reply.ok) return reply.error;
    // A freshly installed app has no provider, so its first launch has no
    // runtime behind the window. Bringing one up here is what stops "configure,
    // quit, reopen" from being the documented first run.
    if (api.launch) {
      await api.launch();
      void load();
    }
    return null;
  }, [refreshProviders, load]);

  const forgetProvider = useCallback(async (id: string) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const reply = await api.forgetProvider(id);
    await refreshProviders();
    return reply.ok ? null : reply.error;
  }, [refreshProviders]);

  /// Read on mount and after a change, like the providers and for the same
  /// reason: this is a file on disk plus what the host recorded at spawn, and
  /// neither changes unless a person changes it.
  const refreshMcp = useCallback(async () => {
    const api = bridge();
    if (!api?.mcpServers) return;
    const reply = await api.mcpServers();
    if (reply.ok) setMcp(reply.value);
  }, []);

  useEffect(() => { void refreshMcp(); }, [refreshMcp]);

  /// Read once. It is a constant of this app's own configuration, not runtime
  /// state, and it changes only when the app restarts the runtime with a
  /// different one. An older preload has no `budget` at all, which stays null
  /// rather than becoming a guess.
  useEffect(() => {
    const api = bridge();
    if (!api?.budget) return;
    void api.budget().then((reply) => { if (reply.ok) setBudget(reply.value); });
  }, []);

  const saveMcpServer = useCallback(async (request: {
    name: string; command: string; args: string[]; cwd: string | null;
    toolNames: string[]; required: boolean;
  }) => {
    const api = bridge();
    if (!api?.saveMcpServer) return "not running in the desktop host";
    const reply = await api.saveMcpServer(request);
    await refreshMcp();
    return reply.ok ? null : reply.error;
  }, [refreshMcp]);

  const forgetMcpServer = useCallback(async (name: string) => {
    const api = bridge();
    if (!api?.forgetMcpServer) return "not running in the desktop host";
    const reply = await api.forgetMcpServer(name);
    await refreshMcp();
    return reply.ok ? null : reply.error;
  }, [refreshMcp]);

  /// One subscription for the window, for the run on screen.
  ///
  /// Following every run at once would hold a connection per run and deliver
  /// events nobody is looking at. The transcript shows one run, so one is
  /// followed -- and it is dropped as soon as the cursor moves.
  useEffect(() => {
    const api = bridge();
    if (!api?.onEvent) return;
    const offEvent = api.onEvent(({ runId, event }) => {
      setStreamed((previous) => {
        const next = new Map(previous);
        next.set(runId, [...(next.get(runId) ?? []), event]);
        return next;
      });
    });
    const offEnded = api.onWatchEnded(({ runId }) => {
      if (watching.current === runId) watching.current = null;
      // The boundary is the cursor's to report, so the end of a stream is a
      // reason to read it now rather than something to render.
      void load();
    });
    return () => { offEvent(); offEnded(); };
  }, [load]);

  /// Follows the Turn in flight, and nothing else.
  ///
  /// The store knows one run is live without guessing: the open conversation's
  /// head names it. Following every run instead would hold a connection each
  /// for logs nobody is reading, and following "the selected run" would follow
  /// finished runs that have nothing left to say.
  const live = sessions.find((session) => session.key === current)?.activeRunId ?? null;
  useEffect(() => {
    const api = bridge();
    if (!api?.watch) return;
    if (watching.current === live) return;
    if (watching.current) void api.unwatch(watching.current);
    watching.current = live;
    if (live) void api.watch({ runId: live, afterSequence: 0 });
  }, [live]);

  const steer = useCallback(async (input: string) => {
    const api = bridge();
    if (!api?.steer) return "not running in the desktop host";
    const at = open.current;
    const runId = at
      ? sessions.find((session) => session.key === keyOf(at))?.activeRunId ?? null
      : null;
    if (!runId) return "这轮已经结束了，直接说下一句";
    // One id per steer, minted here so a retry of the same call is idempotent
    // and two different redirects are two commands.
    const reply = await api.steer({ runId, steeringId: uuidv7(), input });
    if (!reply.ok) return reply.error;
    void load();
    return null;
  }, [sessions, load]);

  const selectSession = useCallback((conversation: { sessionId: string; branchId: string }) => {
    opened.current = true;
    open.current = { sessionId: conversation.sessionId, branchId: conversation.branchId };
    setCurrent(keyOf(conversation));
    void load();
  }, [load]);

  const newConversation = useCallback(() => {
    opened.current = true;
    open.current = null;
    setCurrent(null);
  }, []);

  /// Start a conversation, or add a Turn to the current one.
  ///
  /// Two things here are read from the runtime immediately before the write
  /// rather than remembered: whether a Turn is already in flight, and which
  /// generation the branch is on. Remembering either is how a client sends a
/// What a branch that is still finishing a Turn is called, in one place.
///
/// The check above the write and the runtime's own refusal are the same fact
/// arriving twice, and they were two different sentences -- one written here,
/// one the runtime's internals. The window between them is real: a Runtime
/// restart resumes the Turn it interrupted, so a person typing right after
/// pressing 重启 Runtime can read a head with no active Run and still be
/// refused a moment later.
const BRANCH_BUSY = "这轮还没结束，等它停下再说下一句";

/// The runtime's refusal, said as the thing it means.
///
/// Matched on the runtime's own phrase rather than on a code, because there is
/// no code here to match: the local surface answers a refused mutation with a
/// message. An error this does not recognise is passed through untouched --
/// printing the runtime's words is worse than a sentence, and much better than
/// swallowing something nobody predicted.
function sendRefusal(error: string): string {
  return error.includes("already has an active Turn") ? BRANCH_BUSY : error;
}

  /// Turn into history the person already rolled back, or asks for a second
  /// Turn the branch is bound to refuse.
  const send = useCallback(async (input: string) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const at = open.current;

    if (!at) {
      const ids = { sessionId: uuidv7(), branchId: uuidv7(), runId: uuidv7() };
      const started = await api.sessionStart({ ...ids, input });
      if (!started.ok) return started.error;
      open.current = { sessionId: ids.sessionId, branchId: ids.branchId };
      setCurrent(keyOf(open.current));
      void load();
      return null;
    }

    const head = await api.sessionRead(at);
    if (!head.ok) return head.error;
    if (head.value.active_run_id) return BRANCH_BUSY;
    const reply = await api.sessionContinue({
      ...at, generation: head.value.generation, runId: uuidv7(), input,
    });
    if (!reply.ok) return sendRefusal(reply.error);
    void load();
    return null;
  }, [load]);

  /// What was typed while a Turn was running, and the two facts that decide
  /// what becomes of it.
  ///
  /// The list is held twice on purpose. `queued` is what the screen draws;
  /// `waiting` is the same list readable outside a render, because the drain
  /// runs in an effect and a write has to see what is queued *now* rather than
  /// what was queued when its callback was built.
  const [queued, setQueued] = useState<string[]>([]);
  const waiting = useRef<string[]>([]);
  const hold = useCallback((next: string[]) => {
    waiting.current = next;
    setQueued(next);
  }, []);
  /// The conversation these sentences were typed into.
  ///
  /// A queue belongs to one conversation. Sending it into another one would put
  /// words somewhere the person never typed them, so leaving the conversation
  /// hands them back instead of carrying them along -- and, incidentally, the
  /// only other reading of "the open branch stopped naming an active Run" is
  /// exactly that switch, which is why this is checked before the edge below.
  const queuedFor = useRef<string | null>(null);
  const [handback, setHandback] = useState<Store["handback"]>(null);
  /// Gives sentences back to the box, newest last, with the reason.
  ///
  /// Merged onto a hand-back nobody has taken yet rather than replacing it:
  /// two of these before the composer reads either is unlikely and losing the
  /// first one's text is the exact failure this whole path exists to prevent.
  const giveBack = useCallback((why: string, first: string[] = []) => {
    const lines = [...first, ...waiting.current];
    if (lines.length === 0) return;
    hold([]);
    queuedFor.current = null;
    setHandback((standing) => ({
      text: standing ? `${standing.text}\n${lines.join("\n")}` : lines.join("\n"),
      why,
    }));
  }, [hold]);
  const clearHandback = useCallback(() => setHandback(null), []);

  const queue = useCallback((input: string) => {
    if (waiting.current.length >= QUEUE_LIMIT) return QUEUE_FULL;
    if (waiting.current.length === 0) queuedFor.current = current;
    hold([...waiting.current, input]);
    return null;
  }, [hold, current]);

  const unqueue = useCallback((index: number) => {
    const held = waiting.current;
    if (index < 0 || index >= held.length) return null;
    hold(held.filter((_, at) => at !== index));
    return held[index];
  }, [hold]);

  /// Sends what was queued, as Turns end.
  ///
  /// On the edge this store already has -- the open branch's head stops naming
  /// an active Run -- rather than on a clock of its own. The poll is what knows
  /// a Turn ended; a second timer would learn the same fact later and sometimes
  /// disagree with the screen about it.
  ///
  /// One sentence per edge, never the whole queue. A branch holds one Turn at a
  /// time, so the second sentence waits for the Turn the first one starts.
  /// Codex and opencode both drain one at a time, and this is why.
  ///
  /// Anything but a clean ending gives everything back to the box. A Turn
  /// somebody stopped is the case this is written for: firing the next sentence
  /// into a conversation a person just cancelled is worse than not having a
  /// queue at all. `succeeded` is the only ending that continues, so a Run that
  /// failed, timed out, ended indeterminate, or is no longer on the run list to
  /// be read at all is a hand-back -- being unsure is a reason to give the
  /// words back, never a reason to send them.
  ///
  /// The same strictness has a cost, and it is the right one. A Turn that
  /// started and finished entirely between two polls is never seen running, so
  /// it produces no edge and the rest of the queue waits instead of going. What
  /// waiting looks like is the queue still on screen with every sentence in it,
  /// takeable back with one click, and it is undone by the next Turn that ends;
  /// what the other choice looks like is a sentence sent into a conversation
  /// somebody stopped, which nothing undoes.
  const lastLive = useRef<string | null>(null);
  /// The Run whose ending has not been read yet.
  ///
  /// One poll reads the run list and *then* the session heads (`load()` above),
  /// so a head clearing is always seen before that Run's own lifecycle is seen
  /// turning terminal. Reading the ending off the falling edge alone therefore
  /// asks the older of two samples a question only the newer one can answer,
  /// and every ordinary success passes through that window. It read as "did not
  /// end properly", so the queue handed itself back on the happy path.
  ///
  /// Holding the id instead separates the two states the predicate below used
  /// to conflate: "ended badly" and "has not been read as ended". The first is
  /// a hand-back, the second is a wait -- and it resolves on its own, because
  /// this effect depends on `runs`.
  const endedRun = useRef<string | null>(null);
  useEffect(() => {
    const before = lastLive.current;
    lastLive.current = live;
    if (before !== null && live === null) endedRun.current = before;
    if (waiting.current.length === 0) return;
    if (queuedFor.current !== current) {
      giveBack("换了对话，排队的话放回输入框了");
      endedRun.current = null;
      return;
    }
    // Only the falling edge. A Turn still running has not ended, and a Turn
    // this window never saw running gives nothing to read an ending off.
    const finished = endedRun.current;
    if (finished === null || live !== null) return;
    const record = runs.find((run) => run.id === finished);
    // Gone from the list entirely -- retired, or aged out of what this client
    // read. Nothing can be learned about how it ended, and being unsure is a
    // reason to give the words back.
    if (!record) {
      giveBack("这轮读不到了，排队的话放回输入框了");
      endedRun.current = null;
      return;
    }
    // Present but not settled yet: this is the sampling window, not an ending.
    // Wait for the poll that carries its lifecycle.
    if (record.lifecycle.kind !== "terminal" && record.lifecycle.kind !== "retired") return;
    endedRun.current = null;
    if (record.lifecycle.status !== "succeeded") {
      giveBack("这轮没有正常结束，排队的话放回输入框了");
      return;
    }
    const [next, ...rest] = waiting.current;
    hold(rest);
    void send(next).then((failure) => {
      // The window between reading the head and writing to it is real -- a
      // Runtime restart resumes the Turn it interrupted -- so a drained send
      // can still be refused. It goes back to the box with the rest, first,
      // because it was typed first.
      if (failure) giveBack(failure, [next]);
    });
  }, [live, current, runs, send, giveBack, hold]);

  /// Fork and Rollback, which share everything except what they do.
  ///
  /// Both read the head immediately before the write, for the same two reasons
  /// `send` does: the branch refuses either while a Turn is in flight, and the
  /// generation is a fence a rollback moves. Sending a remembered generation
  /// gets a refusal that was avoidable by asking.
  const fork = useCallback(async (throughTurnOrdinal: number) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const at = open.current;
    if (!at) return "还没有打开的对话";
    const head = await api.sessionRead(at);
    if (!head.ok) return head.error;
    if (head.value.active_run_id) return "这轮还在跑，等它停下再分叉";
    const reply = await api.sessionFork({
      sessionId: at.sessionId,
      sourceBranchId: at.branchId,
      sourceGeneration: head.value.generation,
      throughTurnOrdinal,
      // The caller's id, and the Fork's identity afterwards: a retry that
      // minted a fresh one would cut a second branch rather than find the
      // first. Minted per attempt, which is what makes the retry the same
      // request.
      targetBranchId: uuidv7(),
    });
    if (!reply.ok) return reply.error;
    // Open the head the reply carried, and only because a reply carried it: a
    // refused Fork cut nothing, and moving the person into a branch no answer
    // named would be drawing a conversation that does not exist. The id in that
    // head is the one this call asked for -- the daemon identifies a Fork by
    // its target, which is what makes a retry find the first cut instead of
    // making a second -- so this reads the same value from the side that is
    // authoritative about it.
    open.current = { sessionId: reply.value.session_id, branchId: reply.value.branch_id };
    setCurrent(keyOf(open.current));
    void load();
    return null;
  }, [load]);

  const rollback = useCallback(async (throughTurnOrdinal: number) => {
    const api = bridge();
    if (!api) return "not running in the desktop host";
    const at = open.current;
    if (!at) return "还没有打开的对话";
    const head = await api.sessionRead(at);
    if (!head.ok) return head.error;
    if (head.value.active_run_id) return "这轮还在跑，等它停下再回滚";
    const reply = await api.sessionRollback({
      ...at, generation: head.value.generation, throughTurnOrdinal,
    });
    if (!reply.ok) return reply.error;
    void load();
    return null;
  }, [load]);

  const [refusals, setRefusals] = useState<Record<string, string>>({});
  const decide = useCallback(
    async (
      runId: string,
      action: "approve" | "deny" | "cancel" | "resume",
      reason?: string,
    ) => {
      const api = bridge();
      const said = await (async () => {
        const api2 = api;
        if (!api2) return "这个窗口没有连到宿主";
        const reply = await api2.control({ action, runId, reason });
        return reply.ok ? null : reply.error;
      })();
      // Cleared on the way in as well as set on the way out: a refusal left
      // standing after the next press succeeded would say the opposite of
      // what happened.
      setRefusals((held) => {
        if (said === null) {
          if (!(runId in held)) return held;
          const next = { ...held };
          delete next[runId];
          return next;
        }
        return { ...held, [runId]: said };
      });
      void load();
      return said;
    },
    [load],
  );
  const decisionRefusal = useCallback(
    (runId: string) => refusals[runId] ?? null,
    [refusals],
  );

  const answerMcpInput = useCallback(
    async (runId: string, responses: Record<string, McpResponse>) => {
      const api = bridge();
      // A host too old to carry this call has no path to the runtime's
      // `ResolveMcpInput`. Saying so beats a button that silently does nothing.
      if (!api?.resolveMcpInput) return "这个宿主还没有回答 MCP 输入的通道";
      // Read from `runs` rather than the merged view on purpose: the pending
      // request is a boundary fact the cursor reported, and `withStreamed`
      // carries it through untouched for the same reason it carries the
      // lifecycle and the approval through untouched.
      const input = runs.find((run) => run.id === runId)?.mcpInput;
      if (!input) return "这个 Run 现在没有在等输入";
      const reply = await api.resolveMcpInput({
        runId,
        inputId: input.inputId,
        inputVersion: input.inputVersion,
        bindingDigest: input.bindingDigest,
        responses,
      });
      void load();
      return reply.ok ? null : reply.error;
    },
    [runs, load],
  );

  // Streamed events are folded in here rather than into `runs`, so the poll's
  // reading of the durable log stays the thing this client holds, and the
  // stream stays an accelerator on top of it.
  const merged = runs.map((run) => withStreamed(run, streamed.get(run.id) ?? []));

  return {
    link, runs: merged, policies: readPolicies(merged), loading, listedAt,
    sessions,
    current: sessions.find((session) => session.key === current) ?? null,
    selectSession, newConversation, send, steer, fork, rollback,
    queued, queueLimit: QUEUE_LIMIT, queue, unqueue, handback, clearHandback,
    providers, saveProvider, forgetProvider,
    budget, mcp, mcpFailures: readMcpFailures(merged), saveMcpServer, forgetMcpServer,
    submit, decide, decisionRefusal, answerMcpInput, refresh: () => void load(),
  };
}
