# 2026-08-15 MCP 能力目录与反向权限证据

## 已验证

| 场景 | 当前结果 |
| --- | --- |
| HTTP Resources/Prompts-only | 2025 Server 初始化成功，返回空 Tool 目录，不发送 `tools/list` |
| stdio Resources/Prompts-only | 真实 `/bin/sh` 子进程初始化成功，能力精确为 Resources+Prompts，无 list marker |
| 认证 Gateway→Worker | schema 2 能力穿过真实 TCP MCP、Gateway、带 workload token 的 gRPC 与 Worker |
| 目录摘要 | Tools-only 与 Tools+Resources 产生不同 SHA-256；Tool schema 继续参与摘要 |
| wire 兼容 | schema 1 只推断 Tools；schema 2 保留 Resources/Prompts |
| fail-closed | 未知 schema、未知 capability、空 capability、无 Tools 却有 Tool rows 均拒绝 |
| 调用时降权 | 新调用会话若不再声明 Tools，普通与 lifecycle 调用都在副作用前返回目录漂移 |
| 反向权限回归 | 既有 HTTP/stdio Roots 与 Sampling request 仍按 request ID 返回 `-32601` |

专项实跑：

```text
agent-model-gateway lib                         16 passed
resource/prompt-only HTTP federation            1 passed
resource/prompt-only stdio process               1 passed
authenticated Gateway -> Worker directory        1 passed
Worker directory schema contract                 3 passed
fresh-session Tool capability fence              2 passed
```

stdio 专项二次构建只用 0.37 秒、行为执行 0.03 秒，证明保留 `target` 增量缓存可复用；本轮未执行
`cargo clean`。

最终 Rust 全工作区精确列出 708 项：702 通过、0 失败、6 个外部 live 用例显式忽略。全量行为门禁使用
`--test-threads=8`，匹配 M1 Pro 16GB 的本地资源边界。Clippy
workspace/all-targets/all-features `-D warnings`、`cargo fmt --check` 与 `git diff --check` 全绿。全量 test 与
all-features 静态检查后 `runtime/target` 为 14 GiB；它是可复用缓存，按当前本地开发策略保留且不进入 Git。

全量门禁还暴露了一个与 MCP 无关但会污染验收的 PTY 启动边界错误：外部 supervisor 在任何子进程创建前
失败时，旧代码会留下 `Starting/unprepared` 并错误返回 `indeterminate`。新增真实不可用 supervisor RED
用例后，Runtime 只在精确 `unprepared` 证据下持久收敛为 `Terminated/start_failed`；一旦状态越过
`prepared` 仍保持 `indeterminate`，不会为通过测试而降低副作用安全性。默认高并发曾另出现一次 macOS
进程组 close 的 `EPERM`；专项重跑及 8 线程 Tool/全工作区门禁均通过，继续作为负载稳定性风险跟踪。

## 尚未验证或明确未实现

- capability 可见性阶段当时尚无稳定读取 API；该缺口已由后续 ADR-0116 与
  `2026-08-15-bounded-mcp-resources-and-prompts.md` 关闭。
- OAuth onboarding、PKCE、refresh/persist、租户 credential-store indirection 和撤销尚未实现；当前只有 sealed
  credential envelope 的静态 egress 边界。
- schema 2 的滚动升级只在 Worker parser 层验证；没有旧 Gateway 二进制与新 Worker 的双版本进程验收。
- 真实外部 MCP Server、真实 OAuth Provider、长稳 session 与 capability-change notification 尚未实跑。

## 对标判断

- 对 Codex：本项目已对齐“服务端表面不等于反向权限”，并把支持的 capability 纳入多租户 Run 冻结摘要；
  Codex 已有实际 Resources API 与成熟 OAuth refresh/persist，明显领先。
- 对 OpenClaw：本项目已对齐默认空 client capabilities，以及 Resources/Prompts 不依赖 Tool capability；
  OpenClaw 已有分页 list/read/get 与 Session runtime 生命周期，能力广度领先。
- 本项目的差异只在窄面成立：Gateway 凭证域、完整 workload identity 和目录 digest 更适合共享多租户 Runtime；
  这不构成对完整 MCP 产品面的领先声明。
