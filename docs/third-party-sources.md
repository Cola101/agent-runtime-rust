# 第三方来源登记

本文件登记从参考项目**移植或借鉴**的设计与代码。逐条说明借了什么、没借什么，
以及为什么可以这样做。没有登记的移植一律视为违规。

## OpenAI Codex（Apache-2.0）

来源仓库：`https://github.com/openai/codex`
本地参考快照：`/Users/cola/Documents/Code/agent-source-research/codex`

### 1. macOS Seatbelt 容器化方案

- **本项目文件**：`runtime/crates/tool-runtime/src/seatbelt.rs`
- **参考文件**：`codex-rs/sandboxing/src/seatbelt.rs`、`codex-rs/sandboxing/src/seatbelt_base_policy.sbpl`
- **决策记录**：[ADR-0036](adr/0036-seatbelt-contained-trusted-tools-and-workspace-writes.md)、
  [ADR-0037](adr/0037-sensitive-path-read-containment.md)

**借鉴的（设计判断，非代码）**：

| 判断 | Codex 出处 | 为什么必须照做 |
| --- | --- | --- |
| `/usr/bin/sandbox-exec` 用绝对路径 | `seatbelt.rs:26-30` 注释明确说明理由 | 走 PATH 会让能写 PATH 的攻击者把沙箱换成空操作 |
| 路径作为 profile 参数传入，不做字符串插值 | `seatbelt.rs` 的 `(param "…")` 用法 | 含 profile 语法的路径否则能改写用来约束它的策略 |
| 敏感路径用「先全开、再挖洞」而非白名单 | `seatbelt.rs:687-695` 的 `/` 可读根 + `require-not`；`seatbelt.rs:467` 的 `(deny file-read* (regex …))` | 白名单会让 dyld 加载失败、进程 SIGABRT（本项目实测）；挖洞形态可行 |
| deny 段必须拼在 allow 段**之后** | `seatbelt.rs:741-747` 的 `policy_sections` 顺序 | SBPL 后规则覆盖先规则，顺序即语义 |
| `subpath` 与 `literal` 成对使用 | `seatbelt.rs:381-389` 及其注释 | 只有 `subpath` 时目录节点本身仍可达 |
| 路径进 profile 前先规范化 | `seatbelt.rs` 的 `normalize_path_for_sandbox` | macOS `/var`、`/tmp` 是指向 `/private` 的符号链接，未规范化会产生永不匹配的规则 |

**已更正的记载**：本表原有一行写「**不限制读** — `seatbelt.rs:683` 默认 `(allow file-read*)`」。
那是误读：`:683` 只在**没有**不可读根时成立，Codex 在有不可读根时确实限制读。详见 ADR-0037。

**没有借鉴的**：Codex 的网络策略文件、代理/Unix socket 参数化、可写根的
`protected_metadata_names`、`restricted_read_only_platform_defaults.sbpl`、glob→regex 转换
（本项目的受保护路径是固定目录，不需要 glob）。这些服务于 Codex 自己的 Tool 生态。

**没有复制任何源代码。** profile 文本与执行器集成均为本项目原创。若将来需要复制 Codex 源码，
必须保留其 Apache-2.0 头并在此另立条目。

## OpenClaw

来源仓库：`https://github.com/openclaw/openclaw`
本地参考快照：`/Users/cola/Documents/Code/agent-source-research/openclaw`

**当前仅作架构参考，未移植任何设计或代码。** 已阅读并在 ADR 与证据文档中作为对标提及的部分
（Skill 树摘要、session snapshot、tool dispatch、静态扫描、node-host、模型 fallback、优雅排空）
均未进入实现。**复制前必须先复核其具体许可证**，并在此登记。
