# 拆分日志设置 (Split Log Settings)

## 实现计划 (Implementation Plan)
1. **后端 (Rust) 改造**
   - 提取 `LogSettings` 数据模型，从 `SystemSettings` 中分离日志相关字段。
   - 在 `Settings` 结构体中添加 `log` 节点。
   - 实现并注册针对日志设置查询和更新的 REST API `GET /settings/log` 和 `POST /settings/log`。
   - 修改日志初始化、系统遥测任务和日志清理任务，使其正确引用新的 `settings.log` 配置。
2. **OpenAPI 客户端更新**
   - 重构后端完成后，重新生成 OpenAPI Json 配置，并使用 Kubb 在前端更新 TypeScript Types 以及 React Query Hooks。
3. **前端 (React) 改造**
   - 新建 `LogSettings` 页面，专门处理日志配置。使用新生成的 Hooks 进行数据读取和提交。
   - 修改 `SystemSettings`，移除遗留的日志配置项 UI。
   - 更新路由配置（添加 `/system/log`）。
   - 更新系统侧边栏，加入“日志设置”菜单。
   - 同步修改中英文多语言配置文件 (`zh-CN` / `en-US`)。

## 任务列表 (Task List)
- [x] 后端拆分 `SystemSettings` 与 `LogSettings` 模型。
- [x] 添加日志配置的默认值 (`Default` trait)。
- [x] 开发并注册 `/settings/log` 接口。
- [x] 替换后端全局代码中对日志和回溯选项配置的读取。
- [x] 确保后端容错处理（避免由于缺少配置文件引发崩溃）。
- [x] 重新生成 Vite 项目中的 OpenAPI 定义及客户端代码。
- [x] 新建 `vite-project/src/features/settings/log-settings.tsx`。
- [x] 删除 `vite-project/src/features/settings/system-settings.tsx` 中的日志配置字段。
- [x] 更新应用路由表及侧边栏。
- [x] 完成翻译文件更新。
- [x] 修复前端 Radix UI Select 组件不能被 React Hook Form 的 `reset` 正常触发回显的问题（通过 `key` 以及 `defaultValue` 的搭配）。

## 执行总结 (Walkthrough)
在实施过程中，首先从后端的配置模型入手，将日志级别 (`log_level`)、回溯配置 (`traceback`) 以及日志清理任务的阈值等提取到了新的 `LogSettings` 结构体下。接着编写并注册了新的 API 路由。修改了后端中几处引用到了原来 `system` 下的日志选项的代码，特别是系统遥测 (telemetry) 的初始化和异步定期清理任务。

在后端调整完成后，生成了最新的 `openapi.json`，并利用 Kubb 自动生成了前端客户端需要的 Types 和 Hooks (`useQueryLogSettings`, `useUpdateLogSettings`)。

前端层面，将日志表单的 UI 独立出来，并在 `SystemSettings` 中去除了相关的输入框和类型定义。更新了侧边栏菜单及路由，并且增加了必要的多语言支持。最后，在前后端联调测试时，发现了 Radix UI 中 `Select` 组件受到 React Hook Form 异步 `reset` 数据回填影响导致显示未及时刷新的问题，通过查阅相关处理方法，引入 `key` 等技术强制重置重渲染，彻底修复了状态显示同步的问题。最终通过 TypeScript 类型检查并顺利提交代码。