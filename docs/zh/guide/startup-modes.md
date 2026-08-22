# 启动模式

`server` 二进制通过 `--startup-mode`（或 `-s`）支持多种启动模式。配置文件路径用 `-c` 指定。

```bash
cargo run -- --startup-mode <MODE>
cargo run -- --help
```

## 可用模式

| 模式 | 角色 |
|---|---|
| `default` | 完整模式——在单进程中运行 信令 + Desk Server + WebRTC + 采集。 |
| `signaling` | 仅信令服务（信令 + TURN）。 |
| `desk-server` | 仅被控端（Desk Server）。 |
| `service-daemon` | 系统服务守护进程（SYSTEM / root），管理各会话的 worker。 |
| `session-worker` | 由守护进程在用户桌面会话中启动的内部工作进程。 |
| `mcp-stdio` | 面向本地 AI 助手的只读 MCP 服务（stdio）。 |

## 默认模式

最简部署：同一套逻辑上的 daemon → 对等连接 → worker 流水线运行在一个操作系统进程内，并使用进程内通道。适合便携使用与开发。

## service-daemon 进程模型

为了采集 Windows **UAC** 或**锁屏**等安全界面，service-daemon 模式将操作跨权限边界拆分：

![Service-daemon 进程与 IPC 模型](/architecture/process-model-cn.svg)

**ServiceDaemon**（以 SYSTEM / root 运行）持有 WebRTC 连接、信令与子进程；它在每个桌面会话中启动一个 **SessionWorker**，负责采集、编码、输入、文件与剪贴板。

二者使用三条独立传输：双向 **event pipe** 承载信令与控制；单向 **media pipe** 承载编码后的音视频帧；双向 **file pipe** 承载文件命令与数据块。文件传输独立后，其背压不会阻塞控制事件。

这种拆分让会话工作进程可以在用户切换时重启，而**不中断浏览器连接**，因为 WebRTC 对等连接由守护进程持有。

目前只有 Windows 实现了原生系统服务集成；Linux 和 macOS 上的 `service-daemon` 暂时以交互方式运行，但逻辑进程模型相同。

## MCP stdio 模式

`--startup-mode mcp-stdio` 把设备变成一个[只读 MCP 服务](/zh/features/mcp-server)。该模式下 stdin/stdout 承载 MCP JSON-RPC，因此服务端**绝不能向 stdout 打日志**。
