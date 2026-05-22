# CLAUDE.md

本文件旨在为 Claude Code (claude.ai/code) 在处理本仓库代码时提供指导。

## 项目概述

`lcxl-remote-desk` 开源 WebRTC 远程桌面解决方案。后端使用 Rust (Actix-Web)，前端使用 React + TypeScript (Vite)。`server` 二进制文件支持多种运行模式：完整模式 (`default`)、仅信令模式 (`signaling`)、仅被控端模式 (`desk-server`)、系统服务守护进程模式 (`service-daemon`) 以及会话工作进程模式 (`session-worker`)。

## 构建与运行

```bash
# 后端
cargo run -p lcxl-remote-desk-server                  # default (完整) 模式
cargo run -p lcxl-remote-desk-server -- --help         # 查看所有启动标志
cargo build --workspace --release
cargo test --workspace

cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# 前端
cd vite-project && npm ci && npm run dev               # 开发服务器 (默认端口 :5174)
cd vite-project && npm run build                       # 类型检查 + 构建

# 后端 API 变更后，重新生成前端客户端代码
# Windows: cd vite-project && .\update_openapi.ps1
# Linux/macOS: cd vite-project && ./update_openapi.sh
# (需要 server 运行在 :8081 端口)
```

### Linux 系统依赖

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev \
  libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev \
  clang libclang-dev cmake libvpx-dev
```

### API 文档 (服务器运行时)

Swagger UI: `http://localhost:8081/swagger-ui/` | OpenAPI 规范: `http://localhost:8081/openapi.json`

## 模块概览

| 模块 | 角色 |
|---|---|
| `server/` | Desk server: REST API (Actix-Web), WebRTC, 设置, 文件/终端管理（支持 ServiceDaemon 和 SessionWorker 模式） |
| `signal/` | 信令服务器 + TURN (核心文件: `signal/src/service.rs`) |
| `vite-project/` | React 19 + TanStack Query 前端 — 包含管理 UI 和 Web 控制端客户端 |
| `tauri-app/` | Tauri 壳程序，用于在被控机本地渲染防窥屏/白板功能 |
| `signal-facade/` | 共享的信令协议模型 (供 `signal` 和 `manager` 使用) |
| `capture-engine/`| 屏幕/音频捕获与编码逻辑 |
| `input-injection/`| 鼠标/键盘输入注入与剪贴板控制 |
| `ipc-protocol/` | 用于 Service ↔ Worker 之间通信的 IPC 消息定义 |
| `utils/` | 通用工具类 |
| `turn/` | TURN 服务器 (与信令服务器捆绑) |
| `server-version/` | API 版本常量 |

## 添加新 API 接口的步骤

1. 在 `server/src/model/` 中定义模型
2. 在 `server/src/service/` 中实现逻辑
3. 在 `server/src/controller/` 中添加带 `utoipa` 注解的路由处理函数
4. 在 `server/src/main.rs` 中注册路由
5. 运行 OpenAPI 更新脚本以重新生成前端客户端代码

## 添加新信令类型的步骤

1. 在 `signal-facade/src/model/signal.rs` 的 `SignalingType` 中添加新的枚举变体（带唯一整数值）。
2. 在 `signal/src/service.rs` 的 `handle_message` 函数中处理它 — 添加到转发分支或编写专用的匹配分支。**绝对不要添加 `_ =>` 兜底匹配**（由编译器强制检查穷尽性）。
3. 更新前端：运行 `/update_openapi`，然后在 `vite-project/src/features/desk/hooks/useDeskRTC.ts` 中添加 `onMessage` 处理程序。

## 信令鉴权 (CRITICAL)

| 连接 | 鉴权方法 |
|---|---|
| Desk Server → Local Signaling | `settings.system.local_signaling_token` (自动生成，仅限 `default` 模式) |
| Desk Server → Remote Signaling | WebSocket URL 中传递 `?token=<settings.system.signaling_token>` |
| Desk Server → Manager | WebSocket URL 中传递 `?token=<settings.system.manager_api_token>` |
| Browser → Signaling / Manager | **不带 token 参数。**仅使用 Actix-Session Cookie。路由提取器必须使用 `Option<web::Query<VersionInfo>>`；并将 manager 信令路由排除在全局 Session 中间件之外。 |

## 前端规则

- **国际化 i18n (强制):** 所有用户可见的文本必须使用 `useTranslation()` / `t()` — 禁止硬编码字符串。每个新键必须同时添加到 `vite-project/src/locales/zh-CN/pages.ts` **和** `vite-project/src/locales/en-US/pages.ts`。
- **自动生成代码:** `vite-project/src/services/` 下的文件（Kubb 输出）是自动生成的 — 请勿手动修改。
- **Tauri IPC:** 通过外部 HTTP URL 加载的 Windows 失去 `__TAURI_INTERNALS__`。绝不能从前端页面调用 `invoke()` 或 `listen()`。前端调用 Rust 请使用 REST API；Rust 触发前端事件请使用 `window.eval()` + `dispatchEvent`。使用原生的 `window.addEventListener` 进行监听。

## 代码规范与规则

1. **工作语言规则：所有回复、思考过程及任务清单，均须使用中文。**
2. **Rust:** 使用 `rustfmt` 格式化，函数/模块名使用 `snake_case`，类型名使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`。
3. **TypeScript/React:** 4 个空格缩进，组件名使用 `PascalCase`，钩子名使用 `useXxx`，`src/components/ui` 中的文件名使用 `kebab-case`。
4. **代码注释**必须使用**英文 (English)**。
5. **Git 提交信息**必须使用**英文 (English)**，并遵循 Conventional Commits 规范 (`feat:`, `fix:`, `chore:`)。
6. **测试用例 (CRITICAL)**：更改代码必须要增加测试用例。
