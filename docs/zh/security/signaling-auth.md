# 信令鉴权

本项目存在多条采用**不同鉴权机制**的连接路径，绝不能混淆。

| 连接 | 鉴权方法 |
|---|---|
| Desk Server → 本地信令 | `settings.system.local_signaling_token`（自动生成，仅限 `default` 模式） |
| Desk Server → 远程信令 | WebSocket URL 中的 `?token=<settings.system.signaling_token>` |
| Desk Server → Manager | WebSocket URL 中的 `?token=<settings.system.manager_api_token>` |
| Browser → 信令 / Manager | **不带 token 参数。**仅使用 Actix-Session Cookie 鉴权。 |

## 说明

- **浏览器连接不带 token**，用 Actix-Session Cookie 鉴权。路由提取器必须用 `Option<web::Query<VersionInfo>>`，且 manager 信令路由要排除在全局 Session 中间件之外。
- 本地信令 token 自动生成，仅在 `default` 模式使用。
- 远程信令与 manager 的 token 通过 WebSocket URL 查询串传递。

这些机制刻意彼此独立；混用即为安全缺陷。
