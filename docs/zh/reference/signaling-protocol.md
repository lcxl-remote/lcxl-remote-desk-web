# 信令协议

信令通过 WebSocket 承载 SDP / ICE 交换及一组控制消息。共享协议模型位于 `signal-facade/`，信令服务器本身在 `signal/`。

## 消息类型

信令消息建模为 `signal-facade/src/model/signal.rs` 中的 `SignalingType` 枚举。每个变体有**唯一整数值**，并在 `signal/src/service.rs` 的 `handle_message` 中被穷尽处理——刻意**没有 `_ =>` 兜底**，因此编译器会强制确保每种类型都被处理。

## 鉴权

信令端点依调用者不同采用不同鉴权——见[信令鉴权](/zh/security/signaling-auth)。简言之：

- Desk Server 连接信令服务时，使用 WebSocket URL 查询参数中的令牌。
- 浏览器连接信令服务时，使用 Actix-Session Cookie，**不在 URL 中携带令牌参数**。

## 添加新信令类型

1. 在 `SignalingType` 中添加变体（带唯一整数值）。
2. 在 `handle_message` 中处理它——添加转发分支或专用匹配分支。
3. 更新前端：重新生成客户端并在前端 RTC hook 中添加 `onMessage` 处理程序。

跨切面清单见[模块地图](/zh/reference/modules#添加新信令类型)。

## 连接发现

OSS 与 Manager 的 `GET /api/desk/connections` 使用相同的 `RestResponse` 包络：成功时 `success: true`，空列表为 `data: []`；读取失败时 `success: false`，不能显示为设备离线或空设备列表。Manager 共享存储不可用返回 `109`，记录损坏或数据库错误返回 `1`。WebSocket 的 `FetchConnections` 无论成功或失败均返回原请求关联的 `ConnectionsFetched`；客户端先检查 `response_state`，再解析列表。

## 对端报告

信令角色只有 `browser` 和 `server`。节点凭据仅认证 `server`；浏览器权限与显示名称由可信 Cookie 主体裁定。

`ConnectionModel` 分别提供 `version_info`（API 版本、构建号、提交哈希与 debug 标记）、`peer`（角色、操作系统、显示名称）、`exec_capabilities` 与 `ai_assistant_settings`。未报告的版本为 `null`，未报告的系统为 `Other`。debug 标记为 false 不证明是 release 构建；能力和设置检查独立于版本号。

节点凭据留在握手 Query 中，不进入连接元数据。私有 Server 登记包含客户端身份、已验证的 shell、PTY 能力、命令时限与助手设置。shell 只在 URL Query 中使用 CSV，公开能力中使用数组。不再报告源码仓库 URL。

Server 连接的 `device_id` 稳定：OSS 使用持久客户端身份，Manager 使用已登记设备 UUID。该标识不授予访问权限；每次重连的 `connection_id` 会变化。本机浏览器配对还要求 HTTP 服务的 `ServerInfo.local_device_id` 与目标一致；中心服务、info 接口不可用或身份未知时不开放配对。

浏览器桌面、文件与终端连接共用真实客户端构建报告。`ServerInfo.ai_assistant.device_settings` 明确表示当前 HTTP 服务是否实现设备级 AI 设置更新，客户端不能根据设备 ID 是否存在来推断该能力。
