# `workspace.write_text` 静默覆写：修好了（2026-08-19）

**修了。** 形状是「执行器记住这个 Run 读过什么，写入时把摘要带给工具，工具比对当前文件」——
也就是原来三个选项里的第 2 条。下面记的是它为什么是这条，以及守卫长什么样。

## 它曾经是什么

`write_text`（`apps/trusted-workspace-tool/src/main.rs`）只检查两件事：

```rust
let candidate = resolve_within_workspace(requested)?;   // 包含关系
if metadata.file_type().is_file() == false { … }        // 不能盖目录或设备节点
fs::write(&candidate, text.as_bytes())                  // 然后无条件写
```

**它不看文件当前的内容。** 所以：

1. agent 某个时刻读了文件，产出新内容
2. 人在审批卡上看到对比，批准
3. 这中间文件若被改过——编辑器、另一个 Run、另一个工具——写入**静默盖掉**

差距清单第 21 行「Workspace 冲突处理」写着「不得静默覆盖」。它当时就是。

而审批卡画出这次写入会改掉什么，让这件事**更**要紧而不是更安全：那份对比是客户端
**另读一次文件**算出来的，和 Runtime 真正落盘的那一刻之间隔着人做决定的整段时间。
批准的是一个可能已经过期的对比。

## 读了 Codex 之后，我推翻了自己的排序

原来这份文档倾向「2 或 3」，把 3（改成 patch/apply，Codex 的形状）说成同等甚至更好。
去读 Codex 真正怎么做的之后，这个说法不成立：

`codex-rs/apply-patch/src/seek_sequence.rs:12-40` —— 它拿 hunk 的**上下文行**去文件里找位置，
strictness 逐级放宽（精确 → 去尾部空白 → 去两端空白），找不到就在
`lib.rs:735` / `lib.rs:788` 报错。

**Codex 没有做整文件摘要比对。** 它的冲突检测是 **hunk 粒度**的：文件别处变了不影响这次写入，
只有这一处的上下文对不上才拒绝。

这直接反驳了我原来打算做的事——整文件摘要会在**文件无关的另一处**被改动时误拒。
那是一种「安全但很烦」的失败：人会学会绕过它。

那为什么最后还是做了整文件摘要？因为 `write_text` 的契约是**整文件替换**，
不是 patch。在整文件替换的语义下，「我读到的整个文件」就是这次写入的前置状态，
误拒的窗口和真实冲突的窗口是同一个。要拿到 hunk 粒度，得先把工具改成 patch/apply——
那是一个更大的改动，而且它自己还需要这一层保证不被绕过。
**所以第 2 条不是第 3 条的替代品，是它的前置。** 这一点原来那份排序没有说对。

## 修法：两半，各自都不够

**执行器这半**（`crates/tool-runtime/src/lib.rs`，`TrustedNativeExecutor`）：

```rust
seen: Arc<Mutex<HashMap<(Uuid, String), String>>>,   // (Run, 路径) → 摘要
const MAX_SEEN_FILES: usize = 4_096;                 // 有界，满了丢最旧的
```

- `workspace.read_text` / `workspace.write_text` **成功之后**记下 (Run, 路径) → 内容摘要
- 下一次 `workspace.write_text` 若这个 Run 读过这个路径，注入 `expected_sha256`
- 键里带 Run id：**一个 Run 的读，不构成另一个 Run 的约束**

**工具这半**（`trusted-workspace-tool`）：给了期望就在打开文件之前比对，不匹配返回
`file_changed_since_read`；**没给期望就照写**——工具自己没有记忆，记忆在执行器。

模型什么都不用改。这是选第 2 条而不是第 1 条的原因：第 1 条要模型自觉地先读、
记住摘要、再在写入时带上，一个不自觉的模型就绕过了整个保护。

## 守卫

`crates/tool-runtime/tests/write_after_read.rs`，5 条，其中 3 条是半边、2 条是端到端。

端到端那条跑**真实的工具二进制**和真实文件，顺序就是桌面会话真实产生的那个：
读 → 有人在旁边改 → 写。断言写被拒、且**手改的内容还在**。

各自破坏一次确认它们不是空的：

| 破坏什么 | 结果 |
| --- | --- |
| 执行器不再注入期望 | `2 passed; 1 failed` |
| 两个 Run 共用一份记录 | `another Run's read must not become this Run's expectation` 红 |
| 工具不再比对摘要 | 端到端那条红 |
| 全部复原 | `5 passed; 0 failed` |

还有一条 `a_run_that_writes_twice_is_not_refused_for_its_own_first_write`：
同一个 Run 连写两次不算和自己冲突（写入之后也记摘要，就是为了这个）。

## 删掉的那条测试

`trusted-workspace-tool/tests/write_text.rs::a_write_refuses_to_replace_a_file_that_changed_after_it_was_read`
原来标着 `#[ignore]`，记录的是「工具自己应当拒绝」。

**这条被删掉了，不是解封。** 按最后选的形状，工具没有记忆，不带期望的写入**本来就该成功**——
它编码的是一个已经被推翻的假设。留着它比没有更坏：下一个人会照它去改工具。
它要守的东西现在由端到端那条守着，而那条驱动的是完整的链路。

## 已经量到的

- `write_text` 三处检查（含新增的摘要比对），没有第四处。
- 审批卡的对比确实是客户端另读一次文件算的（`WriteReview` 用 `readFile`）。
- Codex 的 `seek_sequence` 上下文行寻址，读的是源码不是文档。
- 差距清单第 21 行的措辞不是我加的，它一开始就写着不得静默覆盖。

## 没有做的

- 没有把 `write_text` 改成 patch/apply。它仍然是整文件替换，
  于是编辑一个两千行的文件仍然要模型重写两千行——在 400k token 预算下付得起，仍然浪费。
  这件事和冲突检测已经**分开**了：现在做它是为了省 token，不是为了安全。
- 没有覆盖 `workspace.write_text` 之外的写入路径（进程会话里的 shell 重定向绕开这一层）。
  那是另一个边界，进程会话本来就在审批之后交出整台机器的一个 shell。
