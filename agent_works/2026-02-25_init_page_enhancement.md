# 初始化页面修复与增强工作记录 (2026-02-25)

## 任务目标

1. **国际化补全**：为系统初始化页面提供完整的 `zh-CN` 和 `en-US` 支持。
2. **导航逻辑修复**：初始化成功后自动跳转至首页 (`/`)。
3. **遥测功能集成**：在初始化页面添加遥测选项勾选框，默认状态为关闭。
4. **质量保障**：为后端接口添加单元测试。

## 实施过程

### 1. 后端增强 (Backend)

- **文件**: `server/src/controller/init.rs`
- **变更**:
  - `InitParams` 结构体新增 `telemetry_consent: bool`。
  - `init_system` 处理函数现将遥测设置同步至 `SystemSettings`。
  - **单元测试**: 增加了 `test_init_system_success` 和 `test_init_system_already_initialized`，验证了初始化流程的正确性和安全性。

### 2. 国际化资源 (I18n)

- **文件**: `vite-project/src/locales/zh-CN/pages.ts` / `en-US/pages.ts`
- **变更**: 补全了 `pages.init.*` 命名空间下的所有翻译键，涵盖标题、描述、表单占位符、校验信息及遥测选项说明。

### 3. 前端 UI 与逻辑 (Frontend)

- **文件**: `vite-project/src/features/auth/init-page.tsx`
- **变更**:
  - 引入 `Checkbox` 组件支持遥测勾选。
  - 修复 Zod 验证信息的硬编码问题，实现全面国际化。
  - 初始化成功后的跳转目标修复为 `/`。
- **文件**: `vite-project/src/services/types.ts`
  - 手动更新了 `InitParams` 类型定义以匹配后端变更。

## 验证结论

- **后端**: 单元测试通过，确保了拦截逻辑映射正确（200 OK + `success: false` 模式）。
- **前端**: 视觉验证中英文文案显示正常，表单交互符合预期，成功跳转逻辑已生效。

## 提交信息

`fix: enhance init page with i18n, telemetry consent and correct navigation`
