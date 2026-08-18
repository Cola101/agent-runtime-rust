import { useState } from "react";
import { register } from "./registry";
import { effectLabel, sandboxLabel, shortId, since } from "./model";
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
      <div className="wide">
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
      </div>
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
        <div className="wide">
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
        </div>
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
      <Restart />
      <p className="note">
        密钥存在登录钥匙串里，配置文件只留一个环境变量名 —— 这正是 runtime-host 路由配置要的形状。
        保存后会立刻尝试启动 Runtime。已经在跑的 Runtime <b>是在启动时读这份配置的</b>，
        所以换了 Provider 要重启它才算数 —— 重启的是 Runtime，不是这个应用。
      </p>
    </>
  );
}

/// Restarts the runtime so a provider change takes effect.
///
/// Separate from 保存 rather than folded into it. A restart drains whatever is
/// in flight, and doing that silently because someone edited a field is a
/// decision the person did not make. What it costs is said before it is asked
/// for, and what it found is said afterwards.
function Restart() {
  const desk = useDesk();
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<string | null>(null);
  const [refused, setRefused] = useState(false);

  const run = async () => {
    setBusy(true);
    setSaid(null);
    setRefused(false);
    const reply = await window.desk!.runtime!.restart();
    setBusy(false);
    if (!reply.ok) {
      setRefused(true);
      setSaid(reply.error);
      return;
    }
    // `ok` is about the request reaching the host. Whether it restarted is a
    // separate field, and reading only the first would report a restart that
    // did not happen -- after which the next Run is still answered by the old
    // provider, with nothing on screen to say so.
    if (!reply.value.restarted) {
      setRefused(true);
      setSaid("这个 Runtime 不是这个应用启动的，只能停掉它自己启动的那个。要换配置就退出应用再打开。");
      return;
    }
    const report = reply.value.report ?? {};
    const active = Number(report.active_runs ?? 0);
    const queued = Number(report.queued_runs ?? 0);
    const cut = active > 0 || queued > 0
      ? `停的时候还有 ${active} 个在跑、${queued} 个排队，它们是被这次重启打断的。`
      : "停的时候没有 Run 在跑。";
    setSaid(
      `${cut}${reply.value.escalated ? "它没有按时退出，是被强制结束的：下次启动要从 Checkpoint 恢复。" : ""}`,
    );
    desk.refresh();
  };

  return (
    <div className="restart">
      <button type="button" onClick={() => void run()} disabled={busy}>
        {busy ? "重启中" : "重启 Runtime"}
      </button>
      {said && <span className={refused ? "err" : "note-inline"}>{said}</span>}
    </div>
  );
}

/// Where local MCP servers are configured.
///
/// What this page may claim is narrower than it looks. The list is this app's
/// own configuration file, and the runtime reads that file once, at startup
/// (`load_mcp_servers`). Whether a server then came up is
/// `McpServerDiscoveryStatus`, which stays inside the runtime process — the
/// local socket has no call that returns it. So a configured server is not a
/// running server, and this page says which of the two it is showing.
///
/// The one exception is the whole reason 必需 is offered: a required server
/// that fails discovery fails the Run, and names itself in the durable log.
function Mcp() {
  const desk = useDesk();
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [cwd, setCwd] = useState("");
  const [tools, setTools] = useState("");
  const [required, setRequired] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const applied = desk.mcp.applied;
  const live = applied === null ? null : new Set(applied.map((entry) => entry.digest));
  // Servers the runtime was started with that are no longer configured. It is
  // still running them, and a list that quietly dropped them would be saying
  // the removal had taken effect.
  const configured = new Set(desk.mcp.servers.map((server) => server.name));
  const leftover = (applied ?? []).filter((entry) => !configured.has(entry.name));

  const save = async () => {
    setBusy(true);
    const failure = await desk.saveMcpServer({
      name: name.trim(),
      command: command.trim(),
      // One per line: an MCP server's arguments are usually paths, and a
      // space-separated field cannot carry one with a space in it.
      args: args.split("\n").map((arg) => arg.trim()).filter(Boolean),
      cwd: cwd.trim() === "" ? null : cwd.trim(),
      // Split on anything a tool name cannot contain, so nothing is lost.
      toolNames: tools.split(/[\s,]+/).filter(Boolean),
      required,
    });
    setError(failure);
    setBusy(false);
    if (!failure) {
      setName(""); setCommand(""); setArgs(""); setCwd(""); setTools(""); setRequired(false);
    }
  };

  return (
    <>
      {desk.mcp.servers.length > 0 && (
        <div className="wide">
          <table className="rows">
            <thead>
              <tr>
                <th>名字</th><th>命令</th><th>工具</th><th>授权</th><th>必需</th>
                <th>配置</th><th className="num" />
              </tr>
            </thead>
            <tbody>
              {desk.mcp.servers.map((server) => (
                <tr key={server.name}>
                  <td className="p mono">{server.name}</td>
                  <td className="mono" title={[server.command, ...server.args].join(" ")}>
                    {server.command.split("/").pop()}
                  </td>
                  <td className="mono">{server.toolNames.join("・")}</td>
                  <td className="mono">{server.scope}</td>
                  <td>{server.required ? "必需" : "可选"}</td>
                  <td className={live?.has(server.digest) ? "" : "warn"}>
                    {live === null
                      ? "不知道有没有生效"
                      : live.has(server.digest) ? "Runtime 启动时拿到了" : "还没生效"}
                  </td>
                  <td className="num">
                    <button type="button" className="flat"
                      onClick={() => void desk.forgetMcpServer(server.name)}>
                      删掉
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {applied === null && (
        <p className="note">
          这个 Runtime 不是这个应用启动的（或者还没启动），它读的是哪一份 MCP 配置，这个应用无从知道 ——
          所以上面那一列只说「不知道」。
        </p>
      )}
      {leftover.length > 0 && (
        <p className="note">
          Runtime 启动时还带着已经删掉的 {leftover.map((entry) => entry.name).join("、")}，
          它现在仍在跑。退出应用再打开才会真的没有。
        </p>
      )}

      {desk.mcpFailures.length > 0 && (
        <>
          <div className="note"><span>这些是 Run 日志里真的报过起不来的服务</span></div>
          <div className="wide">
            <table className="rows">
              <thead>
                <tr><th>服务</th><th>Run</th><th className="num">时间</th></tr>
              </thead>
              <tbody>
                {desk.mcpFailures.map((failure) => (
                  <tr key={`${failure.runId}-${failure.server}`}>
                    <td className="p mono warn">{failure.server}</td>
                    <td className="mono">{shortId(failure.runId)}</td>
                    <td className="num" title={failure.at}>{since(failure.at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}

      <div className="form">
        <label>名字<input value={name} onChange={(e) => setName(e.target.value)} placeholder="filesystem" /></label>
        <label>命令<input value={command} onChange={(e) => setCommand(e.target.value)} placeholder="/opt/homebrew/bin/npx" /></label>
        <label>
          参数
          <textarea value={args} onChange={(e) => setArgs(e.target.value)}
            placeholder={"一行一个\n-y\n@modelcontextprotocol/server-filesystem"} />
        </label>
        <label>工作目录<input value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="留空就不设" /></label>
        <label>工具名<input value={tools} onChange={(e) => setTools(e.target.value)} placeholder="read_file write_file" /></label>
        <label>
          必需
          <input type="checkbox" checked={required} onChange={(e) => setRequired(e.target.checked)} />
        </label>
        <button type="button"
          disabled={busy || !name.trim() || !command.trim() || !tools.trim()}
          onClick={() => void save()}>
          {busy ? "保存中" : "保存"}
        </button>
      </div>
      {error && <div className="err">{error}</div>}

      <p className="note">
        配置了不等于起来了。某个服务有没有起来，是 Runtime 进程里的 <code>McpServerDiscoveryStatus</code>，
        本地 socket 没有任何调用能读到它 —— 所以这一页只说配了什么，不说它们活着没有。
        标成「必需」是例外：它起不来时 Run 会直接失败，日志里是 <code>run.failed</code> ・
        <code>required_mcp_unavailable</code> 并点名，那是这台机器上唯一看得见的地方。
      </p>
      <p className="note">
        工具名要自己写：Runtime 拿它当白名单，发现阶段只会收窄，不会加上没写的工具。
        环境变量这里不收 —— stdio 服务的环境只能写进配置文件，而密钥不写进配置文件是上面那条规矩。
      </p>
      <p className="note">
        Runtime 只在启动时读这份配置。改完要退出应用再打开，新的服务才会真的在。
      </p>
    </>
  );
}

/// Everything not yet reachable says exactly what is missing, and where the
/// gap is tracked. An empty section that only says "coming soon" tells a
/// person nothing they can act on.
const PENDING: Record<string, { need: string }> = {
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
        {section === "mcp" && <Mcp />}
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
    { id: "settings:mcp", title: "MCP 服务", hint: "配了什么；起没起来读不到" },
  ],
});
