---
description: 归档前端透出 TurnClientSettings 设置并根据启动类型控制设置菜单展示记录
---

## Implementation Plan

### 目标 (Objective)
将后端 `server/src/model/settings/turn_client.rs` 中的 `TurnClientSettings` 在前端展示并支持编辑。同时，根据 `startup_mode` 属性，在左侧导航菜单中动态控制相关设置页面的展示逻辑。

### 涉及的核心文件 (Key Files & Context)
1. **后端 (Rust)**:
   - `server/src/controller/settings.rs` (新增接口 `query_turn_client_settings` 与 `update_turn_client_settings`)
   - `server/src/lib.rs` (注册新接口至路由表)
   - `server/src/openapi.rs` (注册 `TurnClientSettings` 和 `TraversalMode` 到额外生成的 Schema)
2. **前端 (React + Vite)**:
   - `vite-project/src/features/settings/turn-client-settings.tsx` (新建的独立 Turn 客户端设置页面)
   - `vite-project/src/app/router.tsx` (注册新的 `/system/turn-client` 路由)
   - `vite-project/src/features/layout/app-sidebar.tsx` (根据 `startup_mode` 动态渲染侧边栏的“设置”子菜单)
   - 多语言配置文件 (`en-US/menu.ts`, `en-US/pages.ts`, `zh-CN/menu.ts`, `zh-CN/pages.ts`)

---

## Task List & Execution Walkthrough

### 阶段 1：后端 API 及模型修改
- [x] 在 `server/src/controller/settings.rs` 中新增了 `GET /api/desk/settings/turn-client` (查询) 和 `POST /api/desk/settings/turn-client` (更新) 控制器方法。
- [x] 在 `server/src/lib.rs` 路由表中注册了上述控制器方法。
- [x] 修改 `server/src/openapi.rs`，确保 `TurnClientSettings` 和 `TraversalMode` 包含在 OpenAPI Schema 中。

### 阶段 2：OpenAPI 与前端客户端更新
- [x] 后台启动 Rust `desk-server`。
- [x] 执行 `update_openapi.ps1` 同步并重新生成最新的前端 `kubb` 客户端代码。
- [x] 前端成功获得了 `useQueryTurnClientSettings` 和 `useUpdateTurnClientSettings` 以及相关的类型定义。

### 阶段 3：前端界面与交互实现
- [x] **开发 TurnClientSettings 新页面**：
  - 创建了 `turn-client-settings.tsx`，使用 `react-hook-form` 与 `zod` 对配置结构（包括 TraversalMode）进行校验和绑定。
  - 使用了 Radix UI Select，支持 `turn`、`stun`、`none` 三种枚举切换及持久化保存。
- [x] **路由与导航栏**：
  - 在 `router.tsx` 的 `/system/` 下新增 `turn-client` 路由。
  - 修改 `app-sidebar.tsx`：通过识别 `serverInfo.startup_mode` 属性，决定 `Turn Client设置`、`安全设置`、`TURN 设置` 以及 `设备码管理` 等子菜单项的注入逻辑。
    - `isDeskServer`（`default` / `desk-server`）成立的场景开放 `TurnClientSettings` 和 `SecuritySettings`。
    - `isSignaling`（`default` / `signaling`）成立的场景开放 `TurnSettings` 和 `DeviceCodeList`。

### 阶段 4：前端多语言补全 (i18n)
- [x] 补充了相应的语言词条：
  - **英文 (`locales/en-US`)**: 添加了 `menu.settings.turnClient` 和所有相关的 `pages.turnClient.settings.*` 定义。
  - **中文 (`locales/zh-CN`)**: 同步添加了 `menu.settings.turnClient` 对应的 "TURN 客户端设置" 以及表单相关的中文定义。
- [x] 修复了因合并代码导致的部分多语言字段名重复定义的编译错误。
- [x] 执行 `tsc --noEmit` 通过类型检查。

### 结果与验证
整个端到端流程已通过后端的 `cargo check` / `cargo build`，及前端的 `npx tsc --noEmit` 校验。功能成功对齐最初透出 TURN 客户端配置并优化页面展示逻辑的需求。
