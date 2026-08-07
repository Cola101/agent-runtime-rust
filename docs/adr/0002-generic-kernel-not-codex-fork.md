# ADR-0002：新建通用 Kernel，不直接 fork Codex Core

## 状态

Accepted

## 决策

建立自有模型事件 IR 与 Agent Kernel。Codex 仅作为执行状态、Tool、审批、压缩和沙箱语义的参考或 Apache-2.0 模块来源。

## 原因

当前 Codex Core 的模型客户端与 OpenAI Responses 请求、传输和元数据深度耦合，直接 fork 会把通用 Provider 设计长期绑在其内部协议上。

