import type { Link } from "../runtime";

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
