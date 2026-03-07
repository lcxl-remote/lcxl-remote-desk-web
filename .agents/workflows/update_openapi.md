---
description: 自动更新前端 OpenAPI 接口代码
---

当后端 OpenAPI 接口发生变更，需要同步更新前端代码时，请执行此工作流。

1. **启动后端服务**
   请在 `server` 目录下启动后端服务。你需要确保服务正常运行并监听在 `8081` 端口（这是前端脚本期望的端口）。
   可以新开一个终端或在后台运行 `cargo run`。

// turbo
2. **执行更新脚本**
   等待后端服务启动成功后，在 `vite-project` 目录下执行 OpenAPI 更新脚本。
   命令：`./update_openapi.sh` (或在 Windows 下根据环境使用 `bash update_openapi.sh` 或 Git Bash 等)。

3. **停止后端服务**
   如果第一步是你专门为了生成 OpenAPI 而临时启动的后端服务，更新完成后请将其关闭。
