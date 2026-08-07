# 持久 Run steering 与原生恢复证据

日期：2026-08-02

## 已确认

- Java 控制面、PostgreSQL V24、Rust Worker/Checkpoint Recovery v3 和 Vue Console 已实现同一
  公开 Run 内的持久 steer；幂等键、Tenant/Application、attempt、Worker incarnation 和输入摘要均绑定。
- Worker 在应用 steer 时取消旧模型流，丢弃该 generation 的迟到输出，并在继续模型前先持久化新输入
  与回执。Worker 故障时，未完成命令由 Recovery v3 重绑 replacement attempt。
- 工作负载身份命令与签名 Token 共用一个显式 `issued_at`。原生门禁要求观察到成功续期，并在出现
  命令与 Token 时间不一致时失败。
- 已决定审批若在旧 Worker 收到前发生恢复，会把最新 approved/denied 记录与原审批版本、binding
  digest 一并重绑和重发给 replacement attempt。

## 最新真实原生主链

```text
动态 Provider / Skill / Agent 配置
→ 两次安全 Provider 切换
→ Tool Call 与审批 Checkpoint
→ 硬杀 Worker
→ 新 attempt / owner epoch 恢复
→ 工作负载身份续期
→ 浏览器审批
→ Tool 单次执行
→ 13 个 SSE 事件连续重放并成功终止
```

- Run：`ab58a4a1-12a2-4821-a8a8-dc018fa2cef5`
- 旧/新 attempt：`68965fdc-c612-4d78-9bda-49d5fc950a74` → `73c465bf-94c2-4b4c-b488-606bf305fd32`
- owner epoch：`1` → `2`
- 安全 Provider 切换：2 次
- 浏览器审批：通过
- 事件：`run.started` 到 `run.succeeded` 共 13 个关键事件
- 全部原生服务 RSS：`677040 KiB`（约 661.2 MiB）
- Docker、虚拟机、Kubernetes：未使用
- 外部下载：`127.0.0.1:10808`；所有回环服务直连
- 测试结束：项目进程、端口、日志、临时根目录、密钥与构建产物均清除；系统代理未停止

## 门禁

- Java：136 个测试，0 失败、0 错误，1 个可选 live 测试显式跳过。
- Rust：全工作区测试通过；格式和全 targets/features Clippy 零警告。
- Console：24 个 Vitest；TypeScript、ESLint 与 Vite build 通过。
- 原生恢复：Worker replacement、身份续期、审批重绑、Tool 单次执行和 SSE 终态通过。

## 尚未证明

- Console steer 已完成组件和 API 契约测试，但本次真实浏览器主链执行的是恢复后审批，不是中途 steer。
- 过期或永久拒绝的直接 steer envelope 尚无控制面终态负回执，命令可能保持 pending，仍需 Reconciler。
- 尚缺 steer 限速、队列压缩、富输入/附件，以及 1000 活跃 Run 压测。

## 对标判断

- 相比 Codex，本平台的数据库权威命令、跨 Worker Checkpoint-first 回执与多租户围栏更强；Codex 的
  富输入、交互式 wait/send/close 和 rollout 体验仍领先。
- 相比 OpenClaw，本平台保持同一 Run、预算和 Workspace 谱系；OpenClaw 的 generation 生命周期、
  steer 限速、restart reconciliation 与投递状态治理仍领先。
