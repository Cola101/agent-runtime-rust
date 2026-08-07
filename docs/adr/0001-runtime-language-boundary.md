# ADR-0001：Rust 数据面与 Java 控制面

## 状态

Accepted

## 决策

Agent Kernel、Worker、Model Gateway 和 Edge Node 使用 Rust；资源治理、IAM 接入、配额、审批、审计和 Console BFF 使用 Java 21 / Spring Boot。

## 结果

数据面获得可预测的内存与并发模型，控制面复用成熟企业治理生态；代价是必须维护 OpenAPI、Protobuf 和跨语言契约测试。

