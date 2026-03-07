---
description: 引导式添加新信令类型
---

当需要新增跨端通信信令（Signaling）时，执行此工作流。

// turbo
1. **后端定义**
   - 修改 `signal-facade/src/model/signal.rs`。
   - 在 `SignalingType` 枚举中添加新常量。**必须**分配一个唯一的整数 ID。

2. **服务端适配**
   - 修改 `signal/src/service.rs` 中的 `handle_message`。
   - 根据 [.agents/rules/signaling-protocol.md](.agents/rules/signaling-protocol.md) 规则，将新类型加入 `match` 分支。
   - 提示：如果只是透传，请将其加入“Forwarding types”联合分支。

3. **前端同步**
   - 执行工作流 `/update_openapi` 以更新前端枚举。

4. **业务实现**
   - 询问用户需要在哪个前端 Hook/组件中处理此信令（默认为 `useDeskRTC.ts`）。
   - 在对应的 `onMessage` 处理器中增加处理逻辑。