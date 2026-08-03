---
layout: home

hero:
  name: LCXL Remote Desk
  text: AI 原生远程桌面
  tagline: 一款高性能、开源的 WebRTC 远程桌面，把 AI 当作与浏览器并列的一等控制端——安全优先、模型无关。
  actions:
    - theme: brand
      text: 开始使用
      link: /zh/guide/introduction
    - theme: alt
      text: 快速开始
      link: /zh/guide/quick-start
    - theme: alt
      text: 在 GitHub 查看
      link: https://github.com/lcxl/lcxl-remote-desk-web

features:
  - icon: 🤖
    title: AI 原生设计
    details: 内置诊断助手可以读取设备状态并提出命令建议。只有设备所有者逐条确认后，命令才会执行；权限与风险等级始终由服务端判断。
    link: /zh/features/ai-diagnostics
    linkText: AI 诊断
  - icon: 🔌
    title: 只读 MCP 服务
    details: 通过 Model Context Protocol 把设备的只读能力开放给本地 AI 助手——静态白名单，无执行 / 写入 / 控制类工具。
    link: /zh/features/mcp-server
    linkText: MCP 服务
  - icon: ⚡
    title: 高性能串流
    details: 基于 WebRTC 传输，支持 AV1 / H.264 / VP8 / VP9 软硬件编码，配合 Opus 音频实现超低延迟。
    link: /zh/features/streaming
    linkText: 串流
  - icon: 🔒
    title: 安全优先
    details: 权限由服务端统一判断；脱敏失败会立即中止请求；API 密钥只保存在服务端；审计记录不含原始内容。
    link: /zh/security/ai-security-model
    linkText: 安全模型
  - icon: 🖥️
    title: 终端、文件与更多
    details: 内置 xterm.js 终端、带回收站的文件管理、双向剪贴板同步、虚拟显示器、防窥屏与远程白板。
    link: /zh/features/terminal-files-clipboard
    linkText: 实用功能
  - icon: 🦀
    title: Rust + React
    details: 后端采用 Rust 与 Actix Web，前端采用 React、Vite 和 Tailwind CSS，支持 Linux、Windows 与 macOS。
    link: /zh/reference/architecture
    linkText: 架构
---
