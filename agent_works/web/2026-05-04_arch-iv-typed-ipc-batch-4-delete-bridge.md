# Arch IV typed-IPC migration — batch 4: delete the SignalingMessage bridge

## 背景

批 1–3 已把所有 worker-bound 的 `SignalingType` 都迁到 typed IPC 变体。
此时 `ServiceToWorker::SignalingMessage` 与 `WorkerToService::SignalingMessage`
两个透明信封桥变体已没有任何 active 流量。批 4 把这套桥彻底删除：

- `ServiceToWorker::SignalingMessage(SignalingPayload)` ✗
- `WorkerToService::SignalingMessage(SignalingPayload)` ✗
- `SignalingPayload { message, connection_id }` struct ✗
- `RouteOutcome::ForwardToWorker` 枚举值（连同整个 `RouteOutcome` enum）✗

## 关键设计：错误响应改走通用 typed `SignalingError`

worker 端的 `service::signaling::DeskSession::send_error` 调用是先前桥仍承担的最后流量来源 ——
这些错误响应可能针对任何 `SignalingType`（如 `StartTerminal` 权限被拒、
`ManagerFileList` 读盘失败、handle_message 兜底 `_ => UNKNOWN_SIGNALING_TYPE` 等）。

如果为每个 `SignalingType` 各加一个 typed 错误变体，会产生 ~12 个
近似冗余的变体。改用一个通用的：

```rust
WorkerToService::SignalingError(SignalingErrorPayload {
    request_id: String,
    connection_id: String,
    #[bincode(with_serde)]
    signaling_type: SignalingType,   // 原请求类型，浏览器据此匹配响应
    error_code: i32,
    error_message: Option<String>,
})
```

worker 端 `build_outbound_payload_from_desk_text` 检测到
`response_state.error_code != 0` 时，无论原 `SignalingType` 是什么，统一构造
`SignalingError`。daemon `signaling_proxy` 在 `WorkerToService::SignalingError`
分支用 `SignalingModel::new_response(...)` 重建错误响应模型写回 ws。

## RouteOutcome 删除 + route() 简化

之前 `route()` 返回 `Result<RouteOutcome, RouterError>`，调用方判断
`HandledByDaemon` vs `ForwardToWorker`。批 4 后只剩前者，因此整个 enum
删除，`route()` 改返回 `Result<(), RouterError>`，调用方只需 `route(...).await?`。

`handle_inbound_signaling_text` 由原来 70 行（解析 + route + fallback +
转发到 worker）压缩到 15 行；`maintain_proxy_connection` 与 3 个
spawn 闭包不再需要 `worker_mgr` 参数（router_ctx 内已自带），一并删除。

## Error / Unknown 归宿

之前 `Error | Unknown` 在 `classify` 中是 `RouteOwnership::Worker`
（worker 兜底打日志）。批 4 后改为 `RouteOwnership::Daemon` 并加入 `route` 的
swallow 列表。理由：桥已没了；让 daemon 在源头打 trace 更直接，没必要
往 worker 兜一圈。

## 文件清单

| 文件 | 变更 |
|---|---|
| `web/ipc-protocol/src/message.rs` | 删 `ServiceToWorker::SignalingMessage` + `WorkerToService::SignalingMessage` + `SignalingPayload`；新增 `WorkerToService::SignalingError` + `SignalingErrorPayload`；新增 round-trip 测试；清理多处 doc 注释 |
| `web/server/src/daemon/signaling_router.rs` | 删 `RouteOutcome` enum；`route()` 改返回 `Result<(), RouterError>`；`Error \| Unknown` 移到 daemon-owned + swallow；删 `_ => ForwardToWorker` 兜底；更新 module-level doc 与所有测试断言 |
| `web/server/src/daemon/signaling_proxy.rs` | 删 `WorkerToService::SignalingMessage` 反向分支；新增 `WorkerToService::SignalingError` 反向分支（重建 `SignalingModel::error(...)`）；删 `handle_inbound_signaling_text` 的 fallback forward 路径；`maintain_proxy_connection` 删除 `worker_mgr` 参数 |
| `web/server/src/worker/session.rs` | 删 `ServiceToWorker::SignalingMessage` 入站分支；`build_outbound_payload_from_desk_text` 改返回 `Option<WorkerToService>`，对错误响应路径走 typed `SignalingError`，无对应 typed 路径时 log + drop；测试更新（断言 None / 断言新 typed `SignalingError`） |

## 测试

- `desk-ipc-protocol --lib`: 42 全过（41 → 42，新增 `signaling_error_round_trips_bincode`）
- `lcxl-remote-desk-server --lib`: 249 全过（248 → 249，新增
  `outbound_dispatch_routes_error_responses_to_typed_signaling_error`；改写
  `outbound_dispatch_drops_malformed_signaling_text` /
  `outbound_dispatch_drops_unrecognised_signaling_types` /
  `outbound_dispatch_manager_response_without_to_connection_is_dropped`
  从原来期待 `SignalingMessage` fallback 改为期待 `None`）

## 影响面

- IPC 协议总变体数：`ServiceToWorker` 22 → 21；`WorkerToService` 16 → 16（删 1 加 1）
- 桥相关结构体：`SignalingPayload` 删除
- daemon → worker：永远走 typed 变体，每个 SignalingType 一个明确的 helper
- worker → daemon：成功响应 / 通知走 typed 变体；错误响应走 `SignalingError`
- 浏览器侧（vite-project）的 `SignalingMessage` TypeScript 类型与本批无关（前端是 ws wire 类型，与 IPC 桥同名但不同物）

## 至此 Arch IV typed-IPC migration 完成

| 批次 | 内容 | 状态 |
|---|---|---|
| 0 | `AcceptControl` / `DenyControl` swallow | ✅ |
| 1 | `EnablePrivateScreen` / `UpdateDeskSettings` / `PrivateScreenStateChanged` typed + 3 swallow | ✅ |
| 2 | 5 个 manager 平面 typed + `ManagerSystemStatue` swallow | ✅ |
| 3 | 终端 8 个 typed | ✅ |
| 4 | 删 `SignalingMessage` 桥 + `RouteOutcome::ForwardToWorker` + 引入 `SignalingError` | ✅ |
