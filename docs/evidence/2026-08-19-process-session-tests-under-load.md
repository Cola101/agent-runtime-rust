# `interrupt_reaches_...`：找到了，是产品缺陷（2026-08-19）

**结论：`process.interrupt` 在「runtime-host 以后台作业启动」时是静默空操作。**
不是负载，不是竞态，不是界限偏紧。已修，守卫先红后绿。

## 一句话机制

SIG_IGN 的处置**跨 fork 且跨 exec 继承**（handler 不会，SIG_IGN 会）。POSIX 规定
非交互 shell 把后台作业的 SIGINT/SIGQUIT 设成 SIG_IGN。于是：

```
bash -c '... &'  →  cargo  →  测试二进制  →  /bin/sh 会话 leader
        ↑ SIGINT=SIG_IGN，一路继承到最后一个
```

`killpg(pgid, SIGINT)` **成功返回**，manifest 记下 `interrupt_intent`，工具报告信号已发，
而进程照跑不误。调用方拿不到任何信号说它没起作用。

## 复现（完全确定，不是概率）

| 起法 | 结果 |
| --- | --- |
| 前台 | 通过，0.85 秒 |
| `nohup bash -c '...' &`（门禁就是这么起的） | 失败，`leader_alive=Some(true)` |

前台连跑 37 次（单条 10 次、整个二进制 25 次、顺序 12 次）**一次没失败**；
用 `&` 起**必然失败**。五次全量门禁里这条测试挂了五次，全是我用 `&` 起的。

## 是怎么问出来的

前一版这份文档的结论是「机制还没找到」，并且列了两个假设，**两个都被自己的测量推翻**：

- **`CLOSE_GRACE` 让发布终局态最坏要 1.5 秒**：上界是真的，那个顺序也是对的
  （有后代还持着 identity 时不能先发布终局态）。但实测正常路径 12–23ms 走完——
  进程组已空，`signal_group_with_identity_fence` 直接返回。**1.5 秒不是这里发生的事。**
- **多个 current-thread runtime 抢 SIGCHLD**：和观察到的状态吻合，但它预测
  「单独跑时更容易卡」，而单独跑 10 次全部 12–24ms。**预测反了。**

把两个都否掉之后，剩下的只有一个没被回答的问题：**leader 到底还活着没有。**
原来的失败信息一个字都不说。加上 `leader_alive` 之后，下一次干净门禁直接给出
`leader_alive=Some(true)` —— 一个数字就把「死了但没被发布」整条路线砍掉了。

**这条诊断是整件事的转折点。** 在它之前我读了三处源码、提了两个都错的假设；
在它之后，一次失败就够了。

## 修法

会话 leader 的 `pre_exec` 里把 SIGINT / SIGQUIT 重置为 `SIG_DFL`
（`process_session.rs`，和 rlimit 设置在同一个闭包里）。理由写在代码旁边：
**leader 可被中断是这个 manager 承诺的事，而它上游是谁、怎么起的，它问不到。**

守卫不依赖启动方式：测试**自己**把 SIGINT 设成 SIG_IGN，那正是后台 shell 做的事，
并在离开时用 `Drop` 还原，免得一条测试替这个二进制里其余的测试做了决定。
撤掉修复 → 红（`an inherited SIG_IGN survived into the session leader`）；
恢复 → 绿；再用门禁那个 `&` 的起法跑整个二进制 → 19/19 绿。

## 附：我自己弄脏的前三次门禁（保留，因为它记的是当时的判断）

| 轮次 | 失败项 | 我当时在同时跑什么 |
| --- | --- | --- |
| ws.log | `interrupt_reaches_...` | 正在编辑它所属的源码树，另开了两个 `cargo test -p agent-runtime-host` |
| ws2.log | `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded` | `vite build` + `vitest run` |
| ws3.log | 上面两条 | 两次 `vitest run` |

ws2 还额外说明：`cargo test` 默认 fail-fast，那轮只跑了 411 条就停了，
而我第一次读日志时**它根本还没跑完**——据此报出的 "742 passed, 0 failed" 无效。
之后一律加 `--no-fail-fast`，读之前先确认日志里有 `EXIT=`。

## 干净门禁的最新一次（含中断修复）

**887 passed, 2 failed。** `interrupt_reaches_...` 已经消失——而且那一次门禁是
**故意用 `&` 起的**，正是原来必然让它失败的方式。

### `one_thousand_runs_...`：指标行现在在失败时也打印

把 `println!` 挪到断言之前的目的就在这里：

```
elapsed_ms=198099        ← 界限 180s，超 10%
recovery_scan_ms=763     ← 界限 2s
fd_baseline=42 fd_peak=42 fd_final=12
rss 18.5MB → 27.7MB      ← 界限 512MB
```

对照单独跑的 **161.7 秒**：门禁自身的负载让它慢了 **22%**。而这条测试名字里写的那几条
资源界限，依然全部以 30~90 倍余量通过。**紧的只有那条墙钟兜底，而它不是这条测试的主题。**

### `a_network_caller_...`：我加的 host 侧诊断对它是空转的

早些时候给 `LocalRuntimeError::StateRoot` 加了 `tracing::warn!`，把脱敏前的真实错误
留在 host 侧。**这次失败里一行都没出现。**

原因很直白：`tracing_subscriber` 只在 `main.rs` 里装（`fmt().json().init()`），
测试二进制没有装。所以那条诊断帮的是**真在跑的 host**（桌面应用的 runtime），
不是测试。**这是那次改动的一个限制，之前没说清楚。**

给这条测试补了一条它自己能说的话：失败时把**它自己的状态根里有什么**打印出来。
先看到过实际输出：

```
state root /var/folders/…/.tmpFdiekh held ["runtime-state.lock", "retention", "sessions", "runs"]
```

下次它在门禁里翻红，就能看出 `sessions` 目录是不是还在——那是"文件系统瞬时错误"
和"目录根本不在"之间的第一个岔路口。仍然**没有放宽任何边界**。

## 还开着的两条

- `one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded`：
  断言的就是 `started.elapsed() < 180s`，是资源墙钟界限，和上面这条无关。
- `a_network_caller_starts_continues_forks_and_reads_a_real_session`：长期挂着的
  `Session storage is unavailable`，被客户端契约边界**有意脱敏**。要查清得先在
  host 侧留未脱敏诊断，而不是放宽边界——和这次一样，先让失败会说话。
