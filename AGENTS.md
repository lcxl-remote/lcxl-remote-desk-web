# 仓库指南 (Repository Guidelines)

## 项目结构与模块组织
本代码仓库是一个包含 Vite 前端的 Rust 工作空间。

- `server/`: Desk server: REST API (Actix-Web), WebRTC, 设置, 文件/终端管理（支持 ServiceDaemon 和 SessionWorker 模式）。
- `signal/`: 信令服务器 + TURN (核心文件: `signal/src/service.rs`)。
- `vite-project/`: React 19 + TanStack Query 前端 — 包含管理 UI 和 Web 控制端客户端。
- `tauri-app/`: Tauri 壳程序，用于在被控机本地渲染防窥屏/白板功能。
- `capture-engine/`, `input-injection/`, `ipc-protocol/`, `signal-facade/`, `server-version/`, `utils/`, `turn/`: 功能模块库，涵盖捕获编码、输入注入、IPC 协议、信令协议模型等。
- `conf/config.toml`: 运行时配置文件；`openapi.json` + `vite-project/openapi.json`: API 规范文档。

## 构建、测试与开发命令
除非另有说明，请在仓库根目录下运行命令。

- `cargo run -p lcxl-remote-desk-server`: 在默认模式下启动后端。
- `cargo run -p lcxl-remote-desk-server -- --help`: 查看启动标志 (`-m default|signaling|desk-server|service-daemon|session-worker`)。
- `cargo build --workspace --release`: 构建所有的 Rust crates。
- `cargo test --workspace`: 运行 Rust 测试（包括 `server/tests/test_utils.rs`）。
- `cargo fmt --all` 以及 `cargo clippy --workspace --all-targets -- -D warnings`: 格式化并检查（lint）后端代码。
- `cd vite-project && npm ci && npm run dev`: 在 Vite 开发服务器启动前端（默认端口 `5174`）。
- `cd vite-project && npm run build`: 类型检查并构建前端。
- `cd vite-project && ./update_openapi.ps1`: 从 `http://localhost:8081/openapi.json` 刷新前端 API 客户端。

## 代码风格与命名规范
- Rust: 遵循 `rustfmt`，模块/文件/函数使用 `snake_case`，类型名使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`。
- TypeScript/React: 当前代码使用 4 个空格缩进，组件名使用 `PascalCase`，钩子使用 `useXxx`，`src/components/ui` 中的文件名使用 `kebab-case`。
- 自动生成的 API 产物必须保留在 `vite-project/src/services/` 下；请勿手动编辑生成的 hook/type 文件。

## 测试指南
- 优先在 `server/tests/` 下编写 crate 本地的单元测试和集成测试。
- 测试名称应描述其行为（例如：`test_rejects_invalid_turn_secret`）。
- 对于前端更改，至少要通过 `npm run build` 进行验证和手动流程检查（如果相关，请参阅 `vite-project/test_flow.mjs`）。

## 提交与拉取请求 (PR) 指南
- 遵循 Conventional Commits 规范 (`feat:`, `fix:`, `chore:`)，与近期的提交历史保持一致。
- 保持提交专注且功能独立；如果需要，请在同一次提交中包含配置/Schema的更新。
- PR 应包含：简明扼要的描述、受影响的模块（如 `server/service/signaling`）、测试/验证步骤、关联的 Issue，以及前端更改的 UI 截图。

## 安全与配置提示
- 绝不提交真实的凭证（credentials）；在本地开发时使用 `conf/config.toml` 中的占位符。
- 在审查涉及鉴权、信令、TURN 或文件传输路径的变更时，需要格外小心。

## 信令鉴权与多角色连接架构 (CRITICAL)
本项目具备复杂的四向信令连接机制。在处理逻辑时，必须严格遵守以下双轨鉴权规范：
- **Desk Server -> Local Signaling Server:** 仅在 `default` 模式下启动本地连接。通过自动生成并持久化的 `settings.system.local_signaling_token` 进行鉴权。
- **Desk Server -> Remote Signaling Server:** 通过 WebSocket URL 的 query 参数传递 `token` (`settings.system.signaling_token`) 进行身份验证。
- **Desk Server -> Manager Server:** 通过 WebSocket URL 的 query 参数传递 `token` (`settings.system.manager_api_token`) 进行身份验证（在 manager 数据库中验证其有效性）。
- **Browser -> Signaling / Manager Server:** 浏览器端连接信令时**不带任何 Token Query 参数**。它们**必须**回退通过会话 (Actix-Session Cookie) 进行认证。后端路由提取参数必须使用 `Option<web::Query<VersionInfo>>` 以兼容浏览器行为，且 manager 端的信令路由必须排除在全局 Session 拦截中间件之外。
