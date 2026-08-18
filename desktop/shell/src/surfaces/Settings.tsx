import { useState } from "react";
import { register } from "./registry";
import { effectLabel, sandboxLabel, since } from "./model";
import { useDesk } from "../desk";

/// Which of these actually reach the runtime, and which only configure this
/// client. Saying so on the page matters: a person who changes a theme and a
/// person who changes an approval policy are doing very different things, and
/// nothing in a settings list otherwise distinguishes them.
const SECTIONS = [
  { id: "connection", title: "连接", scope: "runtime" },
  { id: "tools", title: "工具与授权", scope: "runtime" },
  { id: "models", title: "模型", scope: "runtime" },
  { id: "mcp", title: "MCP 服务", scope: "runtime" },
  { id: "appearance", title: "外观", scope: "client" },
  { id: "advanced", title: "高级", scope: "client" },
] as const;

function Connection() {
  const desk = useDesk();
  const link = desk.link;
  return (
    <dl className="facts-list">
      <dt>传输</dt>
      <dd>本机 Unix socket（runtime-host 的本地适配器）</dd>
      <dt>状态</dt>
      <dd>
        {link.state === "live" && "已连上"}
        {link.state === "unreachable" && `连不上 —— ${link.reason}`}
        {link.state === "unconfigured" && "没有配置 —— 客户端不会去猜路径"}
        {link.state === "no-bridge" && "不在桌面宿主里运行"}
      </dd>
      <dt>socket</dt>
      <dd className="mono">
        {link.state === "live" || link.state === "unreachable" ? link.socketPath : "—"}
      </dd>
      <dt>本次列出的 Run</dt>
      <dd>{desk.listedAt === null ? "还没查" : `${desk.runs.length} 个`}</dd>
      <p className="note">
        换一个 Runtime 要改环境变量 <code>RUNTIME_DESK_STATE_ROOT</code> 再重开。
        本地适配器没有提供切换连接的调用，界面上放一个假的切换按钮不如没有。
      </p>
    </dl>
  );
}

/// Observed policy, not configured policy.
///
/// The local adapter has no call for reading or writing the per-tool policy
/// table, so this page cannot show "your settings". What it can show is the
/// snapshot the runtime froze into calls that actually happened, which is the
/// thing that actually governed them.
function Tools() {
  const desk = useDesk();
  if (desk.policies.length === 0) {
    return (
      <div className="empty">
        还没有见过任何工具调用。
        <span className="sub">
          这一页显示的是 Runtime 真正冻结进某次调用的策略快照，不是一份可编辑的设置表 ——
          本地适配器还没有读写策略表的调用。
        </span>
      </div>
    );
  }
  return (
    <>
      <table className="rows">
        <thead>
          <tr><th>工具</th><th>副作用</th><th>隔离</th><th>是否询问</th><th className="num">最近一次</th></tr>
        </thead>
        <tbody>
          {desk.policies.map((policy) => (
            <tr key={policy.toolName}>
              <td className="p mono">{policy.toolName}</td>
              <td>{effectLabel(policy.effect)}</td>
              <td className={policy.sandbox === "trusted_native" ? "warn" : ""}>
                {sandboxLabel(policy.sandbox)}
              </td>
              <td>{policy.approval === "ask" ? "每次问" : policy.approval}</td>
              <td className="num" title={policy.seenAt}>{since(policy.seenAt)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="note">
        这是 Runtime 冻结进某一次调用的策略快照，不是一份可编辑的设置。
        已经在跑的 Run 保留它被准入时的策略，所以恢复会重放同样的决定。
      </p>
    </>
  );
}

/// Everything not yet reachable says exactly what is missing, and where the
/// gap is tracked. An empty section that only says "coming soon" tells a
/// person nothing they can act on.
const PENDING: Record<string, { need: string }> = {
  models: { need: "本地适配器没有读取路由配置或切换模型的调用；模型现在由 runtime-host 的环境变量决定。" },
  mcp: { need: "MCP 服务清单在 Runtime 侧已有读取契约（ADR-0116/0117），但本地适配器还没有暴露出来。" },
  appearance: { need: "只影响这个客户端。还没做。" },
  advanced: { need: "只影响这个客户端。还没做。" },
};

function SettingsView() {
  const [section, setSection] = useState<string>("connection");
  const current = SECTIONS.find((entry) => entry.id === section);
  return (
    <div className="pane split">
      <nav className="idx">
        {SECTIONS.map((entry) => (
          <button
            key={entry.id}
            type="button"
            className={entry.id === section ? "on" : ""}
            onClick={() => setSection(entry.id)}
          >
            {entry.title}
            {entry.scope === "runtime" && <span className="live">Runtime</span>}
          </button>
        ))}
      </nav>
      <div className="body">
        {section === "connection" && <Connection />}
        {section === "tools" && <Tools />}
        {PENDING[section] && (
          <div className="empty">
            {current?.title} —— 还没接。
            <span className="sub">{PENDING[section].need}</span>
            <span className="sub">缺口清单在 <code>docs/desktop-ui-gap.md</code>。</span>
          </div>
        )}
      </div>
    </div>
  );
}

register({
  id: "settings",
  label: "设置",
  group: "setup",
  view: SettingsView,
  toolbar: () => <b>设置</b>,
  commands: [
    { id: "settings:connection", title: "连接设置", hint: "连的是哪个 Runtime" },
    { id: "settings:tools", title: "工具与授权", hint: "实际生效过的策略快照" },
  ],
});
