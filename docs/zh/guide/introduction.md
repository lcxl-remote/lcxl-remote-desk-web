# 介绍

**LCXL Remote Desk** 是一款 **AI 原生（AI-Native）**、开源、高性能的远程桌面。它把 AI 当作**与浏览器并列的一等控制端**：除了基于浏览器的远程控制，它还内置一个只读诊断 AI Agent，读取设备当前状态来排查问题，并通过一个只读 [MCP](https://modelcontextprotocol.io/) 服务把这些只读能力开放给外部 AI 助手。

后端使用 Rust (Actix-Web) 编写，前端使用 React + Vite + Tailwind CSS。

::: warning 免责声明
本项目目前处于早期开发阶段。代码库可能不稳定、存在未修复的 bug 或功能不完整。

**安全警告**：远程桌面技术涉及对计算机系统的深度访问。使用本项目时请确保你的网络环境安全。作者不对使用本项目造成的任何损害负责。
:::

## 为什么是 AI 原生？

AI 层是**安全优先、模型无关**的（兼容 OpenAI 与 Anthropic API）。其设计建立在几条不变量之上：

- **服务端是权限的唯一可信源**——控制端永远无法自报身份、范围或风险。
- **模型默认只给建议而非执行**——更高风险动作需经服务端中介的显式确认。
- **数据在传输前严格脱敏，且 fail-closed**——脱敏失败会在调用模型之前就阻断请求。
- **每次调用都被审计**（仅元数据），且 **API Key 仅存服务端**。

完整内容见 [AI 安全模型](/zh/security/ai-security-model)。

## 核心功能

- **AI 诊断**——用自然语言提问；系统采集只读状态（系统信息、进程、端口、日志），本地脱敏后发给模型分析并给出修复建议。
- **只读 MCP 服务**——`--startup-mode mcp-stdio` 向本地 AI 助手暴露一个静态白名单的只读工具集，无执行或写入权限。
- **高性能串流**——基于 WebRTC，支持 AV1 / H.264 / VP8 / VP9 编码与 Opus 音频。
- **远程终端**——内置 xterm.js 终端，支持完整 shell 交互。
- **文件管理**——上传、下载、删除，并带回收站机制。
- **剪贴板同步**——文本剪贴板双向同步。
- **远程白板**——在远端屏幕上绘制与批注（需 `tauri-app`）。
- **防窥屏**——远程操作期间锁定本地显示与输入（需 `tauri-app`）。
- **系统音频**——采集并同步远端音频播放。
- **多语言**——UI 与文档提供中英文。

## 接下来

- 新手？从[快速开始](/zh/guide/quick-start)起步。
- 想理解各组件？阅读[核心概念](/zh/guide/concepts)与[启动模式](/zh/guide/startup-modes)。
- 正式部署？见[部署](/zh/guide/deployment)。
- 评估安全性？跳到[安全](/zh/security/ai-security-model)章节。

## 许可证

本项目采用 [Apache-2.0](https://github.com/lcxl/lcxl-remote-desk-web/blob/main/LICENSE) 许可证。
