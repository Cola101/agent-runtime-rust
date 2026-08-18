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

const PROTOCOLS = [
  { id: "openai_compatible", label: "OpenAI 兼容 /chat/completions" },
  { id: "openai_responses", label: "OpenAI Responses" },
  { id: "anthropic_messages", label: "Anthropic Messages" },
];

/// Where a provider is configured, and the only place a secret is typed.
///
/// The secret goes one way. It is written to the login Keychain by the host
/// process and handed to the runtime as an environment variable on the child;
/// no bridge call returns it, and this page has nothing to render it into. What
/// it can say is whether one is on file and when it was set.
function Models() {
  const desk = useDesk();
  const [id, setId] = useState("");
  const [protocol, setProtocol] = useState(PROTOCOLS[0].id);
  const [endpoint, setEndpoint] = useState("");
  const [model, setModel] = useState("");
  const [secret, setSecret] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    setBusy(true);
    const failure = await desk.saveProvider({
      id: id.trim(), protocol, endpoint: endpoint.trim(), model: model.trim(),
      secret: secret === "" ? null : secret,
    });
    setError(failure);
    setBusy(false);
    if (!failure) {
      // Cleared on success rather than kept for convenience: a key sitting in
      // a form field is a key on screen.
      setSecret("");
      setId(""); setEndpoint(""); setModel("");
    }
  };

  return (
    <>
      {desk.providers.length > 0 && (
        <table className="rows">
          <thead>
            <tr><th>名字</th><th>协议</th><th>模型</th><th>密钥</th><th className="num" /></tr>
          </thead>
          <tbody>
            {desk.providers.map((provider) => (
              <tr key={provider.id}>
                <td className="p mono" title={provider.endpoint}>{provider.id}</td>
                <td>{PROTOCOLS.find((entry) => entry.id === provider.protocol)?.label ?? provider.protocol}</td>
                <td className="mono">{provider.model}</td>
                <td className={provider.hasSecret ? "" : "warn"}>
                  {provider.hasSecret
                    ? <>在钥匙串里{provider.secretSetAt && ` ・ ${since(provider.secretSetAt)}存的`}</>
                    : "缺密钥 —— Runtime 起不来"}
                </td>
                <td className="num">
                  <button type="button" className="flat" onClick={() => void desk.forgetProvider(provider.id)}>
                    删掉
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <div className="form">
        <label>名字<input value={id} onChange={(e) => setId(e.target.value)} placeholder="local-stub" /></label>
        <label>
          协议
          <select value={protocol} onChange={(e) => setProtocol(e.target.value)}>
            {PROTOCOLS.map((entry) => <option key={entry.id} value={entry.id}>{entry.label}</option>)}
          </select>
        </label>
        <label>地址<input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} placeholder="http://127.0.0.1:8080/v1/chat/completions" /></label>
        <label>模型<input value={model} onChange={(e) => setModel(e.target.value)} placeholder="stub" /></label>
        <label>
          密钥
          <input type="password" value={secret} onChange={(e) => setSecret(e.target.value)}
            placeholder="存进钥匙串，不写进配置文件" />
        </label>
        <button type="button" disabled={busy || !id.trim() || !endpoint.trim() || !model.trim()}
          onClick={() => void save()}>
          {busy ? "保存中" : "保存"}
        </button>
      </div>
      {error && <div className="err">{error}</div>}
      <p className="note">
        密钥存在登录钥匙串里，配置文件只留一个环境变量名 —— 这正是 runtime-host 路由配置要的形状。
        改完要重开应用才生效：Runtime 是在启动时读这份配置的。
      </p>
    </>
  );
}

/// Everything not yet reachable says exactly what is missing, and where the
/// gap is tracked. An empty section that only says "coming soon" tells a
/// person nothing they can act on.
const PENDING: Record<string, { need: string }> = {
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
        {section === "models" && <Models />}
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
    { id: "settings:models", title: "模型与密钥", hint: "密钥进钥匙串，不进配置文件" },
  ],
});
