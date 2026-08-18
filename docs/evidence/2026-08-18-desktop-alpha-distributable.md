# macOS Desktop Alpha 可分发验收证据（2026-08-18）

## 产物

`desktop/scripts/package-app.sh` → `Runtime Desk.app`（293M，含 Electron、renderer bundle、
release runtime 二进制、proto、36 个宿主进程依赖）。**未签名**：签名与公证是另一件带 Apple 账号的决定，
加一个 ad-hoc 签名只会让它看起来可分发而其实不是。

## 为什么是手工装配而不是 @electron/packager

这台机器上 npm registry 元数据可达，但 **tarball 约 800 B/s**（12KB 用了 15 秒），
装一棵 packager 的依赖树不是能跑完的事。手工脚本做的正是 packager 为一个未签名本地构建做的事：
复制 Electron bundle、把应用放进去、改几个 Info.plist 键、把 runtime 放在旁边。

**代价写在脚本里**：辅助进程在活动监视器里仍叫 "Electron Helper"，没有图标。两条都是外观问题。

## 脚本自己抓到的两个真问题

1. **只拷直接依赖的包装不出能跑的应用。** 文件检查全过，加载时 `@grpc/grpc-js` 找不到
   `@js-sdsl/ordered-map` —— 一个本仓库任何地方都没有提到的包。传递依赖不是可以省的优化，
   它是这棵树的大部分。脚本末尾那句"让 bundle 从自己内部加载宿主模块"就是为此存在的。
2. **socket 回退目录两端各自从环境里取。** state root 超过 `sockaddr` 长度时，
   客户端与 daemon 都退到临时目录，一个问 `os.tmpdir()`，一个问 `std::env::temp_dir()`。
   两边都读 `TMPDIR`，**两边都没错，但可以不一致**：在没有 `TMPDIR` 的环境里启动，
   应用连不上一个正在 `/var/folders` 好好监听的 Runtime。

   修法不碰 Runtime 契约：应用**显式把 `TMPDIR` 传给它 spawn 的子进程**，
   让两个答案是同一个字符串，而不是同一条规则作用在两个环境上。打破后测试变红。

   **机制我没有查到底**：为什么子进程会拿到父进程环境里没有的 `TMPDIR`，我没有确认。
   我确认的是症状，以及固定它之后分歧消失。

同时补了一句诊断：「没有开始监听」现在会说**它在哪个 socket 上等**、state root 是什么。
之前那句话会把下一个人送去读 Runtime 源码，而答案通常是两边路径不一致，不是没人监听。

## 干净目录验收

`.app` 复制到另一个目录、全新 userData、**`env -i` 清空环境**（正是那个曾经失败的环境）：

| 步骤 | 结果 |
| --- | --- |
| 首次启动，无 Provider | 窗口打开，6 个面，明说「没有配置 Provider」，不崩不静默 |
| 用 bundle 内的模块配置 Provider | 密钥进钥匙串，配置文件不含密钥 |
| 再次启动 | `started runtime-host` → `local runtime at /tmp/agent-runtime-host-….sock` → `link: live` |
| 真实一轮对话 | `turns: 1`，assistant 有回复 |
| 退出 | `runtime stopped — 0 active and 0 queued before draining`；runtime-host 0 个，Runtime Desk 0 个 |

运行的确实是 bundle 内的二进制：
`.../installed/Runtime Desk.app/Contents/Resources/agent-runtime-host serve`。

## 一次我自己的测试错误

第一次「退出后有残留」是**我杀错了进程** —— `pgrep | head -1` 拿到的不是主进程，应用根本没收到信号。
不是产品缺陷。对着正确的 pid 重做后，Runtime 随应用一起停止，无残留。

## 仍然没做

- 签名与公证。
- 图标与辅助进程命名。
- 安装器（DMG/zip）；现在是一个 `.app` 目录。
