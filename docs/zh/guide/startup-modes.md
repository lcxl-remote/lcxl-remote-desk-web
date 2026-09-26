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

Windows 通过原生服务控制管理器运行服务；Linux 提供 systemd 服务安装器。macOS 不使用这套系统服务安装入口。

### 安装 Linux systemd 服务

在本机桌面客户端以设备 owner 登录，进入**系统设置 → Linux 系统服务**，点击**安装服务**，再完成桌面的管理员授权。安装需要 systemd、`pkexec` 和可用的桌面授权代理；客户端不会接收管理员密码。无需再启用实验性开关。

安装路径固定为 `/usr/lib/lcxl-remote-desk`，系统配置位于 `/etc/lcxl-remote-desk/config.toml`。守护进程以 root 运行，桌面 worker 以已登录用户身份运行。Linux 不提供 Windows 虚拟显示驱动选项。

弹窗会等待安装进程结束，分别提示完成、取消授权、授权未通过、缺少 `pkexec`、已有操作进行中或安装器失败。“请求已提交”本身不代表安装成功。卸载导致与 daemon 的连接断开后，本机客户端仍可显示最终结果；如果无法取得最终回执，会提示结果未知，不自动重复操作。服务卡另外显示服务是否已安装、是否正在运行。

**卸载服务**会停止并禁用服务、移除 systemd unit，保留已安装的程序文件和配置。安装和卸载均需管理员授权。命令行方式可在管理员权限下运行 server 的 `--install-service --config-file-path /当前用户配置的绝对路径/config.toml`（首次安装需要迁移当前用户配置）；卸载使用 `--uninstall-service`。

安装服务不会授予桌面采集或输入权限。Linux AI 桌面能力面向已登录的 GNOME Wayland 会话；安装成功不代表支持登录界面或已经具备完整的无人值守桌面能力。桌面授权与真实主机上的安装、卸载仍需分别验证。

## MCP stdio 模式

`--startup-mode mcp-stdio` 把设备变成一个[只读 MCP 服务](/zh/features/mcp-server)。该模式下 stdin/stdout 承载 MCP JSON-RPC，因此服务端**绝不能向 stdout 打日志**。
