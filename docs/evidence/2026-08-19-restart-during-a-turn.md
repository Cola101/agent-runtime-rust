# 重启 Runtime 打断一轮之后,对话还接得上吗（2026-08-19）

**猜错了一次,记在这里。**

## 我先以为它接不上

`prepare_session_continue`（`runtime-host/src/lib.rs:6156-6160`）写着：

```rust
if branch.active_turn.is_some() {
    return Err(LocalRuntimeError::Execution(
        "root Session branch already has an active Turn".into(),
    ));
}
```

而排空路径（`embedded.rs:2027-2030`）把没有 checkpoint 的 Run 记成
`LocalRunState::Interrupted`——**那不是内核意义上的终局**：没有终局事件、没有 checkpoint，
于是 `project_terminal_session_turn` 在「没有 checkpoint 就提前返回」那一句停下，
`active_turn` 看起来会永远挂着。

窗口上的 **重启 Runtime** 按钮正好制造这个状态，而那个按钮是这个应用**自己让人按**的
（改完 Provider、改完 MCP 服务都提示要重启）。

## 第一次复现「成功」了,但它证明的是别的事

两个进程（一个 state root 拒绝第二个 Runtime owner，所以进程内根本写不出这个测试）：
起 host → `session_start` → owner `shutdown` 排空 → 杀掉进程 → 起新的 → `session_continue`。

```
{"message":"local execution was refused: root Session branch already has an active Turn"}
```

**看起来坐实了。但没有。** 我那个 provider 是「接上连接永远不答」——
重启之后恢复出来的 Run **也**挂在同一个 provider 上，所以分支当然一直忙着。
我测出来的是「我的 fixture 让 Run 永远跑不完」，不是「对话卡死了」。

## 改成能分辨两件事之后：结论相反

provider 改成「第一条连接不答，之后正常答」——第一轮在飞行中被打断，
重启之后的一切都能跑完。这样「分支正忙着把恢复出来的 Turn 跑完」和
「分支被一个永远没人完成的 Turn 占着」才分得开。

**结果：接得上。** 而且窗口很短——重试到成功需要 **2 次**（约 100ms），三次测量都是 2。

原因在我先前漏读的地方：`plan_unfinished_recovery`（`embedded.rs:2696-2705`）在
启动恢复时，对没有 checkpoint 的活跃 Turn 会 `clear_active_session_turn`；
有 checkpoint 的那种则**恢复并跑完它**，跑完 Turn 自然落地。两条路都会放开分支。

**所以「被打断的对话再也接不上」这个说法不成立。** 记在这里，因为我照它写了半个测试。

## 真正的缺陷比那个小,但真的会咬人

窗口是真的存在的：重启之后那一小段时间里，`session_continue` 会返回
`root Session branch already has an active Turn`。而窗口里客户端做了两件错事：

1. **打的字没了。** 输入框在写之前就 `setDraft("")`，所以任何一次被拒
   都让人对着一个错误和一个空框——他写的那句话只有按 ↑ 才找得回来，
   而他没有理由知道这件事。
2. **说的是 Runtime 的内部话。** 界面上直接印
   `local execution was refused: root Session branch already has an active Turn`。

两条都修了：被拒就把句子放回框里（被拒的发送没有发生，那句话还是他的）；
Runtime 那句话映射成写检查本来就在用的同一句人话，并且**认不出的错误原样透出**——
印出 Runtime 的原话比一句话差，但比吞掉一个没人预料到的东西好得多。

## 留下的覆盖

`runtime/apps/runtime-host/tests/restart_during_a_turn.rs` —— 两个真实进程，
第一轮在飞行中被杀，新进程起来之后对话继续。**在此之前没有任何测试跑过这条路。**
