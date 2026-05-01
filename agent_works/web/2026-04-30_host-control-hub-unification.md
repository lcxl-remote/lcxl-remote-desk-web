# Host Control Hub: Unified Tauri Bridge (Option C 重构)

归档日期：2026-05-01
分支：`fix/service-mode-dtls-rustls-provider`
计划文件：`C:\Users\lcxl\.claude\plans\cheerful-stirring-fiddle.md`

## 背景

`SessionWorker` 模式下 `ExternalChannels` 全 `None`：所有需要 Tauri 弹框确认的操作（请求控制、隐私屏、白板、剪贴板、终端、文件浏览/传输）默认 deny；隐私屏 / 白板命令亦无法投递。portable 模式走同进程 mpsc，daemon 模式走 ws，工作进程"第三套"链路即将出现。

继续打补丁会让链路越堆越多，不可维护。决定一次性统一为 daemon 那套 ws 桥架构。

## 实现总览

新增 `web/server/src/host_control/` 模块，三模式 Hub：

- **Local**：portable 嵌入式，hub 持有 broadcast，本进程注册 `/ws/tauri_ipc` 给 ipc_client 连接。
- **Aggregator**：daemon 父进程，路由 `/ws/host_upstream`（worker forwarder）和 `/ws/tauri_ipc`（Tauri shell）之间的命令；本身不发起业务请求，只做转发与定向 submit 路由。
- **Forwarder**：worker 子进程，持有一个 ws client (`UpstreamForwarder`) 把所有命令转发给 daemon。

业务侧（`DeskSession` / `check_security_permission` / 文件管理器 / 隐私屏 / 白板 …）只看到一个 `Arc<HostControlHub>`，模式无感。

## 任务清单

| # | 阶段 | 提交 |
|---|------|------|
| 1 | 新增 host_control 模块 + 协议 / endpoint / upstream / bridge 子模块 + 单元测试 | b1fb1b0 |
| 2 | portable 切换到 hub Local + tauri-app 走 ipc_client ws | 69fc537 |
| 3 | 业务调用点替换 `ExternalChannels` → `HostControlHub`；`check_security_permission` 接口翻新 | bebec43 |
| 4 | daemon 切换到 Aggregator + 注册 `/ws/host_upstream`；定向 submit 路由 | 26c0e4a |
| 5 | worker 切换到 Forwarder + `WorkerInitPayload.host_upstream_url` | 1d8d745 |
| 6 | 删除 `ExternalChannels` 残余 + 精简 `daemon/tauri_ipc.rs`（320 → 75 LOC） | be36387 |
| 7 | 异常兜底（forwarder 断线 deny、Tauri loss cancel-all）+ 完整测试矩阵 + 归档 | (本次) |

## Step 7 关键改动

### 异常兜底实现

1. **Forwarder upstream 断开 → 即刻 deny 所有 pending oneshot**
   - `UpstreamForwarder` 增加 `connection_state_tx: watch::Sender<bool>`，`mark_connected/mark_disconnected` 时通过 `send_replace` 更新（避免 receiver 全部 drop 时丢值）。
   - `HostControlHub::new_with_mode` 在 Forwarder 模式 spawn `spawn_forwarder_disconnect_watcher`：sync 阶段读取 `prev = *rx.borrow()`（避免 task 调度延迟错过 transition），异步循环上 `changed().await`，true→false 跳变时调 `deny_all_pending`。
   - 业务侧 `request_approval` 因此再也不会因 ws 断开而无限 await。

2. **Aggregator: Tauri 全部断开 → cancel 在飞 approval**
   - 新增 `HostControlHub::cancel_all_for_tauri_loss()`：把 `pending_routes` 与 `pending_replay` 清空，按 `session_id` 给每个 forwarder 路由 `SecurityApprovalCancel`。
   - `endpoint::on_disconnect` 在 `(Tauri, Aggregator)` 路径检查 `tauri_client_count` 是否归零，归零时调用此清理。

3. **多 Tauri 客户端精确计数**
   - 旧 `has_tauri_subscriber: AtomicBool` 在多 Tauri 场景下错误（第二个 client 还连着，第一个 client 走时被错误置 false）。
   - 改为 `tauri_client_count: AtomicUsize`：`mark_tauri_connected` 增 1，`mark_tauri_disconnected` 用 `fetch_update` 饱和减 1 并返回新值。
   - `has_tauri_ui()` 在 Local/Aggregator 模式下检查计数 > 0。

### 协议改动

无新增 wire 字段（`WhiteboardHide` 已在 Step 6 添加）。

### 测试矩阵实施情况

| 类别 | 实施 | 文件 |
|------|------|------|
| 协议 round-trip / 不兼容字段 / 默认 role / replay filter | ✓ U-1, U-2, ready_default_role, replay_filter, req_id_extraction, wire_compat | `host_control/protocol.rs` |
| Hub 三模式行为：fail-fast、pending lifecycle、broadcast、并发、定向 submit、replay snapshot | ✓ U-3, U-3b, U-3c, U-4, U-5, U-7, U-8, U-9, U-10, U-11, U-12, U-13, U-14, U-14b, U-14c, U-14d, aggregator denies, drain unregisters, route closed receiver, deny_all_pending | `host_control/mod.rs` |
| 兜底：forwarder 断线 deny、aggregator cancel-on-tauri-loss、计数器饱和 | ✓ forwarder_upstream_disconnect_denies_pending, aggregator_cancel_all_for_tauri_loss_routes_directionally, tauri_client_count_saturates_at_zero, cancel_all_for_tauri_loss_only_aggregator | `host_control/mod.rs` |
| Endpoint：token 校验、role 过滤、route 注册（Aggregator-only `/ws/host_upstream`） | ✓ U-15 (3), check_query_token_good, verify_token_*, role_filter_tauri, role_filter_forwarder, role_filter_pre_ready_suppresses, ws_handler_rejects_bad_token, ws_upstream_handler_404_on_non_aggregator, ws_upstream_handler_reachable_on_aggregator, endpoint_state_with_tauri_is_admin_sets_field | `host_control/endpoint.rs` |
| Upstream：backoff 序列、outbound 单次取走、inbound 广播、连接状态 watch transition | ✓ U-17, outbound_rx_can_be_taken_once, send_and_take_outbound_round_trip, inbound_broadcast_to_subscribers, connected_flag_round_trip, connection_state_watch_emits_transitions_only | `host_control/upstream.rs` |
| Bridge：Show/Hide/Draw 转发、Quit 终止 | ✓ private_screen_bridge_*, whiteboard_bridge_* | `host_control/bridge.rs` |
| Security approval：短路 Some(true)/Some(false)、remember 写盘、不 remember 不写、7 种 SecurityPermissionType 字段映射 | ✓ U-6, U-18, U-19, U-20, U-21 | `model/security_approval.rs` |
| 集成（无 ws 真实 server，跨模式行为契约） | ✓ I-2, I-6, I-10, I-17 (改写为 cancel-on-loss), forwarder inbound resolves, forwarder disconnect denies all | `tests/host_control_integration.rs` |
| 回归 | ✓ `cargo test -p lcxl-remote-desk-server --lib`：87 通过；`--test host_control_integration`：6 通过；`-p lcxl-remote-desk-tauri --lib`：4 通过；`cargo fmt --all -- --check` 干净 | — |

未实施（计划 §1 矩阵 U-16/U-16b/U-22/I-13）：完整 ws 握手 + TauriToken 时序 + replay 在 ws-Ready 时自动 push 的端到端测试。`run_ws_session` 内部 `actix_ws::Session` 不易 mock，且 `register_routes` 对 actix-session 的隔离（U-22）属于配置层验证，已在 endpoint 路由注册（`/ws/tauri_ipc` 不在 `/api` scope 下）就绪，不挂 actix-session middleware。后续如需，再以 `actix-test` ws client 串到独立集成测试中。

E2E 用例（E-1 → E-19）属于人工 / 脚本验证，不写入归档代码层。

## 涉及文件

**新增**

- `web/server/src/host_control/{mod.rs, protocol.rs, endpoint.rs, upstream.rs, bridge.rs}`
- `web/server/tests/host_control_integration.rs`

**重大改动**

- `web/server/src/lib.rs`（删除 `ExternalChannels` 结构、`run_with_channels` → `run_with_hub`）
- `web/server/src/model/security_approval.rs`（参数从 `Option<&SecurityApprovalSender>` 改为 `&Arc<HostControlHub>`）
- `web/server/src/service/{signaling.rs,data_channel.rs,file_transfer.rs,file_manager.rs}`（替换 sender 参数）
- `web/server/src/worker/session.rs`（构造 Forwarder hub）
- `web/server/src/daemon/{mod.rs,local_api.rs,worker_manager.rs}`（接入 Aggregator）
- `web/server/src/daemon/tauri_ipc.rs`（320 → 75 LOC，仅保留元数据）
- `web/server/src/controller/{settings.rs,service_mgmt.rs,info.rs}`（dispatch 改走 hub）
- `web/tauri-app/src-tauri/src/{lib.rs,ipc_client.rs}`（portable 改造为 ipc_client + URL 参数化）
- `web/ipc-protocol/src/message.rs`（`WorkerInitPayload.host_upstream_url`）

## 行为不变量验证

- portable 模式：嵌入 server 启动 → 主线程等待 ipc_client 拿到 `TauriToken` 后再 `WebviewWindowBuilder` 打开窗口（`run_tauri_app` 路径与 `run_tauri_service_shell` 一致的 token_holder + 60s 超时）。
- daemon 模式：Aggregator hub 在 worker forwarder ws Ready 时 `register_forwarder_session`；`SecurityApprovalSubmit` 仅通过 `route_to_forwarder` 定向送达发起 worker，不广播。
- worker 模式：Forwarder hub upstream 离线时 `request_approval` 立即返回 `ApprovalResponse::deny()`；上线后正常等待。
- 异常恢复：worker forwarder ws 中途断开 → daemon 端 `drain_upstream_pending` 清理路由 → tauri 通过 broadcast 外的 cancel 拿到取消；worker 端通过 `disconnect_watcher` 把 in-flight oneshot 全 deny。
- Tauri 全部断开 → daemon `cancel_all_for_tauri_loss` 给每个 forwarder 发 cancel；最后一个 Tauri 走时才触发（`tauri_client_count` 计数）。

## 风险与回退

每个 commit 独立可回滚：

- 回滚 Step 7（仅本次）：撤销异常兜底 + 测试，hub 行为退回到 Step 6 状态（pending oneshot 在 ws 断开时不会自动 deny，业务侧可能 hang）。
- 回滚 Step 6：恢复 `ExternalChannels` + 旧 `daemon/tauri_ipc.rs`。但需注意 daemon 已切到 Aggregator hub，channels 兼容层不存在。建议成对回滚 Step 4-7。
- 回滚 Step 1-3：portable 路径恢复同进程 mpsc。

## 后续可选项

1. 父仓库子模块指针更新（手动 `git submodule update` 后在 workspace 提交一次 web 子模块指针）。
2. 完整 ws 端到端集成测试（U-16, U-16b, I-13）：用 `actix-test::TestServer` 跑 hub endpoint，awc ws client 当 mock Tauri / forwarder。
3. 计划 §6 的 Tauri 30s 阈值"延后取消"语义：当前实现是"立即取消"（更安全，无 stale 弹框），如未来希望宽容短暂掉线，可加 `tokio::time::sleep_until` 任务。
