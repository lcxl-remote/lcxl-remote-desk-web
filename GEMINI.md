# LCXL Remote Desk Web 项目指南

## 项目概述
LCXL Remote Desk Web 是一个基于 WebRTC 技术的高效现代远程桌面解决方案。该项目允许用户仅通过 Web 浏览器即可获得高性能的远程计算机访问与控制，无需安装额外插件或专用客户端软件。
- **后端技术栈**: Rust
- **前端技术栈**: React + Vite + Tailwind CSS + TypeScript
- **桌面客户端**: Tauri (Rust + 网页前端)

### 核心架构与模块
- **`server`**: 运行于宿主机的核心远程桌面服务，负责屏幕采集、音频捕获、命令执行和文件管理。
- **`signal`**: 信令服务器模块（默认在 `server` 中启用，也可独立部署），使用 WebSocket 协调对等连接。
- **`vite-project`**: Web 前端应用程序，用作管理仪表板和远程客户端。
- **`tauri-app`**: 增强型带 GUI 的服务端程序，提供隐私屏和白板等依赖本地 UI 的高级功能。
- **`turn`**: 集成的 TURN/STUN 服务，确保复杂网络环境下的 NAT 穿透。
- **`signal-facade`** / **`utils`**: 信令服务接口定义与通用工具包模块。

## 构建与运行指南

### 从源码运行（开发者推荐）
1. **环境准备**:
   - 安装最新稳定版 Rust
   - 安装 Node.js 和 pnpm

2. **启动后端服务**:
   ```bash
   cd server
   cargo run --release
   ```
   *默认将同时启动信令服务和远程桌面服务。*

3. **启动前端服务**:
   ```bash
   cd vite-project
   npm ci
   npm run dev
   ```
   *启动后访问 `http://localhost:5173`。*

### Tauri 桌面客户端
适用于需要“隐私屏”或“白板”等高级功能的场景：
```bash
cd tauri-app
cargo tauri dev
```

### Docker 部署
根目录下提供了一键式部署支持：
```bash
docker-compose up -d
```
*启动后访问 `http://localhost:8081` 进行初始管理员设置。*

## 开发约定与规范

在进行开发迭代时，**必须**严格遵守以下核心准则：

### 1. 通用规范
- **代码注释**: 所有代码注释必须使用**英文**编写。
- **Git 提交**: 所有的 commit message 必须使用**英文**。

### 2. 前端国际化 (i18n)
- **禁止硬编码**: 严禁在 React 组件中直接硬编码中英文文本。
- **提取 Key**: 必须使用 `useTranslation` 钩子及 `t` 函数（例如：`t('pages.system.settings.auto_start', '开机自动启动')`）。
- **同步更新**: 每当添加新的 Key 时，必须同步更新 `vite-project/src/locales/zh-CN/pages.ts` 和 `en-US/pages.ts`。

### 3. 信令协议 (Signaling)
- **后端定义**: 在 `signal-facade/src/model/signal.rs` 的 `SignalingType` 中添加新类型，并分配唯一 ID。
- **转发逻辑**: 在 `signal/src/service.rs` 的 `handle_message` 中必须**穷举**处理所有类型，**严禁使用 `_ => { ... }` 兜底分支**。
- **前端集成**: 在 `vite-project/src/features/desk/hooks/useDeskRTC.ts` 中注册新信令处理逻辑。

### 4. Tauri IPC 规范 (核心铁律)
> [!IMPORTANT]
> Tauri 窗口加载外部 URL 时，由于跨域隔离，`__TAURI_INTERNALS__` (如 `invoke`) 无法生效。
- **禁止调用**: 网页前端严禁直接调用 `invoke` 或 `listen` API。
- **通信路径**: 前端调用 Rust 逻辑必须通过后端 Actix-Web 提供的 **REST API**。
- **Rust 唤醒前端**: Rust 后端必须获取 `WebviewWindow` 后调用 `eval` 进行 JavaScript 派发：
  ```rust
  window.eval("window.dispatchEvent(new CustomEvent('my-event', { detail: payload }));");
  ```
- **前端响应**: 前端应使用原生 `window.addEventListener` 监听事件。

## 标准工作流 (Workflows)

当执行以下任务时，请遵循标准化流程：

### 1. 添加新 API 接口
1. 在 `server/src/model/` 中定义 Request/Response 结构体。
2. 在 `server/src/service/` 中实现业务逻辑。
3. 在 `server/src/controller/` 中编写路由处理器（添加 `#[utoipa::path(...)]` 注解）并在 `main.rs` 中注册。
4. 执行 `/update_openapi` 自动生成前端 Hook。
5. 在前端使用 Kubb 生成的 `use...` React Query 钩子。

### 2. 添加新信令类型
1. 修改 `signal-facade/src/model/signal.rs`。
2. 按照 **信令协议规范** 更新 `signal/src/service.rs`。
3. 执行 `/update_openapi` 同步前端枚举。
4. 在前端 Hook（默认为 `useDeskRTC.ts`）中增加处理逻辑。

### 3. 同步 OpenAPI
自动化同步流程：
- 检查后端 `8081` 端口状态。
- 若未启动，在 `server` 目录启动临时服务直至 `openapi.json` 可用。
- 在 `vite-project` 执行 `.\update_openapi.ps1` (Windows) 或 `./update_openapi.sh` (Unix)。

### 4. 文档多语言同步
- 当 `README_CN.md` 或 `DEVELOPMENT_CN.md` 发生变更时，必须同步翻译并更新对应的英文版文件。
- 确保翻译符合技术写作习惯，避免生硬机翻。

### 5. 任务归档
- 完成任务后，将 Implementation Plan 和 Task List 整理为 Markdown。
- 命名格式：`agent_works/yyyy-MM-dd_{title}.md` (如 `2026-03-15_feat-add-new-api.md`)。
- 确保已进行脱敏处理。

---
> **注意**: 本指南旨在确保项目的一致性与稳定性。在应用变更前，务必查阅相关规范。
