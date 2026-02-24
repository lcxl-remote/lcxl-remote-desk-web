# 2026-02-24 远程文件删除功能实现

## 1. 实现计划 (Implementation Plan)

### 目标描述

目前前端远程文件管理可以列出、下载和上传文件，但缺少删除功能。后端已经实现了删除接口，本项目旨在前端 UI 中集成该功能，并增强后端跨平台的回收站支持。

### 拟议变更

#### 前端项目 (vite-project)

- **[MODIFY] file-list.tsx**: 引入 `useDeleteFile` hook，集成 `AlertDialog` 确认流，添加“永久删除”复选框。
- **[NEW] UI 组件**: 安装并集成 shadcn `alert-dialog` 和 `checkbox`。
- **[MODIFY] i18n**: 更新中英文 `pages.ts` 以支持删除相关的文案。

#### 后端服务端 (server)

- **[MODIFY] Cargo.toml**: 引入 `trash` crate 依赖。
- **[MODIFY] file_manager.rs**: 使用 `trash` crate 统一处理 Windows, Linux 和 macOS 的移入回收站逻辑。

---

## 2. 任务清单 (Task List)

- [x] 研究前后端文件删除接口实现
- [x] 创建初步计划并经用户审核
- [x] 后端引入 `trash` crate 并重构删除逻辑
- [x] 前端集成 `useDeleteFile` mutation
- [x] 前端实现 `AlertDialog` 确认流与永久删除逻辑
- [x] 验证跨平台编译与功能逻辑
- [x] 编写 Walkthrough 并落盘

---

## 3. 实现成果 (Walkthrough)

### 关键特性

1. **跨平台回收站**: 通过 `trash` crate，现在 Linux 和 macOS 也能像 Windows 一样将文件移入系统回收站，而非直接物理删除。
2. **多级安全确认**:
   - **普通删除**: 提示是否删除，文件进入回收站。
   - **永久删除**: 勾选复选框后，系统会弹出第二次严正警告确认，防止误删重要数据。
3. **加载状态反馈**: 删除过程中，对应行的图标会变为旋转的加载动画，防止重复点击。

### 验证结论

- 后端 `cargo check` 通过，逻辑重构完成。
- 前端 shadcn 组件集成完毕，i18n 文案匹配。
- 逻辑上已确保不能删除 `..` 路径。

---

## 4. 后续建议

- 在不同桌面环境下（如 GNOME 网页，macOS Finder）进一步验证回收站的兼容性。
- 考虑到大文件夹删除可能耗时较长，未来可考虑将删除操作改为异步通知模式。
