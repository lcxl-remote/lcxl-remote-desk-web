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

## 能力受限会话（设备码 / 支援码）

控制端通过在连接框兑换一枚**访问码**连上在线被控端（`POST /api/desk/redeem-code`）——可以是长期的**设备码**，也可以是**支援码**（带 TTL 的设备码，为一次性协助按需签发）。兑换出的会话连的是被控端的**常规 live 连接**；**不再有**独立的「支援」上游。

由于被兑换的会话并非 owner，它是**能力受限**的，且由**被控端 fail-closed 强制**（不是信令服务器）：

- **per-code 能力上限随会话携带。**owner 可为每个码配置三态上限（允许 / 询问 / 拒绝），覆盖远程控制、剪贴板、私有屏、白板、终端、文件浏览、文件传输。未配置的码默认全询问，绝不默认全权。详见[访问码](/zh/guide/access-codes)。
- **最终生效权限是三方 meet。**每个动作，被控端先取「码上限」与自身全局 `[security]` 设置中更严者，若结果仍为「询问」则弹框请本地用户现场审批。任一处为**拒绝**即硬拒；任一维未配置一律弹框。（这取代了此前「剪贴板 / 文件传输 / 白板一律拒绝」的固定规则。）
- **特权信令按白名单放行。**受限会话上只放行会话建立与控制面帧；任何可能泄露被控端凭据（`signaling_token` / `manager_api_token`）的帧都在被控端信令门被拒。
- **会话有时限。**支援码带 TTL；被控端本地用户也可随时结束，且关闭只清理这一条连接。

签发支援码是**中心大脑（manager）的能力**——开源信令服务器只路由连接，**不签发**支援码。在纯开源信令服务器上，`Support` 角色只是普通的 routing-only（等同 `Browser`：无设备 presence、无特权）；而设备码兑换与上述能力受限强制无论如何都属于开源基线的一部分。

## 获取 host 令牌（`POST /api/tokens`）

已登录的客户端若要以 host（`remote_desk_type=server`）身份连接，通过 `POST /api/tokens`（凭会话 Cookie 调用）获取须出示的令牌。两端该端点的请求/响应形态一致，故客户端无需探测服务器类型：

- **开源 desk-server** 返回同机的 `local_signaling_token`（即内嵌 host worker 所用的密钥），忽略请求中的 `name`、不落库。仅在内嵌信令服务器的模式（`default` / `signaling` / `service-daemon`）注册；纯 `desk-server` 不提供它。
- **企业版 manager** 在令牌表中按用户签发一枚令牌。

## 说明

- **浏览器连接不带 token**，用 Actix-Session Cookie 鉴权。路由提取器必须用 `Option<web::Query<VersionInfo>>`，且 manager 信令路由要排除在全局 Session 中间件之外。
- `local_signaling_token` 是 host 凭据：日志中已脱敏（`SystemSettings` 的 `Debug` 输出会遮蔽它及其他密钥），绝不明文打印。
- 远程信令与 manager 的 token 通过 WebSocket URL 查询串传递。

这些机制刻意彼此独立；混用即为安全缺陷。
