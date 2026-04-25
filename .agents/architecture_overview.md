# 项目架构概览 (Architecture Overview)

本项目是一个基于 WebRTC 的远程桌面解决方案，采用 Rust (后端) + React (前端) 的技术栈。

## 1. 核心模块说明

- **`server/` (Desk Server)**:
    - 运行在被控端。支持 REST API (Actix-Web), WebRTC, 设置, 文件/终端管理。
    - 采用 **Service Daemon + Session Worker** 多进程架构，以实现 UAC/锁屏穿透。
    - **Service Daemon** 模式：以 SYSTEM/root 权限运行，管理设置、监控会话并管理 Worker 生命周期。
    - **Session Worker** 模式：在用户会话中运行，负责音视频捕获、输入注入及实际控制逻辑。

- **`signal/` (Signaling & TURN)**:
    - 提供 WebSocket 信令服务：用于 WebRTC 握手及自定义指令转发。
    - **内嵌 TURN 服务**：与信令服务器捆绑（核心文件: `signal/src/service.rs`）。

- **`vite-project/` (Frontend)**:
    - React 19 + TanStack Query 架构。
    - 包含管理控制台 UI 以及 Web 控制端客户端。
    - 通过 Kubb 自动生成接口 SDK。

- **`tauri-app/`**:
    - 被控端 GUI 壳程序，用于本地渲染隐私屏、白板等功能。

- **功能库 (Crates)**:
    - `capture-engine/`: 屏幕/音频捕获与编码逻辑。
    - `input-injection/`: 鼠标/键盘输入注入与剪贴板控制。
    - `ipc-protocol/`: Service ↔ Worker 通信的 IPC 消息定义。
    - `signal-facade/`: 共享的信令协议模型。

## 2. 通信流程

1. **认证与初始化**：前端通过 `server` API 登录并获取设置。
2. **建立信令连接**：前端通过 `server` 代理或直连，升为 WebSocket 协议与 `signal` 模块通信。
3. **WebRTC 握手**：
    - 双方通过信令服务器交换 SDP 和 ICE Candidates。
    - `signal-facade` 定义了全套信令协议模型。
4. **媒体/数据传输**：握手成功后，视频流和控制指令通过 WebRTC DataChannel/MediaStream 传输。

## 3. 关键规则

- **i18n**：遵循 [.agents/rules/frontend-i18n.md](./rules/frontend-i18n.md)。
- **信令协议**：遵循 [.agents/rules/signaling-protocol.md](./rules/signaling-protocol.md)。
- **接口同步**：使用指令 `/update_openapi`。
- **信令鉴权与多角色连接架构 (CRITICAL)**：本项目存在复杂的 4 条信令连接链路，处理相关逻辑时必须严格遵守以下双轨鉴权与传参规范：
   - **Desk Server -> Local Signaling Server**: 仅在 `default` 模式下启动本地连接。通过自动生成并持久化的 `settings.system.local_signaling_token` 进行鉴权验证。
   - **Desk Server -> Remote Signaling Server**: 通过 WebSocket URL 的 query 参数传递 `token` (`settings.system.signaling_token`) 进行身份验证。
   - **Desk Server -> Manager Server**: 通过 WebSocket URL 的 query 参数传递 `token` (`settings.system.manager_api_token`)，并在 `manager` 数据库中验证有效性。
   - **Browser -> Signaling / Manager Server**: 浏览器端连接信令时**不带任何 Token Query 参数**，**必须**回退通过 Cookie (Actix-Session) 进行会话认证。后端路由提取参数必须使用 `Option<web::Query<VersionInfo>>` 以兼容浏览器行为，且 `manager` 端的信令路由必须排除在全局 Session 拦截中间件之外。
