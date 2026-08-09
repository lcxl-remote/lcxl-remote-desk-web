# CLI 参数

```bash
cargo run -- --help
```

## 参数

- `-c, --config-file-path <PATH>`——配置文件路径（无默认值；用于覆盖统一的平台 profile）。
- `-s, --startup-mode <MODE>`——启动模式：
  - `default`——含信令与被控端的完整模式。
  - `signaling`——仅信令模式（信令 + TURN）。
  - `desk-server`——仅被控端模式。
  - `service-daemon`——系统服务守护进程（SYSTEM / root），管理各会话 worker。
  - `session-worker`——由守护进程在用户桌面会话中启动的工作进程。
  - `mcp-stdio`——走 stdio 的只读 MCP 服务。

各模式的用途与适用场景见[启动模式](/zh/guide/startup-modes)。
