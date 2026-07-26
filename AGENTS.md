# AGENTS.md

本文件旨在为各类 AI 编码 Agent 在处理本仓库代码时提供指导。

## 项目概述

`lcxl-remote-desk` 是一个 **AI 原生（AI-Native）** 的开源 WebRTC 远程桌面解决方案，把 AI 当作与浏览器并列的一等控制端。后端使用 Rust (Actix-Web)，前端使用 React + TypeScript (Vite)。除了浏览器远程控制，它还内置了一个**设备诊断 AI Agent**（模型无关：OpenAI 兼容 / Anthropic）——既能读取设备状态排障，也能在**设备 owner 逐条明确确认**后执行命令（见 `agent-protocol/src/exec.rs` 与 `signal/src/agent_exec.rs`）；同时以一个**只读 MCP 服务**把设备的只读能力开放给外部 AI 助手。

> **"AI 只读"是已作废的旧描述**：诊断 Agent 早已不是只读（owner 确认后可执行）。**只读的是 MCP 服务**（静态白名单，不含 exec / write / control 工具）。写文档或对外文案时不要再复述"只读诊断 AI Agent"。

`server` 二进制文件支持多种运行模式：完整模式 (`default`)、仅信令模式 (`signaling`)、仅被控端模式 (`desk-server`)、系统服务守护进程模式 (`service-daemon`)、会话工作进程模式 (`session-worker`)，以及只读 MCP stdio 模式 (`mcp-stdio`)。

## 构建与运行

```bash
# 后端
cargo run -p lcxl-remote-desk-server                  # default (完整) 模式
cargo run -p lcxl-remote-desk-server -- --help         # 查看所有启动标志
cargo build --workspace --release
cargo test --workspace

# 工具链已由 rust-toolchain.toml 钉死(1.90.0)、rustfmt.toml 设 edition 2024，全仓已 fmt-clean；
# 提交前直接 `cargo fmt --all`(不会有版本漂移)，裸 `rustfmt <file>` 也无需再带 --edition。
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# 前端
cd vite-project && npm ci && npm run dev               # 开发服务器 (默认端口 :5174)
cd vite-project && npm run build                       # 类型检查 + 构建

# 后端 API 变更后，重新生成前端客户端代码（离线 dump，无需运行中的 server）
# Windows: cd vite-project && .\update_openapi.ps1
# Linux/macOS: cd vite-project && ./update_openapi.sh
# (脚本把离线 spec 写入系统临时文件，交给 Kubb 后自动删除；仓库不跟踪 openapi.json)
```

### Linux 系统依赖

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev \
  libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev \
  clang libclang-dev cmake libvpx-dev
```

### API 文档 (离线生成)

运行时**不再**提供 Swagger UI / ReDoc / RapiDoc / Scalar 与 `/openapi.json`（无鉴权、公网自建会暴露 API 攻击面，且前端客户端已走离线生成）。如需查看规范，用离线子命令本地生成：`cargo run -p lcxl-remote-desk-server -- dump-openapi --out openapi.json`。

## 模块概览

| 模块 | 角色 |
|---|---|
| `server/` | Desk server: REST API (Actix-Web), WebRTC, 设置, 文件/终端管理（支持 ServiceDaemon 和 SessionWorker 模式）；AI 诊断编排器在 `server/src/diagnose/`（采集 → 脱敏 → 模型 → 渲染），模型适配在 `server/src/diagnose/model/`（`openai.rs` / `anthropic.rs`） |
| `signal/` | 信令服务器 + TURN (核心文件: `signal/src/service.rs`) |
| `vite-project/` | React 19 + TanStack Query 前端 — 包含管理 UI 和 Web 控制端客户端（含 AI 设置页与诊断面板 `features/desk/diagnose-panel.tsx`） |
| `tauri-app/` | Tauri 壳程序，用于在被控机本地渲染防窥屏/白板功能 |
| `agent-protocol/` | 设备能力协议（`desk-agent-protocol`）：AI 调用的 wire 类型 + `DeviceAgent` trait + 审计 / 诊断 / exec 协议。纯协议、无平台实现；**服务端是所有受信字段的唯一可信源** |
| `mcp-server/` | 只读 MCP 服务（`desk-mcp-server`）：基于官方 `rmcp` SDK + stdio，暴露只读工具静态白名单（无 exec/write/control 工具）。具体读 agent 与诊断编排器由 `server` 注入 |
| `signal-facade/` | 共享的信令协议模型 (供 `signal` 和 `manager` 使用) |
| `capture-engine/`| 屏幕/音频捕获与编码逻辑 |
| `input-injection/`| 鼠标/键盘输入注入与剪贴板控制 |
| `ipc-protocol/` | 用于 Service ↔ Worker 之间通信的 IPC 消息定义 |
| `virtual-display/` | 虚拟显示器（IddCx）用户态抽象（`desk-virtual-display`，trait + Windows IDD impl + 其他平台 stub） |
| `virtual-display-driver-ops/` | 虚拟显示驱动安装/卸载操作封装（`desk-virtual-display-driver-ops`，pnputil 等） |
| `server-user/` | 服务端用户/账户模型 |
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

## AI Native / Agent 架构

本项目把 AI 当作与浏览器并列的一等控制端。涉及 AI / Agent 代码时遵循以下不变量（**安全相关，破坏即回归**）：

- **服务端是唯一可信源**：`request_id` / `target` / `actor` / `scope` / `caller` / 最终 `risk` / `approval_id` 全部由服务端注入与校验，控制端永远无法自报——浏览器侧请求体 `AgentRequestData` 在结构上就不含这些字段（见 `agent-protocol/src/lib.rs`）。
- **能力协议面向设备、与控制端无关**：`agent-protocol` 是纯协议 crate（wire 类型 + `DeviceAgent` trait），描述「对设备能做什么」，不关心调用来自浏览器 / android / MCP。读操作的权限点由输入**派生**（`OperationInput::capability()`），杜绝能力、采集分发、审计三者漂移。
- **默认只给建议，执行须 owner 逐条确认**：`ExecutionMode` 默认 `SuggestOnly`（模型只能建议命令、不能执行）。要真正执行必须走服务端中介的确认闭环（`agent-protocol/src/exec.rs`：suggest → confirm → execute → backfill）——服务端做风险分级与黑名单硬拒，把批准的命令冻结成 `ExecPlan`（program + argv，参数已绑定、无 shell 元字符），**worker 只按 argv 逐字执行、从不重新解析命令字符串**，每次真实执行都由服务端铸出 `approval_id`。
- **脱敏 fail-closed**：诊断编排器（`server/src/diagnose/mod.rs`）按 **采集 → 脱敏 → 模型 → 渲染** 运行；脱敏失败会在调用模型**之前**中止。证据在到达模型 trait 之前一定已脱敏。
- **API Key 是服务端密钥**：AI 模型 api_key 绝不回传浏览器、不进任何 `/settings` 公开 DTO、不写日志。审计只记录无内容的摘要（计数 / 大小 / token 用量 / provider / adapter），绝不留存原始输出、截图或 prompt。
- **模型无关**：`server/src/diagnose/model/` 用 adapter 隔离 wire 协议（`openai.rs` = OpenAI 兼容、`anthropic.rs` = Anthropic Messages），按调用解析 provider。新增供应商 = 新增一个 adapter，不改编排器。
- **MCP 只读**：`mcp-server` 工具集是**静态白名单**，刻意不存在 exec / write / control 工具（「未定义即不可达」）；`lcxl_diagnose` 的 provider 签名不带截图选项，MCP 客户端在结构上无法抓屏。`mcp-stdio` 模式下 stdin/stdout 承载 MCP JSON-RPC，**绝不能向 stdout 打日志**。

## 前端规则

- **国际化 i18n (强制):** 所有用户可见的文本必须使用 `useTranslation()` / `t()` — 禁止硬编码字符串。每个新键必须同时添加到 `vite-project/src/locales/zh-CN/pages.ts` **和** `vite-project/src/locales/en-US/pages.ts`。
- **自动生成代码:** `vite-project/src/services/` 下的文件（Kubb 输出）是自动生成的 — 请勿手动修改。
- **Tauri IPC:** 通过外部 HTTP URL 加载的 Windows 失去 `__TAURI_INTERNALS__`。绝不能从前端页面调用 `invoke()` 或 `listen()`。前端调用 Rust 请使用 REST API；Rust 触发前端事件请使用 `window.eval()` + `dispatchEvent`。使用原生的 `window.addEventListener` 进行监听。

## 代码规范与规则

1. **工作语言规则：所有回复、思考过程及任务清单，均须使用中文。**
2. **Rust:** 使用 `rustfmt` 格式化（工具链由 `rust-toolchain.toml` 钉死 1.90.0、`rustfmt.toml` 设 `edition = "2024"`，全仓已建立基线且 `cargo fmt --all --check` 全绿；提交前 `cargo fmt --all` 即可，无版本漂移，裸 `rustfmt <file>` 也无需 `--edition`）。函数/模块名使用 `snake_case`，类型名使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`。
3. **TypeScript/React:** 4 个空格缩进，组件名使用 `PascalCase`，钩子名使用 `useXxx`，`src/components/ui` 中的文件名使用 `kebab-case`。
4. **代码注释**必须使用**英文 (English)**。
5. **注释只描述当前代码 (CRITICAL)**：注释只能说明代码**当前**的行为、意图与约束，**禁止保留开发阶段标记**——例如 `PR-A` / `PR 6` / `cut 4` / `Cut 5` / `phase-1` / `Arch III` / `Arch IV` / `batch 2` 这类指代某次 PR、某个开发阶段或某代架构的字样。这些标记对读代码的人毫无意义、只会造成困惑。改写时把"曾经如何、某阶段会做什么"重述为对现状的客观描述（如需保留历史背景，用 "previously" / "legacy" / "an earlier design" 等中性措辞，不要带阶段代号）。**例外**：描述算法/状态机当前运行步骤的 "Phase 1/2/3"、"Step N" 属于对当前逻辑的说明，可保留。
6. **Git 提交信息**必须使用**英文 (English)**，并遵循 Conventional Commits 规范 (`feat:`, `fix:`, `chore:`)。
7. **测试用例 (CRITICAL)**：更改代码必须要增加测试用例。
8. **代码改动须同步核实文档 (CRITICAL)**：任何改变**用户可见行为**的代码改动（功能、启动模式、配置参数、API / 信令语义、AI 诊断 / MCP、安全模型等），**完成后必须核实文档站 `docs/`（VitePress）是否需要同步更新，保证代码与文档一致**。该站点是**双语**（英文为默认 root，中文在 `docs/zh/`），改动须**中英两版同步**；仅当确认无需改文档时才算完成，判断“是否需要”时宁可多核实一遍。
