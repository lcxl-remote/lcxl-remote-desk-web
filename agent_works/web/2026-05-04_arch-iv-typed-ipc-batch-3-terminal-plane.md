# Arch IV typed-IPC migration — batch 3: terminal plane

## 背景

Arch IV daemon-WebRTC 重构持续推进。前序批已完成：

- 批 0：`AcceptControl` / `DenyControl` swallow（清理）
- 批 1：`EnablePrivateScreen` / `UpdateDeskSettings` / `PrivateScreenStateChanged` typed + 3 个 swallow（`ChangeDisplaySettings` / `AudioPlaybackError` / `ManagerSystemStatue` 的预备 swallow 在批 2 落入）
- 批 2：5 个 manager 平面请求/响应 typed + `ManagerSystemStatue` swallow

留下来的 worker-owned 类型仍然走 `ServiceToWorker::SignalingMessage` /
`WorkerToService::SignalingMessage` 透明信封桥。批 3 把终端 8 个
`SignalingType` 全部迁到 typed IPC，桥上仅剩 `Error` / `Unknown` 两个安全网类型。

## 涉及的 8 个 SignalingType

**浏览器 → worker（请求）**

| SignalingType | payload | 是否需要响应 | 响应类型 |
|---|---|---|---|
| `StartTerminal` | `StartTerminalSession { command }` | 是 | `TerminalStarted` |
| `SendDataToTerminal` | `TerminalInputData { content }` | 否 | — |
| `ResizeTerminal` | `TerminalResizeData { rows, cols }` | 否 | — |
| `CloseTerminal` | 无 | 否 | （子进程退出时另发 `TerminalClosed`） |
| `ListTerminal` | 无 | 是 | `ListTerminal`（带 `TerminalList`） |

**worker → 浏览器（响应 / 通知）**

| SignalingType | payload | 触发点 | 与 request_id 关联 |
|---|---|---|---|
| `TerminalStarted` | 无 | `handle_manager_terminal_start` 中 `success_response` | 是 |
| `ReplyFromTerminal` | `TerminalOutputData { content }` | PTY reader 线程 `new_request` | 否 |
| `TerminalClosed` | 无 | monitor 任务 `new_request` | 否 |
| `ListTerminal`（响应方向） | `TerminalList` | `handle_list_terminals` 中 `send_response` | 是 |

## 设计决策

### 1. 终端反向通知归类为 daemon-owned + swallow

`ReplyFromTerminal` / `TerminalStarted` / `TerminalClosed` 只可能 worker
→ daemon → 浏览器。浏览器永远不会回送这些类型；如果出现，那是协议错误。
所以这三个类型：
- `classify` 中归为 `RouteOwnership::Daemon`
- `route` 中放入 swallow 列表（trace log + `HandledByDaemon`）

这与批 1 对 `PrivateScreenStateChanged` 的处理一致。

### 2. `ReplyFromTerminal` 高频但走 event pipe

PTY reader 线程的读缓冲是 1 KB，每次 read 最多发 1 KB chunk。即使在终端
快速滚动场景下（e.g. `dir /s C:\`），event pipe 的 P99 延迟仍远低于
媒体管道（POC 已证实双管道下 4K IDR 不会阻塞 event 流量）。**不**为
ReplyFromTerminal 单独开管道。

### 3. CloseTerminal 用 `dispatch_typed_signaling_with_request_id`

`CloseTerminal` 本身无响应，但用 `with_request_id` helper（传 `"typed-ipc"`
占位 + `Option::<&()>::None`）让所有需要从 worker 端 forward 的类型走
统一调用形式，方便日后日志/trace 分类。

### 4. 复用 `dispatch_typed_signaling*` 而不重写 handler

延续批 1+2 的思路：典型的 typed payload 不直接调 `handle_manager_terminal_*`，
而是 worker 端 `dispatch_typed_signaling_with_request_id` 把 typed payload
反序列化回 `SignalingModel`，喂给 `DeskSession::handle_message`，复用现有
`SignalingType::*` 分支。

理由：
- 现有 handler 在 portable / DeskServer WS 路径也用，重写会双份维护。
- 反向 `try_route_typed_outbound` 把 outbound `SignalingModel` 拍回 typed
  `WorkerToService::*` 完成闭环。

## 实施

### 文件清单

| 文件 | 变更 |
|---|---|
| `web/ipc-protocol/src/message.rs` | 5 个 ServiceToWorker + 4 个 WorkerToService 变体 + 9 个 payload struct + 8 个 round-trip 测试 |
| `web/server/src/daemon/signaling_router.rs` | 5 个 typed dispatch helper；3 个反向类型移到 daemon-owned + swallow；终端请求测试 |
| `web/server/src/daemon/signaling_proxy.rs` | 4 个 outbound 分支 + 新建 `send_terminal_notification` helper（new_request 形态） |
| `web/server/src/worker/session.rs` | 5 个 inbound 分支；`try_route_typed_outbound` 新增 4 个分支；4 个新单测 |

### 细节

**`StartTerminalRequestPayload`**：包含 `request_id` + `connection_id` + `session: StartTerminalSession`，daemon → worker 时 echo `request_id`，worker 端的 `handle_manager_terminal_start` 用这个 id 构造 `success_response::<TerminalStarted>`，`try_route_typed_outbound` 读 `model.request_id` + `model.to_connection_id` 还原回 `TerminalStartedPayload`，daemon `signaling_proxy` 用 `send_manager_response` 把它写回 ws。

**`SendDataToTerminalRequest` / `ResizeTerminalRequest`**：worker 端用 `dispatch_typed_signaling`（无 request_id），现有 `handle_manager_terminal_data` / `_resize` 直接生效。

**`CloseTerminalRequest`**：与 SendData / Resize 同理，但用 `with_request_id` helper（占位 id "typed-ipc"）。子进程被杀后 monitor 任务发 `TerminalClosed` `new_request`，`try_route_typed_outbound` 走 `SignalingType::TerminalClosed` 分支，daemon `send_terminal_notification` 把它当 `new_request` 写回 ws（不是 success_response — 因为这是服务端发起的通知）。

**`ListTerminalRequest`**：worker 端 `handle_list_terminals` 用 `send_response` 带 `TerminalList`。`try_route_typed_outbound` 读 `model.request_id` + `model.to_connection_id` + `TerminalList` 还原回 `ListTerminalResponsePayload`。daemon `send_manager_response` 写回 `SignalingType::ListTerminal` success_response。

**`ReplyFromTerminal`**：PTY reader 线程发 `new_request` 带 `TerminalOutputData`。反向走 `send_terminal_notification`（`new_request` 形态）。

### 测试

- ipc-protocol: 8 个新增 round-trip 测试覆盖每个 payload struct（含一个 4 KB chunk 的 ReplyFromTerminal 大块测试） — 41 测试全过
- signaling_router: `route_terminal_requests_handled_inline_not_bridged`（5 个请求类型）+ `route_terminal_request_without_connection_id_is_noop` + `route_start_terminal_with_invalid_payload_is_dropped` + `classify_*` 修正
- signaling_router 现有 swallow 测试新增 3 个反向类型
- worker session: `outbound_dispatch_routes_terminal_started_to_typed_variant` / `terminal_closed` / `reply_from_terminal` / `list_terminal` 4 个新增
- 现有 `outbound_dispatch_falls_back_to_signaling_message_for_unmigrated_types` 改名为 `_for_unrecognised_types` 并改用 `SignalingType::Error` 作示例（终端不再 fallback 到桥）

### 顺手清理

`-D warnings` clippy 在 `desk-ipc-protocol` + `lcxl-remote-desk-server`（no-deps）上跑出 3 个先前批次留下的 lint 错（`field_reassign_with_default` × 2 + 一处 `+ ` 起首被识别成 list item）+ 我之前批次留下的部分 unused import。这次顺手清理：
- `web/ipc-protocol/src/message.rs` 两个 `let mut info / settings = ::default(); info.x = ...` 改为 struct-update syntax
- `web/server/src/worker/session.rs` 移除批 1+2 中只在 `match payload =>` 里析构使用、不在类型位置使用的 imports（6 个 `*Payload` + 2 个 facade types）；并修一处 `field_reassign_with_default`

## 测试结果

| 范围 | 数量 | 结果 |
|---|---|---|
| `desk-ipc-protocol --lib` | 41 | 全过（含 8 个新增） |
| `lcxl-remote-desk-server --lib` | 248 | 全过（242→248，+6） |

## 仍未迁移

- `Error` / `Unknown` 仍走 `SignalingMessage` 桥，这两个是兜底通道；批 4 会评估是否一并删除桥。
- 批 4：删除 `ServiceToWorker::SignalingMessage` / `WorkerToService::SignalingMessage` / `SignalingPayload` 结构 + `RouteOutcome::ForwardToWorker` 枚举值，把残留的 `Error` / `Unknown` 换为日志路径或留给 daemon 直接处理（已经 daemon-owned 了，路径几乎没了）。

## 关键提交点

- web `feat/arch-iv-daemon-webrtc` 分支：批 3 commit
- 父仓库子模块指针对应推进
