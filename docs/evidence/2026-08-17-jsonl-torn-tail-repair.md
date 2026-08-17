# JSONL 崩溃尾部修复证据（2026-08-17）

## RED

一个真实 Run 先停在 Tool 审批，测试保留已有完整事件，再向 `events.jsonl` 追加一个没有换行的 JSON 前缀。
旧实现读取完整历史立即失败：

```text
StateRoot("EOF while parsing a string at line 1 column 17")
```

这证明单个未提交尾部会遮蔽全部已提交事件，且下一次审批恢复可能把新事件拼到半行后面。

## GREEN

- 完整回放和分页 Event Cursor 忽略唯一的 EOF 未换行尾部，保留此前全部事件。
- 审批恢复前，writer 截断到最后一个换行并同步，再继续 Tool、模型与唯一 `run.succeeded`；sequence 连续。
- 真实流式订阅在 Provider 请求阻塞时遇到半行，保存已提交字节偏移；Provider 释放后跨修复继续到 typed
  succeeded boundary，没有重送或 `CorruptLog`。
- `"{not-json}\n"` 这种已提交损坏仍然失败，不能被尾部修复掩盖。

## 已执行门禁

- `approval_flow`：9/9。
- `daemon_recovery`：9/9。
- `embedded_control`：9/9。
- `embedded_multi_tenant`：11/11。
- `embedded_recovery_all`：2/2。
- `grpc_invocation_watch`：3/3。
- `cargo clippy -p agent-runtime-host --all-targets --all-features -- -D warnings`：通过。

共 42 个相关测试通过；未启用 Docker、Java、数据库、NATS、Edge 或外部凭据。

## 结论边界

本轮证明进程级未完成 append 可以被安全丢弃并续写，不证明掉电 RPO、跨机器共享文件或任意磁盘损坏可自动
修复。总体 Rust Runtime 进度保持 70–75%。
