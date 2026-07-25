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
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

脚本走 `dump-openapi` 子命令，从路由注册离线导出规范——不连 DB / Redis / HTTP。规范通过临时文件交给 Kubb，生成结束后自动删除；仓库不再跟踪生成的 `openapi.json`。

::: tip
`vite-project/src/services/` 下的文件由 Kubb 生成——请勿手动修改。
:::
