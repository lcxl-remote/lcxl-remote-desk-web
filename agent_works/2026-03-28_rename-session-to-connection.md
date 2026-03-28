# 任务归档：将信号服务器会话 ID 重命名为连接 ID

## 任务目标
信号服务器在 desk-server 或浏览器连接时分配的 `session_id` 概念不准确，因为每次连接都会变化。本次任务将其重命名为 `connection_id`，以更准确地反映其作为“信号连接标识”的本质。

## 实施方案与任务清单

### 1. 后端模型层重构 (`signal-facade`)
- [x] 将 `signal-facade/src/model/session.rs` 重命名为 `connection.rs`。
- [x] 重命名结构体：`SessionModel` -> `ConnectionModel`、`SessionList` -> `ConnectionList`。
- [x] 更新字段：`session_id` -> `connection_id`、`from_session_id` -> `from_connection_id`、`to_session_id` -> `to_connection_id`。
- [x] 更新信令枚举：`FetchSessions` -> `FetchConnections`、`SessionList` -> `ConnectionList`。
- [x] 更新 Getter 方法：`check_and_get_from_session_id` -> `check_and_get_from_connection_id`。

### 2. 信号服务器层重构 (`signal`)
- [x] 重命名类型：`SessionState` -> `ConnectionState`、`SharedSessionMap` -> `SharedConnectionMap`。
- [x] 重命名控制器：`SessionController` -> `ConnectionController`（文件重命名为 `connection.rs`）。
- [x] 更新 API 路由：`/sessions` -> `/connections`、`/terminals/{session_id}` -> `/terminals/{connection_id}`。
- [x] 全面替换 `service.rs` 和各 `controller` 中的变量名。

### 3. 服务端核心层重构 (`server` & `server-user`)
- [x] 更新 `server-user` 中的 `CurrentUser`：`target_session_id` -> `target_connection_id`。
- [x] 更新 `login` 逻辑：`target_session_id` -> `target_connection_id`、`SharedSessionMap` -> `SharedConnectionMap`。
- [x] 更新 `signaling` 服务：替换所有 `session_id` 相关变量。
- [x] 更新 `terminal` 服务：重命名 OS 会话 ID 相关的误导性变量（改为 `os_session_id` 以示区分）。
- [x] 更新 `openapi.json` 定义。

### 4. Tauri 桌面应用重构 (`tauri-app`)
- [x] 在 `private_screen.rs`、`whiteboard.rs` 和 `security_approval.rs` 中同步重构 ID 引用。
- [x] 更新事件载荷中的字段名。

### 5. 前端应用重构 (`vite-project`)
- [x] 重新运行 `npx kubb generate` 同步 OpenAPI 变更。
- [x] 替换所有组件和 Hook 中的 `sessionId`、`session_id`、`from_session_id`、`to_session_id`。
- [x] 更新 `useDeskSignaling` 和 `useDeskRTC` 中的核心信令逻辑。
- [x] 国际化同步：更新 `zh-CN` 和 `en-US` locale 文件，将 UI 上的“会话 ID”改为“连接 ID”。
- [x] 更新测试脚本 `test_flow.mjs`。

## 验证结果
- [x] 后端通过 `cargo check`。
- [x] 前端成功通过 Kubb 生成代码并完成逻辑同步。
- [x] UI 文本显示已更新为“连接 ID”。
