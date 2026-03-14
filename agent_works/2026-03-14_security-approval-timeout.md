# 任务归档：增加安全授权超时自动拒绝功能 (2026-03-14)

## 1. 任务背景
用户希望在权限确认弹窗（Security Approval Dialog）中增加超时自动拒绝功能。例如，当授权框弹出后，如果用户在 30 秒内未进行任何操作，系统应自动执行“拒绝”逻辑。此外，该超时时间需支持在“安全设置”页面进行自定义配置。

## 2. 实现计划 (Implementation Plan)
- **后端 (Rust)**:
    - 在 `signal-facade/src/model/security_settings.rs` 的 `SecuritySettings` 结构体中新增 `approval_timeout: Option<u32>` 字段（单位：秒）。
- **前端 (React + TypeScript)**:
    - **Schema 更新**: 
        - 手动更新 `vite-project/src/services/schemas/securitySettings.json` 以同步 OpenAPI 定义。
        - 更新 `vite-project/src/features/settings/security-settings.tsx` 中的 `securitySettingsSchema` (Zod)。
    - **设置页面**: 
        - 在 UI 中新增“授权行为”区块，并使用 `Select` 组件提供常见的超时选项（10s, 30s, 1m, 2m, 5m, Never）。
        - 修复 `Select` 组件在 `onValueChange` 时可能传入空字符串导致表单状态被错误重置为 `null` 的 Bug。
    - **授权弹窗**: 
        - 引入 `useQuerySecuritySettings` 钩子获取全局配置。
        - 使用 `useState` 维护 `timeLeft` 倒计时状态。
        - 通过 `useEffect` 实现秒级倒计时，并在归零时触发 `handleResponse(false)` 自动拒绝。
        - 在“拒绝”按钮上动态显示剩余秒数。
- **多语言 (i18n)**:
    - 同步更新 `en-US` 和 `zh-CN` 的 `pages.ts` 语言包。

## 3. 任务列表 (Task List)
- [x] **后端**: 修改 `SecuritySettings` 结构体，增加 `approval_timeout`。
- [x] **前端**: 更新 JSON Schema，确保前端代码生成与后端对齐。
- [x] **前端**: 在 `security-settings.tsx` 中增加超时配置 UI。
- [x] **前端**: 修复设置页面 `Select` 组件空值校验 Bug。
- [x] **前端**: 在 `security-approval-dialog.tsx` 中实现核心倒计时与自动拒绝逻辑。
- [x] **多语言**: 完成中英文翻译项的添加。

## 4. 执行总结 (Walkthrough)
1. **模型定义**: 后端 `SecuritySettings` 增加 `approval_timeout` 字段后，由于系统采用 TOML 持久化配置，无需进行数据库迁移。
2. **逻辑实现**: 
    - 弹窗逻辑利用了 React 的 `useEffect` 闭包特性。每当 `currentRequest` 变更时重置计时器，确保多个连续请求也能正确处理。
    - 在 `handleResponse` 的 `finally` 块中重置 `timeLeft` 为 `null`，防止计时器在无请求状态下继续运行。
3. **细节优化**: 
    - 设置页面针对 `Select` 的 `onValueChange` 进行了严谨的 `(val !== undefined && val !== null && val !== "")` 校验，解决了刷新页面导致配置回退为“从不”的 Bug。
    - UI 表现上，仅在设置了大于 0 的超时时间时才在按钮上显示倒计时，提升了用户体验。
