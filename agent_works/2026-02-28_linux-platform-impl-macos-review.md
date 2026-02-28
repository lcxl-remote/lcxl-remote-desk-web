# Linux 平台隐私屏实现 + macOS Review

## macOS Review

**✅ 合理** — CGEventTap 输入拦截实现完善：OnceLock+Mutex 线程安全、mpsc 防竞态、事件 Null 吞掉、资源正确释放、失败降级。

## Linux 实现

### `tauri-app/src-tauri/src/platform/linux.rs`
使用 x11rb 实现 X11 输入拦截：
- `grab_keyboard(false, root, CURRENT_TIME, ASYNC, ASYNC)` + `grab_pointer` 
- `OnceLock<Mutex<Option<X11Grabber>>>` 持久化连接
- Wayland 降级跳过

### `server/src/service/system_setting/linux_system_setting.rs`
- `enable_private_screen` 签名对齐：添加 `from_session_id` 参数
- `PrivateScreenCommand::Show/Hide` 传入 session_id
- import 路径简化

## 验证
- Windows 编译通过 `cargo check -p lcxl-remote-desk-server` — 0 errors
- Linux/macOS 需在对应环境验证
