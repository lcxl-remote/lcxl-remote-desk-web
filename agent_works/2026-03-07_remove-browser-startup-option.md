# 移除“启动时打开浏览器”选项并增加 Windows 更新脚本

## 任务列表 (Task List)

- [x] 规划与架构分析 @2026-03-07
- [x] 后端修改: 移除 `SystemSettings` 中的 `open_browser_on_startup`
- [x] 后端逻辑: 移除 `lib.rs` 中的浏览器启动逻辑
- [x] 前端修改: 移除 `system-settings.tsx` 中的配置项与逻辑
- [x] 国际化清理: 移除多语言文件中的相关翻译键
- [x] 文档更新: 更新 `DEVELOPMENT.md` 与 `DEVELOPMENT_CN.md`
- [x] 脚本新增: 创建 `vite-project/update_openapi.ps1` 以支持 Windows
- [x] 工作流更新: 修改 `.agents/workflows/update_openapi.md` 以支持多平台
- [x] 接口同步: 运行更新脚本同步前端类型
- [x] 验证与测试

## 实现计划 (Implementation Plan)

### 后端 (Server)
- 修改了 `server/src/model/settings.rs`，从 `SystemSettings` 结构体中移除了 `open_browser_on_startup` 字段及其默认值。
- 修改了 `server/src/lib.rs`，移出了在服务器启动后调用 `webbrowser::open` 的逻辑。

### 前端 (Vite Project)
- 更新了 `vite-project/src/features/settings/system-settings.tsx`，移除了对应的 Zod 校验 Schema、表单默认值、重置逻辑以及 UI 渲染组件。
- 删除了 `vite-project/src/locales/zh-CN/pages.ts` 和 `vite-project/src/locales/en-US/pages.ts` 中相关的翻译条目。
- 新增了 `vite-project/update_openapi.ps1` 脚本，逻辑与 `update_openapi.sh` 保持一致，使用 PowerShell 实现。

### 工作流与文档
- 修改了 `.agents/workflows/update_openapi.md`，将更新步骤修改为支持多平台的脚本执行说明。
- 更新了 `DEVELOPMENT.md` 和 `DEVELOPMENT_CN.md`，移除了配置说明中关于该选项的引用。

## 执行总结 (Walkthrough)

我已成功从项目中完全移除了启动时自动打开浏览器的功能，并优化了跨平台开发体验。

### 验证结果
1. **后端验证**: 确认 `cargo run` 启动后不再自动打开浏览器。
2. **前端验证**: 设置页面中的开关已消失，且通过 `update_openapi.ps1` 成功同步了不含该字段的接口定义。
3. **脚本验证**: 在 Windows 环境下成功运行了新增加的 PowerShell 更新脚本。
