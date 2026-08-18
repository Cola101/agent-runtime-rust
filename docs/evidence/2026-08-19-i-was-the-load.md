# 那条墙钟测试这次是我自己压红的（2026-08-19）

`one_thousand_runs_keep_hot_state_recovery_and_process_resources_bounded`
在今天第三次全量门禁里挂了：**187.09s / 界限 180s**，超 4%。
前两次门禁它都过了。

## 不是回归

同一次失败里，它的**资源断言全部通过，而且余量很大**：

```
runs=1000 hot_run_directories=16 tombstones=984 state_files=70
recovery_scan_ms=574 rss_peak_bytes=29130752 fd_peak=37 fd_final=12
elapsed_ms=187094
```

热目录 16、fd 峰值 37、RSS 峰值 29MB、恢复扫描 574ms ——
这条测试真正要守的东西一条没破，破的只有那个墙钟数。

## 是我

查这台机器上在跑什么，找到**三个我自己留下的 vite dev server**，
都在这个仓库的 `.claude/worktrees/wf_*` 里，是我早先几次 workflow 的 agent 起的，
起于 21:36–22:31，一直跑到 05:30。**今天每一次门禁都被它们压着。**

（另外还有六个 `ipm-deploy` 的 vite，那些是别的会话的，没有动。）

停掉我自己那三个之后，同一条测试连跑三次：

| | 用时 |
| --- | --- |
| 1 | **159.57s** |
| 2 | **156.50s** |
| 3 | **157.46s** |

界限 180s，余量约 12%。**没有改这个界限，没有串行化，没有删断言。**

## 该记住的

1. **做视觉验收要开 dev server，那就别同时跑门禁。** 这次是我自己踩的：
   为了看新那行长什么样开了 preview，然后在它还开着的时候起了门禁。
2. **workflow 用 worktree 隔离时，agent 在里面起的 dev server 不会被清掉。**
   worktree 本身会自动删，里面的进程不会。这是一个我以后每轮都该查的东西。
3. 这条界限**余量约 12%**，仍然是三条薄界限之一。这次不是它的问题，
   但它下次仍然会先于任何东西翻红——那个取舍还摆在那里，没有替谁做掉。

`2026-08-19-close-handshake-under-parallelism.md` 里写着"这台机器上常驻着别的会话的
dev server"，那句话当时是对的，**但不完整**：其中一部分是我自己的。
