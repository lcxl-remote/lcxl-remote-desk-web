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
   pnpm install
   pnpm dev
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

根据项目中 `.agents/rules/` 目录下的规则配置，请在开发时严格遵守以下约定：

1. **代码注释**: 所有代码注释必须使用**英文**编写。
2. **Git 提交**: 如果要求提交到 Git 仓库，所有的 commit message 必须使用**英文**。
3. **架构与配置**: 项目核心配置位于 `conf/config.toml`，请根据环境调整监听地址、日志级别、编码器类型（vpx/openh264）等参数。
4. **多语言支持 (i18n)**: UI 和文档均支持中英双语，开发时需注意维护对应的多语言文件。

---
> **注意**: 本项目目前处于早期开发阶段，进行任何代码修改前，请务必进行充分的验证和测试。
