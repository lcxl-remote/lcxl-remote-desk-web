# 模块地图

Rust workspace 拆分为若干聚焦的 crate，外加前端与 Tauri 壳。

| 模块 | 角色 |
|---|---|
| `server/` | Desk server：REST API（Actix-Web）、WebRTC、设置、文件/终端管理（支持 ServiceDaemon 与 SessionWorker 模式）。AI 诊断编排器在 `server/src/diagnose/`（采集 → 脱敏 → 模型 → 渲染），模型适配在 `server/src/diagnose/model/`（`openai.rs` / `anthropic.rs`）。 |
| `signal/` | 信令服务器 + TURN。 |
| `signal-facade/` | 共享的信令协议模型。 |
| `turn/` | TURN / STUN 服务（与信令服务器捆绑）。 |
| `vite-project/` | React 19 + TanStack Query 前端——管理 UI 与 Web 控制端（含 AI 设置页与诊断面板）。 |
| `tauri-app/` | Tauri 壳，用于在被控机本地渲染防窥屏 / 白板。 |
| `agent-protocol/` | 设备能力协议（`desk-agent-protocol`）：线路数据类型、`DeviceAgent` 接口，以及审计、诊断和命令执行协议。只定义协议，不包含平台实现；所有受信字段都以服务端为准。 |
| `mcp-server/` | 只读 MCP 服务（`desk-mcp-server`）：`rmcp` SDK + stdio，静态白名单的只读工具（无 exec/write/control）。 |
| `capture-engine/` | 屏幕 / 音频采集与编码。 |
| `input-injection/` | 鼠标 / 键盘注入与剪贴板控制。 |
| `ipc-protocol/` | daemon ↔ worker 的 IPC 消息定义。 |
| `virtual-display/` | 虚拟显示器（IddCx）用户态抽象（`desk-virtual-display`）。 |
| `virtual-display-driver-ops/` | 虚拟显示驱动安装 / 卸载封装。 |
| `server-user/` | 服务端用户 / 账户模型。 |
| `utils/` | 通用工具。 |
| `server-version/` | API 版本常量。 |

## 添加新 REST API

1. 在 `server/src/model/` 中定义模型。
2. 在 `server/src/service/` 中实现逻辑。
3. 在 `server/src/controller/` 中添加带 `utoipa` 注解的路由处理函数。
4. 在 `server/src/main.rs` 中注册路由。
5. 运行 OpenAPI 更新脚本重新生成前端客户端。

## 添加新信令类型

1. 在 `signal-facade/src/model/signal.rs` 的 `SignalingType` 中添加新变体（带唯一整数值）。
2. 在 `signal/src/service.rs` 的 `handle_message` 中处理它——添加转发分支或专用匹配分支。绝不添加 `_ =>` 兜底（穷尽性由编译器强制）。
3. 更新前端：重新生成客户端，然后在前端 RTC hook 中添加 `onMessage` 处理程序。
