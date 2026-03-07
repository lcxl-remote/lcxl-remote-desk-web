# 项目架构概览 (Architecture Overview)

本项目是一个基于 WebRTC 的远程桌面解决方案，采用 Rust (后端) + React (前端) 的技术栈。

## 1. 核心模块说明

- **`server/` (Desk Server)**:
    - 运行在被控端。
    - 提供 REST API (Actix-web)：用于管理设置、文件、终端和触发 WebRTC 连接。
    - 持久化配置 (`conf/config.toml`)。
    - API 文档基于 Utoipa 自动生成。

- **`signal/` (Signaling & TURN)**:
    - 提供 WebSocket 信令服务：用于 WebRTC 的 Offer/Answer/ICE 交换及自定义控制指令转发。
    - 内置 TURN 服务（默认开启集成）。
    - 关键文件：`signal/src/service.rs` 处理所有信令流。

- **`vite-project/` (Frontend)**:
    - 既是管理后台，也是 Web 控制端。
    - 使用 React 19 + TanStack Query。
    - **接口同步**：通过 Kubb 工具链从后端 OpenAPI JSON 自动生成前端 SDK。

- **`tauri-app/`**:
    - 被控端的 GUI 外壳，提供隐私屏、白板显示等需要在本地渲染画面的功能。

## 2. 通信流程

1. **认证与初始化**：前端通过 `server` API 登录并获取设置。
2. **建立信令连接**：前端通过 `server` 代理或直连，升为 WebSocket 协议与 `signal` 模块通信。
3. **WebRTC 握手**：
    - 双方通过信令服务器交换 SDP 和 ICE Candidates。
    - `signal-facade` 定义了全套信令协议模型。
4. **媒体/数据传输**：握手成功后，视频流和控制指令通过 WebRTC DataChannel/MediaStream 传输。

## 3. 关键规则

- **i18n**：遵循 [.agents/rules/frontend-i18n.md](file:///d:/source/lcxl-remote-desk-web/.agents/rules/frontend-i18n.md)。
- **信令协议**：遵循 [.agents/rules/signaling-protocol.md](file:///d:/source/lcxl-remote-desk-web/.agents/rules/signaling-protocol.md)。
- **接口同步**：使用指令 `/update_openapi`。
