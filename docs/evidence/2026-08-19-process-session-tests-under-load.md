# `interrupt_reaches_...` 在干净门禁里失败（2026-08-19）

**结论先写在前面：机制还没找到。** 两个假设都被自己的测量推翻了。
这份记录的是量到的数、排除掉的东西，以及下一步该怎么问。

## 一、先说清楚三次不作数的门禁

前三次全量的失败**都是我自己造成的**：

| 轮次 | 失败项 | 我当时在同时跑什么 |
| --- | --- | --- |
| ws.log | `interrupt_reaches_...` | 正在编辑它所属的源码树，另开了两个 `cargo test -p agent-runtime-host` |
| ws2.log | `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded` | `vite build` + `vitest run` |
| ws3.log | 上面两条 | 两次 `vitest run` |

ws2 还额外说明：`cargo test` 默认 fail-fast，那轮只跑了 411 条就停了，
而我第一次读日志时**它根本还没跑完**——据此报出的 "742 passed, 0 failed" 无效。
之后一律加 `--no-fail-fast`，读之前先确认日志里有 `EXIT=`。

## 二、干净门禁（ws4，机器上没有别的东西）

**883 passed, 3 failed。**

| 失败项 | 消息 |
| --- | --- |
| `interrupt_reaches_the_registered_process_group_and_converges_terminal` | `SIGINT never reached a terminal process-session state` |
| `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded` | `started.elapsed() < 180s` |
| `a_network_caller_starts_continues_forks_and_reads_a_real_session` | `Unavailable: Session storage is unavailable`（旧账，已知未归因） |

第一条在干净环境下也失败，**所以它跟我制造的负载无关**，值得查。

## 三、量到的数

给那条测试的轮询循环加了计时（临时改动，已还原）：

| 条件 | 次数 | 从 SIGINT 到终局态 |
| --- | --- | --- |
| 单独跑这一条 | 10 | 12–24 ms，**0–1 次轮询** |
| 整个二进制（18 条并行） | 25 | 最大 23 ms，**0 次卡住** |
| 整个二进制顺序连跑 | 12 | 12/12 通过 |

测试的预算是 200 × 10ms ≈ 2 秒。正常值离它**差两个数量级**。

**所以门禁里的失败是「卡住」，不是「慢」。** 这个区分很重要：
它把"界限偏紧"这条路直接排除了——不存在可以靠调参数解决的余量问题。

## 四、两个假设，都被推翻

### 假设一：`CLOSE_GRACE` 让发布终局态最坏要 1.5 秒

读代码得到的：唯一会把退出写进 manifest 的是 `start` 里 `tokio::spawn` 的那个任务，
它先 `.await` `terminate_resource_identity`，**再** `finalize_exited_manifest`
（`process_session.rs:2205` 起）。而 `terminate_process_group` 在最坏情况下要等
`CLOSE_GRACE`（500ms）再加最多 100 × 10ms 等 identity 释放。

这个顺序本身是**对的**，注释也写明了理由：identity 锁要等所有继承它的后代退出才释放，
终局态不能先于它发布。

**但测量否掉了它作为本次失败的解释**：正常路径上进程组已空，
`signal_group_with_identity_fence` 直接返回 false，整条链路 12–23ms 走完。
1.5 秒是理论上界，不是这里发生的事。

### 假设二：多个 current-thread runtime 抢 SIGCHLD，`child.wait()` 漏掉唤醒

一个进程里有 ~18 个 `#[tokio::test]` 各自的 runtime。若某个 runtime 的
`child.wait()` 漏掉一次 SIGCHLD，reaper 任务就一直停着，manifest 永远停在 `Running`——
**这和失败时观察到的状态完全吻合**（`state: Running, pid: Some(...)`）。

**但这个假设预测：单独跑这一条时 SIGCHLD 流量最少，应该更容易卡。**
实测单独跑 10 次全部 12–24ms。**预测反了，假设被削弱。**

## 五、已经看清、但还不构成诊断的一点

`interact(Poll)` **不检查存活**。`ensure_attached_identity` 只在 Interrupt / Close
路径上跑。也就是说：如果那个 reaper 任务因为任何原因没能发布终局态，
Poll 会**永远报 Running**，而它本可以看出 leader 已经不在了。

这正是失败时的现象。但要注意：不能简单让 Poll 去发布终局态——
第四节那条 identity 规则说了，有后代还持着 identity 时不能发布。
所以这里是一处**真实的观测缺口**，不是一个可以顺手改的地方。

## 六、没有做的事

- 没有延长 timeout。测量已经证明这条路答非所问：差的是两个数量级，不是余量。
- 没有串行化测试，没有删断言。
- **没有把它记成 flaky 放过。**
- 也没有改产品代码——我还说不出它为什么卡，改了就是猜。

## 七、下一步该问什么

1. 在门禁里复现并抓现场：给这条测试加上"失败时把 manifest 原文和
   `process_alive(pid)` 一起打出来"。**leader 到底还活着没有**，
   一句话就能把"进程没死"和"死了但没被发布"分开——这两者要查的地方完全不同。
2. 若是后者，问题在 reaper 任务；`interact(Poll)` 不检查存活让它从"一次延迟"
   变成"永久错误"，这一点无论根因是什么都成立。
3. 另外两条失败是独立的账：一条是墙钟资源界限（`elapsed() < 180s`），
   一条是长期挂着的 `Session storage is unavailable`（被客户端契约边界脱敏，
   要查得先在 host 侧留未脱敏诊断，而不是放宽边界）。
