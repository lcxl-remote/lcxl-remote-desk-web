# 信令一致性处理规范 (Signaling Protocol)

为了确保新增功能时信令能够被正确转发和处理，必须遵循以下流程。

## 1. 后端定义 (signal-facade)
在 `signal-facade/src/model/signal.rs` 的 `SignalingType` 枚举中添加新类型。
- **注意**：必须为每一个新类型分配唯一的整数值。

## 2. 信令服务端转发逻辑 (signal service)
在 `signal/src/service.rs` 的 `handle_message` 函数中，必须处理新增的 `SignalingType`。
- **穷举检查**：由于移除了 `_` 分支，编译器会强制要求你处理所有类型。
- **处理方式**：
    - **通用转发**：如果只是简单的端到端转发，将其加入到 `// Forwarding types` 的联合分支中。
    - **业务逻辑**：如果 signal 需要解析内容（如校验权限、记录状态），请编写专属的 `match` 分支。
    - **禁止处理**：如果是仅由服务端发出的类型（如 `StartTerminal`），应在分支中调用 `log::warn!` 记录异常接收情况。

## 3. 前端同步 (vite-project)
1. **更新 OpenAPI**：运行 `/update_openapi` 脚本，确保前端生成最新的 `SignalingType` 常量。
2. **注册处理器**：在 `vite-project/src/features/desk/hooks/useDeskRTC.ts` (或相关特定功能的 Hook) 中，添加对新信令类型的 `onMessage` 处理逻辑。

---

> [!IMPORTANT]
> **绝对禁止**在 `signal/src/service.rs` 中重新引入 `_ => { ... }` 兜底分支，这会导致新增信令时失去编译期提醒。
