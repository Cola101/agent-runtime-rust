import type { Link } from "../runtime";

/// What the link is, in two lengths, from one place.
///
/// The rail's identity and the settings page's connection row each wrote their
/// own list and each stopped short: the row named four of the seven states and
/// drew a blank for the rest, and the rail funnelled everything it did not name
/// into 无宿主 -- which is wrong for exactly the state a fresh install starts
/// in, where the host is present and has no Provider yet.
///
/// A `switch` over the union with no `default`, so a new state is a compile
/// error here rather than a blank on screen. `LinkBanner` below keeps the long
/// sentences; these are the short ones, and there is no third list.
export function linkSummary(link: Link): { short: string; row: string } {
  switch (link.state) {
    case "live":
      return { short: "已连上", row: "已连上" };
    case "unreachable":
      return { short: "连不上", row: `连不上 —— ${link.reason}` };
    case "unconfigured":
      return { short: "未配置", row: "没有配置 —— 客户端不会去猜路径" };
    case "no-bridge":
      return { short: "无宿主", row: "不在桌面宿主里运行" };
    case "no-provider":
      return { short: "缺 Provider", row: "没有 Provider，所以没启动 Runtime —— 这不是故障" };
    case "no-binary":
      return { short: "缺 Runtime", row: "这个应用里没有 Runtime 可启动 —— 打包缺了东西" };
    case "start-failed":
      return {
        short: "起不来",
        row: link.said ? `Runtime 起不来 —— ${link.said}` : "Runtime 起不来，退出前什么都没说",
      };
  }
}

/// One banner, shown by every surface that renders runtime data.
///
/// The point is that a surface never draws an empty table and lets the person
/// conclude "there is nothing". Empty because there is nothing, empty because
/// no runtime is configured, and empty because the runtime is not answering
/// are three different facts and each one says which it is.
export function LinkBanner({ link }: { link: Link }) {
  if (link.state === "live") return null;
  if (link.state === "no-bridge") {
    return <div className="offline">这个界面不在桌面宿主里运行，连不上任何 Runtime。</div>;
  }
  if (link.state === "unconfigured") {
    return (
      <div className="offline">
        还没有告诉客户端 Runtime 在哪。设置 <code>RUNTIME_DESK_STATE_ROOT</code> 指向
        runtime-host 的 state root 后重开。这里不会去猜一个默认路径。
      </div>
    );
  }
  if (link.state === "no-provider") {
    return (
      <div className="offline">
        <b>还没配 Provider，所以没有启动 Runtime。</b>
        {" 去"}<b>设置</b>
        {" 里加一个，保存之后它会自己起来。这不是故障，是还差一步。"}
      </div>
    );
  }
  if (link.state === "no-binary") {
    return (
      <div className="offline">
        <b>这个应用里没有 Runtime，所以没什么可启动的。</b>
        {" 这是打包或下载缺了东西 —— 设置里改什么都不会有用。"}
      </div>
    );
  }
  if (link.state === "start-failed") {
    return (
      <div className="offline">
        <b>Runtime 起不来。</b>
        {link.said
          ? <> 它退出前说的是：<code>{link.said}</code></>
          : " 它退出前什么都没说 —— 这种情况少见，日志里可能有更多。"}
      </div>
    );
  }
  return (
    <div className="offline">
      <b>连不上 Runtime。</b> {link.socketPath} 没有回应 —— {link.reason}
    </div>
  );
}

/// Where the model's words come from. A stub provider produces genuine runs
/// with scripted text, and a client that cannot tell the two apart will one
/// day show one as the other.
export function ProvenanceNote({ text }: { text: string }) {
  return <div className="prov">{text}</div>;
}
