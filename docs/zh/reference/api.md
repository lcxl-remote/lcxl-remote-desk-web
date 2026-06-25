# REST API

后端 REST API 使用 [`utoipa`](https://github.com/juhaku/utoipa) 注解并暴露 OpenAPI 规范。交互式文档由运行中的 server 提供——它始终与构建保持一致。

## 交互式文档（server 运行时）

server 运行后，可在以下地址浏览 API：

- **Swagger UI**——`http://localhost:8081/swagger-ui/`
- **ReDoc**——`http://localhost:8081/redoc`
- **RapiDoc**——`http://localhost:8081/rapidoc`
- **Scalar**——`http://localhost:8081/scalar`

原始规范位于 `http://localhost:8081/openapi.json`。

## 重新生成前端客户端

前端客户端（`vite-project/src/services/`）由 [Kubb](https://kubb.dev/) 从 OpenAPI 规范生成。后端 API 变更后，重新生成它（离线 dump，无需运行中的 server）：

```bash
cd vite-project
# Windows:
.\update_openapi.ps1
# Linux/macOS:
./update_openapi.sh
```

脚本走 `dump-openapi` 子命令，从路由注册离线导出规范——不连 DB / Redis / HTTP。

::: tip
`vite-project/src/services/` 下的文件由 Kubb 生成——请勿手动修改。
:::
