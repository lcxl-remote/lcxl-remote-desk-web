# Tauri 与外部 Web URL 集成规范

本项目中，Tauri 的 `main` 和 `whiteboard` 等窗口主要是为了在被控端本地显示控制相关界面（如隐私屏、白板、安全审批弹窗等），而且前端的页面资源是通过本地 Server 暴露的 localhost / 127.0.0.1 外部链接载入的，而不是打包在 `tauri://` 协议中的预编译静态文件。

## 核心痛点：`__TAURI_INTERNALS__` 的丢失

当 Tauri `WebviewWindow` 加载**外部 HTTP/HTTPS 链接**时，由于跨域和安全上下文隔离原因，Tauri 的默认 IPC 注入代码（如 `__TAURI_INTERNALS__`）默认无法生效。这意味着你在该前端网页中执行任何原生的 Tauri API 将直接报错或无响应，例如：

- `@tauri-apps/api/core` 下的 `invoke`
- `@tauri-apps/api/event` 下的 `listen` / `emit`

## 开发铁律

为了防止反复踩坑，涉及到 Tauri 和前端（尤其是由于弹窗触发的通信）的整合开发时，必须遵循以下规则：

1. **不要在被外部 URL 加载的前端组件中依赖 Tauri API：** 前端页面无论运行在浏览器里还是 Tauri 窗口里，都不应该调用 `invoke`。
2. **从前端调用 Rust：** 应当通过 Rust 后端（Actix-Web）提供的标准 **REST API** 发送请求。后端接管逻辑并在必要时通过进程内共享的数据（如 `Arc<Mutex<...>>`）与其他 Rust 模块完成同步。
3. **从 Rust 唤醒前端：** 如果 Tauri 后端需要向前端派发事件（例如让前端显示某个弹窗），不能使用 `app_handle.emit()`。必须获取对应的 `WebviewWindow` 对象后调用 `eval()` 执行 JavaScript 强制 dispatch：
   ```rust
   if let Some(window) = app_handle.get_webview_window("window-label") {
       let safe_json = serde_json::to_string(&payload).unwrap_or_else(|_| "\"\"".to_string());
       let script = format!(
           "window.dispatchEvent(new CustomEvent('my-custom-event', {{ detail: {} }}));",
           safe_json
       );
       let _ = window.eval(&script);
   }
   ```
4. **前端响应后端事件：** 在 React 组件中应当使用标准的原生 `window.addEventListener('my-custom-event', handler)` 来收听 Rust 派发的数据。

> 请所有参与迭代的大模型，在涉及 tauri-app 和 vite-project 跨越交互的任何环节优先查阅此规范！
