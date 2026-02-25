# 初始化页面修复与增强记录 (2026-02-25)

## 1. 任务目标

- **修复重定向循环**：解决初始化后用户被反复踢回 `/init` 的体验问题。
- **视觉与 UX 增强**：增加品牌欢迎语区域、微动效，并优化 Light/Dark 模式对比度。
- **功能补全**：添加遥测 (Telemetry) 勾选支持，标准化后端响应格式，并补全单元测试。
- **国际化**：完整支持中英文切换。

## 2. 实施计划 (Implementation Plan)

- **后端 (Rust)**: 修订 `init.rs` 中的 `InitParams` 增加 `telemetry_consent`，统一返回 `RestResponse` JSON。
- **前端 (React/TS)**:
  - 修改 `init-page.tsx` 支持新字段与 `react-i18next`。
  - 引入 `removeQueries` 主动清理缓存以破除状态滞后导致的循环。
  - 结合 `tailwindcss-animate` 实现视觉增强。
- **类型安全**: 手动同步前端 `InitParams` 定义。

## 3. 最终验收实录 (Walkthrough)

### 3.1 核心修复与优化点

- **重定向循环破除**:
  - 现象：`/init` (成功) -> `/` -> `/user/login` (触发逻辑) -> `/init`。
  - 根因：`LoginPage` 由于缓存滞后，瞬间读取到 `initialized: false` 而触发回退。
  - 解决：在成功回调中显式调用 `queryClient.removeQueries`。
- **可见度修复**: 调整欢迎语颜色为 `text-slate-900 dark:text-white`，提升 Light 模式识别度。
- **协议标准化**: 后端由 `Empty Response` 切换为 `RestResponse` JSON，消除 API Hook 解析异常。

### 3.2 验证结果

- [x] 后端 `controller::init::tests` 单元测试全部通过。
- [x] 前端视觉效果符合预期，动效流畅。
- [x] 初始化成功后正确导航至首页。

## 4. 任务列表进度 (Task Status)

- [x] 修改后端 Init 接口支持遥测项目
- [x] 标准化响应格式并更新单元测试
- [x] 前端增加缓存主动清除策略
- [x] 视觉与体验重构 (i18n, 动效, 对比度)
- [x] 验证全流程业务闭环

## 5. 提交信息

`fix: fully enhance init page with visual optimization, telemetry and redirection fix`
