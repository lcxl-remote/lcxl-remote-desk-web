---
description: 自动更新前端 OpenAPI 接口代码
---

当后端 OpenAPI 接口发生变更，需要同步更新前端代码时，请执行此工作流。此流程已优化为全自动化。

// turbo
1. **自动同步流程**
   Antigravity 将依次执行以下操作：
   - **检测后端状态**：检查 `8081` 端口是否已有服务运行。
   - **启动临时服务**（如果需要）：如果 `8081` 端口未占用，在 `server` 目录下执行 `cargo run`。等待服务启动并响应 `http://localhost:8081/openapi.json`。
   - **执行更新脚本**：在 `vite-project` 目录下执行系统对应的更新脚本：
     - **Windows (PowerShell)**: `.\update_openapi.ps1`
     - **Linux/macOS (Bash)**: `./update_openapi.sh`
   - **环境清理**：如果启动了临时后端，同步完成后将其安全关闭。

2. **验证结果**
   检查 `vite-project/src/api/` (或 Kubb 配置的输出目录) 下的文件是否已更新。
