# MCP 服务

运行 `--startup-mode mcp-stdio` 会把设备变成一个 [Model Context Protocol](https://modelcontextprotocol.io/) 服务，向本地 AI 助手提供一个**静态白名单的只读工具集**。

## 设计

- 基于官方 `rmcp` SDK，走 **stdio**。
- 暴露一个**静态白名单**的只读上下文工具——从结构上就**不存在执行、写入或控制类工具**（“未定义即不可达”）。
- **不存在 `lcxl_diagnose` 工具**：完整的 AI 诊断由中心信令大脑编排，而非由设备承担。MCP 暴露面是纯只读上下文，供本地 AI 助手使用；它从不拨号模型、也不抓屏。

## 为何单独一个模式？

MCP 暴露面刻意比会话内诊断 Agent 更窄。模型推理、屏幕采集及任何控制/执行类工具被完全排除，以将面向本地 AI 助手的攻击面降到最小。

## 运行

```bash
cargo run -- --startup-mode mcp-stdio
```

::: warning stdout 被保留
在 `mcp-stdio` 模式下，stdin/stdout 承载 MCP JSON-RPC。该模式下服务端**绝不能向 stdout 打日志**——否则会破坏协议流。
:::

## 接入 AI 助手

将任意支持 MCP 的客户端指向上述命令作为 stdio 服务。客户端会发现只读上下文工具白名单；没有任何额外配置能授予写入、执行或诊断权限，因为那些工具根本不存在。

另见 [AI 安全模型](/zh/security/ai-security-model)了解 MCP 暴露面如何契合整体信任边界。
