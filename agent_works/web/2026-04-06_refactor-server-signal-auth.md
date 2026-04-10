# 归档：Server 到 Signal 认证架构重构

## 1. 任务背景 (Background)
目前的 `server` 连接 `signal` 服务时使用账号密码登录获取 Cookie 的方式，存在语义不明确、安全性低以及无法同时连接本地和远程 Manager 的局限性。

## 2. 实现计划 (Implementation Plan)
- **Token 认证机制**：在 `VersionInfo` 中增加 `token` 字段，并在信令服务端实现 `NodeTokenValidator` 校验。
- **并发连接架构**：重构 `start_desk_session`，使其能同时维持本地和远程 Manager 的信令连接。
- **事件广播化**：使用 `tokio::sync::broadcast` 处理 Tauri 隐私屏状态事件，确保多路连接同步。
- **清理旧逻辑**：移除基于 HTTP 登录获取 Cookie 的所有代码。

## 3. 任务列表 (Task List)
- [x] 修改 `desk-signal-facade` 的 `VersionInfo` 模型，添加 `token` 字段。
- [x] 定义 `NodeTokenValidator` trait 并处理 `dyn compatibility`（使用 Boxed Future）。
- [x] 改造 `open_signaling_handle` 控制器，支持 Token 绕过 Session 认证。
- [x] 重构 `server::lib.rs`，注入 `LocalNodeTokenValidator` 并生成随机 `local_node_token`。
- [x] 重构 `server::signaling.rs`，实现 `start_desk_session` 的多路并发连接与事件广播转发。
- [x] 修复 `SharedSettings` 及其内部 `RwLock` 的克隆问题。
- [x] 修复 401 错误：将信令路由移出 `reject_anonymous_users` 中间件范围。
- [x] 同步更新 `openapi.json` 及前端类型钩子。
- [x] 修复 500 错误：处理因 `turn` 服务启动失败导致的 `TurnApiState` 数据提取失败，使注入参数为 `Option` 类型。

## 4. 执行总结 (Walkthrough)
1. **模型变更**：在 `signal-facade/src/model/version.rs` 中为 `VersionInfo` 增加了 `token` 属性。
2. **服务端认证**：在 `signal/src/controller/signaling.rs` 中优先检查 `query.token`，若校验通过则直接创建系统管理员 User。
3. **路由调整**：发现 `/api` 范围受 `reject_anonymous_users` 中间件保护导致 Token 无法通过握手。已将 `signaling` 路由移至独立的 `/api/desk` 作用域，确保 Token 逻辑能正常进入 Handler。
4. **连接重构**：`start_desk_session` 不再阻塞于单一 URL，而是启动两个独立的异步 Task。本地 Task 默认连接 `127.0.0.1` 并附带随机生成的节点 Token；Manager Task 则根据配置连接。
5. **编译修复**：解决了由于 `ExternalChannels` 包含不可 Clone 的 Receiver 导致的并发问题，通过 `broadcast` 机制实现了事件在多路连接间的共享。
6. **验证**：通过 `cargo check` 确认代码逻辑完整，通过 `update_openapi.ps1` 同步了前端定义。
7. **修复 500 错误 (URL 解析)**：使用 `url::Url` 解析 `ws://` 协议时，它将其视作不包含 query 的特殊格式从而移除了 `?` 导致访问了错误的路径并落入了 default_handler。已改用手动 `format!` 拼接解决该问题。
8. **修复 500 错误 (依赖注入)**：修复了在没有可用 UDP 接口等情况下 `turn` 服务启动失败，导致 `TurnApiState` 没有被注入到 Actix app data 从而引发 Actix-Web `Extractor` 返回 500 的问题。将其改为 `Option<web::Data<TurnApiState>>` 允许优雅降级。

## 5. 结论 (Conclusion)
该重构显著提升了系统的可扩展性与安全性。节点间的通讯不再依赖用户凭据，且支持了企业级场景下本地与云端双连接的并发需求。
