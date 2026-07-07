# 信令鉴权

本项目存在多条采用**不同鉴权机制**的连接路径，绝不能混淆。

| 连接 | 鉴权方法 |
|---|---|
| Desk Server → 本地信令 | `settings.system.local_signaling_token`（自动生成；凡内嵌信令服务器的模式都会生成：`default` / `signaling` / `service-daemon`） |
| Desk Server → 远程信令 | WebSocket URL 中的 `?token=<settings.system.signaling_token>` |
| Desk Server → Manager | WebSocket URL 中的 `?token=<settings.system.manager_api_token>` |
| Browser → 信令 / Manager | **不带 token 参数。**仅使用 Actix-Session Cookie 鉴权。 |

## 角色裁定

信令服务器自行裁定每条连接的 `remote_desk_type`——**绝不信任**客户端自报的角色：

- 出示**有效**节点 token 的连接，按其自报角色鉴权（被控端 host 自报 `server`）。
- 出示**非空但无效** token 的连接直接 **401** 拒绝——**不会**被静默降级为 cookie/Browser 会话。这样 token 失效的客户端能清缓存并重新签发，而不是以匿名 Browser 身份空转。
- **不带 token** 的连接走会话 Cookie 鉴权，且**一律为 `browser`**，无论自报什么角色。

开源信令服务器与企业版 manager 遵循同一套契约。

## Support 上游（临时支援）

被控端可以在 **`Support`** 角色下另开一条专用上游，让**无账号的第三方**被支援者协助（TeamViewer QuickSupport 式的流程）。该上游复用被控端已有的 `manager_api_token`；它**不注册**任何东西（无设备 presence），只承载这条支援会话。

由于临时支援者只是半信任方，凡是从 Support 上游到达的入站连接都**天生受限**，且由**被控端 fail-closed 强制**（不是信令服务器）：

- **信令按白名单放行。**受限连接上只接受会话建立与控制面帧；任何特权帧（设置查询，以及一切可能泄露 `signaling_token` / `manager_api_token` 的帧）都在单一 `route()` 收口处被丢弃。
- **数据通道动作同样按白名单放行。**剪贴板、文件传输、白板一律拒绝；鼠标 / 键盘输入仍需经过与普通远程控制相同的本地控制审批弹窗。
- **会话有时限。**支援码过期或本地用户结束支援时即拆链，且关闭只清理这一条连接。

签发支援者兑换的一次性码是**中心大脑（manager）的能力**——开源信令服务器只路由 `Support` 角色，**不签发**支援码。因此在纯开源信令服务器上，被控端的 Support 上游没有码签发者；而上述受限会话的强制约束无论如何都属于开源基线的一部分。

## 获取 host 令牌（`POST /api/tokens`）

已登录的客户端若要以 host（`remote_desk_type=server`）身份连接，通过 `POST /api/tokens`（凭会话 Cookie 调用）获取须出示的令牌。两端该端点的请求/响应形态一致，故客户端无需探测服务器类型：

- **开源 desk-server** 返回同机的 `local_signaling_token`（即内嵌 host worker 所用的密钥），忽略请求中的 `name`、不落库。仅在内嵌信令服务器的模式（`default` / `signaling` / `service-daemon`）注册；纯 `desk-server` 不提供它。
- **企业版 manager** 在令牌表中按用户签发一枚令牌。

## 说明

- **浏览器连接不带 token**，用 Actix-Session Cookie 鉴权。路由提取器必须用 `Option<web::Query<VersionInfo>>`，且 manager 信令路由要排除在全局 Session 中间件之外。
- `local_signaling_token` 是 host 凭据：日志中已脱敏（`SystemSettings` 的 `Debug` 输出会遮蔽它及其他密钥），绝不明文打印。
- 远程信令与 manager 的 token 通过 WebSocket URL 查询串传递。

这些机制刻意彼此独立；混用即为安全缺陷。
