# 2026-03-07 Fixing Input Blur and WebRTC Connection Leaks

## 原定计划 (Implementation Plan)

### 问题分析

#### 1. 焦点丢失导致按键未弹起
**原因**：在远程桌面控制期间，用户的按键（键盘或鼠标）按下后如果切换了页面、弹出了其他系统窗口（导致 `blur` 本地窗口失去焦点），前端无法收到原本预期的 `keyup` 或 `mouseup` 事件。由于被控端只收到了 `keydown`/`mousedown`，这就会导致按键一直处于“被按下”的异常状态。
**解决思路**：在 `use-desk-input.ts` 中维护一个当前所有已按下按键（包括键盘的 `keyCode` 和鼠标的 `button`）的集合。监听 `video` 元素及 `window` 的 `blur` (失去焦点) 事件，一旦触发 `blur`，则自动为集合中所有的按键合成并发送对应的 `keyup` / `mouseup` 事件，随后清空该集合。

#### 2. 页面切换时 WebRTC 连接未断开
**原因**：在组件 `desk-session.tsx` 和 Hook `use-desk-rtc.ts` 中，虽然 `use-desk-signaling.ts` 正确在卸载时清理了 WebSocket 连接，但是 `RTCPeerConnection` (WebRTC核心连接对象) 却没有在其对应的生命周期内显式地调用 `close()` 进行清理。这导致直接通过路由切换到其他页面时，底层的 WebRTC 音视频传输和数据通道仍然在后台保持连接。
**解决思路**：在 `use-desk-rtc.ts` 中补充生命周期的清理逻辑，当组件卸载 / hook 销毁时，自动调用 `peerConnection.current.close()` 释放资源。

#### 3. 后端 Server 和 Signal 信令服务器断连
**原因**：当页面直接切换时，前端触发 WebRTC 连接断开，随后后端 Server 监听到底层 WebRTC 断开，触发清除逻辑并尝试发送 `PrivateScreenStateChanged` 等状态信令广播给客户端。由于客户端已经断开，该信令未携带 `to_session_id`。而 Signal 服务器中的 `do_handle_signaling` 接收到缺失 `to_session_id` 的信令后返回了 Error。由于代码中使用了 `self.handle_message(text).await?` 导致 Err 被向上抛出，使得处理当前 Server 连接的 WebSocket 循环意外终止 (`break`)，最终引发了后端 Server 与 Signal 信令服务器断链。
**解决思路**：修改 `signal/src/service.rs` 中的 `do_handle_signaling` 循环，对于单条 `handle_message` 和 `binary` 处理返回的 Error，改为使用 `log::error` 记录日志，但不终止整个 WebSocket 循环，增强信令服务器的鲁棒性。
---

## 任务进度 (Task List)

- [x] Investigate how to track pressed keys and mouse buttons in `use-desk-input.ts`.
- [x] Implement `blur` event listener on video element to release pending keys/buttons.
- [x] Investigate where and how the WebRTC connection is managed during page navigation.
- [x] Implement cleanup logic to close WebRTC connection when the remote desktop component unmounts.
- [x] Investigate why backend server WebSocket drops when frontend navigates away.
- [x] Fix signal server `do_handle_signaling` to catch and log errors instead of crashing the WebSocket loop.
- [x] Write implementation plan in Chinese for user review.
- [x] Verify build and compilation.
---

## 执行记录 (Walkthrough)

### 修复按键焦点丢失导致未弹起的问题
通过在 `use-desk-input.ts` 中追踪当前按下的按键（键盘 `keyCode` 和鼠标 `button`），我们监听了 `video` 元素和 `window` 对象的 `blur` 事件。
当触发 `blur` 事件时，系统会自动将所有“未松开”的按键合成为对应的 `keyup` / `mouseup` 释放事件并通过 DataChannel 发送给受控端，从而避免由于焦点切换导致的按键长按或卡死现象。

1. **`pressedKeysRef`** 和 **`pressedButtonsRef`**: 新增的 Set 结构，用于分别记录键盘和鼠标当前的按下状态。
2. **`handleBlur`**: 在该函数中统一处理释放逻辑，并向 `keyboardChannel` 和 `mouseChannel` 发送相应的清空事件。
3. **事件绑定**: 绑定了 `blur` 监听以响应页面失焦、浏览器切后台等场景。

### 修复页面切换时 WebRTC 连接未断开的问题
问题的原因是在 React 组件卸载时，`RTCPeerConnection` 没有被正确销毁。由于 WebRTC 连接是底层的网络会话，如果不显式调用 `close()`，即使 DOM 元素不存在了，它依然会保持后台运行并占用前后端资源。

1. **清理逻辑 (`use-desk-rtc.ts`)**: 我们在 Hook 返回前添加了一个专门负责卸载清理的 `useEffect`。
2. 当组件因为路由切换（例如点击进入“系统配置”页面）而销毁时，该 Effect 会安全地调用 `peerConnection.current.close()`，重置 `isRTCConnected` 状态并将远端媒体流 `remoteStream` 置空。

### 测试与验证已完成
- 自动化运行了 `npm run build` 以确保新增状态变量没有破坏 Typescript 的类型规范，并且代码能够正确编译。

### 修复被控端 Server 与 Signal 信令服务断连的问题
当用户直接切换路由时，前端立即断开 WebRTC。后端 Server 检测到流断开，进行内部的隐私屏状态清理，并向 Signal 发送状态变更广播，但未附加 `to_session_id`。因 Signal 信令服务对缺失 `to_session_id` 采用了严格拦截（`DeskSignalError`），原先的错误向上抛出机制（`?` 语法糖）导致了 Signal 直接中断 Server 的 WebSocket 监听循环，直接踢掉了 Server。
我们在 `signal/src/service.rs` 的循环监听逻辑中，将 `self.handle_message(text).await?;` 和 `self.binary(bin).await?;` 改进为手动捕获错误并输出 log。这使得无论后端发送了多么畸形的信令帧（如缺失目标ID），它都只会在日志中报错，而绝不会崩溃循环导致重连。通过 `cargo check` 确保此后端逻辑编译完美通过。
