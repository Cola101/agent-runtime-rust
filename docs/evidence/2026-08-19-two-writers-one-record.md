# 「Session storage is unavailable」终于有名字了（2026-08-19）

这条含糊话是测试自己的注释说的：「**已经这样好几周了，在负载下，没有办法区分
描述符耗尽和一次瞬时 ENOENT**」。这一轮把它查到底了。

## 一、先让它自己说话

`grpc_session_contract` 那条测试在门禁里挂了。它的失败信息只有：

```
Status { code: Unavailable, message: "Session storage is unavailable" }
```

这是**故意脱敏**的——宿主路径不能穿过客户端契约。宿主确实记了真正的错误
（`client.rs` 的 `note_state_root`），但**测试二进制不装 tracing subscriber**，
所以那一行谁也看不到。

把 subscriber 装进测试，跑 25 次，4 次挂（16%），于是：

```
WARN Session storage failed on the state root
     error=local state root is not usable: No such file or directory (os error 2)
     operation="session_mutation"
```

**ENOENT**——而测试自己列出来的 state root 明明在，里面有
`runtime-state.lock / retention / sessions / runs`。

## 二、机制：两个写者共用一个暂存文件名

`durable_file::replace`：

```rust
let staging = path.with_extension("json.partial");   // ← 从目标路径推出来的固定名字
…
io.rename(&staging, path)                            // ← 输的那个在这里 ENOENT
```

**同一条记录的两次并发写用同一个暂存名。** 一方 rename 走了，另一方还指望它在，
于是第二个 rename 拿到 ENOENT。

这不是假想的并发：`persist_session_record` 有**九个**调用点，
而只有投影那条路（`project_session_head`）拿了 shard 锁。
一个 Run 提交自己的 Turn，和一个读者投影同一条分支，**按构造就会撞**。
所以它在会话的读和写两条路上都出现过。

### 确定性复现

8 个线程、同一个路径、各写 40 次：

```
concurrent writers to one record must not fail:
  ["local state root is not usable: No such file or directory (os error 2)", …×190]
```

**和那条 flaky 测试一模一样的错误字符串。** 到此机制没有第二种解释。

## 三、修法：按记录串行，而不是改名字

改成"每个写者一个唯一暂存名"看着更自然，**但不能这么改**：
`.json.partial` 这个后缀是**有承载的**——恢复和清理靠它精确识别半写文件，
`embedded.rs`、`event_archive.rs`、`retention.rs`、`lib.rs` 四处都在 strip 这个后缀。
改名要同时改这四处扫描，而且会把"一个固定名字被就地覆盖"变成
"每次崩溃留一个残留文件"。

所以是**按路径分片串行化**（64 个 shard，按路径哈希）：
两条不同记录互不等待，同一条记录的写者排队——而那本来就是必须串行的。

进程内，因为这就是问题的实际范围：两个**进程**写同一个 state root 已经被
单写锁挡住了。

## 四、修完之后

| | 之前 | 之后 |
| --- | --- | --- |
| `durable_file` 并发写守卫 | 190 次失败 | **0** |
| `grpc_session_contract` × 30 | 25 次里挂 4 次（16%） | **30 次全过** |

## 五、这件事对桌面的意义

`read_session` 和 `continue_session` 是这个客户端**每一次**打开对话都会走的两条路。
16% 的概率上，它们会告诉人"存储不可用"——而存储好好的。
一个人看到那句话唯一能做的事是重试，而重试确实会好，
于是它看起来像"偶尔抽风"，而不是像一个有名字的缺陷。

**这一条不是我这一轮改出来的**：测试注释说它已经存在好几周，
六次门禁里前五次没打到它，第六次打到了。
