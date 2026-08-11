# REST API

后端 REST API 使用 [`utoipa`](https://github.com/juhaku/utoipa) 注解，并从路由注册生成 OpenAPI 规范。

## 运行时不再提供文档端点

server **不再**在运行时提供交互式 API 文档或原始规范：Swagger UI / ReDoc / RapiDoc / Scalar 端点以及 `/openapi.json` 均已移除。它们无需鉴权，在公网自建部署上只会把 API 攻击面暴露给任何人——而前端客户端是**离线**生成的（见下文），运行时规范并无用途。

如需查看规范，用离线的 `dump-openapi` 子命令在本地生成：

```bash
cargo run -p lcxl-remote-desk-server -- dump-openapi --out openapi.json
```

## 重新生成前端客户端

前端客户端（`vite-project/src/services/`）由 [Kubb](https://kubb.dev/) 从 OpenAPI 规范生成。后端 API 变更后，重新生成它（离线 dump，无需运行中的 server）：

```bash
cd vite-project
npm ci        # 装上 lockfile 钉死的那个 Kubb 版本
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

脚本走 `dump-openapi` 子命令，从路由注册离线导出规范——不连 DB / Redis / HTTP。规范通过临时文件交给 Kubb，生成结束后自动删除；仓库不再跟踪生成的 `openapi.json`。

Kubb 的版本被精确钉死，且以 `npx --no-install` 调用，因此生成器既不会跨 patch 版本漂移，也不会在依赖缺失时被悄悄下载——committed 的客户端始终能从 lockfile 复现。若重新生成时提示找不到 Kubb，先跑 `npm ci`。

::: tip
`vite-project/src/services/` 下的文件由 Kubb 生成——请勿手动修改。
:::

::: warning 重新生成不是可选步骤
`npm run build` 只跑 tsc 和 vite，从不重新生成客户端。所以后端改动了规范之后，陈旧的客户端照样能编译通过——而一个改掉的数值根本不会报错，只会继续被发出去。CI 每次 push 都会重新生成，并在结果与 committed 版本不一致时失败。
:::

## 认证契约

浏览器/控制端认证使用唯一的规范 JSON 接口面：

- `POST /api/auth/login`
- `POST /api/auth/logout`
- `GET /api/auth/me`
- `PATCH /api/auth/credentials`
- `POST /api/auth/tauri-login`（仅独立桌面被控端）
- `GET /api/init/requirements`（公开只读，只说明是否需要 bootstrap token）
- `POST /api/init`（初始化前公开，可选 bootstrap token gate）

所有响应都使用 `RestResponse`。登录和改密的凭据/业务失败保持 HTTP 200 且
`success=false`；`/api/auth/me` 在登录会话缺失或过期时返回 HTTP 401，但响应正文仍为
同一 JSON 包络。公开字段统一使用 snake_case。OAuth authorize/callback continuation
继续位于 `/api/oauth/*`，不属于这组基础认证路由。

账户登录失败仍使用 HTTP 200。`ILLEGAL_CREDENTIALS` 不区分用户名或密码；达到阈值时
返回 `ACCOUNT_LOCKED`，`LoginOutcomeDto.retry_after_sec` 给出向上取整的剩余锁定秒数；
独立服务器明确返回 `captcha_required=false`。bootstrap 校验失败使用
`PERMISSION_ERROR`，bootstrap/probe 配额耗尽使用 `TOO_MANY_ATTEMPTS`。

## 错误码

`DeskErrorCode`（`utils/src/error.rs`）由 `desk_error_codes!` 宏统一声明，宏同时产出常量与 `ALL` 名值表。该表以带 `x-enum-varnames` 的 int32 enum 形式发布进规范，于是生成的客户端提供具名成员 `deskErrorCodeEnum`——前端据此分支，不必再手写数值镜像。

该类型不被任何请求或响应体直接引用（`RestResponse.code` 在线路上只是一个整数），只有在 `server/src/openapi.rs` 中显式注册后才会进入规范。新增错误码时，需要先在宏清单中添加一项，再重新生成客户端。

前端的「码 → 文案」映射统一走 `src/lib/desk-error-i18n.ts`，各个领域各自维护一张只含自己会收到的码的小表。未命中的码怎么兜底由调用方决定：显示后端 `message`，或显示一句本地化的通用提示。每次构建前都会跑 `verify-error-codes` 检查，把写成裸数字的错误码拦下来，确保生成的常量是唯一来源。
