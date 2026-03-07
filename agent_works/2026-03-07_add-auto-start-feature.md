# 2026-03-07: 实现应用程序开机自启动与后台运行功能 归档

## 1. 任务清单 (Task List)

- [x] 调查自启动实现方案（后端/Tauri）
- [x] 在 `server/src/model/settings.rs` 中的 `SystemSettings` 添加 `auto_start: bool` 字段，`Args` 添加 `hidden: bool`
- [x] 修改 `tauri-app/src-tauri/src/lib.rs`，如果是 `hidden` 启动，则不在加载完成后展示主窗体
- [x] 使用 `auto-launch` crate 在系统设置更新时实现自启动逻辑（带 `--hidden` 参数）
- [x] 运行 OpenAPI 更新脚本 (`/update_openapi`) 同步前后端接口
- [x] 更新前端 `system-settings.tsx` UI 组件
- [x] 在 Windows 上进行验证测试


## 2. 实现方案 (Implementation Plan)

### 后端服务 (Rust)
- 在 `[dependencies]` 中添加 `auto-launch = "0.5.0"` 依赖。
- 在 `SystemSettings` 结构体中添加 `pub auto_start: Option<bool>` 字段。
- 在 `Args` 结构体中添加 `pub hidden: bool` 字段，以支持命令行隐式启动 `#[arg(long)]`。
- 在 `tauri-app` 中读取 `settings.args.hidden`，并在 `on_page_load` 回调中控制 `window.show()` 和 `window.set_focus()` 的调用逻辑，实现后台只在托盘运行。
- 创建 `auto_start.rs` 使用 `auto-launch` crate 配合 `std::env::current_exe()` 和参数 `&["--hidden"]` 配置当前程序的开机自启动。
- 在 `update_settings` 接口中，检测到配置修改时主动调用更新系统自启动状态。

### 前端项目 (Vite + React)
- 运行 `/update_openapi` 对应的脚本自动同步 OpenAPI 声明及前端 Typescript 接口文件。
- 在系统设置页面 UI 中添加“开机自启动”的开关，绑定后端数据。
- 修复因大整数(`bigint`)引发的前端强类型报错，为前端增加了国际化 (i18n) 语言文本配置 (`Auto-Start at Login`)。


## 3. 执行总结 (Walkthrough)

此功能增强使得应用具备了注册当前操作系统后台驻留及开机自动启动的能力。

### 主要变更内容：
1. **Backend & CLI Updates (`server/src/model/settings.rs`)**
   - 新增 `auto_start: Option<bool>` 到 `SystemSettings`。
   - 新增了 `hidden: bool` flag 以方便通过 `--hidden` 启动进程。

2. **Auto-Start Service (`server/src/service/auto_start.rs`)**
   - 依赖集成： `auto-launch` crate 提供跨主流系统的后台启动项托管支持。
   - 包含的执行指令为 `{执行文件路径} --hidden`，确保下次自启时完全透明隐式。

3. **Settings Controller Hook (`server/src/controller/settings.rs`)**
   - `/api/desk/settings/system` 控制器新增联动。触发更新且 `auto_start` 被启用或关闭时，同时注册或取消注册 OS 自启动项。

4. **Tauri Headless Launch (`tauri-app/src-tauri/src/lib.rs`)**
   - 控制主窗口仅仅当没有 `--hidden` 参数时才正常显示初始窗体。

5. **Frontend UI & OpenAPI (`vite-project`)**
   - 生成了对齐后端数据的新 TypeScript 接口。
   - 在 `system-settings.tsx` 新增了“Auto-Start at Login” 开关组件，并修复了页码相关 `bigint` TypeScript 编译警告。
   - 新增了前置规则 `.agents/rules/frontend-i18n.md` 实现后续所有前端开发时强制检查和支持多语言属性 (`pages.system.settings.auto_start`)。

### 验证：
- **浏览器交互**：在前端设置界面点击自启开关时，数据正常持久化与回显。
- **系统验证**：在 Windows 平台上进行实机拉起，修改前端开关后正常操作到了系统的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 注册表项。
