---
layout: home

hero:
  name: LCXL Remote Desk
  text: AI-native remote desktop
  tagline: A high-performance, open-source WebRTC remote desktop that treats AI as a first-class control plane alongside the browser — security-first and model-agnostic.
  actions:
    - theme: brand
      text: Get Started
      link: /guide/introduction
    - theme: alt
      text: Quick Start
      link: /guide/quick-start
    - theme: alt
      text: View on GitHub
      link: https://github.com/lcxl-remote/lcxl-remote-desk-web

features:
  - icon: 🤖
    title: AI-Native by Design
    details: A built-in diagnostic agent reads device state and may propose commands. Owner-only commands run only after an explicit per-command confirmation; the server remains the sole authority on permissions and risk.
    link: /features/ai-diagnostics
    linkText: AI Diagnostics
  - icon: 🔌
    title: Read-Only MCP Server
    details: Expose the device's read capabilities to local AI assistants over the Model Context Protocol — a static whitelist with no execute, write, or control tools.
    link: /features/mcp-server
    linkText: MCP Server
  - icon: ⚡
    title: High-Performance Streaming
    details: WebRTC transport with AV1 / H.264 / VP8 / VP9 software & hardware encoding, plus Opus audio for ultra-low latency.
    link: /features/streaming
    linkText: Streaming
  - icon: 🔒
    title: Security-First
    details: Server-side authority, fail-closed redaction, server-only API keys, and metadata-only audit trails. Built for trust from the ground up.
    link: /security/ai-security-model
    linkText: Security Model
  - icon: 🖥️
    title: Terminal, Files & More
    details: Built-in xterm.js terminal, file management with a recycle bin, bidirectional clipboard sync, virtual displays, privacy screen, and a remote whiteboard.
    link: /features/terminal-files-clipboard
    linkText: Productivity Features
  - icon: 🦀
    title: Rust + React
    details: A Rust (Actix-Web) backend with multiple startup modes and a React + Vite + Tailwind frontend. Cross-platform across Linux, Windows, and macOS.
    link: /reference/architecture
    linkText: Architecture
---
