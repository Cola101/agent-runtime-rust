# 单一 PTY Owner、握手与有界 Attach 证据（2026-08-11）

## RED / GREEN

1. 无外部 supervisor 的 `tty=true` 首次真实创建了 PTY；删除 Host-owned 新建路径后，在持久化 session 和
   spawn child 前返回配置错误，普通 pipe session 不受影响。
2. 真实 Unix socket 上的旧 `Pong` 响应首次被接受；协议 v2 `Hello` 加入精确版本和五项必需能力后，
   旧端点被拒绝且不会被删除或接管。
3. supervisor 首次没有持久生命周期；现在 owner-only 摘要文件可观察 clean idle stop。SIGKILL 后启动的
   replacement 明确记录旧 generation、PID 与 `clean_shutdown=false`。
4. 真实 Agent Loop 首次无法激活 `process.attach`；注册 Pure Tool 后，模型在原 Host 与 replacement Host
   中都能读取指定字节数的输出尾部，并观察起止游标和截断标志。

## 恢复与安全门禁

- PTY master 只有 supervisor 一个新建 owner；旧 Host-owned compatibility path 已删除。
- 能响应但协议或能力不兼容的 socket fail-closed；仍活着的前任生命周期阻止双 supervisor 接管。
- 生命周期文件记录 ready/stopping/stopped、活跃会话数、退出原因和前任清洁度，权限保持当前 UID 私有。
- `process.attach` 分别限制 stdout/stderr 源字节窗口，不推进 poll cursor，不修改 Manifest，也不重新执行 Tool。
- replacement Host 真实完成 write→poll→attach→close；supervisor SIGKILL 后仍回收原组并形成 indeterminate。

## 质量门禁

- Rust 全工作区共 583 项：577 通过、0 失败、6 个外部 live 用例显式忽略。
- `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 与差异门禁通过。
- 最终检查无 Cargo、PTY supervisor 或测试 child 残留；临时 Cargo target、Graphify 输出、PTY socket 和测试
  临时目录均在复核后删除。

## 对标快照

- Codex `ff352fab6209`：`codex-rs/utils/pty` 已有 Unix/Windows PTY、resize、进程组及有界
  mpsc/broadcast，`unified_exec` 已形成 yield/write 产品语义；本平台不宣称总体追平。
- OpenClaw `58b4b9430457`：Gateway Terminal 已有 bounded output ring、attach/text、owner/viewer、
  pause/resume 和 WebSocket 高低水位；本平台本轮只对齐有界 attach，并保留跨 Host durable owner 的窄优势。
- 下一项固定为 supervisor 输出高低水位、pause/resume 与慢消费者恢复门禁，不进入 GUI 或控制面。
