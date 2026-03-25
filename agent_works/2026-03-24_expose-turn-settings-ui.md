# Task: 透出 TURN 设置并在前端单独增加配置页面

## Implementation Plan

### 目标 (Objective)
将后端 `turn/src/model.rs` 中的 `TurnSettings` 在前端展示并支持编辑。同时，在左侧导航菜单中独立增加一个“Turn”设置项，并将原“系统设置”中的“TURN 安全配置”模块迁移到新的“Turn”设置页面。

### 涉及的核心文件 (Key Files & Context)
1. **后端 (Rust)**:
   - `turn/src/model.rs` (为 `TurnSettings` 增加 OpenAPI Schema 派生)
   - `server/src/controller/settings.rs` (新增接口 `query_turn_settings` 与 `update_turn_settings`)
   - `server/src/lib.rs` (注册新接口至路由表)
   - `server/src/openapi.rs` (注册 `TurnSettings` 到额外生成的 Schema)
2. **前端 (React + Vite)**:
   - `vite-project/src/features/settings/turn-settings.tsx` (新建的独立 Turn 设置页面)
   - `vite-project/src/features/settings/system-settings.tsx` (移除旧有的“TURN 安全配置”模块)
   - `vite-project/src/app/router.tsx` (注册新的 `/system/turn` 路由)
   - `vite-project/src/features/layout/app-sidebar.tsx` (在侧边栏的“设置”组下添加 `Turn` 子菜单)
   - 多语言配置文件 (`en-US/menu.ts`, `en-US/pages.ts`, `zh-CN/menu.ts`, `zh-CN/pages.ts`)

---

## Task List & Execution Walkthrough

### 阶段 1：后端 API 及模型修改
- [x] 为 `desk_turn::model::TurnSettings` 以及 `TurnInterface` 结构体添加 `ToSchema` 派生。
- [x] 在 `server/src/controller/settings.rs` 中新增了 `GET /api/desk/settings/turn` (查询) 和 `POST /api/desk/settings/turn` (更新) 控制器方法。
- [x] 在 `server/src/lib.rs` 路由表中注册了上述控制器方法。
- [x] 修改 `server/src/openapi.rs`，确保 `TurnSettings` 和 `TurnInterface` 包含在 OpenAPI Schema 中。
- [x] 清理后端因重构或迁移逻辑引发的“未使用导入(unused import)”编译警告（例如 `TurnSettings`, `LcxlRTCIceServer`, `TurnTransport`）。

### 阶段 2：OpenAPI 与前端客户端更新
- [x] 根据更新后的后端模型重新生成前端使用的 `openapi.json` 及 `src/services/` 下的相关客户端代码（由开发者本地通过脚本完成并提交）。
- [x] 前端成功获得了 `useQueryTurnSettings` 和 `useUpdateTurnSettings` 以及相关的类型定义。

### 阶段 3：前端界面与交互实现
- [x] **重构 SystemSettings**：从 `system-settings.tsx` 页面中清除了原有的 `TURN Security` Card（包含重置密钥逻辑）及 `useRegenerateTurnSecret` 引用。
- [x] **开发 TurnSettings 新页面**：
  - 创建了 `turn-settings.tsx`，使用 `react-hook-form` 与 `zod` 对配置结构（包括 Realm、中继端口范围、STUN/TURN 开关等）进行校验和绑定。
  - 使用 `useFieldArray` 实现了对 `interfaces`（网络监听与外部地址配置）的动态增删改查。
  - 成功将原本属于系统设置中的 “重新生成 TURN 密钥 (TURN Security)” 功能迁移到了新页面的底部。
  - 保存时，执行了合并逻辑以避免丢失前端不可见的关键认证信息（如 `static_auth_secret`）。
- [x] **路由与导航栏**：
  - 在 `router.tsx` 的 `/system/` 下新增 `turn` 路由。
  - 在侧边栏导航组件 `app-sidebar.tsx` 内增加指向 `/system/turn` 的菜单项。

### 阶段 4：前端多语言补全 (i18n)
- [x] 补充了相应的语言词条：
  - **英文 (`locales/en-US`)**: 添加了 `menu.settings.turn` 和所有相关的 `pages.turn.settings.*` 定义。
  - **中文 (`locales/zh-CN`)**: 同步添加了 `menu.settings.turn` 对应的 "TURN 设置" 以及表单相关的中文定义。
- [x] 执行 `tsc --noEmit` 通过类型检查。

### 结果与验证
整个端到端流程已通过后端的 `cargo check` / `cargo build`，及前端的 `npm run build` 和 `npx tsc --noEmit` 校验。功能成功对齐最初透出 TURN 配置的需求并具备完整的独立视图。