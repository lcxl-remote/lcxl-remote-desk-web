# Aggregator Hub Originates Daemon-Self Approvals (Arch IV Migration Gap)

## 背景

Arch IV PR 2 把 WebRTC PeerConnection 从 SessionWorker 进程搬进了 ServiceDaemon 进程，于是 `RequireControl` 信令的处理路径也从 worker 端的 `service/signaling.rs` 搬到了 daemon 端的 `daemon/pc_manager.rs`。`pc_manager::handle_require_control` 通过 `check_security_permission` 调用 `HostControlHub::request_approval`，期望 hub 把 `SecurityApprovalRequest` 广播给本地连接的 Tauri 客户端，等用户点 Approve / Deny。

但 daemon 进程的 hub 是 Aggregator 模式，而 Aggregator 模式下 `request_approval` 还保留着 Arch III 时代的硬性 deny：

```rust
HubMode::Aggregator => {
    // The aggregator is a router — it does not originate requests.
    warn!("[Hub/Aggregator] request_approval invoked — denying ...");
    return ApprovalResponse::deny();
}
```

Arch III 中这个约束是合理的——那时 PC 在 worker 端，worker Forwarder hub 才是 approval 的发起方，daemon Aggregator 只负责把 worker 转来的 `SecurityApprovalRequest` 广播给 Tauri，所以"Aggregator 不发起请求"成立。Arch IV 把 PC 搬进 daemon 后这个前提不再成立，但 PR 2 没相应放开 Aggregator 的 `request_approval`。

## 现象

在用户测试 daemon 模式时表现为：浏览器点"控制"按钮 → Tauri 窗口完全不弹对话框。日志稳定复现：

```
[pc_manager] <conn_id> RequireControl: SignalRequestControlData { accept: true, ... }
[Hub/Aggregator] request_approval invoked — denying (aggregator should not request)
[pc_manager] <conn_id>: RemoteControl denied
```

Tauri 端 IPC 链路本身正常（`role=Tauri session_id=N` 已注册），只是 `SecurityApprovalRequest` 在 hub 入口就被拦截，从未被广播。

跟 daemon 重启没有关系——任何时刻 daemon 模式点控制都会落入这条路径。

## 影响范围

迁移遗漏的 daemon 端入口（都用 Aggregator hub，全部受影响）：

| 文件 | 入口 | 权限类型 |
|---|---|---|
| `daemon/pc_manager.rs:1970` | `RequireControl` 主路径 | RemoteControl |
| `daemon/pc_manager.rs:2009` | 同函数内 ClipboardSync 分支 | ClipboardSync |
| `daemon/signaling_router.rs:274` | 路由 `RequireControl` 到 `pc_manager` | 上面两条的入口链 |

worker session 的 hub 是 `new_forwarder`，所以 worker 内部仍调用的 `check_security_permission`（FileBrowse / FileTransfer / Terminal / Whiteboard / FileTransferDispatcher 缓存路径）走 Forwarder→IPC→daemon endpoint→Tauri 链路，**不受影响**——这条路在 endpoint 层是消息中转，没经过 Aggregator hub 自己的 `request_approval` API。

## 实施计划

修复点全部集中在 `web/server/src/host_control/mod.rs`：

1. **`request_approval`**：把 Aggregator 与 Local 合并到同一个分支：
   - `has_tauri_ui()` 为 true → 走广播 + pending_approvals + pending_replay 标准路径
   - 为 false → 立即 deny（与 Local 无 subscribers 时一致）
2. **`submit_approval`**：Aggregator 模式增加双源解析：
   - 先尝试 `pop_upstream_for_req(req_id)`：拿到说明是 worker 来源 → 走原有 directional dispatch 给 forwarder
   - 拿不到回落到本地 `pending_approvals`：有则解析 oneshot（daemon-self 来源）
   - 都没有 → 返回 false
   - `notify_tauri_finished` 在两条 Aggregator 路径上都广播
3. **`cancel_all_for_tauri_loss`**：Tauri 断连时，除了原有的给每个 owning forwarder 发 `SecurityApprovalCancel`，还调用 `deny_all_pending()` 解析 daemon-self 的 oneshot。否则 daemon 模式下用户关掉 Tauri 后业务 task 会一直挂在 `request_approval` await 上。

设计原则：daemon-self 与 worker 来源用 `pending_routes` 是否登记区分，永不混淆。replay snapshot 对两种来源都生效（让 Tauri 重连时能恢复弹框）。

## 任务清单

- [x] 修改 `request_approval` 让 Aggregator 与 Local 合并分支
- [x] 修改 `submit_approval` 实现 Aggregator 双源 fallback
- [x] 修改 `cancel_all_for_tauri_loss` 兼顾 daemon-self pending
- [x] 替换过时测试 `aggregator_request_approval_denies`
- [x] 新增 `aggregator_request_approval_no_tauri_denies_fast`：无 Tauri 时立即 deny
- [x] 新增 `aggregator_request_approval_pends_until_submit`：daemon 模式 RequireControl 主路径回归
- [x] 新增 `aggregator_mixed_daemon_self_and_worker_routes_correctly`：双源不串扰
- [x] 新增 `aggregator_tauri_loss_denies_daemon_self_pending`：Tauri 断连兜底
- [x] `cargo build -p lcxl-remote-desk-server --tests` 通过
- [x] `cargo test -p lcxl-remote-desk-server --lib host_control::` 63/63 通过
- [x] `cargo test -p lcxl-remote-desk-server --lib -- model::security_approval daemon::pc_manager daemon::signaling_router` 88/88 通过
- [x] 用户在 daemon 模式手动 E2E 验证通过
- [x] `style: apply rustfmt to tauri-app security_approval tests`（修补 fd3d81a 漏跑的 rustfmt）

## 执行总结

修改文件：`web/server/src/host_control/mod.rs`（hub 主体 + 测试），加上一次顺便补的 `tauri-app/src-tauri/src/security_approval.rs` rustfmt。

本次修复的 net 行数 +270 / -48，其中测试约占一半。`pc_manager.rs` 与 `signaling_router.rs` 的调用方代码不需要改——错的不是它们的逻辑，是 hub 的实现。

行为上的关键区别：

| 场景 | 修复前 | 修复后 |
|---|---|---|
| daemon 模式 + Tauri 已连 + 浏览器点控制 | hub 直接 deny，无对话框 | 广播 → Tauri 弹框 → 用户决定 |
| daemon 模式 + 无 Tauri + 浏览器点控制 | hub deny | hub deny（行为一致，但走 fast-deny 而非 warning）|
| daemon 模式 + worker 转发的 approval | 不变（走 endpoint 中转）| 不变 |
| daemon 模式 + Tauri 在 approval 中途断连 | worker 路径走 cancel；daemon 路径 await 永挂 | 两条路径都被 deny / cancel |

未一并清理的 Plan 残留事项（不在本次范围）：

- Plan 里 PR 7 的"Arch III 残留清理"显式列了 `preapproved_connections` / `ConnectionAcceptStateChanged` / `should_short_circuit_*`，没把"hub 角色语义"列入。后续如有时间，应在 hub 模块顶部 doc 中重写"Aggregator 在 Arch IV 下既是路由器也是 daemon-self 请求的发起方"这条契约，并视情况评估 `register_upstream_request` / `handle_upstream_approval_request` API 命名是否还要保留 "upstream" 前缀（现在两类来源混用同一组 pending_approvals/pending_replay）。

提交：

- web 子模块 `fba29a6` style: apply rustfmt to tauri-app security_approval tests
- web 子模块 `6104b2d` fix(host-control): aggregator originates daemon-self approvals under Arch IV
- 父仓库随后 bump web 子模块指针。
