# 任务归档：使用 Tabs 重构远程桌面配置对话框

## 1. 实施计划 (Implementation Plan)
### 目标
通过使用 `Tabs` 组件对配置项进行分组展示，减少单页高度，优化在小屏幕上的交互体验，解决“连接”按钮被遮挡的问题。

### 关键文件
- `vite-project/src/features/desk/desk-config-dialog.tsx`
- `vite-project/src/locales/zh-CN/pages.ts`
- `vite-project/src/locales/en-US/pages.ts`

### 实施方案
1.  **引入组件**：导入 `Tabs`, `TabsList`, `TabsTrigger`, `TabsContent` 组件。
2.  **重构表单结构**：
    *   **显示 (Display)**：包含捕获模式、显示设备、鼠标显示、分辨率缩放、自适应分辨率。
    *   **音频 (Audio)**：包含音频开关、捕获模式、设备选择、音频编码器。
    *   **高级 (Advanced)**：包含视频编码器、Wayland 控制模式、视频质量、最大 FPS。
3.  **样式优化**：为 `DialogContent` 添加 `max-h-[90vh]` 和 `overflow-y-auto` 属性。
4.  **i18n 支持**：添加页签所需的翻译字段。

## 2. 任务列表 (Task List)
- [x] 研究现有的 `DeskConfigDialog` 结构和 `Tabs` 组件用法。
- [x] 更新 `DeskConfigDialog` 导入和主体逻辑。
- [x] 移除 `Accordion` 相关冗余代码。
- [x] 添加 i18n 翻译 (zh-CN, en-US)。
- [x] 验证表单提交的数据完整性。

## 3. 执行总结 (Execution Walkthrough)
1.  **代码重构**：成功将 `DeskConfigDialog` 中的所有配置项归类到三个逻辑分明的页签中。相比原来的长列表，Tabs 方式极大地节省了纵向空间。
2.  **可用性提升**：通过在 `DialogContent` 上设置 `max-h-[90vh]` 和 `overflow-y-auto`，确保了即便是极小的浏览器窗口，用户也始终可以滚动到底部点击“连接”或“取消”按钮。
3.  **多语言同步**：为页签新增了相应的 i18n Key，确保了中英文环境下的显示一致性。
4.  **细节优化**：
    *   将 `audio_encoder` 从高级设置中移动到了“音频”页签下，分类更合理。
    *   移除了原有的 `Accordion` 折叠交互，降低了用户的认知负担。
