# 原生 Steering 终结回执与单命令主链证据

日期：2026-08-02

## 结论

- `AGENT_RUNTIME_RUN_INPUT='…' make dev-run` 已从空本地状态启动 PostgreSQL、NATS、Java 控制面、
  Rust Model/Checkpoint Gateway、Runtime Worker 和 Vue Console，完成真实模型流并以
  `run.succeeded` 终止；随后 `supervisor clean` 删除全部项目进程、端口、日志、临时状态、测试密钥、
  `node_modules`、`target`、`dist` 和测试报告。
- 本地种子 Provider 不再只有空 ModelPolicy：配置器通过正式控制面 API 写入 API Key，复用生产
  `RSA-OAEP-256+A256GCM` 封装，再在 PostgreSQL 事务内原子切换固定开发策略的候选；明文不进入
  SQL、命令参数、日志或 Worker。
- 真实 Chrome 在首个模型流尚未结束时提交 steer。Worker 取消旧 HTTP 流，Checkpoint-first 保存
  新输入和 `run.steer.applied`，第二次请求只包含原输入与一次 steer 输入，最终仍是同一 Run。
- 过期或永久拒绝的 steer 会先发布绑定 tenant/run/attempt/worker/incarnation/digest 的
  `runtime.worker.run.steering.outcome.v1`，再终止 JetStream 投递；控制面只接受精确绑定回执并将
  `pending` 收敛为 `rejected`，伪造或跨实例回执不能修改账本。

## 真实运行证据

真实浏览器 Run：`5b088b63-f567-4941-b2d0-e6d91f57903a`

```text
Provider requests: 2
first_request_cancelled: true
steering_input_count: 1
stale_output_absent_from_next_request: true
events: run.started → model.output.delta → run.steer.applied
        → model.output.delta → model.usage → run.succeeded
resident set: 399504 KiB (约 390.1 MiB，目标 < 4 GiB)
```

浏览器截图：

- `2026-08-02-native-steering-running.png`
- `2026-08-02-native-steering-succeeded.png`

## 门禁

- Java：137 tests，0 failure/error，1 个独立凭证环境才运行的 live 测试显式跳过；PostgreSQL、
  JetStream 与 SteeringOutcome 消费链实际运行。
- Rust：全工作区测试、`cargo fmt --check`、全 feature/target Clippy 通过；新增负回执用例另以临时
  回环 NATS 实际运行，1/1 通过，未走跳过分支。
- Console：24 个 Vitest、ESLint、`vue-tsc`、Vite build、生产依赖审计和 Chrome 三视口 E2E 通过；
  真实原生 Chrome steer 门禁通过。
- 原生脚本：Provider API 封装、策略原子绑定、Supervisor 生命周期、下载代理传播、零容器命令图、
  单命令 Run 与完整清理均通过。

## 对标判断

- Codex `ff352fa` 的 `send_input(interrupt=true)`、Thread input queue 和 rich `UserInput` 在单机交互类型
  与 rollout 成熟度上仍领先；本平台的 PostgreSQL 命令账本、Worker incarnation 围栏、拒绝回执和
  Checkpoint-first 恢复更适合多租户多副本 PaaS。
- OpenClaw `58b4b943` 的 2 秒 steer 限速、generation/restart reconciliation、steering delivery lease、
  队列合并和完成结果投递仍更成熟；本平台保持同一 Run/预算/事件序列，并对负回执做数据库级精确绑定，
  审计与跨 Worker 安全边界更强。
