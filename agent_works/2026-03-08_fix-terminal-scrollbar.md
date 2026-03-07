# 2026-03-08 优化远程终端滚动条

## 实施计划 (Implementation Plan)

修复远程终端及布局中的高度计算问题，消除页面双滚动条，确保终端完美适配视口。

### 拟议变更

#### 前端项目 (vite-project)

##### [MODIFY] [layout.tsx](file:///home/lcxl/code/lcxl-remote-desk-web/vite-project/src/features/layout/layout.tsx)

- 移除内容区域的 `min-h-screen`，改用 `flex-1` 填充。
- 添加 `overflow-hidden` 和 `relative` 定位，为嵌套组件提供明确的高度约束。

##### [MODIFY] [terminal-session.tsx](file:///home/lcxl/code/lcxl-remote-desk-web/vite-project/src/features/terminal/terminal-session.tsx)

- 将高度单位从 `h-screen` (100vh) 替换为 `h-full` (100%)。
- 为终端挂载点添加绝对定位包裹层，打破 `fitAddon.fit()` 导致的高度自适应死循环。

---

## 任务列表 (Task List)

- [x] 调研终端组件及其布局 <!-- id: 0 -->
- [x] 制定修复方案 <!-- id: 1 -->
- [x] 实施修复（优化高度计算与溢出处理） <!-- id: 2 -->
- [x] 排查并修复终端高度无限增长问题 <!-- id: 4 -->
- [x] 验证修复结果 <!-- id: 3 -->

---

## 执行总结 (Walkthrough)

成功修复了远程终端页面的双滚动条问题，并解决了因 `ResizeObserver` 与 `fitAddon.fit()` 冲突导致的高度暴走 bug。

### 核心修复策略

通过明确的 CSS 布局加固（绝对定位 + `overflow-hidden`），确保终端组件始终在受限的父级空间内进行尺寸适配。

### 验证结果

- [x] 页面整体不再出现多余的垂直滚动条。
- [x] 终端内部滚动功能正常。
- [x] 布局在缩放视口时保持自适应。
- [x] 进入终端后高度不再无限增长。
