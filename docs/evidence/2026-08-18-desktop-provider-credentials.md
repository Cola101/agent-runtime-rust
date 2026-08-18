# 桌面 Provider 凭据与干净目录验收证据（2026-08-18）

## 复核事实

| 边界 | 修改前 | 风险 |
| --- | --- | --- |
| Provider 密钥 | 由启动应用的人设成环境变量 | 分发包不可能要求用户先设环境变量；密钥同时出现在 shell 历史和进程环境里 |
| 密钥存放 | 无 | 没有存放机制就只能写进配置文件，而配置文件会被同步、备份、粘进 issue |
| renderer 边界 | 无 provider 调用 | 一旦要做，最自然的写法就是加一个"读取当前密钥"回填表单——那正是渲染他人转录的面能够到的调用 |
| workspace 目录 | `AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT` 必填 | 同上，分发包起不来 |
| 协议名 | `FromStr` 收 `openai_compatible`，serde 要 `open_ai_compatible` | **同一个协议两种拼法**；人按其他三处配置路径的写法写，配置解析器拒绝，Runtime 在监听前就退出 |

## 实现结果

- `Credentials`：非密钥部分（id / 协议 / 地址 / 模型）写 `providers.json`；密钥进**登录钥匙串**，
  经 `/usr/bin/security`，值走 **stdin 而不是 argv** —— argv 是同用户任何进程都能从 `ps` 读到的。
- Runtime 启动时现读钥匙串，生成 routing 文件（**只写 `api_key_env` 变量名**）并把密钥放进**子进程环境**。
  这不是自创方案：`LocalModelRoutingFile` 本来就要求指名环境变量而不是携带密钥。
- 桥接**单向**：`providers` / `saveProvider` / `forgetProvider`，**没有任何返回密钥的调用**，
  且有一条测试直接扫 `preload.cjs` 断言不存在这样的方法名。
- `hasSecret` 从钥匙串回读，不看配置文件里的标记位 —— 声称有密钥的文件迟早会说错，而错误在 Runtime 启动时才炸。
- 应用自带 workspace 目录（`<userData>/workspace`）与受信工具路径（就是它刚 spawn 的那个二进制，
  两者永远不会是不同构建）。
- **内核**：`ProviderProtocol` 加 `serde(alias)` 接受 `FromStr` 那套拼法。纯增量 —— 序列化输出不变，
  已落盘或已进 digest 的内容一个字节不动。

## 可执行门禁

| 门禁 | 结果 |
| --- | --- |
| `vitest run`（desktop） | 44 passed |
| `provider_protocol`（runtime） | 2 passed；去掉 alias 后新测试变红 |
| 密钥不入配置文件 | 打破后变红（把 secret 塞进记录） |
| 密钥不入 routing 文件 | 打破后变红（写 `api_key` 而非 `api_key_env`） |
| 桥接无取回密钥的调用 | 打破后变红（加一个 `providerSecret`） |
| 协议名对着 Runtime 源码校验 | 打破 alias 后变红 |

## 干净目录验收

空 state root、**清空全部 `AGENT_RUNTIME_LOCAL_*`**、凭据只来自钥匙串：

```
runtime-desk: started runtime-host (pid 60324)
runtime-desk: local runtime at /var/folders/.../agent-runtime-host-395117d81bc1a05b.sock
runtime-desk: shell mounted, 5 surface(s) registered
runtime-desk: drew {"link":"live","runs":0,"waiting":0,"events":0,"policies":0,"sessions":0,"turns":0}
```

随后在该实例里跑通一轮真实 Session Turn（`turns: 1`，assistant 有回复），退出后：

```
runtime-desk: runtime stopped — 0 active and 0 queued before draining, 0 finished, 0 interrupted
runtime-host still running: 0
```

socket 路径落到临时目录，是长 state root 触发了客户端与 daemon 共有的回退——顺带证实了那条回退是活的。

## 全量门禁的一条失败（不是本轮引入）

`cargo test --workspace --all-targets --all-features`：**574 passed，1 failed**。

```
subagent_concurrency::close_cancels_only_the_targeted_asynchronous_child_and_reaps_its_stream
the close handshake did not complete: "the parent's next turn did not arrive within one second of the close"
```

隔离重跑 1/1 通过。与本轮无关：本轮的内核改动只是一个 `serde(alias)`，而这条测试的 config 是
在 Rust 里构造的，根本不走反序列化。

**但上一轮加的可读性修复在这里第一次付了息。** `docs/evidence/2026-08-18-session-acceptance-atomicity.md`
记过：当时两条失败路径都折叠成同一句话，无法区分是子流没被回收，还是父回合没在 1 秒内到达。
现在它说得很清楚——**子流回收成功了，超时的是父回合的继续**。

于是这条的性质比之前更明确：1 秒界量的是"Runtime 把 close 落盘并让父回合继续"的挂钟耗时，
在满载并行的机器上会被调度和磁盘写吃掉。**本轮仍未改它**——把界抬高正是规矩禁止的那种处理，
而"父回合继续得够快"是不是一条该守的产品门槛，是产品决定。留在原批次。

## 仍然没做

- 打包成可分发 artifact；现在仍是 `pnpm app`。
- workspace 目录选择器；现在只有默认值和一个开发用环境变量覆盖。
- 改完 Provider 后自动重启 Runtime：配置是在 Runtime 启动时读的，界面上写明"要重开"。
- 事件仍是 1.2 秒轮询，不是流式。
