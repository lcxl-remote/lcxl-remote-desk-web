# 服务模式稳定化 + UAC 桌面切换检测

归档日期：2026-05-02
分支：`fix/service-mode-dtls-rustls-provider`
前置归档：`agent_works/web/2026-04-30_host-control-hub-unification.md`（Step 1–7）

## 背景

Step 1–7 把 Host Control Hub 重构落地后，进入服务模式（ServiceDaemon + SessionWorker + 独立 Tauri 壳）实际部署验证。验证过程中暴露出四个独立但级联触发的问题，全部在本归档覆盖：

1. **Worker 启动即崩溃 + 无日志**：worker 进程被 daemon 启动后约 213ms 退出，daemon 日志显示"Worker exited unexpectedly"持续重启；`desk-worker.log` 只来得及打出 telemetry 第一行。
2. **Tauri service-shell 无日志输出**：`tauri-app` 在服务模式下不再嵌入 server，`init_telemetry` 不会被调用，所有 `log::*` 静默丢弃；只能瞎猜哪一步卡住。
3. **Tauri 永远连不上 daemon `/ws/tauri_ipc`**：daemon 把 `EndpointState` 注册成 `web::Data<Arc<EndpointState>>`，handler 提取 `web::Data<EndpointState>`，TypeId 不匹配 → 每次 ws 请求都 500（worker forwarder 同样中招，但 worker 进程不死所以容易被忽视）。
4. **UAC 触发后 worker 不切换桌面**：daemon 的 `session_monitor` 跑在 `Service-0x0-3e7$` 窗口站，`OpenInputDesktop` 跨不过窗口站边界，永远看不到用户会话的 Default → Winlogon 切换。

## 实现总览

| # | 问题 | 修法 |
|---|------|------|
| 1 | worker panic 静退 | worker 入口换 `actix_web::rt::System::new()`（自带 LocalSet），匹配其它 startup mode；解决 `actix_web::rt::spawn` / `awc::Client` 在普通 multi-thread tokio 运行时上 panic 的根因 |
| 2 | tauri service-shell 无日志 | 把 telemetry 文件层抽出为公共 `init_tauri_shell_telemetry`，仅 service-shell 路径调用（portable 走 server `init_telemetry`，避免冲突） |
| 3 | daemon ws 路由 500 | daemon `local_api.rs` 改用 `host_control::endpoint::register_routes(...)` helper，统一 `Data::from(Arc<T>) -> Data<T>` 包装；新增 endpoint 回归测试 `ws_handler_extracts_endpoint_state_through_register_routes` |
| 4 | UAC 桌面切换不识别 | 检测从 daemon 挪到 worker（`OpenInputDesktop` 在用户会话 WinSta0 内能看到 Winlogon）；区分 `Restricted (ACCESS_DENIED)` / `Name(...)` / `OtherError`；新增 IPC 消息 `WorkerToService::DesktopChanged`；daemon 端对 `Winlogon` 暂时只 log、不切，避免连带把 Default worker 杀掉 |

## 任务清单

| # | 阶段 | 状态 |
|---|------|------|
| A | worker runtime 切到 `actix_web::rt::System::new()` + `worker_runtime_supports_actix_local_spawn` 回归测试 | 已完成、用户验证 worker 不再循环重启 |
| B | telemetry 抽 `log_directory()` / `log_file_name_for()` / `init_tauri_shell_telemetry()`；tauri-app 调用 + Cargo.toml 清理 env_logger（中间过渡产物） | 已完成、用户确认 service-shell 日志可见 |
| C | daemon `local_api.rs` 改走 `register_routes`；portable 路径保留手工注册（`utoipa_actix_web::ServiceConfig` ≠ `actix_web::web::ServiceConfig`）；新增回归测试 | 已完成、用户验证 Tauri 能连上 daemon |
| D | IPC `DesktopChanged` 消息；worker `desktop_monitor` 模块（持续轮询 + 状态去重 + ACCESS_DENIED → "Winlogon"）；daemon `signaling_proxy` 处理 + Winlogon 兜底；`session_monitor` non-Windows `get_active_session_id` 桩 | 已完成、用户日志确认 detection 工作 |

## 关键改动详情

### A. Worker runtime（root cause 修复）

`web/server/src/worker/mod.rs` 之前用：

```rust
let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
rt.block_on(async { WorkerSession::run(...).await })
```

worker 在 `host_control::upstream::spawn_upstream_ws_task` 调 `actix_web::rt::spawn(...)`（内部 `tokio::task::spawn_local`），普通 tokio runtime 没 LocalSet → panic → `block_on` 抛出 → process abort，telemetry guard 来不及 flush。`signaling.rs:355,398,430` 也是同源风险点。

修法：`let system = actix_web::rt::System::new(); system.block_on(...)`，与 Default/DeskServer/Signaling/ServiceDaemon 一致，自动设置 LocalSet。回归测试 `worker_runtime_supports_actix_local_spawn` 在 worker 入口的 runtime flavor 上跑一次 `actix_web::rt::spawn`，未来若有人改回 plain tokio 立即挂掉。

### B. Telemetry-driven service-shell 日志

`web/server/src/telemetry.rs` 新增公共助手：

- `pub fn log_directory() -> PathBuf` —— Windows 走 `%ProgramData%\LCXL Remote Desktop\logs`，Linux/macOS 走 `/var/log/lcxl-remote-desk`。原 `init_telemetry` 内联实现改用此函数。
- `pub fn log_file_name_for(&StartupMode) -> &'static str` —— 同上，集中文件名映射。
- `pub fn init_tauri_shell_telemetry(log_level: &str) -> Result<WorkerGuard>` —— 精简版：只装 `EnvFilter` + 每日滚动 `desk-tauri.log` + tracing `Registry.try_init()`，不带 OTLP / stdout / cleanup task。失败时返回 Err（说明 global subscriber 已存在）而不是 swallow，便于发现冲突。

`tauri-app/src-tauri/src/lib.rs` 在 `run_tauri_service_shell` 顶部调用，把 `WorkerGuard` 用 `_telemetry_guard` 持到函数返回（drop 时 flush 非阻塞 writer）。**portable 路径不动**——它在 spawn 出来的 server 线程里调 `init_telemetry`，全局 tracing subscriber + LogTracer 自动桥接 `log::*`，service-shell 与 portable 在 runtime 互斥所以双安装结构上不可能。

### C. daemon `Data<EndpointState>` 注册修复

`web/server/src/daemon/local_api.rs` 之前：

```rust
let endpoint_state_data: web::Data<Arc<EndpointState>> =
    web::Data::new(Arc::clone(&endpoint_state));
.app_data(endpoint_state_data)
.route("/ws/tauri_ipc", web::get().to(ws_handler))
```

handler 签名是 `state: web::Data<EndpointState>`。actix 按 TypeId 查 `app_data`，`Data<EndpointState>` ≠ `Data<Arc<EndpointState>>` → 找不到 → 500 + DEBUG 日志 `Failed to extract Data<EndpointState>`。`/ws/host_upstream` 同样中招（worker forwarder 一直没真正连上 daemon，只是 worker 进程没崩、业务能跑掩盖了）。

修法：daemon 改为：

```rust
.configure(move |cfg| {
    host_control::endpoint::register_routes(cfg, Arc::clone(&endpoint_state_for_routes))
})
```

helper 内部用 `web::Data::from(Arc<T>) → Data<T>`，正好对上 handler 签名。portable 路径无法这样写——它的 cfg 是 `utoipa_actix_web::ServiceConfig`（独立类型），手工注册保留但加注释提醒。

回归测试 `host_control::endpoint::tests::ws_handler_extracts_endpoint_state_through_register_routes`：good token + 无 ws upgrade headers，断言不能 500（500 = `Data<T>` 漂移）。

### D. UAC 桌面切换检测

#### IPC

`ipc-protocol/src/message.rs`：

```rust
pub enum WorkerToService {
    ...
    DesktopChanged(DesktopChangedPayload),
}

pub struct DesktopChangedPayload {
    pub name: String,
}
```

附 `desktop_changed_round_trips` 测试。

#### Worker 端：`desktop_monitor.rs`（新模块）

核心数据：

```rust
pub enum InputDesktopProbe {
    Name(String),
    Restricted,            // OpenInputDesktop 返回 ERROR_ACCESS_DENIED (HRESULT 0x80070005)
    OtherError(String),
}

pub const RESTRICTED_DESKTOP_NAME: &str = "Winlogon";
```

`probe_input_desktop()`（Windows）：

- 成功 → `Name(name)`。
- 失败时 `e.code().0 == 0x80070005` → `Restricted`，否则 `OtherError(format!(...))`。

`run_loop`：

- 1 秒一次循环。
- `Name == bound` → 清 `last_reported`（re-arm，下次切走再触发）。
- `Name != bound` 且 `last_reported != Some(name)` → log + send + 记入 `last_reported`（去重）。
- `Restricted` → 当作 `"Winlogon"` 走同样的去重路径。
- `OtherError(msg)` → 仅在 message 文本变化时 warn，避免 1Hz 刷屏。

线程退出条件：`tx.is_closed()`（receiver drop）。**不再 one-shot exit**——之前一条上报后退出，UAC 多次进出只能识别第一次；现在持续运行 + 状态去重 + 回到 bound 自动 re-arm。

`session.rs` 在 `Init` 之后 spawn monitor，主 select loop 加：

```rust
Some(new_desktop) = desktop_change_rx.recv() => {
    write_message(&mut writer, &WorkerToService::DesktopChanged(...)).await?;
    // 留在 loop 等 daemon 决定要不要发 DesktopSwitching 回来
}
```

注：**不**在收到 drift 时退出 loop，否则 bridge_loop 会以为 worker 崩溃，调 `handle_crash_recovery` 在旧桌面重启，正反相消。

#### Daemon 端：`signaling_proxy.rs`

`worker_rx.recv()` 里加 `WorkerToService::DesktopChanged(payload)` 分支：

```rust
if is_unsupported_capture_desktop(&payload.name) {
    info!("...keeping current worker — Winlogon capture is not yet supported");
    continue;
}
// 否则在独立 actix task 里：notify_desktop_switch + sleep 500ms + start_worker(get_active_session_id, name, browser_ids)
```

`is_unsupported_capture_desktop(name) = (name == RESTRICTED_DESKTOP_NAME)`。**Winlogon 路径只 log、不切**，理由：

- daemon 当前用 `WTSQueryUserToken` 拿到的用户令牌（或其 elevated linked token）调 `CreateProcessAsUserW`。
- 用户令牌（甚至 admin elevated）都没有 Winlogon 桌面 ACL —— Winlogon 是 SYSTEM 拥有的安全桌面。
- 如果走完整切换流程：`notify_desktop_switch` 会先把 `DesktopSwitching` 发给老 worker（老 worker 退出）→ `start_worker(Winlogon)` 必然 access denied → 整个会话没 worker。
- 所以宁可不切，老 Default worker 留着；用户关掉 UAC 回到 Default 时 monitor 自动 re-arm，capture 自然恢复（虽然 UAC 期间画面冻结）。

附 `unsupported_desktops_are_only_winlogon` 单测。

`session_monitor.rs` 加非 Windows `get_active_session_id() -> 0` 桩，让 daemon 在 Linux/Mac 也能编译。**保留**（不删）原 Windows polling，无害（每秒一次 syscall，从 session 0 调 `OpenInputDesktop` 永远失败 silently），后续清理可单独提工单。

## 测试矩阵

| 类别 | 用例 | 文件 |
|------|------|------|
| Worker runtime 回归 | actix_web::rt::spawn 在 worker `System` 里能跑 | `worker/mod.rs::tests::worker_runtime_supports_actix_local_spawn` |
| Telemetry 公共 API | `log_directory()` 平台分支正确；`log_file_name_for` 5 种模式映射 | `telemetry::tests::log_directory_resolves_to_program_data_subtree_on_windows`, `log_file_name_for_each_startup_mode` |
| Endpoint state 提取 | good token + 无 ws upgrade → 不 500 | `host_control::endpoint::tests::ws_handler_extracts_endpoint_state_through_register_routes` |
| IPC round-trip | `DesktopChanged` 序列化稳定 | `ipc-protocol::message::tests::desktop_changed_round_trips` |
| Desktop monitor 行为 | 名字大小写敏感；常量锁定 `Winlogon`；receiver drop 后线程退出；同状态重复观察去重、回 bound 后 re-arm | `worker::desktop_monitor::tests::*`（4 项） |
| Daemon 兜底 gate | Winlogon 唯一被过滤；大小写敏感 | `daemon::signaling_proxy::tests::unsupported_desktops_are_only_winlogon` |
| 回归 | `cargo test -p lcxl-remote-desk-server --lib`：96 通过；`-p desk-ipc-protocol --lib`：4 通过；`-p lcxl-remote-desk-tauri --lib`：4 通过；`cargo fmt --all -- --check` 干净 | — |

E2E（人工）：

- E-A：服务安装后启动 Tauri，主窗口正常打开；`desk-tauri.log` 有 `[ServiceShell] starting...` + `Connected` + `Received TauriToken`。
- E-B：远程桌面连上后操作正常。
- E-C：触发 UAC，`desk-worker.log` 看到 `desktop drift detected: 'Default' -> 'Winlogon'`；`desk-daemon.log` 看到 `keeping current worker — Winlogon capture is not yet supported`。
- E-D：关闭 UAC，`desk-worker.log` 看到 `input desktop returned to bound 'Default' from 'Winlogon'`；远程画面恢复。
- 用户已逐项验证通过。

## 涉及文件

**新增**

- `web/server/src/worker/desktop_monitor.rs`

**修改**

- `web/ipc-protocol/src/message.rs`（`WorkerToService::DesktopChanged` + `DesktopChangedPayload` + 测试）
- `web/server/src/worker/mod.rs`（`actix_web::rt::System::new()` + 注释 + 回归测试）
- `web/server/src/worker/session.rs`（spawn `desktop_monitor` + 主 loop 加 `desktop_change_rx` arm）
- `web/server/src/daemon/local_api.rs`（改走 `register_routes`）
- `web/server/src/daemon/signaling_proxy.rs`（`DesktopChanged` 处理 + `is_unsupported_capture_desktop` gate + 单测）
- `web/server/src/daemon/session_monitor.rs`（non-Windows `get_active_session_id` 桩）
- `web/server/src/host_control/endpoint.rs`（`ws_handler_extracts_endpoint_state_through_register_routes` 回归）
- `web/server/src/lib.rs`（portable host-control 注册路径加注释解释为什么不能 deduplicate）
- `web/server/src/telemetry.rs`（`log_directory`/`log_file_name_for`/`init_tauri_shell_telemetry` + 测试；原 `init_telemetry` 改用新助手）
- `web/tauri-app/src-tauri/src/lib.rs`（`run_tauri_service_shell` 调 `init_tauri_shell_telemetry` 持 guard）

`Cargo.lock` 因依赖图调整产生的次要变化。`tauri-app/src-tauri/Cargo.toml` 中间过程加过 `env_logger` 又移除，最终净差为零。

## 行为不变量

- worker 进程在服务模式下能稳定持续运行；崩溃后由 `handle_crash_recovery` 一次性重启而非死循环。
- Tauri service-shell 启动时打开 ws 连接到 daemon `/ws/tauri_ipc`，daemon 推 `TauriToken` 帧，主窗口拿到 token 后加载页面。
- worker forwarder 在 worker 启动后连上 daemon `/ws/host_upstream`，hub 进入 connected 状态；业务侧 `request_approval` 不再立刻 deny。
- worker monitor 每个 desktop transition 上报一次；Default ↔ 其它桌面的反复进出全部能识别。
- daemon 收到 `DesktopChanged{name: "Winlogon"}` 时**不**改动 active worker；其它名字走原有切换流程（注：当前实际不会触发其它名字，因为用户桌面切走基本就是 Winlogon；若日后出现 ScreenSaver 等需要切的场景再扩展）。

## 风险与回退

各步彼此独立，可分别回滚：

- 回滚 D（UAC 检测）：删 `desktop_monitor.rs`、回滚 IPC 变体、回滚 signaling_proxy 处理。worker 不再上报，回到"UAC 时画面冻结但服务存活"的旧状态——和当前 Winlogon 路径行为一致，回滚无明显回归。
- 回滚 C（Endpoint state）：daemon ws 路由再次 500，Tauri 连不上、worker forwarder 不工作。**严禁**在不修复其它问题前回滚。
- 回滚 B（Tauri telemetry）：service-shell 重新无日志，但功能不破。debug 体验回退而已。
- 回滚 A（worker runtime）：worker 立刻死循环。**严禁**回滚。

## 后续工单（compact 后接续）

1. **Winlogon capture（SYSTEM token + Winlogon DACL）**——本次工作刻意停在"检测 + 兜底"。完整支持需要：
   - 用 daemon 自身 SYSTEM token（不是 `WTSQueryUserToken` 的用户令牌）调 `DuplicateTokenEx` + `SetTokenInformation(TokenSessionId, target_session)`。
   - `OpenDesktopW(WinSta0\\Winlogon)` + `GetUserObjectSecurity` / `SetUserObjectSecurity` 给 Winlogon 桌面 DACL 加 `DESKTOP_*` 访问权（SYSTEM 默认有，但 worker 进程的 SID 需要单独加 ACE 才能 attach 到 Winlogon）。
   - `WorkerManager` 多 worker 槽位（active_worker 从 `Option` 改为按 desktop 索引；信令路由 by connection_id 选 worker）。或者更简单的过渡方案——UAC 期间临时把 Default 替换为 SYSTEM-token Default-bound worker（同一 worker 用 SYSTEM 身份能跨桌面查名字，capture 在 Winlogon 出现时切换 input desktop attach），需先验证 capture-engine 的 DXGI / GDI 后端在 Winlogon 上是否能直接出帧。
   - 仓库 `pocs/poc-winlogon-capture` 已经为这条路准备占位，可作为起点。
2. **`session_monitor` 清理**——Windows polling 在服务模式永远失败，整模块在 worker push 路径上线后已经无价值；可改为 `noop_on_windows` + 保留 `get_active_session_id` 工具函数。次要清理。
3. **完整 ws 端到端集成测试**——前一归档（host-control-hub-unification）已经留过 U-16/U-16b/U-22/I-13 这一档（actix-test ws client 模拟 Tauri / forwarder）。本轮改 `Data<T>` 包装时如果有这一层覆盖就不会漏到生产环境。优先级抬升。
