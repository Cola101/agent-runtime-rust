/// Shapes the surfaces render, and the words they render them with.
///
/// `Lifecycle` is a translation of the runtime's typed boundary, never
/// something derived from the event list. A client that concludes a run is
/// over by looking at the last event it happened to receive will be wrong
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

/// The runtime's status words, in the language the person reading them uses.
///
/// `indeterminate` is the one that must not be softened. It does not mean the
/// run failed; it means nobody can say whether the effect landed, and the only
/// way out is a person deciding. "未知" would read as a display problem.
const STATUS: Record<string, string> = {
  succeeded: "成功",
  failed: "失败",
  cancelled: "已取消",
  timed_out: "超时",
  indeterminate: "结果无法判定",
};

function statusLabel(status: string): string {
  return STATUS[status] ?? status;
}

export function lifecycleLabel(lifecycle: Lifecycle): string {
  switch (lifecycle.kind) {
    case "running": return "运行中";
    case "cancelling": return "取消中";
    case "waiting_approval": return "等你决定";
    case "suspended": return "已挂起";
    case "interrupted": return "被打断";
    case "terminal": return statusLabel(lifecycle.status);
    case "retired": return `${statusLabel(lifecycle.status)}・日志已回收`;
    case "unrecognised": return "本版本不认识的状态";
  }
}

/// The colour a state is allowed to use. Only three earn one; the rest are
/// carried by the word alone.
export function lifecycleTone(lifecycle: Lifecycle): "attention" | "unknown" | "plain" {
  if (lifecycle.kind === "waiting_approval") return "attention";
  if (lifecycle.kind === "interrupted" || lifecycle.kind === "unrecognised") return "unknown";
  if ((lifecycle.kind === "terminal" || lifecycle.kind === "retired") &&
      lifecycle.status === "indeterminate") return "unknown";
  return "plain";
}

/// A tool's effect class, which is what decides whether a lost answer is
/// recoverable. Shown on every approval because it is the reason the runtime
/// is asking at all.
const EFFECT: Record<string, string> = {
  pure: "只读",
  idempotent: "可重复执行",
  non_idempotent: "重复执行会重复生效",
  unknown: "效果未知",
};

export function effectLabel(effect: string): string {
  return EFFECT[effect] ?? effect;
}

const SANDBOX: Record<string, string> = {
  trusted_native: "无隔离・直接在本机执行",
  macos_seatbelt: "macOS Seatbelt 隔离",
};

export function sandboxLabel(sandbox: string): string {
  return SANDBOX[sandbox] ?? sandbox;
}

/// The three answers MCP defines for one input request, in the order they are
/// offered. The wire word travels with the label because it is what lands in
/// `mcp.input.resolved` — the log will say `decline`, and the person who
/// pressed 拒绝 should be able to recognise their own answer there.
export const MCP_ACTIONS = [
  { action: "accept" as const, label: "接受" },
  { action: "decline" as const, label: "拒绝" },
  { action: "cancel" as const, label: "取消" },
];

/// What a request mode asks of a person. Null for a mode this build does not
/// know: the surfaces say so and offer no way to accept it, because a form
/// drawn for an unknown mode would be a guess about what the server wanted.
const MCP_MODE: Record<string, string> = {
  form: "填一张表",
  url: "去链接上完成",
};

export function mcpModeLabel(mode: string): string | null {
  return MCP_MODE[mode] ?? null;
}

/// Event type names are kept verbatim. They are the runtime's own vocabulary
/// and they appear in the durable log, in evidence files and in ADRs; a
/// translated copy on screen would be a second name for the same thing.
const EVENT_NOTE: Record<string, string> = {
  "run.started": "Run 开始",
  "run.succeeded": "Run 成功",
  "run.failed": "Run 失败",
  "run.cancelled": "Run 已取消",
  "run.indeterminate": "结果无法判定",
  "run.resumed": "已批准，继续执行",
  "run.restored": "从 Checkpoint 恢复",
  "run.steer.applied": "已改向",
  /// A terminal, and the only one of the six that had no line. Every other way
  /// a Run ends says so where it ended.
  "run.timed_out": "Run 超时",
  "model.provider.selected": "选定 Provider",
  "model.provider.failed": "Provider 失败",
  /// The pause has a reason and a length. Without this the transcript simply
  /// stops for `delay_ms`, which reads as a hang.
  "model.provider.retry_scheduled": "已安排重试 Provider",
  "model.tool_call": "模型请求调用工具",
  "model.turn.completed": "本轮结束",
  /// The model answered by refusing. It is an answer, not a failure: the Run
  /// keeps running and the next thing on screen is the model's, so a refusal
  /// that left no mark made the reply look like it changed its mind.
  "model.refusal": "模型拒绝回答",
  /// Not bookkeeping: the payload is a summary the model wrote. It is here for
  /// the same reason the refusal is -- the runtime reported words, and words
  /// dropped are words dropped. Named as reasoning so it cannot be mistaken
  /// for the reply it sits next to.
  "model.reasoning": "模型的推理摘要",
  /// A failover carried the conversation to another Provider but could not
  /// carry the first one's private reasoning state. What follows was written
  /// by a model that no longer has it.
  "model.private_state.omitted": "换 Provider 时丢了推理状态",
  "approval.required": "需要你决定",
  /// Recovery re-issued the pending approval under a fresh binding. A decision
  /// taken against the digest that was on screen a moment ago no longer binds.
  "approval.rebound": "这次调用重新绑定了",
  "tool.denied": "已拒绝",
  /// Only ever drawn for a call that did not run to a normal result -- `Chat`
  /// folds a successful one in with the call it answers. A successful one said
  /// nothing this note could carry, and it said it between every two calls,
  /// which is what kept the fold from ever firing.
  ///
  /// This is the wording for a tool that failed on its own. A call a person
  /// declined arrives in the same shape -- `is_error` with an
  /// `approval_denied` code -- and gets its own words, because telling someone
  /// their own decision was a malfunction is not a small thing to get wrong.
  "tool.result": "工具报错",
  /// The call's outcome was never observed, and its effect class made running
  /// it again safe. Whatever the tool touched, it may have touched twice.
  "tool.retry_requested": "结果未知，重试工具",
  /// Everything before this is a summary now. The model's memory of it is not
  /// what the transcript above says, which is exactly the kind of thing a
  /// person re-reading a reply needs to know.
  "context.compacted": "上文压成了摘要",
  /// The other way a Run parks itself on a person. `approval.required` has the
  /// gate to speak for it; this one had nothing, so a Run that stopped waiting
  /// for an answer looked like a Run that had stopped. Named for who is asking,
  /// because "a tool" and "an MCP server" are answerable by different means.
  "mcp.input.required": "MCP 服务要你回答",
  "mcp.input.resolved": "已回答",
  "mcp.input.continuation.started": "带着你的回答继续调用",
};



/// Events whose payload carries words rather than only a name.
///
/// A hairline and a label are enough for an event that *is* its name. They are
/// not enough when the runtime reported prose: a refusal says why it refused
/// and a reasoning summary says what the model thought it was doing. Reducing
/// either to six characters and a type name drops what the runtime said, which
/// is the same loss as not drawing the event at all.
const EVENT_WORDS: Record<string, string> = {
  "model.refusal": "text",
  "model.reasoning": "summary",
};

/// A list rather than a string, because one of the two is a list.
///
/// `ModelStreamEvent::Reasoning` carries `summary: Vec<String>` and the kernel
/// writes it through `json!` unchanged, so what arrives is an array. Read as a
/// string it was never present, and the client drew the hairline and dropped
/// every word the runtime had reported -- which is the loss this path was added
/// to prevent. A refusal is one string and becomes a list of one; the caller
/// draws each part on its own line, because two summary parts are two things
/// the model said and joining them would report one.
/// Which of `run.failed`'s several endings this one was.
///
/// One event type covers a budget, a required MCP server, a provider that
/// would not answer and whatever the runtime adds next. Drawing only the name
/// left a Run that reached the cap in its own status line saying nothing about
/// which cap -- which reads as an agent that broke rather than a limit that was
/// reached. Each sentence is built from the payload's own fields; a kind this
/// build has never seen is printed as it arrived, because "stopped for a reason
/// this client cannot name" is true and silence is not.
function failureReason(payload: Record<string, unknown>): string | null {
  const kind = typeof payload.kind === "string" ? payload.kind : null;
  if (!kind) return null;
  switch (kind) {
    case "budget_exhausted": {
      const dimension = String(payload.dimension ?? "");
      const named: Record<string, string> = {
        tokens: "token 预算用完了",
        cost: "花费预算用完了",
        duration: "时长预算用完了",
      };
      return named[dimension] ?? `预算用完了（${dimension || "没说是哪一项"}）`;
    }
    case "required_mcp_unavailable": {
      const servers = Array.isArray(payload.servers)
        ? payload.servers.map(String).join("、")
        : "";
      return servers
        ? `必需的 MCP 服务起不来：${servers}`
        : "必需的 MCP 服务起不来";
    }
    case "duration_budget":
      return "时长预算用完了";
    default:
      return `这个版本不认识的失败原因：${kind}`;
  }
}

/// What the runtime says went wrong with one tool call.
///
/// The code is the runtime's own: `approval_denied` is written when a reviewer
/// says no, and it is the only way to tell that apart from a tool that broke,
/// because both arrive as a `tool.result` with `is_error`.
function toolResultNote(payload: Record<string, unknown>): string | null {
  if (payload.is_error !== true) return null;
  const content = (payload.content ?? {}) as Record<string, unknown>;
  const error = (content.error ?? {}) as Record<string, unknown>;
  return error.code === "approval_denied" ? "你没让它执行" : null;
}

/// A byte count, for a person rather than for a machine.
///
/// Here rather than on the workspace surface, because the transcript shows the
/// size of a file the agent read and the listing shows the size of a file on
/// disk, and two implementations of one format is two answers to one question.
export function size(bytes: number | null): string {
  if (bytes === null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/// One tool call as a line.
///
/// The arguments are stringified because for most tools they are the call: a
/// command, a path, a query. A write is the exception -- its argument *is* the
/// file, so inlining it puts the whole new content above the comparison that
/// exists to make it readable, and the path is what identifies the call
/// anyway. Here rather than on either surface, because the transcript and the
/// approval queue draw the same call and two spellings of it would be two
/// answers to "what is being asked".
export function callLine(name: string, args: Record<string, unknown>): string {
  if (name === "workspace.write_text" && typeof args.path === "string") {
    return `${name}(${args.path})`;
  }
  return `${name}(${JSON.stringify(args)})`;
}

/// What one event says, in words.
///
/// Takes the payload as well as the type because one type is not always one
/// sentence: a `tool.result` that failed and one a person declined arrive in
/// the same shape, and only the payload separates them.
export function eventNote(type: string, payload: Record<string, unknown> = {}): string | null {
  if (type === "tool.result") {
    const declined = toolResultNote(payload);
    if (declined) return declined;
  }
  return EVENT_NOTE[type] ?? null;
}

/// What a Provider failure means for the person reading it.
///
/// Eight kinds, and what to do about them is different for each: a key that is
/// wrong is a trip to the settings page, a rate limit is a wait, a context
/// overflow is a shorter sentence. "Provider 失败" is none of those and was all
/// the transcript said. A kind this build has never seen is printed as it
/// arrived, for the same reason an unknown `run.failed` kind is.
function providerFailure(payload: Record<string, unknown>): string | null {
  const kind = typeof payload.kind === "string" ? payload.kind : null;
  if (!kind) return null;
  const named: Record<string, string> = {
    authentication: "密钥不对，或者没有权限",
    billing: "账上不让再调了",
    rate_limited: "调得太快，被限流了",
    timeout: "等太久没回应",
    protocol: "回复的格式不对",
    context_overflow: "这轮的上文超过它能接受的长度",
    capability_mismatch: "这个 Provider 给不了这次要用的能力",
    unavailable: "连不上，或者对面不可用",
  };
  const said = named[kind] ?? `这个版本不认识的原因：${kind}`;
  const provider = typeof payload.provider_id === "string" ? payload.provider_id : "";
  return provider ? `${provider}：${said}` : said;
}

/// A wait a person can see is a wait they can decide to sit through.
function providerRetry(payload: Record<string, unknown>): string | null {
  const delay = typeof payload.delay_ms === "number" ? payload.delay_ms : null;
  const attempt = typeof payload.provider_attempt === "number" ? payload.provider_attempt : null;
  if (delay === null && attempt === null) return null;
  const parts: string[] = [];
  if (delay !== null) {
    parts.push(delay >= 1000 ? `${(delay / 1000).toFixed(1)} 秒后再试` : `${delay} 毫秒后再试`);
  }
  if (attempt !== null) parts.push(`第 ${attempt} 次`);
  const because = providerFailure(payload);
  return because ? `${parts.join("・")}（${because}）` : parts.join("・");
}

export function eventWords(type: string, payload: Record<string, unknown>): string[] | null {
  if (type === "model.provider.failed") {
    const why = providerFailure(payload);
    return why ? [why] : null;
  }
  if (type === "model.provider.retry_scheduled") {
    const waiting = providerRetry(payload);
    return waiting ? [waiting] : null;
  }
  if (type === "run.failed") {
    const reason = failureReason(payload);
    return reason ? [reason] : null;
  }
  const field = EVENT_WORDS[type];
  if (!field) return null;
  const words = payload[field];
  const parts = (Array.isArray(words) ? words : [words])
    .filter((part): part is string => typeof part === "string" && part.trim() !== "");
  return parts.length > 0 ? parts : null;
}

/// Bookkeeping the conversation does not need to carry.
///
/// These are state, not content. "Run 开始" says what the status line already
/// says in a word, and which Provider answered is not something a person is
/// reading a conversation to find out -- until it fails, which is a different
/// event and stays visible.
///
/// They are also the events that only exist while a Turn is running: once it
/// commits, the Session's frozen transcript has the exchange and none of this,
/// so leaving them in made the same exchange look like two different things
/// depending on when you looked.
///
/// Not hidden. `⌘I` opens the raw log -- every event, with sequence, payload
/// and digest -- and that drawer is the authority. This is about what the
/// conversation column carries, not about what the client will show.
const ROUTINE: ReadonlySet<string> = new Set([
  "run.started",
  "model.provider.selected",
  "model.turn.completed",
  "model.usage",
  /// The four stages of one tool call. The call itself is already a line of
  /// the transcript, and it is folded with its neighbours precisely so that a
  /// turn with eleven calls is not eleven blocks; a rule drawn between every
  /// stage of every call would undo that and say nothing the call does not.
  /// `auto_approved` is the borderline one -- it means policy exempted a call
  /// that would otherwise have asked -- and it loses on frequency: it happens
  /// once per exempted call, and its reason, snapshot and policy digest are
  /// all in the drawer.
  "tool.execution.requested",
  "tool.execution.auto_approved",
  "tool.execution.started",
  "tool.execution.progress",
  /// The other half of `mcp.input.resolved`, which stays. One resumption, and
  /// the half that says the input landed is the half a person is waiting for.
  "mcp.input.continuation.started",
]);

/// Whether an event belongs in the conversation column.
///
/// The rule is whether it changes what a person should believe about the
/// exchange. A Provider failing, a Run restored from a Checkpoint, a tool
/// denied, a redirect applied -- each of those changes the reading of what
/// follows it, and each stays. Starting and finishing normally does not.
export function belongsInConversation(type: string): boolean {
  return !ROUTINE.has(type);
}

/// Types the conversation column draws as something other than a note.
///
/// The model's prose, its tool calls, the approval gate and the delegation
/// panel are all rendered from these, elsewhere in `Chat`. They are listed
/// here for one reason: so that "this build has never heard of this type"
/// stays a true sentence. An event drawn as a subagent row is not an unknown
/// event, and saying so on screen would be a lie about the client rather than
/// about the runtime.
const DRAWN_ELSEWHERE: ReadonlySet<string> = new Set([
  "model.output.delta",
  "model.tool_call",
  "approval.required",
  "subagent.spawn.requested",
  "subagent.spawned",
  "subagent.forked",
  "subagent.rolled_back",
  "subagent.input.accepted",
  "subagent.input.activated",
  "subagent.terminal.observed",
  "subagent.result.received",
  "subagent.closed",
]);

/// Whether this build has an account of the type at all.
///
/// Three sets, and between them they have to cover everything the runtime
/// emits. A type in none of them is one this client was written before: it has
/// a name, a sequence and a payload, all of them real, and the one thing that
/// must not happen to it is nothing. `Unheard` in `Chat` draws it.
///
/// This is deliberately not the same question as `belongsInConversation`. That
/// one is a judgement about a type someone here has read; there is no such
/// judgement to make about a type nobody has, and defaulting an unknown to
/// "routine" would hide exactly the events this client is least equipped to
/// have an opinion about.
export function knownEvent(type: string): boolean {
  return type in EVENT_NOTE || ROUTINE.has(type) || DRAWN_ELSEWHERE.has(type);
}

/// Relative time, in the granularity a person actually reads. Absolute
/// timestamps stay available on hover.
export function since(iso: string | null): string {
  if (!iso) return "";
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "";
  const seconds = Math.max(0, Math.round((Date.now() - then) / 1000));
  if (seconds < 10) return "刚刚";
  if (seconds < 60) return `${seconds} 秒前`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.round(hours / 24)} 天前`;
}

/// How long a Run has been going, counted rather than described.
///
/// "35 分钟前" answers a different question from "it has been running for 35
/// minutes", and while something is in flight the second is the one being
/// asked. A finished Run is measured end to end; a live one is measured to
/// now, which is why this takes both ends.
export function elapsed(startedAt: string | null, until: string | null): string {
  if (!startedAt) return "";
  const start = Date.parse(startedAt);
  if (Number.isNaN(start)) return "";
  const end = until ? Date.parse(until) : Date.now();
  const seconds = Math.max(0, Math.round(((Number.isNaN(end) ? Date.now() : end) - start) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/// What the Run is doing, from the last event it wrote.
///
/// Every phrase here is something an event says. Nothing estimates progress:
/// "almost done thinking" is a sentence no event supports, and a screen that
/// says it is guessing at a model's interior. When the last event is one this
/// version does not know, that is said too -- with the type, so it can be
/// looked up rather than wondered about.
export function doing(lastEventType: string | null): string | null {
  switch (lastEventType) {
    case null:
      return null;
    case "run.started":
    case "model.provider.selected":
    case "run.resumed":
    case "run.restored":
      return "在想";
    case "model.output.delta":
    case "model.reasoning":
      return "在回答";
    case "model.tool_call":
    case "tool.execution.requested":
    case "tool.execution.started":
    case "tool.execution.progress":
      return "在用工具";
    case "approval.required":
      return "等你决定";
    // Its own phrase rather than sharing the one above. Both stop the Run on a
    // person, but the gate that appears a moment later says 等你回答, and a
    // status line that said 等你决定 first would be two words for one stop.
    // The window is real: with streaming the event arrives before the cursor
    // moves the boundary, so this line is what a person reads while the client
    // still believes the Run is running.
    case "mcp.input.required":
      return "等你回答";
    case "run.steer.applied":
      return "刚改了向";
    case "subagent.spawned":
    case "subagent.spawn.requested":
      return "在派子代理";
    case "model.usage":
    case "model.turn.completed":
      return "在收尾";
    default:
      return null;
  }
}

/// Cost in micro-dollars, as the runtime reports it. Rendered at the precision
/// the number actually has: a run that cost nothing says so.
export function costLabel(micros: number): string {
  if (micros === 0) return "—";
  const dollars = micros / 1_000_000;
  return dollars < 0.01 ? `<$0.01` : `$${dollars.toFixed(2)}`;
}

export function shortId(id: string): string {
  return id.slice(0, 8);
}
