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
