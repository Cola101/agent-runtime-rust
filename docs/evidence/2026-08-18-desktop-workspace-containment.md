# 桌面工作区读取与 containment 证据（2026-08-18）

## 复核事实

| 边界 | 风险 |
| --- | --- |
| renderer 能请求哪些路径 | renderer 渲染的是**本应用没有编写**的转录内容。一个接受任意路径的调用，等于把路径的选择权交给模型最后说的那句话 |
| 字符串前缀判断 | `a/b` 在字符串上「在里面」，`realpath` 之后可能在外面。**必须在 realpath 之后判断** |
| 少一个分隔符 | `/x/workspace-evil` 能通过 `startsWith("/x/workspace")` |
| 「代理动过的路径」 | 事件记录的是「某个工具被**要求**做什么」。当成文件系统改动展示，就是在报告尚未发生的工作 |

## 实现结果

- 路径一律相对，`realpath` 之后再判断包含关系，且比较时带尾部分隔符。
- 目录列表与文件预览都有上界；二进制文件报告为二进制，而不是渲染成一屏替换字符。
- 应用**只在自己知道时**才声称有工作目录：挂在别人启动的 Runtime 上且没有配置时，
  界面直说「不知道」，而不是显示一个看着像的目录。
- 「代理动过的路径」来自持久事件里带路径的工具调用，措辞是「被要求过」，
  并明确写出停在审批上的调用也在其中。

## 可执行门禁

| 门禁 | 打破方式 | 结果 |
| --- | --- | --- |
| 符号链接逃逸 | 把判断挪到 `realpath` 之前 | 两条变红 |
| 同名前缀兄弟目录 | 去掉尾部分隔符 | 一条变红 |
| 路径来自真实事件 | 跳过 `model.tool_call` | 两条变红 |
| 不从没有路径的调用里编路径 | 用工具名兜底 | 一条变红 |

`vitest run`：**61 passed**。

## 实测中发现的一处 fake 偏离

真实 Runtime 的 `model.tool_call` 载荷是**平铺**的 `{name, arguments, id}`；
`approval.required` 才把 call 嵌在 `execution.call` 里。而测试 fake 两处都用了嵌套形状——
那正是它自己的头注释警告过的：**让 renderer 对着 Runtime 没有的形状通过**。已按真实 dev-runtime 日志改正。

## 端到端

真实 release Runtime、真实工作目录、干净 state root：

```
runtime-desk: shell mounted, 6 surface(s) registered
runtime-desk: drew {"link":"live","runs":0,"sessions":0,"turns":0}
```

一轮「读一下 notes.txt」得到：

```
approval.required {"name":"workspace.read_text","arguments":{"path":"notes.txt"}}
state: {"state":"waiting_approval"}
```

——正好是界面那句措辞对应的情形：路径被**要求**过，调用停在人身上，没有执行。
退出后 `runtime stopped`，无残留。

## 仍然没做

- 只读。从窗口写工作区没有做，也不打算在 Alpha 里做：写入要经过 Runtime 的工具与审批，
  而不是绕过它们。
- 没有 diff 视图；「动过的路径」不是改动记录。
- 打包成可分发 artifact 仍未做。
