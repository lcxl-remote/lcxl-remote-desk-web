# 信令协议

信令通过 WebSocket 承载 SDP / ICE 交换及一组控制消息。共享协议模型位于 `signal-facade/`，信令服务器本身在 `signal/`。

## 消息类型

信令消息建模为 `signal-facade/src/model/signal.rs` 中的 `SignalingType` 枚举。每个变体有**唯一整数值**，并在 `signal/src/service.rs` 的 `handle_message` 中被穷尽处理——刻意**没有 `_ =>` 兜底**，因此编译器会强制确保每种类型都被处理。

## 鉴权

信令端点依调用者不同采用不同鉴权——见[信令鉴权](/zh/security/signaling-auth)。简言之：

- Desk Server → 信令 使用 WebSocket URL 查询串里的 token。
- Browser → 信令 使用 Actix-Session Cookie，**不带 token 参数**。

## 添加新信令类型

1. 在 `SignalingType` 中添加变体（带唯一整数值）。
2. 在 `handle_message` 中处理它——添加转发分支或专用匹配分支。
3. 更新前端：重新生成客户端并在前端 RTC hook 中添加 `onMessage` 处理程序。

跨切面清单见[模块地图](/zh/reference/modules#添加新信令类型)。
