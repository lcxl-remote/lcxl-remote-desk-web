# 归档记录：安全确认框置顶提醒功能实现

## 任务目标
提高安全确认框（Security Approval Dialog）在被控端程序不在前台时的可见性。当有新的控制请求（如远程控制、剪贴板同步等）到来时，强制主窗口置顶显示，直到用户处理完所有挂起的请求或请求超时。

## 实现方案
基于后端信道（Channel）的闭环控制方案，避免前端直接调用 Tauri IPC：
1. **模型定义**：在 `server` 层定义 `SecurityApprovalCommand` 枚举（`Request` 和 `Finish`）。
2. **逻辑触发**：
    - 服务端（`signaling.rs`）在发起审批请求时发送 `Request` 指令。
    - 审批处理接口（`settings.rs`）在用户提交响应后检查待处理队列，若队列清空则发送 `Finish` 指令。
3. **窗口管理**：`tauri-app` 中的 `SecurityApprovalManager` 监听信道，收到 `Request` 时设置 `always_on_top(true)`，收到 `Finish` 时恢复 `always_on_top(false)`。

## 修改文件清单

### 后端 (server)
- `server/src/model/security_approval.rs`: 定义 `SecurityApprovalCommand` 枚举及更新信道类型。
- `server/src/service/signaling.rs`: 发送 `Request` 指令，并在异常/超时清理队列后适时发送 `Finish`。
- `server/src/lib.rs`: 将 `security_approval_sender` 注入 Actix-Web app data。
- `server/src/controller/settings.rs`: 在提交审批处理函数中，若队列清空则发送 `Finish` 指令。

### Tauri 客户端 (tauri-app)
- `tauri-app/src-tauri/src/security_approval.rs`: 扩展监听循环以处理 `Request` 和 `Finish` 指令，动态控制窗口置顶状态。

## 执行总结
- **验证通过**：经 `cargo check` 确认，后端服务与 Tauri 客户端的类型定义和逻辑调用完全匹配。
- **架构对齐**：方案严格遵守了“外部加载前端不直接调 `invoke`”的规范，通过后端进程内信道实现了 UI 状态的强制控制。
- **用户体验**：解决了审批框在后台时难以被察觉的问题，通过物理置顶增强了视觉提醒，且在任务完成后能自动释放置顶，不干扰用户后续操作。

## 后续建议
- 目前 `signaling.rs` 和 `model/security_approval.rs` 中存在重复的权限检查逻辑，建议后续合并以提升可维护性。
