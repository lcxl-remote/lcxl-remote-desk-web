# 前端国际化 (i18n) 开发规范

在进行前端页面的开发、修改或添加新的用户可见文本时，**必须**严格遵守本国际化（i18n）规则。所有对用户可见的硬编码字符串都必须进行多语言适配，不得有任何遗漏。

## 规则详解
1. **禁止硬编码中文或英文本文**：涉及到用户界面中展示的任何文本（如按钮、标签、弹窗提示、表格列名、页面标题），禁止直接将其硬编码（Hardcode）在 React 组件 (`.tsx`/`.jsx`) 或其它前端逻辑代码中。
   
2. **提取和添加翻译 Key**：
   - 必须通过 `useTranslation` 钩子 (hook) 来获取 `t` 函数。例如:
     ```tsx
     import { useTranslation } from "react-i18next"
     // ...
     const { t } = useTranslation()
     ```
   - 所有的文本必须定义一个具有层次结构的唯一 Key。按照页面的功能模块进行划分（例如 `'pages.system.settings.auto_start'`）。
   - 在组件中**必须**提供后备默认文本。例如：`t('pages.system.settings.auto_start', '开机自动启动')` 或者 `t('pages.system.settings.auto_start', 'Auto-Start at Login')`。

3. **同步更新语言配置文件**：
   每当你添加了新的 i18n key 时，**必须同时更新所有受支持的语言配置文件**（至少包含中文和英文）。
   - 中文配置：`vite-project/src/locales/zh-CN/pages.ts`
   - 英文配置：`vite-project/src/locales/en-US/pages.ts`
   
   在更新这些文件时，务必保持 JSON/字典 键名的对齐与完整。

4. **预先检查**：如果在代码中遇到了新增的需求或反馈，在动手修改组件的 UI 前，应主动检查和分配对应的 i18n key 并在语言文件中提前定义好。
