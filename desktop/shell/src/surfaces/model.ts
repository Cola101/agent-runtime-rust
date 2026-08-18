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
  "model.provider.selected": "选定 Provider",
  "model.provider.failed": "Provider 失败",
  "model.tool_call": "模型请求调用工具",
  "model.turn.completed": "本轮结束",
  "approval.required": "需要你决定",
  "tool.denied": "已拒绝",
  "tool.result": "工具返回",
};

export function eventNote(type: string): string | null {
  return EVENT_NOTE[type] ?? null;
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
    case "mcp.input.required":
      return "等你决定";
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
